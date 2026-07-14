# Neolink — homelab fork

Forked from https://github.com/QuantumEntangledAndy/neolink. All local changes live on branch `homelab-frigate`.

## Why we maintain this fork

Reolink RLC-820A cameras produce non-monotonic DTS on their h264 main RTSP stream (~16% of packets have duplicate timestamps from multi-NAL access units). Chrome's VideoToolbox refuses to play recordings made from this with `PIPELINE_ERROR_DECODE` / `-12909`. Neolink re-muxes via GStreamer, producing clean monotonic DTS — but upstream had a handful of bugs that broke Frigate's multi-client pattern.

## Changes vs upstream

Local patches (originally squashed when the fork was vendored into homelab-apps; they now live as normal commits on branch `homelab-frigate` in this repo, and CI publishes the built binary as the `homelab-frigate` release asset):

- **`crates/core/src/bc_protocol/connection/bcconn.rs`**: BC protocol per-message channel size `100` → `10000`. Prevents ping-reply starvation when a downstream GStreamer appsrc briefly stalls.
- **`src/rtsp/factory.rs`** `send_to_sources`: fps=0 fallback `.max(1)` replaced with `if stream_config.fps > 0 { stream_config.fps } else { 25 }`. The old code produced 1s-per-frame timestamps during the initial buffered replay before fps was learned — catastrophic for downstream muxers.
- **`src/rtsp/gst/factory.rs`**: `factory.set_shared(false)` → `set_shared(true)`. One GStreamer pipeline fans out to all RTSP clients; without this, every new client spawns a new Baichuan subscription and hits the camera's concurrent-session limit.
- Observability (also in `src/rtsp/factory.rs`): per-client task exit log, `check_live` failure log, and a GStreamer bus watch (registered via `connect_media_configure`, where the media pipeline and a real bus exist) that logs Error/Warning/EOS tagged by stream.
- **`src/rtsp/factory.rs`** per-client `aud_ts` / `vid_ts` widened from `u32` to `u64`. The u32 µs counter wrapped at `2^32 µs = 71 min 35 s`, producing a backwards PTS jump that stalled hardware-accel H.264 decoders downstream (QSV libmfx futex wait ~25 s; VAAPI hwdownload `Failed to sync surface`). Observed 72.4-min cycle on RLC-820A @ ~24.7 fps. Signature change required `duration as u64` / `(…) as u64` casts at the two increment sites.
- **`crates/core/src/bc_protocol/connection/bcconn.rs`** poller task: the receive task wrapped `poller.run()` in `loop { if Err return }`, so an `Ok(())` return re-ran `run()`. `run()` returns `Ok(())` only when its `PollCommand` stream has closed, after which `reciever.next()` yields `Ready(None)` instantly and forever — the task busy-spun one core at 100 % in userspace with no syscalls, and because the spinning `select!` branch never returned `Pending`, the `cancelled()` branch could never be polled so `BcConnection::drop`'s `cancel()` couldn't stop it (observed: neolink pegged for hours on the Frigate LXC). Replaced the loop with `v = poller.run() => v` so the task ends when the stream closes.
- **`src/rtsp/gst/factory.rs`**: `RTSPSuspendMode::Reset` → `RTSPSuspendMode::None`. With `Reset`, any camera network blip that caused Frigate's ffmpeg to time out and disconnect would tear down the shared RTSPMedia pipeline. The data-feeding thread would then pick up frames from the reconnected camera, call `check_live`, get "App source is closed" (pipeline bus is None), and exit permanently. Every subsequent Frigate reconnect got a dead or stale pipeline, requiring a neolink container restart to recover. With `None` the pipeline is never suspended: camera drops are absorbed by the feed thread blocking on `media_rx` and resuming when data flows again; Frigate reconnects get the live running pipeline immediately.
- **`src/rtsp/factory.rs`** `send_to_appsrc`: DTS/PTS are stamped through a per-appsrc `MonotonicTs` guard so they can never move backwards. gst-rtsp-server queues RTP buffers for a slow TCP client on a per-transport backlog, and `gst_rtsp_stream_transport_backlog_push` asserts `queue_duration >= 0` (`rtsp-stream-transport.c`, still present on upstream master): a buffer entering the backlog may not predate the buffer at its head. It is a `g_assert`, so a violation aborts the whole process instead of dropping the slow client. Live appsrcs stamp buffers with the pipeline running time (`clock - base_time`), and GStreamer hands the media a fresh `base_time` each time it re-enters PLAYING, which restarts that running time near zero; if a client is holding a backlog at that moment (Frigate's ffmpeg stalled writing segments to the NFS recording mount) the next buffer lands below the backlog head and neolink dies with `Bail out! ... assertion failed: (queue_duration >= 0)`. On a backwards step the guard re-anchors its offset so stamps carry on from the last value at the source's own rate; clamping instead would pin the timestamp for as long as the pipeline had already been running. Observed on the Frigate LXC at 2026-07-14 01:05, where `restart: unless-stopped` recovered neolink ~1 s later.

### Feed-thread resilience pass

`SuspendMode::None` + `set_shared(true)` mean the shared pipeline is built once and lives for the life of the process, so the single feed thread is the only thing keeping it fed: any way it can stop becomes a permanent, health-check-green silent outage that only an external watchdog restart clears. This pass removes every way it can stop.

- **`src/rtsp/factory.rs`** `send_to_sources`: a malformed/short AAC or ADPCM frame used to `.expect()` on `duration()` and panic the feed thread (permanent outage). Now logs and skips the frame. (`aac.duration()` returns `None` on a non-ADTS or <8-byte payload; the deserialiser does no validation, so a camera desync after a reconnect can reach this.)
- **`src/rtsp/factory.rs`** `make_factory`: the feed is now a **supervised loop**. Per-frame send errors are logged and skipped (never fatal). If `media_rx` closes (the camera stream task ended non-retryably), it re-acquires `stream_while_live` after a short backoff and keeps feeding the same appsrcs, instead of leaving a dead pipeline. The blocking send work moved from a detached `std::thread` to an awaited `spawn_blocking` so its exit is observable and recoverable.
- **`src/rtsp/factory.rs`** `make_factory`: the frame-learning loop is wrapped in a `tokio::time::timeout`. An offline camera at first connect used to block RTSP media construction (`blocking_recv`) forever; now it fails the DESCRIBE cleanly so the client retries.
- **`src/rtsp/factory.rs`** `StreamConfig`: clamp `fps` `0 → 25` in `new()` and `update_fps()`. The PTS path already guarded this, but the four `set_min_latency(1000 / fps)` build sites would divide-by-zero panic on a stream that reported `fps == 0` before any `InfoV1/V2` frame.
- **`crates/core/src/bc_protocol/connection/tcpsource.rs`**: enable TCP keepalive (20s idle / 10s interval) on the camera socket, so a silent half-open link (power-cut / AP blip with no FIN/RST) surfaces as a read error and drives a reconnect instead of wedging the reader task forever (upstream issue #229).
- **`src/common/camthread.rs`**: the ping watchdog used to `futures::future::pending().await` forever on the first `UnintelligibleReply` (camera doesn't support `get_linktype`), disabling liveness detection. Now it `continue`s so a later half-open socket still trips the existing ping-timeout → reconnect path.
- **`crates/core/src/bcmedia/model.rs`**: `BcMediaAdpcm::block_size()` uses `saturating_sub(4)` and `duration()` returns `None` for <4-byte frames, avoiding a `u32` underflow.
- **`crates/core/src/bc_protocol/connection/bcconn.rs`** `Poller::run` (upstream PR #399): when a subscriber channel is full, `try_send` (drop one frame) instead of a blocking `send().await`. The poll loop also routes keepalive replies, so blocking it risked a keepalive timeout and a full session reconnect; dropping a frame is a sub-GOP gap. Complements the existing `100 → 10000` buffer bump.

## CI

Two workflows, deliberately split:

- **`checks.yml`** runs `cargo fmt --check`, `cargo clippy` and `cargo test` on every PR into `homelab-frigate` and on every push to it. It holds `contents: read`, so it can report but never ship. It builds in `rust:slim-bookworm`, the same base as the image, so the code is compiled against the GStreamer the container actually runs (1.22) rather than whatever version a laptop happens to have.
- **`homelab-image.yml`** builds the image, pushes `ghcr.io/kempson/neolink:homelab-frigate`, and replaces the `homelab-frigate` rolling release that `bootstrap.sh` downloads. It runs **only** on push to `homelab-frigate`, because every step of it changes what is deployed.

So: open a PR against `homelab-frigate`, let the checks run, then merge. Merging is what deploys.

Clippy is not run with `-D warnings`. Upstream's own code trips a handful of style lints, and denying them would mean editing files we otherwise leave alone, growing the diff we carry across each upstream rebase. Clippy's correctness lints are deny-by-default, so a real-bug lint still fails the build.

The unit tests in `src/rtsp/factory.rs` guard the invariants these patches depend on. They matter more than their size suggests: upstream does not know these patches exist, so a rebase can silently break one, and at least one of them (monotonic timestamps) fails by aborting the process on the live CCTV box.

## Build (local iteration)

Uses a persistent builder container so cargo's `target/` cache persists across iterations. First build ~5 minutes; incremental ~30–60s.

Run from a clone of this repo. The source is no longer vendored into homelab-apps.

```bash
# One-time setup (from the root of this repo)
docker run -d --platform linux/amd64 --name neolink-builder \
  -v "$PWD:/src" -w /src \
  rust:slim-bookworm sleep infinity
docker exec neolink-builder bash -c 'apt-get update -qq && apt-get install -y -qq \
  build-essential openssl libssl-dev ca-certificates \
  libgstrtspserver-1.0-dev libgstreamer1.0-dev libgtk2.0-dev \
  protobuf-compiler libglib2.0-dev'

# Every iteration
docker exec -d neolink-builder bash -c 'cargo build --release > /tmp/cargo.log 2>&1'
docker exec neolink-builder tail -f /tmp/cargo.log | grep -E "error|Finished"
```

Binary lands at `target/release/neolink`.

## Deploy to Frigate LXC (container 111 on node4)

```bash
# Direct SSH (usually works)
scp target/release/neolink frigate:/tmp/neolink-bin
ssh frigate 'chmod +x /tmp/neolink-bin && docker cp /tmp/neolink-bin neolink:/usr/local/bin/neolink && docker restart neolink'

# If SSH to frigate times out (VPN), hop via node1 → node4 → pct push
scp target/release/neolink node1:/tmp/neolink-bin
ssh node1 'scp /tmp/neolink-bin node4:/tmp/ && ssh node4 "pct push 111 /tmp/neolink-bin /tmp/neolink-bin && pct exec 111 -- bash -c \"chmod +x /tmp/neolink-bin && docker cp /tmp/neolink-bin neolink:/usr/local/bin/neolink && docker restart neolink\""'
```

The `docker cp` above is **ephemeral**: it patches the running container's
filesystem but is lost on any container recreation (`docker-compose up -d` with a
config change wipes it). Fine for a quick test; use the durable bake below for a
real deploy.

The neolink container runs image `neolink:patched`. **Re-bakes must build on top
of `neolink:patched`, not from `neolink:original`.** `apps/frigate/scripts/bootstrap.sh` creates
`neolink:original` from upstream and uses the `docker create` / `docker cp` /
`docker commit` dance for the *first* bake; that works only because upstream
carries no anonymous VOLUME on `/etc/neolink.toml`. The `commit` bakes that VOLUME
into `:patched` (it was committed from a container that bind-mounted the config),
so every subsequent re-bake that instantiates a container from `:patched` fails
with `cannot mount volume over existing file`. On a deployed host only
`neolink:patched` and `neolink:prev` remain, so `docker rmi neolink:patched`
followed by a rebuild from `:original` would also destroy the only working image.
Use the COPY-only build below instead.

## Deploy properly (durable bake)

```bash
# Run on the Frigate LXC, or remotely via `ssh frigate ...`, after staging the
# binary at /tmp/neolink-bin (scp target/release/neolink frigate:/tmp/neolink-bin).

# 1. Tag the current working image as a rollback point. Per-bake timestamp so a
#    second bake on the same day can't clobber an earlier known-good rollback tag.
docker tag neolink:patched "neolink:rollback-$(date +%Y%m%d-%H%M%S)"

# 2. Bake the new binary into a fresh neolink:patched via a COPY-only build.
#    Do NOT use `docker create`/`docker commit`, and do NOT add a `RUN` step:
#    the image carries an anonymous VOLUME at /etc/neolink.toml (it was committed
#    from a container that bind-mounted the config there), so ANY container
#    instantiation fails with "cannot mount volume over existing file". A plain
#    `docker build` with FROM+COPY never instantiates a container, and COPY keeps
#    the source file's mode, so no `RUN chmod` is needed in the Dockerfile.
mkdir -p /tmp/nlbake && cp /tmp/neolink-bin /tmp/nlbake/neolink-bin && chmod +x /tmp/nlbake/neolink-bin
printf 'FROM neolink:patched\nCOPY neolink-bin /usr/local/bin/neolink\n' > /tmp/nlbake/Dockerfile
docker build -t neolink:patched /tmp/nlbake

# 3. Recreate the container. The config bind-mount satisfies /etc/neolink.toml,
#    so recreation works even though a bare instantiation would not. The host
#    uses docker-compose v1 (`docker-compose`, not `docker compose`).
cd /opt/frigate && docker-compose up -d --force-recreate neolink
```

### Verify

neolink runs with `network_mode: host`, so its RTSP server is on the LXC host at
`192.168.0.35:18554`. Decode a few frames with Frigate's bundled ffmpeg to confirm
the media path works (4K h264; cameras `garden` / `front`, both RLC-820A). The
ffmpeg path is versioned (`/usr/lib/ffmpeg/<ver>/bin`), so discover it rather than
hardcoding:

```bash
docker exec neolink sha256sum /usr/local/bin/neolink   # matches the new binary
docker inspect neolink --format 'restarts={{.RestartCount}} status={{.State.Status}}'
FF=$(docker exec frigate bash -lc 'ls -d /usr/lib/ffmpeg/*/bin/ffmpeg | tail -1')
docker exec frigate "$FF" -rtsp_transport tcp \
  -i rtsp://192.168.0.35:18554/garden/main -frames:v 60 -f null -
```

Expect 60 frames decoded with no corruption errors and `restarts=0`. A one-second
burst of AAC non-monotonic-DTS warnings in the Frigate log at neolink startup is
the normal buffered-replay artefact, not an ongoing fault.

## Rollback

Pick the saved rollback tag (`docker images neolink`), re-point `:patched` at it,
and recreate:

```bash
docker tag neolink:rollback-YYYYMMDD-HHMMSS neolink:patched
cd /opt/frigate && docker-compose up -d --force-recreate neolink
```

`neolink:prev` is an older baked image kept for the same purpose. Reverting to
stock `quantumentangledandy/neolink` is a last resort only: it reintroduces the
non-monotonic-DTS bug this fork exists to fix.

## Neolink config: `stream = "Both"` per camera

To serve both main and sub streams from a single camera entry, use `stream = "Both"` (TOML, alias of `both`). This makes neolink subscribe to both Baichuan streams and expose them under `/<name>/main` and `/<name>/sub`:

```toml
[[cameras]]
name = "garden"
username = "admin"
password = "…"
address = "192.168.50.100:9000"
stream = "Both"
```

Sub streams go through the same clean-DTS GStreamer remux as main. Costs 2 Baichuan sessions per camera instead of 1 (Reolink RLC-820A supports ≥6 concurrent, plenty of headroom). Benefit: consistent output to downstream consumers — Frigate's detect ffmpeg gets the same monotonic timestamps as record.

## Frigate config: every path goes through neolink (not go2rtc restream)

In `/mnt/cloudnode/cctv/config/config.yaml` on the Frigate LXC, both `record` and `detect` roles pull **directly from neolink**, not via go2rtc's restream:

```yaml
go2rtc:
  streams:
    garden:
      - rtsp://192.168.0.35:18554/garden/main
    garden_sub:
      - rtsp://192.168.0.35:18554/garden/sub

cameras:
  garden:
    live:
      streams:
        Garden: garden
    ffmpeg:
      inputs:
        - path: rtsp://192.168.0.35:18554/garden/main
          input_args: preset-rtsp-generic
          hwaccel_args: preset-vaapi
          roles:
            - record
            - detect
```

Why: Frigate's ffmpeg processes periodically error (timestamp hiccups on any RTSP source) and the watchdog restarts them. When any role is routed via `rtsp://127.0.0.1:8554/<cam>` (go2rtc restream), every ffmpeg restart removes the last consumer from go2rtc → go2rtc stops its producer → TEARDOWN to neolink → neolink's shared RTSPMedia unprepares → ~24-second cycle of 503 DESCRIBE responses during rebuild. Pulling directly from neolink for every Frigate role breaks that feedback loop; ffmpeg restarts no longer affect go2rtc's connection to neolink. go2rtc's stream entries still point at neolink so the browser MSE live view works (consumer set is stable when the browser is open).

//! Backtrace on abort.
//!
//! GLib turns a failed `g_assert` into `abort()`, and the only thing that
//! reaches the log is the assertion text. gst-rtsp-server's backlog assertion
//! in `rtsp-stream-transport.c` carries no logging of its own, and its debug
//! category (`rtspmediatransport`) has no log statements at any level, so the
//! text alone cannot say which element pushed the offending buffer. A
//! backtrace separates a buffer arriving from the payloader on the normal path
//! from one replayed by retransmission, which is the open question.

use std::backtrace::Backtrace;

/// How long the handler may take before the kernel ends the process anyway.
const HANDLER_DEADLINE_SECS: libc::c_uint = 5;

/// Capturing a backtrace allocates and takes locks, so it is not
/// async-signal-safe and can deadlock against whatever the aborting thread
/// held. A hung process is worse than a crashing one here, because the
/// container restart policy only fires once the process actually exits, so
/// arm SIGALRM first. The kernel delivers that whatever this thread is doing,
/// which makes death within the deadline unconditional.
extern "C" fn on_abort(_signum: libc::c_int) {
    unsafe { libc::alarm(HANDLER_DEADLINE_SECS) };

    let backtrace = Backtrace::force_capture();
    eprintln!("neolink: aborting, backtrace follows\n{backtrace}");

    // SA_RESETHAND has already restored the default disposition, so re-raising
    // ends the process with the original signal and the usual exit status.
    unsafe { libc::raise(libc::SIGABRT) };
}

/// Arrange for a native backtrace to be printed when the process aborts.
///
/// Reports whether the handler was installed. A failure is not worth ending
/// startup over, because losing crash diagnostics still leaves a working
/// stream.
pub(crate) fn install_abort_backtrace() -> bool {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = on_abort as libc::sighandler_t;
    action.sa_flags = libc::SA_RESETHAND;

    unsafe {
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(libc::SIGABRT, &action, std::ptr::null_mut()) == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_a_successful_install() {
        assert!(install_abort_backtrace());
    }

    #[test]
    fn installing_twice_still_succeeds() {
        assert!(install_abort_backtrace());
        assert!(install_abort_backtrace());
    }

    /// The handler is only worth shipping if it survives a real abort, so this
    /// re-runs itself in a child process that installs it and calls `abort`,
    /// then reads the child's stderr. Installing the handler in the test
    /// process itself would leave it armed for every later test.
    #[test]
    fn prints_a_backtrace_on_a_real_abort() {
        const MARKER: &str = "NEOLINK_CRASH_TEST_CHILD";

        if std::env::var_os(MARKER).is_some() {
            install_abort_backtrace();
            unsafe { libc::abort() };
        }

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "crash::tests::prints_a_backtrace_on_a_real_abort",
                "--exact",
                "--nocapture",
            ])
            .env(MARKER, "1")
            .output()
            .expect("child test process should start");

        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("aborting, backtrace follows"),
            "child stderr carried no backtrace banner: {}",
            stderr
        );
        assert!(
            !output.status.success(),
            "child should have died from the abort"
        );
    }
}

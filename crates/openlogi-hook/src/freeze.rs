//! Notifying a host process about a freeze-hazard exit about to happen.
//!
//! The macOS watchdogs `std::process::exit` the agent when the tap thread (or
//! the tap callback) stops making progress, because only process death lets
//! macOS tear a wedged HID tap down. That exit is not a crash signal: no
//! report is generated and nothing in the process observes it — the menu-bar
//! item and remapping simply stop. A handler registered here runs first and
//! can record why, and the host decides what to do about it (the agent writes
//! a marker the next startup reports, and respawns itself in development).

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, OnceLock};

/// Why the freeze-hazard watchdog is force-exiting the process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FreezeExitReason {
    /// The tap thread stopped marking progress while the tap stayed active
    /// (or during arming / a capability probe) past its budget.
    TapThreadStalled,
    /// A stop was requested but the tap thread never reported teardown.
    StopTimedOut,
    /// The tap callback stayed entered past its budget; the callback owns the
    /// active HID tap, so nothing else can make progress.
    CallbackStuck,
}

impl FreezeExitReason {
    /// Stable machine-readable name, for markers and reports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TapThreadStalled => "tap_thread_stalled",
            Self::StopTimedOut => "stop_timed_out",
            Self::CallbackStuck => "callback_stuck",
        }
    }
}

/// A freeze-hazard exit about to happen, handed to the registered handler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FreezeExit {
    /// Which watchdog decided to exit.
    pub reason: FreezeExitReason,
    /// The last blocking call the tap thread entered (`TapSegment`'s name).
    pub segment: &'static str,
    /// Uptime in milliseconds since the stall began.
    pub elapsed_ms: u64,
    /// Whether an explicit hook stop was in flight. A stop that timed out is
    /// still a freeze exit, but the host must not treat the process as
    /// eligible for automatic respawn: something asked it to shut down.
    pub stop_requested: bool,
}

/// A freeze-exit handler is already registered.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("a freeze-exit handler is already registered")]
pub struct FreezeExitHandlerInstalled;

type Handler = Arc<dyn Fn(FreezeExit) + Send + Sync>;

static HANDLER: OnceLock<Handler> = OnceLock::new();

/// Register the callback run immediately before a freeze-hazard exit.
///
/// The handler runs on the watchdog thread that decided to exit, must return
/// quickly (the process is about to die, but nothing awaits it either), and
/// may be invoked at most once per process because the watchdogs exit right
/// after. Its panic is caught so it cannot unwind past the `exit` call and
/// leave a wedged tap alive. Returns [`FreezeExitHandlerInstalled`] when a
/// handler was already registered.
pub fn set_freeze_exit_handler(
    handler: impl Fn(FreezeExit) + Send + Sync + 'static,
) -> Result<(), FreezeExitHandlerInstalled> {
    HANDLER
        .set(Arc::new(handler))
        .map_err(|_| FreezeExitHandlerInstalled)
}

/// Run the registered handler for `exit`, if any. Never panics.
pub(crate) fn notify(exit: FreezeExit) {
    let Some(handler) = HANDLER.get() else {
        return;
    };
    let _ = catch_unwind(AssertUnwindSafe(|| handler(exit)));
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn freeze_exit_reason_names_are_stable_and_distinct() {
        assert_eq!(
            FreezeExitReason::TapThreadStalled.as_str(),
            "tap_thread_stalled"
        );
        assert_eq!(FreezeExitReason::StopTimedOut.as_str(), "stop_timed_out");
        assert_eq!(FreezeExitReason::CallbackStuck.as_str(), "callback_stuck");
    }

    #[test]
    fn handler_registration_invocation_and_rejection_are_safe() {
        // Registration is process-global, so this one test owns the sequence:
        // notify before registration, the first registration, a delivered
        // exit, and the rejected duplicate.
        static RECEIVED: Mutex<Option<FreezeExit>> = Mutex::new(None);

        notify(FreezeExit {
            reason: FreezeExitReason::TapThreadStalled,
            segment: "RunLoopSlice",
            elapsed_ms: 1_500,
            stop_requested: false,
        });
        assert_eq!(*RECEIVED.lock().unwrap(), None, "no handler, no delivery");

        set_freeze_exit_handler(|exit| {
            *RECEIVED.lock().unwrap() = Some(exit);
        })
        .expect("the first registration wins");

        let sent = FreezeExit {
            reason: FreezeExitReason::CallbackStuck,
            segment: "RunLoopSlice",
            elapsed_ms: 205,
            stop_requested: false,
        };
        notify(sent);
        assert_eq!(*RECEIVED.lock().unwrap(), Some(sent));

        assert_eq!(
            set_freeze_exit_handler(|_| {}),
            Err(FreezeExitHandlerInstalled),
            "the second registration is rejected, not swapped in"
        );
    }
}

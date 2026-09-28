//! Making the sender thread behave like the real-time task it is.
//!
//! # Why this exists
//!
//! The buffered pipeline sends one AAC frame roughly every 23 ms, and each one
//! carries a play deadline the receiver enforces. Arriving late is not a
//! slowdown, it is a dropout: the receiver has nothing to play at the instant
//! it was told to play it, and the gap is audible.
//!
//! A normal-priority thread on a busy desktop is at the mercy of everything
//! else running. A compile, a browser tab, a game launcher -- anything that
//! saturates the CPU can delay a wake-up past the deadline, and the user
//! blames the network, or us.
//!
//! # What this does about it
//!
//! On Windows, MMCSS ("Multimedia Class Scheduler Service") is what every
//! audio application uses for exactly this. Registering a thread under the
//! "Pro Audio" task gets it scheduled ahead of ordinary work, with the
//! scheduler still guaranteeing a slice to everything else -- so it cannot
//! wedge the machine the way a raw real-time priority can.
//!
//! Elsewhere this is a no-op for now. Linux wants `SCHED_FIFO`, which needs
//! either privileges or an `RLIMIT_RTPRIO` the user has granted, so it belongs
//! with the rest of the Linux work rather than being half-done here.

/// A thread's raised scheduling priority, restored when this is dropped.
///
/// Held rather than fire-and-forget because MMCSS registration is per-thread
/// and wants unregistering: a thread that exits while still registered leaves
/// the scheduler holding a characteristics handle for a thread that is gone.
#[derive(Debug)]
pub struct RealtimePriority {
    #[cfg(windows)]
    handle: Option<windows::Win32::Foundation::HANDLE>,
    /// What actually happened, for logging and for tests.
    outcome: Outcome,
}

/// Whether the thread was actually promoted, and if not why not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Registered with the platform's audio scheduler.
    Raised,
    /// The platform has nothing we can use without privileges we do not have.
    Unsupported,
    /// The call was made and refused.
    Failed(String),
}

impl Outcome {
    pub fn raised(&self) -> bool {
        matches!(self, Outcome::Raised)
    }
}

impl RealtimePriority {
    pub fn outcome(&self) -> &Outcome {
        &self.outcome
    }
}

/// Raise the calling thread to audio priority for as long as the returned
/// guard lives.
///
/// Never fails outward: priority is an optimisation, and a stream that
/// refused to start because the scheduler said no would be a far worse
/// outcome than one that runs at normal priority. The result is logged and
/// carried on the guard for anyone who wants to report it.
pub fn raise_current_thread() -> RealtimePriority {
    let guard = platform_raise();
    match guard.outcome() {
        Outcome::Raised => tracing::debug!("sender thread raised to audio priority"),
        Outcome::Unsupported => {
            tracing::debug!("no audio thread priority available on this platform")
        }
        Outcome::Failed(why) => {
            tracing::warn!("could not raise the sender thread to audio priority: {why}")
        }
    }
    guard
}

#[cfg(windows)]
fn platform_raise() -> RealtimePriority {
    use windows::core::w;
    use windows::Win32::System::Threading::{AvSetMmThreadCharacteristicsW, AvSetMmThreadPriority};
    use windows::Win32::System::Threading::{AVRT_PRIORITY, AVRT_PRIORITY_HIGH};

    // The task index is an out-parameter MMCSS fills in; we have no use for
    // it, but the call requires somewhere to put it.
    let mut task_index: u32 = 0;
    // SAFETY: `w!` gives a null-terminated wide string with static lifetime,
    // and `task_index` is a valid writable u32 for the duration of the call.
    let handle = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) };

    match handle {
        Ok(handle) => {
            // HIGH rather than CRITICAL: we wake on a timer roughly every
            // 23 ms and then sleep, which is not the sub-millisecond duty
            // cycle CRITICAL exists for, and taking it would be rude to the
            // actual audio device driver on the same machine.
            // SAFETY: `handle` is the live characteristics handle just
            // returned by MMCSS.
            if let Err(e) =
                unsafe { AvSetMmThreadPriority(handle, AVRT_PRIORITY(AVRT_PRIORITY_HIGH.0)) }
            {
                // Registered but not promoted: still better than nothing, so
                // the handle is kept and reverted normally.
                return RealtimePriority {
                    handle: Some(handle),
                    outcome: Outcome::Failed(format!(
                        "thread registered but priority refused: {e}"
                    )),
                };
            }
            RealtimePriority {
                handle: Some(handle),
                outcome: Outcome::Raised,
            }
        }
        Err(e) => RealtimePriority {
            handle: None,
            outcome: Outcome::Failed(e.to_string()),
        },
    }
}

#[cfg(not(windows))]
fn platform_raise() -> RealtimePriority {
    RealtimePriority {
        outcome: Outcome::Unsupported,
    }
}

#[cfg(windows)]
impl Drop for RealtimePriority {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            use windows::Win32::System::Threading::AvRevertMmThreadCharacteristics;
            // SAFETY: `handle` came from AvSetMmThreadCharacteristicsW on this
            // thread and has not been reverted -- `take` above guarantees this
            // runs at most once.
            if let Err(e) = unsafe { AvRevertMmThreadCharacteristics(handle) } {
                tracing::debug!("could not revert thread characteristics: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raising_a_thread_reports_what_happened() {
        let guard = raise_current_thread();
        // Deliberately not asserting `Raised`: MMCSS can legitimately refuse
        // (the service is disabled, or we are in a constrained container), and
        // a test that failed on a machine where the *product* works correctly
        // would be a test worth deleting.
        match guard.outcome() {
            Outcome::Raised => {}
            // Windows always has MMCSS, so Unsupported there would mean the
            // cfg wiring is wrong rather than the machine being unusual.
            #[cfg(windows)]
            Outcome::Unsupported => panic!("Windows has MMCSS; this should not be Unsupported"),
            #[cfg(not(windows))]
            Outcome::Unsupported => {}
            Outcome::Failed(why) => assert!(!why.is_empty(), "a refusal must say why"),
        }
    }

    #[test]
    fn the_guard_reverts_without_panicking() {
        // The Drop impl runs on a real MMCSS handle here, which is the only
        // place that path is exercised at all.
        drop(raise_current_thread());
    }

    #[test]
    fn raising_twice_on_one_thread_is_survivable() {
        // Nothing should do this, but a nested call must not corrupt the
        // handle bookkeeping or double-revert.
        let a = raise_current_thread();
        let b = raise_current_thread();
        drop(b);
        drop(a);
    }

    #[test]
    fn only_raised_counts_as_raised() {
        assert!(Outcome::Raised.raised());
        assert!(!Outcome::Unsupported.raised());
        assert!(!Outcome::Failed("no".into()).raised());
    }
}

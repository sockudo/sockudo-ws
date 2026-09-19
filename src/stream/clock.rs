use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

// The explicit feature also selects Tokio's clock when this crate is an
// integration-test dependency, where cfg(test) would not apply.
#[cfg(not(feature = "test-util"))]
pub(super) use quanta::Instant;
#[cfg(feature = "test-util")]
pub(super) use tokio::time::Instant;

/// Initializes the production clock before latency-sensitive Tokio work starts.
///
/// The first call may block for up to 200 ms while quanta calibrates its clock.
/// Call this before constructing the Tokio runtime to keep that one-time cost
/// out of runtime scheduling. This is a no-op when `test-util` selects Tokio's
/// clock.
#[inline]
pub fn init_clock() {
    #[cfg(not(feature = "test-util"))]
    let _ = quanta::Instant::now();
}

/// Keep the logical deadline separate from the runtime's wakeup clock.
pub(super) struct HeartbeatTimer {
    deadline: Instant,
    sleep: Pin<Box<tokio::time::Sleep>>,
}

impl HeartbeatTimer {
    pub(super) fn new(deadline: Instant) -> Self {
        Self {
            deadline,
            sleep: Box::pin(tokio::time::sleep_until(runtime_deadline(deadline))),
        }
    }

    pub(super) fn poll(&mut self, deadline: Instant, cx: &mut Context<'_>) -> Poll<()> {
        // Earlier deadlines must wake promptly. Later inactivity deadlines
        // can reuse the old registration until it fires.
        if deadline < self.deadline {
            self.reset(deadline);
        }
        if self.sleep.as_mut().poll(cx).is_pending() {
            return Poll::Pending;
        }
        if deadline <= Instant::now() {
            return Poll::Ready(());
        }

        // Activity or clock-rate differences can leave time remaining after a
        // wakeup. Rebase from now, then poll to register the waker again; reusing
        // a historical cross-clock epoch could repeatedly arm a past deadline.
        self.reset(deadline);
        self.sleep.as_mut().poll(cx)
    }

    fn reset(&mut self, deadline: Instant) {
        self.deadline = deadline;
        self.sleep.as_mut().reset(runtime_deadline(deadline));
    }
}

fn runtime_deadline(deadline: Instant) -> tokio::time::Instant {
    #[cfg(feature = "test-util")]
    {
        deadline
    }
    #[cfg(not(feature = "test-util"))]
    {
        // Convert only at registration, never by equating the clocks' epochs.
        // Sample the runtime first so preemption between reads can only make
        // the wakeup early; poll rechecks the logical deadline before expiry.
        let runtime_now = tokio::time::Instant::now();
        let remaining = deadline.saturating_duration_since(Instant::now());
        runtime_now + remaining
    }
}

#[cfg(all(test, not(feature = "test-util")))]
#[path = "clock_tests.rs"]
mod tests;

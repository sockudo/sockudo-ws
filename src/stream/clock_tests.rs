use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Wake, Waker};
use std::time::Duration;

use super::{HeartbeatTimer, Instant};

#[tokio::test(start_paused = true)]
async fn early_runtime_wakeup_rearms_and_registers_again() {
    let (clock, mock) = quanta::Clock::mock();
    // An unrelated epoch exposes accidental conversion of absolute timestamps.
    mock.increment(Duration::from_secs(10_000));
    let deadline = quanta::with_clock(&clock, || Instant::now() + Duration::from_secs(1));
    let mut timer = quanta::with_clock(&clock, || HeartbeatTimer::new(deadline));
    let wake = Arc::new(TimerWake::default());
    let waker = Waker::from(wake.clone());
    let mut cx = Context::from_waker(&waker);
    assert!(quanta::with_clock(&clock, || timer.poll(deadline, &mut cx)).is_pending());
    tokio::time::advance(Duration::from_millis(1001)).await;

    // The runtime advanced, but the heartbeat clock still has a second left.
    assert!(quanta::with_clock(&clock, || timer.poll(deadline, &mut cx)).is_pending());
    wake.0.store(false, Ordering::Relaxed);
    tokio::time::advance(Duration::from_millis(1001)).await;
    assert!(
        wake.0.load(Ordering::Relaxed),
        "rearmed timer must wake its reader"
    );
}

#[tokio::test(start_paused = true)]
async fn earlier_heartbeat_deadline_wakes_before_the_old_registration() {
    let (clock, mock) = quanta::Clock::mock();
    let now = quanta::with_clock(&clock, Instant::now);
    let mut timer =
        quanta::with_clock(&clock, || HeartbeatTimer::new(now + Duration::from_secs(2)));
    let wake = Arc::new(TimerWake::default());
    let waker = Waker::from(wake.clone());
    let mut cx = Context::from_waker(&waker);
    let earlier = now + Duration::from_secs(1);
    assert!(quanta::with_clock(&clock, || timer.poll(earlier, &mut cx)).is_pending());

    mock.increment(Duration::from_secs(1));
    tokio::time::advance(Duration::from_millis(1001)).await;

    assert!(
        wake.0.load(Ordering::Relaxed),
        "earlier deadline must wake its reader"
    );
    assert!(quanta::with_clock(&clock, || timer.poll(earlier, &mut cx)).is_ready());
}

#[derive(Default)]
struct TimerWake(AtomicBool);

impl Wake for TimerWake {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::Relaxed);
    }
}

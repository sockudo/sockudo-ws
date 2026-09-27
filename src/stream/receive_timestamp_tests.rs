use std::sync::Arc;
use std::task::{RawWaker, RawWakerVTable, Waker};

use super::*;

macro_rules! pong_timestamp_case {
    ($name:ident, $constructor:expr) => {
        #[rstest::rstest]
        #[case::before_deadline(Duration::ZERO, true)]
        #[case::crossing_deadline(Duration::from_secs(1), false)]
        #[tokio::test(start_paused = true)]
        async fn $name(#[case] elapsed_during_registration: Duration, #[case] accepted: bool) {
            let (clock, mock) = quanta::Clock::mock();
            let config = Config::builder()
                .ping_interval(1)
                .pong_timeout(1)
                .idle_timeout(0)
                .build();
            let (io, _peer) = tokio::io::duplex(1024);
            let mut ws = quanta::with_clock(&clock, || ($constructor)(io, config));
            let payload = ws.heartbeat.ping_due(1_000).unwrap();
            ws.heartbeat.ping_flushed(1_000);
            ws.pending_messages.push(Message::Pong(payload));
            mock.increment(Duration::from_millis(1_500));

            // Model preemption between checking the deadline and delivering a
            // buffered Pong, without depending on real sleeps or scheduler luck.
            let waker = registration_waker(mock, elapsed_during_registration);
            let mut cx = Context::from_waker(&waker);
            let result = quanta::with_clock(&clock, || Pin::new(&mut ws).poll_next(&mut cx));

            assert!(matches!(result, Poll::Ready(Some(Ok(Message::Pong(_))))));
            assert_eq!(
                ws.heartbeat.next_deadline(),
                Some(if accepted {
                    Deadline::Ping(2_500)
                } else {
                    Deadline::Pong(2_000)
                })
            );
        }
    };
}

pong_timestamp_case!(buffered_pong_uses_delivery_time, WebSocketStream::client);
#[cfg(feature = "permessage-deflate")]
pong_timestamp_case!(compressed_buffered_pong_uses_delivery_time, |io, config| {
    CompressedWebSocketStream::client(io, config, crate::deflate::DeflateConfig::default())
});

struct RegistrationClock {
    clock: Arc<quanta::Mock>,
    elapsed: Duration,
}

fn registration_waker(clock: Arc<quanta::Mock>, elapsed: Duration) -> Waker {
    // Each raw waker owns one Arc reference; clone/drop mirror that ownership.
    unsafe fn clone(data: *const ()) -> RawWaker {
        let ptr = data.cast::<RegistrationClock>();
        // SAFETY: data originates from Arc::into_raw and this waker still owns it.
        let state = unsafe { &*ptr };
        state.clock.increment(state.elapsed);
        // SAFETY: the returned waker owns the newly added strong reference.
        unsafe { Arc::increment_strong_count(ptr) };
        RawWaker::new(data, &VTABLE)
    }
    unsafe fn drop(data: *const ()) {
        // SAFETY: consume the single strong reference owned by this raw waker.
        std::mem::drop(unsafe { Arc::from_raw(data.cast::<RegistrationClock>()) });
    }
    unsafe fn wake_by_ref(_: *const ()) {}
    static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, drop, wake_by_ref, drop);

    let ptr = Arc::into_raw(Arc::new(RegistrationClock { clock, elapsed }));
    // SAFETY: the vtable preserves Arc ownership and its state is Send + Sync.
    unsafe { Waker::from_raw(RawWaker::new(ptr.cast(), &VTABLE)) }
}

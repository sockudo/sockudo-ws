use super::{H3SendStream, H3WriteFuture, H3Writer};
use bytes::Bytes;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::task::{Context, Poll};

struct GatedSend {
    ready: Arc<AtomicBool>,
    output: Arc<Mutex<Vec<u8>>>,
}

impl H3SendStream for GatedSend {
    fn send_data(self, data: Bytes) -> H3WriteFuture<Self> {
        Box::pin(async move {
            futures_util::future::poll_fn(|_| {
                if self.ready.load(Ordering::Relaxed) {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
            self.output.lock().unwrap().extend_from_slice(&data);
            (self, Ok(()))
        })
    }
}

#[test]
fn cancelled_pending_write_does_not_report_old_bytes_for_new_buffer() {
    let ready = Arc::new(AtomicBool::new(false));
    let output = Arc::new(Mutex::new(Vec::new()));
    let mut writer = H3Writer::new(GatedSend {
        ready: ready.clone(),
        output: output.clone(),
    });
    let mut cx = Context::from_waker(futures_util::task::noop_waker_ref());

    assert!(matches!(
        writer.poll_write(&mut cx, b"first"),
        Poll::Ready(Ok(5))
    ));
    assert!(writer.poll_flush(&mut cx).is_pending());
    assert!(writer.poll_write(&mut cx, b"cancelled").is_pending());
    ready.store(true, Ordering::Relaxed);
    assert!(matches!(
        writer.poll_write(&mut cx, b"x"),
        Poll::Ready(Ok(1))
    ));
    assert!(matches!(writer.poll_flush(&mut cx), Poll::Ready(Ok(()))));
    assert_eq!(&*output.lock().unwrap(), b"firstx");
}

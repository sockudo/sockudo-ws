//! Native writer diagnostic: partial writes with peer Ping churn, plus Ready control.
use futures_util::task::AtomicWaker;
use sockudo_ws::{Config, Message, WebSocketStream};
use std::{
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
    time::Instant,
};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf};
use tokio::sync::Notify;

struct Gate {
    released: AtomicBool,
    blocked: Notify,
    waker: AtomicWaker,
}
struct Input {
    inner: DuplexStream,
    gate: Arc<Gate>,
    prefix_left: usize,
}
impl AsyncRead for Input {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}
impl AsyncWrite for Input {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.gate.waker.register(cx.waker());
        if self.gate.released.load(Ordering::Acquire) {
            return Poll::Ready(Ok(bytes.len()));
        }
        if self.prefix_left > 0 {
            let n = self.prefix_left.min(bytes.len());
            self.prefix_left -= n;
            return Poll::Ready(Ok(n));
        }
        self.gate.blocked.notify_one();
        Poll::Pending
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn main() {
    let mut args: Vec<_> = std::env::args().filter(|arg| arg != "--bench").collect();
    if args.len() == 1 {
        args.extend(["0"].map(str::to_owned));
    }
    assert_eq!(
        args.len(),
        2,
        "number of peer Pings per pending send; zero selects Ready"
    );
    let pings: usize = args[1].parse().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let samples = runtime.block_on(async {
        let mut samples = Vec::with_capacity(256);
        for iteration in 0..272 {
            let (inner, mut peer) = tokio::io::duplex(4096);
            let gate = Arc::new(Gate {
                released: AtomicBool::new(pings == 0),
                blocked: Notify::new(),
                waker: AtomicWaker::new(),
            });
            let io = Input {
                inner,
                gate: gate.clone(),
                prefix_left: 3,
            };
            let (mut reader, mut writer) = WebSocketStream::client(io, Config::default()).split();
            let start = Instant::now();
            if pings == 0 {
                writer
                    .send(Message::text("application payload"))
                    .await
                    .unwrap();
            } else {
                let send = tokio::spawn(async move {
                    writer
                        .send(Message::text("application payload"))
                        .await
                        .unwrap();
                    writer
                });
                gate.blocked.notified().await;
                for _ in 0..pings {
                    peer.write_all(b"\x89\x01x").await.unwrap();
                    assert!(matches!(
                        reader.next().await.unwrap().unwrap(),
                        Message::Ping(_)
                    ));
                }
                gate.released.store(true, Ordering::Release);
                gate.waker.wake();
                writer = send.await.unwrap();
            }
            let elapsed = start.elapsed().as_nanos();
            if iteration >= 16 {
                samples.push(elapsed);
            }
            drop(writer);
            drop(reader);
            drop(peer);
            // Complete cancellation outside the next sample's interval.
            tokio::task::yield_now().await;
        }
        samples
    });
    println!("sample,completion_ns");
    for (sample, value) in samples.into_iter().enumerate() {
        println!("{sample},{value}");
    }
}

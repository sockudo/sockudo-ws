//! Client delivery diagnostic: real payloads, TLS, native split and retained messages.
//! Setup is excluded; raw per-message samples go to stdout for paired analysis.

use sockudo_ws::{Config, Http1, Message, WebSocketStream, stream::Stream};
use std::{
    collections::{BTreeMap, VecDeque},
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::Barrier,
};

#[derive(Clone)]
struct Case {
    payload: Arc<Vec<u8>>,
    count: usize,
    burst: usize,
    retain: usize,
    pause_us: u64,
    boxed: bool,
    trace: bool,
    barrier: Arc<Barrier>,
}

// Use only in separate diagnostic runs: histogram updates perturb delivery.
struct ObservedIo<S> {
    inner: S,
    pending: usize,
    reads: BTreeMap<usize, usize>,
}

impl<S: AsyncRead + Unpin> AsyncRead for ObservedIo<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let result = Pin::new(&mut this.inner).poll_read(cx, buf);
        match &result {
            Poll::Pending => this.pending += 1,
            Poll::Ready(Ok(())) => *this.reads.entry(buf.filled().len() - before).or_default() += 1,
            Poll::Ready(Err(_)) => {}
        }
        result
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for ObservedIo<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        data: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, data)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

impl<S> Drop for ObservedIo<S> {
    fn drop(&mut self) {
        eprintln!(
            "read_pending={} ready_bytes_histogram={:?}",
            self.pending, self.reads
        );
    }
}

async fn receive<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    io: S,
    sent: Arc<Vec<AtomicU64>>,
    epoch: Instant,
    case: Case,
) -> Vec<u64> {
    // Match the caller's transport monitoring; disabling it is a different contract.
    let ws = WebSocketStream::client(
        io,
        Config::builder()
            .auto_ping(true)
            .ping_interval(30)
            .pong_timeout(10)
            .idle_timeout(120)
            .build(),
    );
    let (mut reader, _writer) = ws.split();
    let mut retained = VecDeque::<Message>::with_capacity(case.retain + 1);
    let mut samples = Vec::with_capacity(case.count);
    case.barrier.wait().await;
    for sequence in 0..case.count {
        let message = reader.next().await.unwrap().unwrap();
        let delivered = epoch.elapsed().as_nanos() as u64;
        let emitted = sent[sequence].load(Ordering::Acquire);
        assert!(emitted != 0);
        samples.push(delivered.checked_sub(emitted).unwrap());
        // Check after the timestamp, retaining its effect on subsequent delivery.
        assert_eq!(message.as_bytes(), case.payload.as_slice());
        retained.push_back(message);
        if retained.len() > case.retain {
            retained.pop_front();
        }
    }
    samples
}

async fn connection<S, P>(io: S, mut peer: P, case: Case) -> Vec<u64>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    P: AsyncWrite + Unpin + Send + 'static,
{
    let sent = Arc::new(
        (0..case.count)
            .map(|_| AtomicU64::new(0))
            .collect::<Vec<_>>(),
    );
    let peer_sent = sent.clone();
    let epoch = Instant::now();
    let peer_case = case.clone();
    let producer = tokio::spawn(async move {
        // Keep the raw fixture unchanged inside an unmasked Text frame.
        let len = peer_case.payload.len();
        let mut wire = vec![0x81];
        if len < 126 {
            wire.push(len as u8);
        } else if len <= u16::MAX as usize {
            wire.push(126);
            wire.extend_from_slice(&(len as u16).to_be_bytes());
        } else {
            wire.push(127);
            wire.extend_from_slice(&(len as u64).to_be_bytes());
        }
        wire.extend_from_slice(&peer_case.payload);
        peer_case.barrier.wait().await;
        for sequence in 0..peer_case.count {
            if sequence % peer_case.burst == 0 && peer_case.pause_us > 0 {
                tokio::time::sleep(Duration::from_micros(peer_case.pause_us)).await;
            }
            peer_sent[sequence].store(epoch.elapsed().as_nanos() as u64, Ordering::Release);
            peer.write_all(&wire).await.unwrap();
            peer.flush().await.unwrap();
        }
        // Keep the connection open until delivery is complete, as in a long-lived feed.
        peer_case.barrier.wait().await;
    });
    let samples = if case.trace {
        let io = ObservedIo {
            inner: io,
            pending: 0,
            reads: BTreeMap::new(),
        };
        if case.boxed {
            receive(Stream::<Http1>::new(io), sent, epoch, case.clone()).await
        } else {
            receive(io, sent, epoch, case.clone()).await
        }
    } else if case.boxed {
        receive(Stream::<Http1>::new(io), sent, epoch, case.clone()).await
    } else {
        receive(io, sent, epoch, case.clone()).await
    };
    case.barrier.wait().await;
    producer.await.unwrap();
    samples
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    assert!(
        args.len() == 9 || (args.len() == 10 && args[9] == "trace"),
        "fixture tls|tcp connections count burst retain pause_us typed|boxed [trace]"
    );
    let payload = std::fs::read(&args[1]).unwrap();
    std::str::from_utf8(&payload).unwrap();
    assert!(!payload.is_empty());
    let tls = match args[2].as_str() {
        "tls" => true,
        "tcp" => false,
        _ => panic!("transport"),
    };
    let connections: usize = args[3].parse().unwrap();
    let count = args[4].parse().unwrap();
    let burst = args[5].parse().unwrap();
    assert!(connections > 0 && count > 0 && burst > 0);
    let case = Case {
        payload: Arc::new(payload),
        count,
        burst,
        retain: args[6].parse().unwrap(),
        pause_us: args[7].parse().unwrap(),
        boxed: match args[8].as_str() {
            "typed" => false,
            "boxed" => true,
            _ => panic!("dispatch"),
        },
        trace: args.len() == 10,
        barrier: Arc::new(Barrier::new(connections * 2)),
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async move {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let rcgen::CertifiedKey { cert, signing_key } =
            rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let key = rustls::pki_types::PrivateKeyDer::try_from(signing_key.serialize_der()).unwrap();
        let server = rustls::ServerConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], key)
            .unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(cert.der().clone()).unwrap();
        let client = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut tasks = Vec::new();
        for _ in 0..connections {
            let io = TcpStream::connect(listener.local_addr().unwrap())
                .await
                .unwrap();
            let (peer, _) = listener.accept().await.unwrap();
            io.set_nodelay(true).unwrap();
            peer.set_nodelay(true).unwrap();
            let task = if tls {
                let (io, peer) = tokio::join!(
                    connector.connect("localhost".try_into().unwrap(), io),
                    acceptor.accept(peer)
                );
                tokio::spawn(connection(io.unwrap(), peer.unwrap(), case.clone()))
            } else {
                tokio::spawn(connection(io, peer, case.clone()))
            };
            tasks.push(task);
        }
        println!("connection,sequence,delivery_ns");
        for (id, task) in tasks.into_iter().enumerate() {
            for (sequence, elapsed) in task.await.unwrap().into_iter().enumerate() {
                println!("{id},{sequence},{elapsed}");
            }
        }
    });
}

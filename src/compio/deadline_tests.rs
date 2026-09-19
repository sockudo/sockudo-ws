use super::*;
use std::cell::RefCell;

struct RecordingWriter(Rc<RefCell<Vec<u8>>>, Rc<Cell<bool>>);

impl AsyncWrite for RecordingWriter {
    async fn write<B: IoBuf>(&mut self, buf: B) -> BufResult<usize, B> {
        let bytes = buf.as_init();
        self.0.borrow_mut().extend_from_slice(bytes);
        BufResult(Ok(bytes.len()), buf)
    }

    async fn flush(&mut self) -> io::Result<()> {
        std::future::poll_fn(|_| {
            if self.1.get() {
                std::task::Poll::Pending
            } else {
                std::task::Poll::Ready(Ok(()))
            }
        })
        .await
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct DriverPeer {
    bytes: Rc<RefCell<Vec<u8>>>,
    flush_blocked: Rc<Cell<bool>>,
    application: mpsc::Sender<ApplicationRequest>,
    control: mpsc::Sender<ControlRequest>,
    _cancel: mpsc::UnboundedSender<()>,
    shared: Rc<CompioSplitShared>,
}

impl DriverPeer {
    fn queue_data(&mut self) {
        for _ in 0..SPLIT_APPLICATION_CAPACITY {
            let (completion, _) = oneshot::channel();
            self.application
                .try_send(ApplicationRequest::Send(
                    Message::text("queued"),
                    completion,
                ))
                .unwrap();
        }
    }
}

fn driver(
    encoder: impl CompioSplitEncoder,
    config: Config,
) -> (DriverPeer, impl Future<Output = ()>) {
    let (control, control_rx) = mpsc::channel(SPLIT_CONTROL_CAPACITY);
    let (application, application_rx) = mpsc::channel(SPLIT_APPLICATION_CAPACITY);
    let (cancel, cancel_rx) = mpsc::unbounded();
    let (terminal_tx, _) = mpsc::unbounded();
    let shared = CompioSplitShared::new(false);
    let bytes = Rc::new(RefCell::new(Vec::new()));
    let flush_blocked = Rc::new(Cell::new(false));
    let task = compio_split_writer_driver(
        RecordingWriter(bytes.clone(), flush_blocked.clone()),
        encoder,
        config,
        CompioDriverChannels {
            control_rx,
            application_rx,
            cancel_rx,
            terminal_tx,
            shared: shared.clone(),
        },
    );
    (
        DriverPeer {
            bytes,
            flush_blocked,
            application,
            control,
            _cancel: cancel,
            shared,
        },
        task,
    )
}

#[compio::test]
async fn expired_idle_precedes_queued_data() {
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let (mut peer, task) = driver(
        Protocol::new(Role::Server, config.max_frame_size, config.max_message_size),
        config,
    );
    let mut task = std::pin::pin!(task);
    assert!(futures_util::poll!(task.as_mut()).is_pending());
    peer.queue_data();
    // The driver stays unpolled while its deadline expires, with data already queued.
    ::compio::time::sleep(Duration::from_millis(1100)).await;
    let _ = futures_util::poll!(task.as_mut());
    assert_eq!(peer.bytes.borrow()[0], 0x88);
    assert!(matches!(
        peer.shared.terminal.get(),
        Some(CompioTerminalCause::IdleTimeout)
    ));
}

#[compio::test]
async fn due_ping_precedes_queued_data() {
    let config = Config::builder().ping_interval(1).idle_timeout(0).build();
    let (mut peer, task) = driver(
        Protocol::new(Role::Server, config.max_frame_size, config.max_message_size),
        config,
    );
    let mut task = std::pin::pin!(task);
    assert!(futures_util::poll!(task.as_mut()).is_pending());
    peer.queue_data();
    // Keep the runtime from dispatching its timer before this manual poll.
    std::thread::sleep(Duration::from_millis(1100));
    let _ = futures_util::poll!(task.as_mut());
    assert_eq!(peer.bytes.borrow()[0], 0x89);
}

#[compio::test]
async fn expired_pong_precedes_queued_data() {
    let config = Config::builder()
        .ping_interval(1)
        .pong_timeout(1)
        .idle_timeout(0)
        .build();
    let (mut peer, task) = driver(
        Protocol::new(Role::Server, config.max_frame_size, config.max_message_size),
        config,
    );
    let mut task = std::pin::pin!(task);
    assert!(futures_util::poll!(task.as_mut()).is_pending());
    ::compio::time::sleep(Duration::from_millis(1100)).await;
    assert!(futures_util::poll!(task.as_mut()).is_pending());
    assert_eq!(peer.bytes.borrow()[0], 0x89);
    peer.bytes.borrow_mut().clear();
    peer.queue_data();
    ::compio::time::sleep(Duration::from_millis(1100)).await;
    let _ = futures_util::poll!(task.as_mut());
    assert_eq!(peer.bytes.borrow()[0], 0x88);
    assert!(matches!(
        peer.shared.terminal.get(),
        Some(CompioTerminalCause::HeartbeatTimeout)
    ));
}

#[compio::test]
async fn queued_timely_pong_precedes_expired_timer() {
    let config = Config::builder()
        .ping_interval(1)
        .pong_timeout(1)
        .idle_timeout(0)
        .build();
    let (mut peer, task) = driver(
        Protocol::new(Role::Server, config.max_frame_size, config.max_message_size),
        config,
    );
    let mut task = std::pin::pin!(task);
    assert!(futures_util::poll!(task.as_mut()).is_pending());
    ::compio::time::sleep(Duration::from_millis(1100)).await;
    assert!(futures_util::poll!(task.as_mut()).is_pending());
    let frame = peer.bytes.borrow().clone();
    assert_eq!(&frame[..2], &[0x89, 8]);
    peer.control
        .try_send(ControlRequest::Pong(
            Bytes::copy_from_slice(&frame[2..]),
            Instant::now(),
        ))
        .unwrap();
    peer.bytes.borrow_mut().clear();
    peer.queue_data();
    ::compio::time::sleep(Duration::from_millis(1100)).await;
    assert!(futures_util::poll!(task.as_mut()).is_pending());
    assert!(peer.shared.terminal.get().is_none());
    let mut decoder = Protocol::new(Role::Client, 65536, 65536);
    let messages = decoder
        .process(&mut BytesMut::from(peer.bytes.borrow().as_slice()))
        .unwrap();
    assert!(
        messages
            .iter()
            .any(|message| matches!(message, Message::Text(text) if text == "queued"))
    );
    assert!(!messages.iter().any(Message::is_close));
}

#[cfg(feature = "permessage-deflate")]
#[compio::test]
async fn compressed_idle_timeout_precedes_queued_data() {
    let config = Config::builder().auto_ping(false).idle_timeout(1).build();
    let (_, encoder) = CompressedProtocol::server(
        config.max_frame_size,
        config.max_message_size,
        DeflateConfig::default(),
    )
    .split(config.max_frame_size, config.max_message_size);
    let (mut peer, task) = driver(encoder, config);
    let mut task = std::pin::pin!(task);
    assert!(futures_util::poll!(task.as_mut()).is_pending());
    peer.queue_data();
    ::compio::time::sleep(Duration::from_millis(1100)).await;
    let _ = futures_util::poll!(task.as_mut());
    assert_eq!(peer.bytes.borrow()[0], 0x88);
    assert!(matches!(
        peer.shared.terminal.get(),
        Some(CompioTerminalCause::IdleTimeout)
    ));
}

#[compio::test]
async fn application_close_bounds_a_blocked_flush() {
    let config = Config::builder()
        .auto_ping(false)
        .idle_timeout(0)
        .close_timeout(0)
        .build();
    let (mut peer, task) = driver(
        Protocol::new(Role::Server, config.max_frame_size, config.max_message_size),
        config,
    );
    peer.flush_blocked.set(true);
    let (completion, _) = oneshot::channel();
    peer.application
        .try_send(ApplicationRequest::Send(Message::Close(None), completion))
        .unwrap();
    let mut task = std::pin::pin!(task);
    assert!(futures_util::poll!(task.as_mut()).is_ready());
    assert!(peer.shared.terminal.get().is_some());
}

#[compio::test]
async fn peer_close_bounds_an_existing_blocked_write() {
    let config = Config::builder()
        .auto_ping(false)
        .idle_timeout(0)
        .close_timeout(0)
        .build();
    let (mut peer, task) = driver(
        Protocol::new(Role::Server, config.max_frame_size, config.max_message_size),
        config,
    );
    peer.flush_blocked.set(true);
    peer.queue_data();
    let mut task = std::pin::pin!(task);
    assert!(futures_util::poll!(task.as_mut()).is_pending());
    peer.control.try_send(ControlRequest::PeerClose).unwrap();
    assert!(futures_util::poll!(task.as_mut()).is_ready());
    assert!(peer.shared.terminal.get().is_some());
}

#[compio::test]
async fn pong_received_during_ping_flush_prevents_false_timeout() {
    let config = Config::builder()
        .ping_interval(1)
        .pong_timeout(1)
        .idle_timeout(0)
        .build();
    let (mut peer, task) = driver(
        Protocol::new(Role::Server, config.max_frame_size, config.max_message_size),
        config,
    );
    peer.flush_blocked.set(true);
    let mut task = std::pin::pin!(task);
    assert!(futures_util::poll!(task.as_mut()).is_pending());
    ::compio::time::sleep(Duration::from_millis(1100)).await;
    assert!(futures_util::poll!(task.as_mut()).is_pending());
    let frame = peer.bytes.borrow().clone();
    assert_eq!(&frame[..2], &[0x89, 8]);
    peer.control
        .try_send(ControlRequest::Pong(
            Bytes::copy_from_slice(&frame[2..]),
            Instant::now(),
        ))
        .unwrap();
    assert!(futures_util::poll!(task.as_mut()).is_pending());
    peer.flush_blocked.set(false);
    assert!(futures_util::poll!(task.as_mut()).is_pending());
    ::compio::time::sleep(Duration::from_millis(1100)).await;
    assert!(futures_util::poll!(task.as_mut()).is_pending());
    assert!(peer.shared.terminal.get().is_none());
}

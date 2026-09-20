use super::*;

#[tokio::test]
async fn cancelling_a_flushed_close_does_not_abort_its_transport() {
    let (io, mut peer) = tokio::io::duplex(64);
    let shared = SplitShared::new(false);
    let (control_tx, mut control_rx) = mpsc::channel(1);
    control_tx.try_send(ControlRequest::LocalCloseSent).unwrap();
    let core = SplitWriterCore {
        sink: Arc::new(tokio::sync::Mutex::new(SplitSink::new(
            io,
            Protocol::new(Role::Server, 1024, 1024),
            64,
        ))),
        control_tx,
        shared: shared.clone(),
    };

    {
        let send = core.send(Message::Close(Some(CloseReason::new(1000, ""))));
        tokio::pin!(send);
        assert!(futures_util::poll!(&mut send).is_pending());
        let mut wire = [0; 4];
        let frame_ready = {
            let read = peer.read_exact(&mut wire);
            tokio::pin!(read);
            match futures_util::poll!(&mut read) {
                Poll::Ready(result) => {
                    result.unwrap();
                    true
                }
                Poll::Pending => false,
            }
        };
        if !frame_ready {
            // A close-start notification may reserve capacity before the frame
            // write. Release the seeded slot, then let that notification keep
            // the final close-sent notification pending instead.
            assert!(matches!(
                control_rx.recv().await,
                Some(ControlRequest::LocalCloseSent)
            ));
            assert!(futures_util::poll!(&mut send).is_pending());
            peer.read_exact(&mut wire).await.unwrap();
        }
        assert_eq!(wire, [0x88, 2, 3, 232]);
        // Only notification is pending: the entire frame has been flushed.
    }

    assert!(!shared.cancel.is_cancelled());
    assert_eq!(shared.status.load(Ordering::Acquire), SPLIT_CLOSING);
}

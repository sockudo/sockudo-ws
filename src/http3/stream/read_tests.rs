use super::{Http3ClientStream, Http3ServerStream};
use bytes::Bytes;
use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::Context,
};
use tokio::io::{AsyncRead, AsyncReadExt, ReadBuf};

#[path = "../../../tests/support/h3_pair.rs"]
mod h3_pair;

#[rstest::rstest]
#[case(false)]
#[case(true)]
#[tokio::test]
async fn empty_buffer_does_not_wait_for_data(#[case] server: bool) {
    let (client, peer, _connection) = h3_pair::pair().await;
    let mut client = Http3ClientStream::new(client);
    let mut peer = Http3ServerStream::new(peer);
    let stream: &mut (dyn AsyncRead + Unpin) = if server { &mut peer } else { &mut client };
    let mut context = Context::from_waker(futures_util::task::noop_waker_ref());
    assert!(matches!(
        Pin::new(stream).poll_read(&mut context, &mut ReadBuf::new(&mut [])),
        std::task::Poll::Ready(Ok(()))
    ));
}

#[rstest::rstest]
#[case(false, 7)]
#[case(true, 7)]
#[case(false, 65536)]
#[case(true, 65536)]
#[tokio::test]
async fn data_crosses_small_reads_and_fin(#[case] server: bool, #[case] read_size: usize) {
    let (mut client, mut peer, _connection) = h3_pair::pair().await;
    let payload = Bytes::from((0..131_073).map(|i| (i % 251) as u8).collect::<Vec<_>>());
    let expected = payload.clone();
    let (mut reader, writer): (Box<dyn AsyncRead + Unpin>, _) = if server {
        (
            Box::new(Http3ServerStream::new(peer)),
            tokio::spawn(async move {
                client.send_data(payload).await.unwrap();
                client.finish().await.unwrap();
            }),
        )
    } else {
        (
            Box::new(Http3ClientStream::new(client)),
            tokio::spawn(async move {
                peer.send_data(payload).await.unwrap();
                peer.finish().await.unwrap();
            }),
        )
    };
    let mut output = Vec::new();
    let mut buf = vec![0; read_size];
    loop {
        let count = reader.read(&mut buf).await.unwrap();
        if count == 0 {
            break;
        }
        output.extend_from_slice(&buf[..count]);
    }
    assert_eq!(output.len(), expected.len());
    assert_eq!(output, expected);
    writer.await.unwrap();
}

struct Owner {
    bytes: Vec<u8>,
    dropped: Arc<AtomicBool>,
}
impl AsRef<[u8]> for Owner {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

#[rstest::rstest]
#[case(false)]
#[case(true)]
#[tokio::test]
async fn consumed_chunk_releases_owner(#[case] server: bool) {
    let (client, peer, _connection) = h3_pair::pair().await;
    let dropped = Arc::new(AtomicBool::new(false));
    let bytes = Bytes::from_owner(Owner {
        bytes: vec![42; 65],
        dropped: dropped.clone(),
    });
    let mut client = Http3ClientStream::new(client);
    let mut peer = Http3ServerStream::new(peer);
    // Inject an owner so this check does not depend on QUIC packet coalescing.
    let stream: &mut (dyn AsyncRead + Unpin) = if server {
        peer.read_buf = bytes;
        &mut peer
    } else {
        client.read_buf = bytes;
        &mut client
    };
    let mut prefix = [0; 7];
    stream.read_exact(&mut prefix).await.unwrap();
    assert_eq!(prefix, [42; 7]);
    assert!(!dropped.load(Ordering::SeqCst));
    let mut rest = [0; 58];
    stream.read_exact(&mut rest).await.unwrap();
    assert_eq!(rest, [42; 58]);
    assert!(dropped.load(Ordering::SeqCst));
}

#[rstest::rstest]
#[case(false)]
#[case(true)]
#[tokio::test]
async fn raw_quic_wrappers_read_partial_data_through_fin(#[case] generic: bool) {
    let (_endpoints, [client, server]) = h3_pair::quic_pair().await;
    let payload: Vec<_> = (0..131_073).map(|i| (i % 251) as u8).collect();
    let expected = payload.clone();
    let writer = tokio::spawn(async move {
        let (mut send, _recv) = client.open_bi().await.unwrap();
        send.write_all(&payload).await.unwrap();
        send.finish().unwrap();
        send.stopped().await.unwrap();
    });
    let (send, recv) = server.accept_bi().await.unwrap();
    let mut reader: Box<dyn AsyncRead + Unpin> = if generic {
        Box::new(crate::Stream::<crate::Http3>::from_quic(send, recv))
    } else {
        Box::new(super::Http3Stream::new(send, recv))
    };
    let mut output = Vec::new();
    let mut buf = [0; 7];
    loop {
        let count = reader.read(&mut buf).await.unwrap();
        if count == 0 {
            break;
        }
        output.extend_from_slice(&buf[..count]);
    }
    assert_eq!(output, expected);
    writer.await.unwrap();
}

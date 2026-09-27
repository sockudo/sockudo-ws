use bytes::Bytes;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tokio::io::{AsyncRead, AsyncReadExt};

struct OwnedChunk {
    bytes: Vec<u8>,
    dropped: Arc<AtomicBool>,
}

impl AsRef<[u8]> for OwnedChunk {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

impl Drop for OwnedChunk {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

pub async fn check_consumed_chunk_is_released<S: AsyncRead + Unpin>(
    wrap: impl FnOnce(h2::SendStream<Bytes>, h2::RecvStream, Bytes) -> S,
) {
    let (client, server) = tokio::io::duplex(65536);
    let peer = tokio::spawn(async move {
        let mut connection = h2::server::handshake(server).await.unwrap();
        let (_, mut response) = connection.accept().await.unwrap().unwrap();
        let _send = response
            .send_response(http::Response::new(()), false)
            .unwrap();
        while connection.accept().await.is_some() {}
    });
    let (mut client, connection) = h2::client::handshake(client).await.unwrap();
    let driver = tokio::spawn(connection);
    let (response, send) = client
        .send_request(
            http::Request::builder()
                .uri("https://localhost/")
                .body(())
                .unwrap(),
            false,
        )
        .unwrap();
    let recv = response.await.unwrap().into_body();
    let dropped = Arc::new(AtomicBool::new(false));
    let data = Bytes::from_owner(OwnedChunk {
        bytes: vec![42; 65536],
        dropped: dropped.clone(),
    });
    let mut stream = wrap(send, recv, data);
    assert_eq!(stream.read_u8().await.unwrap(), 42);
    assert!(!dropped.load(Ordering::SeqCst));
    let mut rest = vec![0; 65535];
    stream.read_exact(&mut rest).await.unwrap();
    assert!(rest.iter().all(|&byte| byte == 42));
    // No further read, next DATA, EOF, or stream drop is needed to release storage.
    assert!(dropped.load(Ordering::SeqCst));
    driver.abort();
    peer.abort();
}

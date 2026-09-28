use bytes::Bytes;
use std::sync::{Arc, Once};

pub type Client = h3::client::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>;
pub type Server = h3::server::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>;

pub struct Connection {
    endpoints: [quinn::Endpoint; 2],
    drivers: [tokio::task::JoinHandle<()>; 2],
}

impl Drop for Connection {
    fn drop(&mut self) {
        for endpoint in &self.endpoints {
            endpoint.close(0u32.into(), b"test complete");
        }
        for driver in &self.drivers {
            driver.abort();
        }
    }
}

pub async fn quic_pair() -> ([quinn::Endpoint; 2], [quinn::Connection; 2]) {
    static CRYPTO: Once = Once::new();
    CRYPTO.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let mut server_tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.cert.der().clone()],
            rustls::pki_types::PrivateKeyDer::try_from(cert.signing_key.serialize_der()).unwrap(),
        )
        .unwrap();
    server_tls.alpn_protocols = vec![b"h3".to_vec()];
    let server = quinn::Endpoint::server(
        quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(server_tls).unwrap(),
        )),
        "127.0.0.1:0".parse().unwrap(),
    )
    .unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.cert.der().clone()).unwrap();
    let mut client_tls = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    client_tls.alpn_protocols = vec![b"h3".to_vec()];
    let mut client = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    client.set_default_client_config(quinn::ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(client_tls).unwrap(),
    )));
    let (client_connection, server_connection) = tokio::join!(
        async {
            client
                .connect(server.local_addr().unwrap(), "localhost")
                .unwrap()
                .await
                .unwrap()
        },
        async { server.accept().await.unwrap().await.unwrap() },
    );
    ([client, server], [client_connection, server_connection])
}

pub async fn pair() -> (Client, Server, Connection) {
    let (endpoints, [connection, server_connection]) = quic_pair().await;
    let (tx, rx) = tokio::sync::oneshot::channel();
    let server_driver = tokio::spawn(async move {
        let mut connection = h3::server::builder()
            .enable_extended_connect(true)
            .build(h3_quinn::Connection::new(server_connection))
            .await
            .unwrap();
        let (request, mut stream) = connection
            .accept()
            .await
            .unwrap()
            .unwrap()
            .resolve_request()
            .await
            .unwrap();
        assert_eq!(request.method(), http::Method::CONNECT);
        stream.send_response(http::Response::new(())).await.unwrap();
        tx.send(stream).ok().unwrap();
        let _ = connection.accept().await;
    });
    let (mut connection, mut sender) = h3::client::new(h3_quinn::Connection::new(connection))
        .await
        .unwrap();
    let mut stream = sender
        .send_request(
            http::Request::builder()
                .method("CONNECT")
                .uri("https://localhost/test")
                .extension(h3::ext::Protocol::WEB_TRANSPORT)
                .body(())
                .unwrap(),
        )
        .await
        .unwrap();
    let client_driver = tokio::spawn(async move {
        let _sender = sender;
        futures_util::future::poll_fn(|cx| connection.poll_close(cx)).await;
    });
    assert_eq!(
        stream.recv_response().await.unwrap().status(),
        http::StatusCode::OK
    );
    (
        stream,
        rx.await.unwrap(),
        Connection {
            endpoints,
            drivers: [client_driver, server_driver],
        },
    )
}

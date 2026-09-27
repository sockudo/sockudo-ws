use futures_util::StreamExt;
use sockudo_ws::Config;
use sockudo_ws::compio::{CompioHttp3ClientStream, CompioHttp3Server, CompioHttp3ServerStream};
use std::sync::Once;

pub async fn pair() -> (
    CompioHttp3ClientStream,
    CompioHttp3ServerStream,
    compio::quic::Endpoint,
    compio::runtime::JoinHandle<()>,
) {
    static CRYPTO: Once = Once::new();
    CRYPTO.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let server_tls = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.cert.der().clone()],
            rustls::pki_types::PrivateKeyDer::try_from(cert.key_pair.serialize_der()).unwrap(),
        )
        .unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.cert.der().clone()).unwrap();
    let client_tls = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let endpoint = compio::quic::ServerBuilder::new_with_rustls_server_config(server_tls)
        .with_alpn_protocols(&["h3"])
        .bind("127.0.0.1:0")
        .await
        .unwrap();
    let server = CompioHttp3Server::from_endpoint(endpoint.clone(), Config::default());
    let (tx, mut rx) = futures_channel::mpsc::unbounded();
    let driver = compio::runtime::spawn(async move {
        server
            .serve(move |ws, _| {
                tx.unbounded_send(ws.into_inner()).unwrap();
                async {}
            })
            .await
            .unwrap();
    });
    let client = sockudo_ws::compio::connect_http3(
        endpoint.local_addr().unwrap(),
        "localhost",
        "/test",
        None,
        client_tls,
        Config::default(),
    )
    .await
    .unwrap()
    .into_inner();
    (client, rx.next().await.unwrap(), endpoint, driver)
}

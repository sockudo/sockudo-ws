//! Ephemeral localhost certificates, generated outside measured intervals.
use std::sync::Once;
pub fn configs(h3: bool) -> (rustls::ServerConfig, rustls::ClientConfig) {
    static CRYPTO: Once = Once::new();
    CRYPTO.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let mut server = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.cert.der().clone()],
            rustls::pki_types::PrivateKeyDer::try_from(cert.signing_key.serialize_der()).unwrap(),
        )
        .unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.cert.der().clone()).unwrap();
    let mut client = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    if h3 {
        server.alpn_protocols = vec![b"h3".to_vec()];
        client.alpn_protocols = vec![b"h3".to_vec()];
    }
    (server, client)
}

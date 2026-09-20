//! Bounded HTTP upgrades and verified TLS transports.
use crate::{
    Error, Result,
    compression::{self, Parameters},
    config::{Spec, Target},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use bytes::BytesMut;
use sha1::{Digest, Sha1};
use std::{collections::BTreeMap, io::BufReader, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
};

pub(crate) trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
pub(crate) type Socket = Box<dyn Io>;
pub(crate) struct Connection {
    pub io: Socket,
    pub initial: BytesMut,
    pub compression: Option<Parameters>,
    pub server: bool,
}
pub(crate) struct Request {
    pub io: Socket,
    pub initial: BytesMut,
    pub path: String,
    pub headers: BTreeMap<String, String>,
}
const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
fn accept_key(key: &str) -> String {
    STANDARD.encode(Sha1::digest(format!("{key}{GUID}").as_bytes()))
}
fn token(value: Option<&String>, wanted: &str) -> bool {
    value.is_some_and(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case(wanted)))
}
fn header_map(headers: &[httparse::Header<'_>]) -> Result<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    for header in headers {
        let key = header.name.to_ascii_lowercase();
        let value = std::str::from_utf8(header.value)
            .map_err(|_| Error::Handshake("non-UTF8 header".into()))?
            .trim()
            .to_owned();
        if let Some(old) = map.get_mut(&key) {
            if matches!(
                key.as_str(),
                "sec-websocket-key" | "sec-websocket-accept" | "sec-websocket-version" | "host"
            ) {
                return Err(Error::Handshake("duplicate singleton header".into()));
            }
            *old = format!("{old}, {value}");
        } else {
            map.insert(key, value);
        }
    }
    Ok(map)
}
async fn read_headers(io: &mut Socket) -> Result<BytesMut> {
    let mut buf = BytesMut::with_capacity(4096);
    loop {
        if let Some(end) = buf.windows(4).position(|s| s == b"\r\n\r\n") {
            if end + 4 > 16384 {
                return Err(Error::Limit("HTTP headers"));
            }
            return Ok(buf);
        }
        if buf.len() >= 16384 {
            return Err(Error::Limit("HTTP headers"));
        }
        let mut chunk = [0; 2048];
        let n = io.read(&mut chunk).await?;
        if n == 0 {
            return Err(Error::Handshake("EOF before upgrade".into()));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}
pub(crate) async fn request(mut io: Socket, timeout: u64) -> Result<Request> {
    let mut buf = tokio::time::timeout(Duration::from_millis(timeout), read_headers(&mut io))
        .await
        .map_err(|_| Error::Timeout)??;
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut req = httparse::Request::new(&mut headers);
    let used = match req
        .parse(&buf)
        .map_err(|e| Error::Handshake(e.to_string()))?
    {
        httparse::Status::Complete(n) => n,
        _ => return Err(Error::Handshake("incomplete headers".into())),
    };
    if req.method != Some("GET") || req.version != Some(1) {
        return Err(Error::Handshake("expected HTTP/1.1 GET".into()));
    }
    let path = req
        .path
        .ok_or_else(|| Error::Handshake("missing request path".into()))?
        .to_owned();
    let map = header_map(req.headers)?;
    if !map.contains_key("host")
        || !token(map.get("upgrade"), "websocket")
        || !token(map.get("connection"), "upgrade")
        || map.get("sec-websocket-version").map(String::as_str) != Some("13")
    {
        return Err(Error::Handshake("invalid WebSocket upgrade headers".into()));
    }
    let key = map
        .get("sec-websocket-key")
        .ok_or_else(|| Error::Handshake("missing key".into()))?;
    if STANDARD
        .decode(key)
        .map_err(|_| Error::Handshake("invalid key".into()))?
        .len()
        != 16
    {
        return Err(Error::Handshake("key must contain 16 bytes".into()));
    }
    let initial = buf.split_off(used);
    Ok(Request {
        io,
        initial,
        path,
        headers: map,
    })
}
impl Request {
    pub async fn upgrade(
        mut self,
        parameter: Option<u8>,
        protocols: &[String],
    ) -> Result<Connection> {
        let key = self
            .headers
            .get("sec-websocket-key")
            .ok_or_else(|| Error::Handshake("missing key".into()))?;
        let mut reply = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\nServer: autobahn-testsuite-rs\r\n",
            accept_key(key)
        );
        let compression = if let (Some(p), Some(offers)) =
            (parameter, self.headers.get("sec-websocket-extensions"))
        {
            if let Some((header, params)) = compression::accept(offers, p)? {
                reply.push_str(&format!("Sec-WebSocket-Extensions: {header}\r\n"));
                Some(params)
            } else {
                None
            }
        } else {
            None
        };
        if let Some(offers) = self.headers.get("sec-websocket-protocol")
            && let Some(p) = offers
                .split(',')
                .map(str::trim)
                .find(|p| protocols.iter().any(|s| s == p))
        {
            reply.push_str(&format!("Sec-WebSocket-Protocol: {p}\r\n"));
        }
        reply.push_str("\r\n");
        self.io.write_all(reply.as_bytes()).await?;
        Ok(Connection {
            io: self.io,
            initial: self.initial,
            compression,
            server: true,
        })
    }
    pub async fn reject(mut self, message: &str) -> Result<()> {
        let response = format!(
            "HTTP/1.1 400 Bad Request\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{message}",
            message.len()
        );
        self.io.write_all(response.as_bytes()).await?;
        self.io.shutdown().await?;
        Ok(())
    }
}

pub(crate) fn client_tls(spec: &Spec) -> Result<Arc<rustls::ClientConfig>> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(path) = &spec.ca {
        for cert in rustls_pemfile::certs(&mut BufReader::new(std::fs::File::open(path)?)) {
            roots.add(cert?).map_err(|e| Error::Config(e.to_string()))?;
        }
    }
    Ok(Arc::new(
        rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ))
}
pub(crate) fn server_tls(spec: &Spec) -> Result<Option<tokio_rustls::TlsAcceptor>> {
    if !spec.url.starts_with("wss:") {
        return Ok(None);
    }
    let cert = spec
        .cert
        .as_ref()
        .ok_or_else(|| Error::Config("WSS needs --cert and --key".into()))?;
    let key = spec
        .key
        .as_ref()
        .ok_or_else(|| Error::Config("WSS needs --cert and --key".into()))?;
    let chain = rustls_pemfile::certs(&mut BufReader::new(std::fs::File::open(cert)?))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let key = rustls_pemfile::private_key(&mut BufReader::new(std::fs::File::open(key)?))?
        .ok_or_else(|| Error::Config("no private key".into()))?;
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .map_err(|e| Error::Config(e.to_string()))?;
    Ok(Some(tokio_rustls::TlsAcceptor::from(Arc::new(config))))
}
pub(crate) async fn connect(
    target: &Target,
    spec: &Spec,
    tls: Arc<rustls::ClientConfig>,
    parameter: Option<u8>,
) -> Result<Connection> {
    tokio::time::timeout(
        Duration::from_millis(spec.handshake_timeout_ms),
        connect_inner(target, spec, tls, parameter),
    )
    .await
    .map_err(|_| Error::Timeout)?
}
async fn connect_inner(
    target: &Target,
    spec: &Spec,
    tls: Arc<rustls::ClientConfig>,
    parameter: Option<u8>,
) -> Result<Connection> {
    let url = url::Url::parse(&target.url).map_err(|e| Error::Config(e.to_string()))?;
    let host = url
        .host_str()
        .ok_or_else(|| Error::Config("missing host".into()))?
        .trim_matches(['[', ']']);
    let port = url
        .port_or_known_default()
        .ok_or_else(|| Error::Config("missing port".into()))?;
    let tcp = TcpStream::connect((host, port)).await?;
    tcp.set_nodelay(true)?;
    let mut io: Socket = if url.scheme() == "wss" {
        let name = rustls::pki_types::ServerName::try_from(
            target.hostname.as_deref().unwrap_or(host).to_owned(),
        )
        .map_err(|e| Error::Config(e.to_string()))?;
        Box::new(
            tokio_rustls::TlsConnector::from(tls)
                .connect(name, tcp)
                .await?,
        )
    } else {
        Box::new(tcp)
    };
    let key = STANDARD.encode(rand::random::<[u8; 16]>());
    let host_header = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    let mut path = url.path().to_owned();
    if let Some(query) = url.query() {
        path.push('?');
        path.push_str(query);
    }
    let mut request = format!(
        "GET {path} HTTP/1.1\r\nHost: {host_header}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {key}\r\n"
    );
    if let Some(p) = parameter {
        request.push_str(&format!(
            "Sec-WebSocket-Extensions: {}\r\n",
            compression::offer(p)
        ));
    }
    if !spec.protocols.is_empty() {
        request.push_str(&format!(
            "Sec-WebSocket-Protocol: {}\r\n",
            spec.protocols.join(", ")
        ));
    }
    for (name, value) in &target.headers {
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
            || value.contains(['\r', '\n'])
            || matches!(
                name.to_ascii_lowercase().as_str(),
                "host"
                    | "upgrade"
                    | "connection"
                    | "sec-websocket-key"
                    | "sec-websocket-version"
                    | "sec-websocket-extensions"
                    | "sec-websocket-protocol"
            )
        {
            return Err(Error::Config(
                "invalid or reserved custom HTTP header".into(),
            ));
        }
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    io.write_all(request.as_bytes()).await?;
    let mut buf = read_headers(&mut io).await?;
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut response = httparse::Response::new(&mut headers);
    let used = match response
        .parse(&buf)
        .map_err(|e| Error::Handshake(e.to_string()))?
    {
        httparse::Status::Complete(n) => n,
        _ => return Err(Error::Handshake("incomplete response".into())),
    };
    if response.code != Some(101) || response.version != Some(1) {
        return Err(Error::Handshake(format!(
            "expected HTTP/1.1 101, got {:?}",
            response.code
        )));
    }
    let map = header_map(response.headers)?;
    if !token(map.get("upgrade"), "websocket")
        || !token(map.get("connection"), "upgrade")
        || map.get("sec-websocket-accept") != Some(&accept_key(&key))
    {
        return Err(Error::Handshake("invalid upgrade response".into()));
    }
    if map
        .get("sec-websocket-protocol")
        .is_some_and(|p| !spec.protocols.contains(p))
    {
        return Err(Error::Handshake("unsolicited subprotocol".into()));
    }
    let compression = match (map.get("sec-websocket-extensions"), parameter) {
        (Some(value), Some(p)) => Some(compression::response(value, p)?),
        (Some(_), None) => return Err(Error::Handshake("unsolicited extension".into())),
        _ => None,
    };
    let initial = buf.split_off(used);
    Ok(Connection {
        io,
        initial,
        compression,
        server: false,
    })
}

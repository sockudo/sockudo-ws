//! WebSocket handshake implementation
//!
//! This module handles the HTTP upgrade handshake for WebSocket connections.
//! It's designed for high performance with:
//! - Zero-copy header parsing where possible
//! - Minimal allocations
//! - Fast Base64/SHA-1 for accept key generation

use base64::Engine;
use bytes::{BufMut, Bytes, BytesMut};
use sha1::{Digest, Sha1};

use crate::WS_GUID;
use crate::error::{Error, Result};

/// Maximum HTTP header size (8KB should be enough for any reasonable request)
const MAX_HEADER_SIZE: usize = 8192;
const RESERVED_HANDSHAKE_HEADERS: &[&str] = &[
    "host",
    "upgrade",
    "connection",
    "sec-websocket-key",
    "sec-websocket-version",
    "sec-websocket-protocol",
    "sec-websocket-extensions",
];

/// WebSocket handshake request (server-side)
#[derive(Debug)]
pub struct HandshakeRequest<'a> {
    /// The request path
    pub path: &'a str,
    /// The Host header
    pub host: Option<&'a str>,
    /// The Sec-WebSocket-Key header
    pub key: &'a str,
    /// The Sec-WebSocket-Version header
    pub version: &'a str,
    /// The Sec-WebSocket-Protocol header (optional)
    pub protocol: Option<&'a str>,
    /// The Sec-WebSocket-Extensions header (optional)
    pub extensions: Option<&'a str>,
    /// The Origin header (optional)
    pub origin: Option<&'a str>,
}

/// Parse a WebSocket upgrade request
///
/// Returns the parsed request and the number of bytes consumed.
pub fn parse_request(buf: &[u8]) -> Result<Option<(HandshakeRequest<'_>, usize)>> {
    if buf.len() > MAX_HEADER_SIZE {
        return Err(Error::InvalidHttp("request too large"));
    }

    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut req = httparse::Request::new(&mut headers);

    match req.parse(buf) {
        Ok(httparse::Status::Complete(len)) => {
            // Validate HTTP method and version
            if req.method != Some("GET") {
                return Err(Error::InvalidHttp("method must be GET"));
            }

            // Extract required headers
            let mut key = None;
            let mut version = None;
            let mut host = None;
            let mut protocol = None;
            let mut extensions = None;
            let mut origin = None;
            let mut upgrade = false;
            let mut connection_upgrade = false;

            for header in req.headers.iter() {
                let name = header.name;
                let value = std::str::from_utf8(header.value)
                    .map_err(|_| Error::InvalidHttp("invalid header value"))?;

                // Case-insensitive comparisons without allocating per header.
                if name.eq_ignore_ascii_case("sec-websocket-key") {
                    key = Some(value);
                } else if name.eq_ignore_ascii_case("sec-websocket-version") {
                    version = Some(value);
                } else if name.eq_ignore_ascii_case("sec-websocket-protocol") {
                    protocol = Some(value);
                } else if name.eq_ignore_ascii_case("sec-websocket-extensions") {
                    extensions = Some(value);
                } else if name.eq_ignore_ascii_case("host") {
                    host = Some(value);
                } else if name.eq_ignore_ascii_case("origin") {
                    origin = Some(value);
                } else if name.eq_ignore_ascii_case("upgrade") {
                    if has_token_ignore_case(value, "websocket") {
                        upgrade = true;
                    }
                } else if name.eq_ignore_ascii_case("connection")
                    && has_token_ignore_case(value, "upgrade")
                {
                    connection_upgrade = true;
                }
            }

            // Validate required headers
            if !upgrade {
                return Err(Error::HandshakeFailed("missing Upgrade: websocket"));
            }
            if !connection_upgrade {
                return Err(Error::HandshakeFailed("missing Connection: Upgrade"));
            }
            let key = key.ok_or(Error::HandshakeFailed("missing Sec-WebSocket-Key"))?;
            let version = version.ok_or(Error::HandshakeFailed("missing Sec-WebSocket-Version"))?;

            if version != "13" {
                return Err(Error::HandshakeFailed("unsupported WebSocket version"));
            }

            let path = req.path.unwrap_or("/");

            Ok(Some((
                HandshakeRequest {
                    path,
                    host,
                    key,
                    version,
                    protocol,
                    extensions,
                    origin,
                },
                len,
            )))
        }
        Ok(httparse::Status::Partial) => Ok(None),
        Err(_) => Err(Error::InvalidHttp("failed to parse HTTP request")),
    }
}

/// Returns true if the comma-separated header `value` contains `token`
/// (ASCII case-insensitive, surrounding whitespace ignored).
#[inline]
fn has_token_ignore_case(value: &str, token: &str) -> bool {
    value
        .split(',')
        .any(|part| part.trim().eq_ignore_ascii_case(token))
}

/// Generate the Sec-WebSocket-Accept key
///
/// This computes: Base64(SHA-1(key + GUID))
#[inline]
pub fn generate_accept_key(key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(WS_GUID.as_bytes());
    let hash = hasher.finalize();
    base64::engine::general_purpose::STANDARD.encode(hash)
}

/// Build a WebSocket upgrade response.
///
/// # Errors
///
/// Returns [`Error::InvalidHttp`] if a generated header value contains bytes
/// that cannot appear in an HTTP field value.
pub fn build_response(
    accept_key: &str,
    protocol: Option<&str>,
    extensions: Option<&str>,
) -> Result<Bytes> {
    validate_header_value(accept_key, "invalid Sec-WebSocket-Accept")?;
    if let Some(protocol) = protocol {
        validate_header_value(protocol, "invalid Sec-WebSocket-Protocol")?;
    }
    if let Some(extensions) = extensions {
        validate_header_value(extensions, "invalid Sec-WebSocket-Extensions")?;
    }

    let mut buf = BytesMut::with_capacity(256);

    buf.put_slice(b"HTTP/1.1 101 Switching Protocols\r\n");
    buf.put_slice(b"Upgrade: websocket\r\n");
    buf.put_slice(b"Connection: Upgrade\r\n");
    buf.put_slice(b"Sec-WebSocket-Accept: ");
    buf.put_slice(accept_key.as_bytes());
    buf.put_slice(b"\r\n");

    if let Some(proto) = protocol {
        buf.put_slice(b"Sec-WebSocket-Protocol: ");
        buf.put_slice(proto.as_bytes());
        buf.put_slice(b"\r\n");
    }

    if let Some(ext) = extensions {
        buf.put_slice(b"Sec-WebSocket-Extensions: ");
        buf.put_slice(ext.as_bytes());
        buf.put_slice(b"\r\n");
    }

    buf.put_slice(b"\r\n");
    Ok(buf.freeze())
}

/// Build a WebSocket upgrade request (client-side).
///
/// # Errors
///
/// Returns [`Error::InvalidHttp`] if the request target or a generated header
/// value contains bytes that could change the HTTP request structure.
pub fn build_request(
    host: &str,
    path: &str,
    key: &str,
    protocol: Option<&str>,
    extensions: Option<&str>,
) -> Result<Bytes> {
    validate_request_fields(host, path, key, protocol, extensions)?;
    Ok(build_request_inner(
        host, path, key, protocol, extensions, None,
    ))
}

/// Build a WebSocket upgrade request with additional HTTP headers.
///
/// Custom headers are emitted in the supplied order. Header names must use the
/// HTTP token syntax, and values must not contain disallowed control bytes.
/// Headers managed by the WebSocket handshake cannot be overridden.
///
/// # Errors
///
/// Returns [`Error::InvalidHttp`] if the request target or a header name or
/// value is invalid, or if a custom header conflicts with a handshake-managed
/// header.
pub fn build_request_with_headers(
    host: &str,
    path: &str,
    key: &str,
    protocol: Option<&str>,
    extensions: Option<&str>,
    extra_headers: Option<&[(String, String)]>,
) -> Result<Bytes> {
    validate_request_fields(host, path, key, protocol, extensions)?;
    if let Some(headers) = extra_headers {
        validate_extra_headers(headers)?;
    }

    Ok(build_request_inner(
        host,
        path,
        key,
        protocol,
        extensions,
        extra_headers,
    ))
}

fn validate_request_fields(
    host: &str,
    path: &str,
    key: &str,
    protocol: Option<&str>,
    extensions: Option<&str>,
) -> Result<()> {
    validate_header_value(host, "invalid Host")?;
    if !path.bytes().all(is_request_target_byte) {
        return Err(Error::InvalidHttp("invalid request target"));
    }
    validate_header_value(key, "invalid Sec-WebSocket-Key")?;
    if let Some(protocol) = protocol {
        validate_header_value(protocol, "invalid Sec-WebSocket-Protocol")?;
    }
    if let Some(extensions) = extensions {
        validate_header_value(extensions, "invalid Sec-WebSocket-Extensions")?;
    }
    Ok(())
}

fn validate_header_value(value: &str, error: &'static str) -> Result<()> {
    if value.bytes().all(is_header_value_byte) {
        Ok(())
    } else {
        Err(Error::InvalidHttp(error))
    }
}

fn validate_extra_headers(headers: &[(String, String)]) -> Result<()> {
    for (name, value) in headers {
        if name.is_empty() || !name.bytes().all(is_header_name_byte) {
            return Err(Error::InvalidHttp("invalid header name"));
        }

        if RESERVED_HANDSHAKE_HEADERS
            .iter()
            .any(|reserved| name.eq_ignore_ascii_case(reserved))
        {
            return Err(Error::InvalidHttp("reserved handshake header"));
        }

        if !value.bytes().all(is_header_value_byte) {
            return Err(Error::InvalidHttp("invalid header value"));
        }
    }

    Ok(())
}

fn is_header_value_byte(byte: u8) -> bool {
    // Keep HTTP/1 validation independent of the optional HTTP/2 and HTTP/3
    // `http` dependency. RFC 9110 permits HTAB, visible bytes, and obs-text.
    byte == b'\t' || (byte >= b' ' && byte != 0x7f)
}

fn is_request_target_byte(byte: u8) -> bool {
    byte > b' ' && byte != 0x7f
}

fn is_header_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn build_request_inner(
    host: &str,
    path: &str,
    key: &str,
    protocol: Option<&str>,
    extensions: Option<&str>,
    extra_headers: Option<&[(String, String)]>,
) -> Bytes {
    let mut buf = BytesMut::with_capacity(512);

    buf.put_slice(b"GET ");
    buf.put_slice(path.as_bytes());
    buf.put_slice(b" HTTP/1.1\r\n");
    buf.put_slice(b"Host: ");
    buf.put_slice(host.as_bytes());
    buf.put_slice(b"\r\n");
    buf.put_slice(b"Upgrade: websocket\r\n");
    buf.put_slice(b"Connection: Upgrade\r\n");
    buf.put_slice(b"Sec-WebSocket-Key: ");
    buf.put_slice(key.as_bytes());
    buf.put_slice(b"\r\n");
    buf.put_slice(b"Sec-WebSocket-Version: 13\r\n");

    if let Some(proto) = protocol {
        buf.put_slice(b"Sec-WebSocket-Protocol: ");
        buf.put_slice(proto.as_bytes());
        buf.put_slice(b"\r\n");
    }

    if let Some(ext) = extensions {
        buf.put_slice(b"Sec-WebSocket-Extensions: ");
        buf.put_slice(ext.as_bytes());
        buf.put_slice(b"\r\n");
    }

    if let Some(headers) = extra_headers {
        for (name, value) in headers {
            buf.put_slice(name.as_bytes());
            buf.put_slice(b": ");
            buf.put_slice(value.as_bytes());
            buf.put_slice(b"\r\n");
        }
    }

    buf.put_slice(b"\r\n");
    buf.freeze()
}

/// Generate a random WebSocket key (client-side)
pub fn generate_key() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;

    let mut bytes = [0u8; 16];
    for byte in &mut bytes {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        *byte = seed as u8;
    }

    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// WebSocket handshake response (client-side parsing)
#[derive(Debug)]
pub struct HandshakeResponse<'a> {
    /// HTTP status code
    pub status: u16,
    /// The Sec-WebSocket-Accept header
    pub accept: Option<&'a str>,
    /// The Sec-WebSocket-Protocol header
    pub protocol: Option<&'a str>,
    /// The Sec-WebSocket-Extensions header
    pub extensions: Option<&'a str>,
}

/// Parse a WebSocket upgrade response (client-side)
pub fn parse_response(buf: &[u8]) -> Result<Option<(HandshakeResponse<'_>, usize)>> {
    if buf.len() > MAX_HEADER_SIZE {
        return Err(Error::InvalidHttp("response too large"));
    }

    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut res = httparse::Response::new(&mut headers);

    match res.parse(buf) {
        Ok(httparse::Status::Complete(len)) => {
            let status = res.code.unwrap_or(0);

            if status != 101 {
                return Err(Error::HandshakeFailed("expected 101 Switching Protocols"));
            }

            let mut accept = None;
            let mut protocol = None;
            let mut extensions = None;

            for header in res.headers.iter() {
                let name = header.name;
                let value = std::str::from_utf8(header.value)
                    .map_err(|_| Error::InvalidHttp("invalid header value"))?;

                if name.eq_ignore_ascii_case("sec-websocket-accept") {
                    accept = Some(value);
                } else if name.eq_ignore_ascii_case("sec-websocket-protocol") {
                    protocol = Some(value);
                } else if name.eq_ignore_ascii_case("sec-websocket-extensions") {
                    extensions = Some(value);
                }
            }

            Ok(Some((
                HandshakeResponse {
                    status,
                    accept,
                    protocol,
                    extensions,
                },
                len,
            )))
        }
        Ok(httparse::Status::Partial) => Ok(None),
        Err(_) => Err(Error::InvalidHttp("failed to parse HTTP response")),
    }
}

/// Validate the server's accept key (client-side)
pub fn validate_accept_key(sent_key: &str, received_accept: &str) -> bool {
    let expected = generate_accept_key(sent_key);
    expected == received_accept
}

/// Perform server-side handshake
#[cfg(feature = "tokio-runtime")]
pub async fn server_handshake<S>(stream: &mut S) -> Result<HandshakeResult>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut buf = BytesMut::with_capacity(4096);

    // Read the HTTP request
    loop {
        if buf.len() > MAX_HEADER_SIZE {
            return Err(Error::InvalidHttp("request too large"));
        }

        let n = stream.read_buf(&mut buf).await?;
        if n == 0 {
            return Err(Error::ConnectionClosed);
        }

        // Try to parse the request
        if let Some((req, consumed)) = parse_request(&buf)? {
            // Extract values before mutably borrowing buf
            let path = req.path.to_string();
            let protocol = req.protocol.map(String::from);
            let extensions = req.extensions.map(String::from);

            // Generate accept key
            let accept_key = generate_accept_key(req.key);

            // Build and send response
            let response = build_response(&accept_key, req.protocol, None)?;
            stream.write_all(&response).await?;
            stream.flush().await?;

            // Check if there's leftover data after the HTTP request
            let leftover = if consumed < buf.len() {
                Some(buf.split_off(consumed).freeze())
            } else {
                None
            };

            return Ok(HandshakeResult {
                path,
                protocol,
                extensions,
                leftover,
            });
        }
    }
}

/// Result of a successful handshake
#[derive(Debug)]
pub struct HandshakeResult {
    /// The request path
    pub path: String,
    /// Negotiated subprotocol
    pub protocol: Option<String>,
    /// Negotiated extensions
    pub extensions: Option<String>,
    /// Bytes read beyond the end of the HTTP handshake.
    ///
    /// High-level `connect*` and `accept*` methods automatically replay these
    /// bytes through the returned WebSocket stream. Direct handshake callers
    /// remain responsible for preserving them.
    pub leftover: Option<Bytes>,
}

/// Perform client-side handshake
#[cfg(feature = "tokio-runtime")]
pub async fn client_handshake<S>(
    stream: &mut S,
    host: &str,
    path: &str,
    protocol: Option<&str>,
) -> Result<HandshakeResult>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    client_handshake_with_headers(stream, host, path, protocol, None).await
}

/// Perform a client-side handshake with additional HTTP headers.
///
/// Header names and values are validated before any bytes are written. Headers
/// managed by the WebSocket handshake cannot be supplied through
/// `extra_headers`.
///
/// Once writing begins, cancelling this future leaves the stream in an
/// indeterminate handshake state and the stream should not be reused.
#[cfg(feature = "tokio-runtime")]
pub async fn client_handshake_with_headers<S>(
    stream: &mut S,
    host: &str,
    path: &str,
    protocol: Option<&str>,
    extra_headers: Option<&[(String, String)]>,
) -> Result<HandshakeResult>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Generate key and build request
    let key = generate_key();
    let request = build_request_with_headers(host, path, &key, protocol, None, extra_headers)?;

    // Send request
    stream.write_all(&request).await?;
    stream.flush().await?;

    // Read response
    let mut buf = BytesMut::with_capacity(4096);

    loop {
        if buf.len() > MAX_HEADER_SIZE {
            return Err(Error::InvalidHttp("response too large"));
        }

        let n = stream.read_buf(&mut buf).await?;
        if n == 0 {
            return Err(Error::ConnectionClosed);
        }

        if let Some((res, consumed)) = parse_response(&buf)? {
            // Validate accept key
            let accept = res
                .accept
                .ok_or(Error::HandshakeFailed("missing Sec-WebSocket-Accept"))?;
            if !validate_accept_key(&key, accept) {
                return Err(Error::HandshakeFailed("invalid Sec-WebSocket-Accept"));
            }

            // Extract values before mutably borrowing buf
            let res_protocol = res.protocol.map(String::from);
            let res_extensions = res.extensions.map(String::from);

            let leftover = if consumed < buf.len() {
                Some(buf.split_off(consumed).freeze())
            } else {
                None
            };

            return Ok(HandshakeResult {
                path: path.to_string(),
                protocol: res_protocol,
                extensions: res_extensions,
                leftover,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_accept_key() {
        // Test vector from RFC 6455
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let accept = generate_accept_key(key);
        assert_eq!(accept, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn test_parse_request() {
        let request = b"GET /chat HTTP/1.1\r\n\
            Host: server.example.com\r\n\
            Upgrade: websocket\r\n\
            Connection: Upgrade\r\n\
            Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
            Sec-WebSocket-Version: 13\r\n\
            \r\n";

        let (req, len) = parse_request(request).unwrap().unwrap();
        assert_eq!(req.path, "/chat");
        assert_eq!(req.key, "dGhlIHNhbXBsZSBub25jZQ==");
        assert_eq!(req.version, "13");
        assert_eq!(len, request.len());
    }

    #[test]
    fn test_parse_request_partial() {
        let request = b"GET /chat HTTP/1.1\r\n\
            Host: server.example.com\r\n";

        assert!(parse_request(request).unwrap().is_none());
    }

    #[test]
    fn test_build_response() {
        let accept = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";
        let response = build_response(accept, None, None).unwrap();

        let response_str = std::str::from_utf8(&response).unwrap();
        assert!(response_str.contains("101 Switching Protocols"));
        assert!(response_str.contains("Upgrade: websocket"));
        assert!(response_str.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo="));
    }

    #[test]
    fn request_builder_rejects_injected_lines() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let injected = "safe\r\nX-Injected: true";

        for result in [
            build_request(injected, "/ws", key, None, None),
            build_request("example.com", "/ws\r\nX-Injected: true", key, None, None),
            build_request("example.com", "/ws", injected, None, None),
            build_request("example.com", "/ws", key, Some(injected), None),
            build_request("example.com", "/ws", key, None, Some(injected)),
        ] {
            assert!(matches!(result, Err(Error::InvalidHttp(_))));
        }
    }

    #[test]
    fn response_builder_rejects_injected_headers() {
        let accept = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";
        let injected = "safe\r\nX-Injected: true";

        for result in [
            build_response(injected, None, None),
            build_response(accept, Some(injected), None),
            build_response(accept, None, Some(injected)),
        ] {
            assert!(matches!(result, Err(Error::InvalidHttp(_))));
        }
    }

    #[test]
    fn test_validate_accept_key() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let accept = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";
        assert!(validate_accept_key(key, accept));
        assert!(!validate_accept_key(key, "invalid"));
    }
}

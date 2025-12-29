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
                let name = header.name.to_ascii_lowercase();
                let value = std::str::from_utf8(header.value)
                    .map_err(|_| Error::InvalidHttp("invalid header value"))?;

                match name.as_str() {
                    "sec-websocket-key" => key = Some(value),
                    "sec-websocket-version" => version = Some(value),
                    "sec-websocket-protocol" => protocol = Some(value),
                    "sec-websocket-extensions" => extensions = Some(value),
                    "host" => host = Some(value),
                    "origin" => origin = Some(value),
                    "upgrade" => {
                        if value.to_ascii_lowercase().contains("websocket") {
                            upgrade = true;
                        }
                    }
                    "connection" => {
                        if value.to_ascii_lowercase().contains("upgrade") {
                            connection_upgrade = true;
                        }
                    }
                    _ => {}
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

/// Build a WebSocket upgrade response
pub fn build_response(accept_key: &str, protocol: Option<&str>, extensions: Option<&str>) -> Bytes {
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
    buf.freeze()
}

/// Build a WebSocket upgrade request (client-side)
pub fn build_request(
    host: &str,
    path: &str,
    key: &str,
    protocol: Option<&str>,
    extensions: Option<&str>,
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
                let name = header.name.to_ascii_lowercase();
                let value = std::str::from_utf8(header.value)
                    .map_err(|_| Error::InvalidHttp("invalid header value"))?;

                match name.as_str() {
                    "sec-websocket-accept" => accept = Some(value),
                    "sec-websocket-protocol" => protocol = Some(value),
                    "sec-websocket-extensions" => extensions = Some(value),
                    _ => {}
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
            let response = build_response(&accept_key, req.protocol, None);
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
    /// Leftover data after HTTP request (if any)
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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Generate key and build request
    let key = generate_key();
    let request = build_request(host, path, &key, protocol, None);

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
        let response = build_response(accept, None, None);

        let response_str = std::str::from_utf8(&response).unwrap();
        assert!(response_str.contains("101 Switching Protocols"));
        assert!(response_str.contains("Upgrade: websocket"));
        assert!(response_str.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo="));
    }

    #[test]
    fn test_validate_accept_key() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let accept = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";
        assert!(validate_accept_key(key, accept));
        assert!(!validate_accept_key(key, "invalid"));
    }
}

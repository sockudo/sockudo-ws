//! WebSocket handshake implementation
//!
//! This module handles the HTTP upgrade handshake for WebSocket connections.
//! It's designed for high performance with:
//! - Zero-copy header parsing where possible
//! - Minimal allocations
//! - Fast Base64/SHA-1 for accept key generation

use std::{borrow::Cow, collections::HashSet};

use base64::Engine;
use bytes::{BufMut, Bytes, BytesMut};
use http::Uri;
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
    "content-length",
    "transfer-encoding",
];

/// WebSocket handshake request (server-side)
#[derive(Debug)]
pub struct HandshakeRequest<'a> {
    /// The resource name derived from the request target.
    ///
    /// Usually borrowed from the request; query-only absolute targets allocate
    /// a leading `/`. Use `path.as_ref()` when a borrowed `&str` is required.
    pub path: Cow<'a, str>,
    /// The effective authority from an absolute target or the Host header.
    /// A Host header is still required for absolute-form requests.
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
///
/// Used by the built-in Tokio and Compio HTTP/1 server handshakes; Axum's
/// upgrade extractor uses Hyper's URI parsing instead.
///
/// Accepts origin-form and absolute HTTP/HTTPS targets. Path and query characters
/// follow `http::Uri`'s compatibility rules (including raw UTF-8 and JSON path
/// characters), with an additional requirement that percent escapes contain two
/// hexadecimal digits. This is not strict RFC 3986 character validation.
/// Fragments, unsupported target forms or schemes, userinfo, empty absolute hosts
/// and nonnumeric ports are rejected. A Host header remains required even when
/// the absolute target supplies the effective authority.
pub fn parse_request(buf: &[u8]) -> Result<Option<(HandshakeRequest<'_>, usize)>> {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut req = httparse::Request::new(&mut headers);
    // A valid header fits within the limit; one more byte detects an oversized partial header.
    let parse_buf = &buf[..buf.len().min(MAX_HEADER_SIZE + 1)];

    match req.parse(parse_buf) {
        Ok(httparse::Status::Complete(len)) => {
            if len > MAX_HEADER_SIZE {
                return Err(Error::InvalidHttp("request too large"));
            }

            // Validate HTTP method and version
            if req.method != Some("GET") {
                return Err(Error::InvalidHttp("method must be GET"));
            }
            if req.version != Some(1) {
                return Err(Error::InvalidHttp("HTTP version must be 1.1"));
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
                    if key.is_some() {
                        return Err(Error::HandshakeFailed("duplicate Sec-WebSocket-Key"));
                    }
                    key = Some(value);
                } else if name.eq_ignore_ascii_case("sec-websocket-version") {
                    if version.is_some() {
                        return Err(Error::HandshakeFailed("duplicate Sec-WebSocket-Version"));
                    }
                    version = Some(value);
                } else if name.eq_ignore_ascii_case("sec-websocket-protocol") {
                    if !is_valid_protocol_list(value) {
                        return Err(Error::HandshakeFailed("invalid Sec-WebSocket-Protocol"));
                    }
                    protocol = Some(value);
                } else if name.eq_ignore_ascii_case("sec-websocket-extensions") {
                    if !is_valid_extension_list(value) {
                        return Err(Error::HandshakeFailed("invalid Sec-WebSocket-Extensions"));
                    }
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
                } else if name.eq_ignore_ascii_case("content-length") {
                    if !is_zero_content_length(value) {
                        return Err(Error::InvalidHttp(
                            "WebSocket handshake must not contain a body",
                        ));
                    }
                } else if name.eq_ignore_ascii_case("transfer-encoding") {
                    return Err(Error::InvalidHttp(
                        "WebSocket handshake must not use Transfer-Encoding",
                    ));
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
            let host = host
                .filter(|value| !value.is_empty())
                .ok_or(Error::HandshakeFailed("missing Host"))?;

            if version != "13" {
                return Err(Error::HandshakeFailed("unsupported WebSocket version"));
            }
            if !is_valid_websocket_key(key) {
                return Err(Error::HandshakeFailed("invalid Sec-WebSocket-Key"));
            }

            let target = req
                .path
                .ok_or(Error::InvalidHttp("missing request target"))?;
            let (path, host) = parse_server_request_target(target, host)
                .ok_or(Error::InvalidHttp("invalid request target"))?;

            Ok(Some((
                HandshakeRequest {
                    path,
                    host: Some(host),
                    key,
                    version,
                    protocol,
                    extensions,
                    origin,
                },
                len,
            )))
        }
        Ok(httparse::Status::Partial) if buf.len() > MAX_HEADER_SIZE => {
            Err(Error::InvalidHttp("request too large"))
        }
        Ok(httparse::Status::Partial) => Ok(None),
        Err(_) => Err(Error::InvalidHttp("failed to parse HTTP request")),
    }
}

fn is_zero_content_length(value: &str) -> bool {
    list_elements(value).all(|length| !length.is_empty() && length.bytes().all(|byte| byte == b'0'))
}

/// Returns true if the comma-separated header `value` contains `token`
/// (ASCII case-insensitive, surrounding HTTP optional whitespace ignored).
#[inline]
fn has_token_ignore_case(value: &str, token: &str) -> bool {
    value
        .split(',')
        .any(|part| trim_optional_whitespace(part).eq_ignore_ascii_case(token))
}

fn is_valid_websocket_key(value: &str) -> bool {
    let mut decoded = [0; 24];
    base64::engine::general_purpose::STANDARD
        .decode_slice(value, &mut decoded)
        .is_ok_and(|len| len == 16)
}

fn parse_server_request_target<'a>(
    target: &'a str,
    header_host: &'a str,
) -> Option<(Cow<'a, str>, &'a str)> {
    if is_valid_origin_form(target) {
        return Some((Cow::Borrowed(target), header_host));
    }

    let uri = target.parse::<Uri>().ok()?;
    let scheme = uri.scheme_str()?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }

    let parsed_authority = uri.authority()?;
    if parsed_authority.host().is_empty() || parsed_authority.as_str().contains('@') {
        return None;
    }
    let port_suffix = parsed_authority
        .as_str()
        .get(parsed_authority.host().len()..)?;
    if !port_suffix.is_empty()
        && !port_suffix
            .strip_prefix(':')
            .is_some_and(|port| port.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return None;
    }

    let scheme_end = target.find("://")?;
    let authority_start = scheme_end + 3;
    let authority_end = authority_start.checked_add(parsed_authority.as_str().len())?;
    let authority = target.get(authority_start..authority_end)?;
    if authority != parsed_authority.as_str() {
        return None;
    }

    let resource = target.get(authority_end..)?;
    // Uri may discard a fragment; a request target must never contain one,
    // including in the query-only branch where we synthesize a leading slash.
    if resource.contains('#') || !has_valid_percent_encoding(resource) {
        return None;
    }
    let path = if resource.is_empty() {
        Cow::Borrowed("/")
    } else if resource.starts_with('?') {
        Cow::Owned(format!("/{resource}"))
    } else if uri
        .path_and_query()
        .is_some_and(|path_and_query| path_and_query.as_str() == resource)
    {
        Cow::Borrowed(resource)
    } else {
        return None;
    };

    Some((path, authority))
}

fn is_valid_origin_form(target: &str) -> bool {
    target.starts_with('/')
        && has_valid_percent_encoding(target)
        && target.parse::<Uri>().is_ok_and(|uri| {
            uri.scheme().is_none()
                && uri.authority().is_none()
                && uri
                    .path_and_query()
                    .is_some_and(|path_and_query| path_and_query.as_str() == target)
        })
}

fn has_valid_percent_encoding(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut index = 0;

    while let Some(offset) = bytes[index..].iter().position(|byte| *byte == b'%') {
        index += offset;
        let Some(encoded) = bytes.get(index + 1..index + 3) else {
            return false;
        };
        if !encoded.iter().all(u8::is_ascii_hexdigit) {
            return false;
        }
        index += 3;
    }

    true
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
/// This raw builder does not validate its arguments. Callers must supply
/// values that follow the WebSocket handshake field grammar.
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

/// Build a WebSocket upgrade request (client-side).
///
/// This raw builder does not validate its arguments. Use
/// [`build_request_with_headers`] for externally supplied values.
pub fn build_request(
    host: &str,
    path: &str,
    key: &str,
    protocol: Option<&str>,
    extensions: Option<&str>,
) -> Bytes {
    build_request_inner(host, path, key, protocol, extensions, None)
}

/// Build a WebSocket upgrade request with additional HTTP headers.
///
/// Custom headers are emitted in the supplied order. Header names must use the
/// HTTP token syntax, values must not contain disallowed control bytes, and
/// Host must be nonempty, the key must encode 16 bytes, and WebSocket protocol
/// and extension values must follow their handshake field grammar. Headers
/// managed by the WebSocket handshake and request body framing headers cannot
/// be supplied.
///
/// # Errors
///
/// Returns [`Error::InvalidHttp`] if the request target or a header name or
/// value is invalid, or if a custom header uses a reserved name.
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
    if host.is_empty() {
        return Err(Error::InvalidHttp("invalid Host"));
    }
    if !path.bytes().all(is_request_target_byte) {
        return Err(Error::InvalidHttp("invalid request target"));
    }
    if !is_valid_websocket_key(key) {
        return Err(Error::InvalidHttp("invalid Sec-WebSocket-Key"));
    }
    if let Some(protocol) = protocol
        && (!is_valid_protocol_list(protocol) || list_elements(protocol).any(str::is_empty))
    {
        return Err(Error::InvalidHttp("invalid Sec-WebSocket-Protocol"));
    }
    if let Some(extensions) = extensions
        && (!is_valid_extension_list(extensions) || list_elements(extensions).any(str::is_empty))
    {
        return Err(Error::InvalidHttp("invalid Sec-WebSocket-Extensions"));
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
        if !is_token(name) {
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

fn is_token(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(is_header_name_byte)
}

pub(crate) fn trim_optional_whitespace(value: &str) -> &str {
    value.trim_matches(|character| matches!(character, ' ' | '\t'))
}

fn list_elements(value: &str) -> impl Iterator<Item = &str> {
    value.split(',').map(trim_optional_whitespace)
}

/// Preserve the default server's historical behavior of accepting any single
/// offered protocol while returning a valid selection for multi-value offers.
/// `value` must come from `parse_request`, which validates the complete offer.
pub(crate) fn select_default_subprotocol(value: Option<&str>) -> Option<&str> {
    value.and_then(|value| list_elements(value).find(|protocol| !protocol.is_empty()))
}

pub(crate) fn validate_supported_protocols(protocols: &[String]) -> Result<()> {
    if protocols.iter().all(|protocol| is_token(protocol)) {
        Ok(())
    } else {
        Err(Error::InvalidHttp("invalid supported subprotocol"))
    }
}

/// Select the first server-preferred protocol present in an offer previously
/// validated by `parse_request`.
pub(crate) fn select_supported_subprotocol<'a>(
    offered: Option<&str>,
    supported: &'a [String],
) -> Option<&'a str> {
    let offered = offered?;
    supported
        .iter()
        .find(|candidate| list_elements(offered).any(|offer| offer == candidate.as_str()))
        .map(String::as_str)
}

fn is_valid_protocol_list(value: &str) -> bool {
    // RFC 6455 requires every offered subprotocol to be unique.
    let mut protocols = HashSet::new();

    for protocol in list_elements(value).filter(|protocol| !protocol.is_empty()) {
        if !is_token(protocol) || !protocols.insert(protocol) {
            return false;
        }
    }

    !protocols.is_empty()
}

fn is_valid_extension_list(value: &str) -> bool {
    let mut has_extension = false;

    for extension in list_elements(value).filter(|extension| !extension.is_empty()) {
        has_extension = true;
        let mut parts = extension.split(';').map(trim_optional_whitespace);
        if !parts.next().is_some_and(is_token) {
            return false;
        }
        if !parts.all(|parameter| {
            if let Some((name, value)) = parameter.split_once('=') {
                is_token(trim_optional_whitespace(name))
                    && is_valid_extension_value(trim_optional_whitespace(value))
            } else {
                is_token(parameter)
            }
        }) {
            return false;
        }
    }

    has_extension
}

fn is_valid_extension_value(value: &str) -> bool {
    if is_token(value) {
        return true;
    }

    // RFC 6455 narrows HTTP quoted-string values: after unescaping, the value
    // must still satisfy the token grammar.
    let Some(value) = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    else {
        return false;
    };
    if value.is_empty() {
        return false;
    }

    let mut bytes = value.bytes();
    while let Some(byte) = bytes.next() {
        let unescaped = if byte == b'\\' {
            let Some(escaped) = bytes.next() else {
                return false;
            };
            escaped
        } else {
            byte
        };
        if !is_header_name_byte(unescaped) {
            return false;
        }
    }

    true
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

/// Generate a random WebSocket key (client-side).
///
/// Uses the selected RNG backend, in priority order: `getrandom`, `rand_rng`,
/// then `fastrand`. The default `fastrand` backend and the no-feature fallback
/// are non-cryptographic; native fastrand seeds from a clock and thread ID.
/// Use `getrandom` or `rand_rng` for cryptographically secure output.
/// With `getrandom`, an entropy-source failure panics,
/// matching frame-mask generation.
pub fn generate_key() -> String {
    let bytes = crate::mask::generate_key_bytes();
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
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut res = httparse::Response::new(&mut headers);
    // A valid header fits within the limit; one more byte detects an oversized partial header.
    let parse_buf = &buf[..buf.len().min(MAX_HEADER_SIZE + 1)];

    match res.parse(parse_buf) {
        Ok(httparse::Status::Complete(len)) => {
            if len > MAX_HEADER_SIZE {
                return Err(Error::InvalidHttp("response too large"));
            }

            let status = res.code.unwrap_or(0);

            if status != 101 {
                return Err(Error::HandshakeFailed("expected 101 Switching Protocols"));
            }
            if res.version != Some(1) {
                return Err(Error::InvalidHttp("HTTP version must be 1.1"));
            }

            let mut accept = None;
            let mut protocol = None;
            let mut extensions = None;
            let mut upgrade = false;
            let mut connection_upgrade = false;

            for header in res.headers.iter() {
                let name = header.name;
                let value = std::str::from_utf8(header.value)
                    .map_err(|_| Error::InvalidHttp("invalid header value"))?;

                if name.eq_ignore_ascii_case("sec-websocket-accept") {
                    if accept.is_some() {
                        return Err(Error::HandshakeFailed("duplicate Sec-WebSocket-Accept"));
                    }
                    accept = Some(value);
                } else if name.eq_ignore_ascii_case("sec-websocket-protocol") {
                    if protocol.is_some() {
                        return Err(Error::HandshakeFailed("duplicate Sec-WebSocket-Protocol"));
                    }
                    if !is_token(value) {
                        return Err(Error::HandshakeFailed("invalid Sec-WebSocket-Protocol"));
                    }
                    protocol = Some(value);
                } else if name.eq_ignore_ascii_case("sec-websocket-extensions") {
                    if extensions.is_some() {
                        return Err(Error::HandshakeFailed("duplicate Sec-WebSocket-Extensions"));
                    }
                    if !is_valid_extension_list(value) {
                        return Err(Error::HandshakeFailed("invalid Sec-WebSocket-Extensions"));
                    }
                    extensions = Some(value);
                } else if name.eq_ignore_ascii_case("upgrade") {
                    // RFC 6455 requires an exact response value, unlike the request-side list.
                    if !value.eq_ignore_ascii_case("websocket") {
                        return Err(Error::HandshakeFailed("missing Upgrade: websocket"));
                    }
                    upgrade = true;
                } else if name.eq_ignore_ascii_case("connection") {
                    connection_upgrade |= has_token_ignore_case(value, "upgrade");
                }
            }

            if !upgrade {
                return Err(Error::HandshakeFailed("missing Upgrade: websocket"));
            }
            if !connection_upgrade {
                return Err(Error::HandshakeFailed("missing Connection: Upgrade"));
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
        Ok(httparse::Status::Partial) if buf.len() > MAX_HEADER_SIZE => {
            Err(Error::InvalidHttp("response too large"))
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

/// Validate the selected subprotocol from a parsed response against the
/// protocol list already validated by the request builder.
pub(crate) fn validate_selected_protocol(
    offered: Option<&str>,
    selected: Option<&str>,
) -> Result<()> {
    let Some(selected) = selected else {
        return Ok(());
    };

    if offered.is_some_and(|offered| list_elements(offered).any(|candidate| candidate == selected))
    {
        Ok(())
    } else {
        Err(Error::HandshakeFailed(
            "server returned an unoffered subprotocol",
        ))
    }
}

/// Perform server-side handshake
#[cfg(feature = "tokio-runtime")]
pub async fn server_handshake<S>(stream: &mut S) -> Result<HandshakeResult>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    server_handshake_with_supported_protocols(stream, None).await
}

#[cfg(feature = "tokio-runtime")]
pub(crate) async fn server_handshake_with_supported_protocols<S>(
    stream: &mut S,
    supported_protocols: Option<&[String]>,
) -> Result<HandshakeResult>
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
            let protocol = match supported_protocols {
                Some(supported) => select_supported_subprotocol(req.protocol, supported),
                None => select_default_subprotocol(req.protocol),
            }
            .map(str::to_owned);
            let extensions = req.extensions.map(String::from);

            // Generate accept key
            let accept_key = generate_accept_key(req.key);

            // Build and send response
            let response = build_response(&accept_key, protocol.as_deref(), None);
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
            validate_selected_protocol(protocol, res.protocol)?;

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
    fn generated_key_decodes_to_sixteen_bytes() {
        use base64::Engine;
        let key = super::generate_key();
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(key)
                .unwrap()
                .len(),
            16
        );
    }

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
    fn request_header_limit_excludes_upgraded_frame_bytes() {
        let mut input = b"GET /chat HTTP/1.1\r\n\
            Host: server.example.com\r\n\
            Upgrade: websocket\r\n\
            Connection: Upgrade\r\n\
            Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
            Sec-WebSocket-Version: 13\r\n\
            \r\n"
            .to_vec();
        let header_len = input.len();
        input.resize(MAX_HEADER_SIZE + 1, 0);

        let (_, consumed) = parse_request(&input).unwrap().unwrap();

        assert_eq!(consumed, header_len);
    }

    #[test]
    fn response_header_limit_excludes_upgraded_frame_bytes() {
        let mut input = b"HTTP/1.1 101 Switching Protocols\r\n\
            Upgrade: websocket\r\n\
            Connection: Upgrade\r\n\
            Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\
            \r\n"
            .to_vec();
        let header_len = input.len();
        input.resize(MAX_HEADER_SIZE + 1, 0);

        let (_, consumed) = parse_response(&input).unwrap().unwrap();

        assert_eq!(consumed, header_len);
    }

    #[test]
    fn request_header_limit_accepts_exactly_eight_kib() {
        let mut input = build_request(
            "server.example.com",
            "/chat",
            "dGhlIHNhbXBsZSBub25jZQ==",
            None,
            None,
        )
        .to_vec();
        input.truncate(input.len() - 2);
        input.extend_from_slice(b"X-Pad: ");
        input.resize(MAX_HEADER_SIZE - 4, b'a');
        input.extend_from_slice(b"\r\n\r\n");
        assert_eq!(input.len(), MAX_HEADER_SIZE);

        assert_eq!(parse_request(&input).unwrap().unwrap().1, MAX_HEADER_SIZE);
        input.insert(MAX_HEADER_SIZE - 4, b'a');
        assert!(matches!(
            parse_request(&input),
            Err(Error::InvalidHttp("request too large"))
        ));
    }

    #[test]
    fn response_header_limit_accepts_exactly_eight_kib() {
        let mut input = build_response("s3pPLMBiTxaQ9kYGzzhZRbK+xOo=", None, None).to_vec();
        input.truncate(input.len() - 2);
        input.extend_from_slice(b"X-Pad: ");
        input.resize(MAX_HEADER_SIZE - 4, b'a');
        input.extend_from_slice(b"\r\n\r\n");
        assert_eq!(input.len(), MAX_HEADER_SIZE);

        assert_eq!(parse_response(&input).unwrap().unwrap().1, MAX_HEADER_SIZE);
        input.insert(MAX_HEADER_SIZE - 4, b'a');
        assert!(matches!(
            parse_response(&input),
            Err(Error::InvalidHttp("response too large"))
        ));
    }

    #[test]
    fn partial_request_still_observes_the_header_limit() {
        let mut input = b"GET /chat HTTP/1.1\r\nX-Pad: ".to_vec();
        input.resize(MAX_HEADER_SIZE, b'a');
        assert!(parse_request(&input).unwrap().is_none());

        input.push(b'a');
        assert!(matches!(
            parse_request(&input),
            Err(Error::InvalidHttp("request too large"))
        ));
    }

    #[test]
    fn oversized_request_stops_parsing_at_the_header_limit() {
        let mut input = b"GET /chat HTTP/1.1\r\nX-Pad: ".to_vec();
        input.resize(MAX_HEADER_SIZE + 1, b'a');
        input.push(0); // Invalid HTTP syntax beyond the size boundary must not be parsed.

        assert!(matches!(
            parse_request(&input),
            Err(Error::InvalidHttp("request too large"))
        ));
    }

    #[test]
    fn partial_response_still_observes_the_header_limit() {
        let mut input = b"HTTP/1.1 101 Switching Protocols\r\nX-Pad: ".to_vec();
        input.resize(MAX_HEADER_SIZE, b'a');
        assert!(parse_response(&input).unwrap().is_none());

        input.push(b'a');
        assert!(matches!(
            parse_response(&input),
            Err(Error::InvalidHttp("response too large"))
        ));
    }

    #[test]
    fn oversized_response_stops_parsing_at_the_header_limit() {
        let mut input = b"HTTP/1.1 101 Switching Protocols\r\nX-Pad: ".to_vec();
        input.resize(MAX_HEADER_SIZE + 1, b'a');
        input.push(0); // Invalid HTTP syntax beyond the size boundary must not be parsed.

        assert!(matches!(
            parse_response(&input),
            Err(Error::InvalidHttp("response too large"))
        ));
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
    fn checked_request_builder_rejects_injected_lines() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let injected = "safe\r\nX-Injected: true";

        for result in [
            build_request_with_headers(injected, "/ws", key, None, None, None),
            build_request_with_headers(
                "example.com",
                "/ws\r\nX-Injected: true",
                key,
                None,
                None,
                None,
            ),
            build_request_with_headers("example.com", "/ws", injected, None, None, None),
            build_request_with_headers("example.com", "/ws", key, Some(injected), None, None),
            build_request_with_headers("example.com", "/ws", key, None, Some(injected), None),
        ] {
            assert!(matches!(result, Err(Error::InvalidHttp(_))));
        }
    }

    #[test]
    fn checked_request_builder_rejects_whitespace_in_request_target() {
        let result = build_request_with_headers(
            "example.com",
            "/ws bad",
            "dGhlIHNhbXBsZSBub25jZQ==",
            None,
            None,
            None,
        );

        assert!(matches!(
            result,
            Err(Error::InvalidHttp("invalid request target"))
        ));
    }

    #[test]
    fn test_validate_accept_key() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let accept = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";
        assert!(validate_accept_key(key, accept));
        assert!(!validate_accept_key(key, "invalid"));
    }

    #[test]
    fn response_rejects_duplicate_subprotocol_headers() {
        let response = b"HTTP/1.1 101 Switching Protocols\r\n\
            Sec-WebSocket-Protocol: chat\r\n\
            Sec-WebSocket-Protocol: superchat\r\n\
            \r\n";

        assert!(matches!(
            parse_response(response),
            Err(Error::HandshakeFailed("duplicate Sec-WebSocket-Protocol"))
        ));
    }

    #[test]
    fn selected_subprotocol_must_exactly_match_one_offer() {
        let cases = [
            (Some("chat,\tsuperchat"), Some("superchat"), true),
            (Some("chat"), None, true),
            (None, Some("chat"), false),
            (Some("chat"), Some("other"), false),
            (Some("chat"), Some("CHAT"), false),
        ];

        for (offered, selected, valid) in cases {
            assert_eq!(
                validate_selected_protocol(offered, selected).is_ok(),
                valid,
                "offered={offered:?}, selected={selected:?}"
            );
        }
    }
}

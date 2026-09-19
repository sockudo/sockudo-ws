//! WebSocket handshake implementation
//!
//! This module handles the HTTP upgrade handshake for WebSocket connections.
//! It's designed for high performance with:
//! - Zero-copy header parsing where possible
//! - Minimal allocations
//! - Fast Base64/SHA-1 for accept key generation

use std::borrow::Cow;

use base64::Engine;
use bytes::{BufMut, Bytes, BytesMut};
use http::{Uri, uri::Authority};
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
    "expect",
];

/// WebSocket handshake request (server-side)
#[derive(Debug)]
pub struct HandshakeRequest<'a> {
    /// The request path
    pub path: Cow<'a, str>,
    /// The effective request authority from the absolute target or Host header
    pub host: Option<&'a str>,
    /// The Sec-WebSocket-Key header
    pub key: &'a str,
    /// The Sec-WebSocket-Version header
    pub version: &'a str,
    /// The Sec-WebSocket-Protocol header (optional)
    pub protocol: Option<Cow<'a, str>>,
    /// The Sec-WebSocket-Extensions header (optional)
    pub extensions: Option<Cow<'a, str>>,
    /// The Origin header (optional)
    pub origin: Option<&'a str>,
}

/// Protocol and extensions selected by a server handshake callback.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HandshakeSelection {
    /// The single selected subprotocol, if any.
    pub protocol: Option<String>,
    /// The negotiated extension response, if any.
    pub extensions: Option<String>,
}

/// Parse a WebSocket upgrade request
///
/// Returns the parsed request and the number of bytes consumed.
pub fn parse_request(buf: &[u8]) -> Result<Option<(HandshakeRequest<'_>, usize)>> {
    // Bytes after the HTTP header belong to the upgraded WebSocket stream.
    let header = &buf[..buf.len().min(MAX_HEADER_SIZE)];

    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut req = httparse::Request::new(&mut headers);

    match req.parse(header) {
        Ok(httparse::Status::Complete(len)) => {
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
                    .map_err(|_| Error::InvalidHttp("invalid header value"))?
                    .trim();

                // Case-insensitive comparisons without allocating per header.
                match name {
                    name if name.eq_ignore_ascii_case("sec-websocket-key") => {
                        if key.replace(value).is_some() {
                            return Err(Error::HandshakeFailed("duplicate Sec-WebSocket-Key"));
                        }
                    }
                    name if name.eq_ignore_ascii_case("sec-websocket-version") => {
                        if version.replace(value).is_some() {
                            return Err(Error::HandshakeFailed("duplicate Sec-WebSocket-Version"));
                        }
                    }
                    name if name.eq_ignore_ascii_case("sec-websocket-protocol") => {
                        append_header_value(&mut protocol, value);
                    }
                    name if name.eq_ignore_ascii_case("sec-websocket-extensions") => {
                        append_header_value(&mut extensions, value);
                    }
                    name if name.eq_ignore_ascii_case("host") => {
                        if host.replace(value).is_some() {
                            return Err(Error::HandshakeFailed("duplicate Host"));
                        }
                    }
                    name if name.eq_ignore_ascii_case("origin") => {
                        if origin.replace(value).is_some() {
                            return Err(Error::HandshakeFailed("duplicate Origin"));
                        }
                    }
                    name if name.eq_ignore_ascii_case("upgrade")
                        && (contains_header_token(value, "websocket")) =>
                    {
                        upgrade = true;
                    }
                    name if name.eq_ignore_ascii_case("connection")
                        && (contains_header_token(value, "upgrade")) =>
                    {
                        connection_upgrade = true;
                    }
                    name if name.eq_ignore_ascii_case("content-length") => {
                        if !is_zero_content_length(value) {
                            return Err(Error::InvalidHttp(
                                "WebSocket handshake must not contain a body",
                            ));
                        }
                    }
                    name if name.eq_ignore_ascii_case("transfer-encoding") => {
                        return Err(Error::InvalidHttp(
                            "WebSocket handshake must not use Transfer-Encoding",
                        ));
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
            let host = host.ok_or(Error::HandshakeFailed("missing Host"))?;

            if version != "13" {
                return Err(Error::HandshakeFailed("unsupported WebSocket version"));
            }
            if !is_valid_websocket_key(key) {
                return Err(Error::HandshakeFailed("invalid Sec-WebSocket-Key"));
            }
            if !is_valid_host(host) {
                return Err(Error::InvalidHttp("invalid Host"));
            }
            if protocol
                .as_deref()
                .is_some_and(|value| !is_valid_protocol_list(value))
            {
                return Err(Error::HandshakeFailed("invalid Sec-WebSocket-Protocol"));
            }
            if extensions
                .as_deref()
                .is_some_and(|value| !is_valid_extension_list(value))
            {
                return Err(Error::HandshakeFailed("invalid Sec-WebSocket-Extensions"));
            }

            let request_target = req
                .path
                .ok_or(Error::InvalidHttp("missing request target"))?;
            let (path, effective_host) = parse_server_request_target(request_target, host)
                .ok_or(Error::InvalidHttp("invalid request target"))?;

            Ok(Some((
                HandshakeRequest {
                    path,
                    host: Some(effective_host),
                    key,
                    version,
                    protocol,
                    extensions,
                    origin,
                },
                len,
            )))
        }
        Ok(httparse::Status::Partial) if buf.len() >= MAX_HEADER_SIZE => {
            Err(Error::InvalidHttp("request too large"))
        }
        Ok(httparse::Status::Partial) => Ok(None),
        Err(_) => Err(Error::InvalidHttp("failed to parse HTTP request")),
    }
}

fn append_header_value<'a>(current: &mut Option<Cow<'a, str>>, value: &'a str) {
    match current {
        Some(current) => {
            current.to_mut().push_str(", ");
            current.to_mut().push_str(value);
        }
        None => *current = Some(Cow::Borrowed(value)),
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
pub fn build_response(
    accept_key: &str,
    protocol: Option<&str>,
    extensions: Option<&str>,
) -> Result<Bytes> {
    if !is_valid_header_text(accept_key) {
        return Err(Error::InvalidHttp("invalid Sec-WebSocket-Accept"));
    }
    if protocol.is_some_and(|value| !is_header_name_token(value)) {
        return Err(Error::InvalidHttp("invalid Sec-WebSocket-Protocol"));
    }
    if extensions.is_some_and(|value| {
        !is_valid_extension_list(value) || value.split(',').any(|item| item.trim().is_empty())
    }) {
        return Err(Error::InvalidHttp("invalid Sec-WebSocket-Extensions"));
    }

    Ok(build_response_inner(accept_key, protocol, extensions))
}

pub(crate) fn build_response_inner(
    accept_key: &str,
    protocol: Option<&str>,
    extensions: Option<&str>,
) -> Bytes {
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
/// Returns [`Error::InvalidHttp`] if a header name or value is invalid, or if
/// a custom header conflicts with a handshake-managed header.
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
    validate_handshake_metadata(host, path, protocol, extensions)?;
    if !is_valid_websocket_key(key) {
        return Err(Error::InvalidHttp("invalid Sec-WebSocket-Key"));
    }
    Ok(())
}

fn validate_handshake_metadata(
    host: &str,
    path: &str,
    protocol: Option<&str>,
    extensions: Option<&str>,
) -> Result<()> {
    if !is_valid_host(host) {
        return Err(Error::InvalidHttp("invalid Host"));
    }
    if !is_valid_request_target(path) {
        return Err(Error::InvalidHttp("invalid request target"));
    }
    if protocol.is_some_and(|value| {
        !is_valid_protocol_list(value) || value.split(',').any(|item| item.trim().is_empty())
    }) {
        return Err(Error::InvalidHttp("invalid Sec-WebSocket-Protocol"));
    }
    if extensions.is_some_and(|value| {
        !is_valid_extension_list(value) || value.split(',').any(|item| item.trim().is_empty())
    }) {
        return Err(Error::InvalidHttp("invalid Sec-WebSocket-Extensions"));
    }
    Ok(())
}

pub(crate) fn validate_client_handshake_inputs(
    host: &str,
    path: &str,
    protocol: Option<&str>,
    extra_headers: Option<&[(String, String)]>,
) -> Result<()> {
    validate_handshake_metadata(host, path, protocol, None)?;
    if let Some(headers) = extra_headers {
        validate_extra_headers(headers)?;
    }
    Ok(())
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
    // RFC 9110 permits HTAB, visible bytes, and obs-text in field values.
    byte == b'\t' || (byte >= b' ' && byte != 0x7f)
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

/// Returns true if the comma-separated header `value` contains `expected`
/// (ASCII case-insensitive, surrounding whitespace ignored).
pub(crate) fn contains_header_token(value: &str, expected: &str) -> bool {
    value
        .split(',')
        .any(|token| token.trim().eq_ignore_ascii_case(expected))
}

pub(crate) fn is_valid_websocket_key(value: &str) -> bool {
    base64::engine::general_purpose::STANDARD
        .decode(value)
        .is_ok_and(|decoded| decoded.len() == 16)
}

pub(crate) fn is_valid_host(value: &str) -> bool {
    !value.contains('@') && value.parse::<Authority>().is_ok()
}

fn is_valid_request_target(value: &str) -> bool {
    value.starts_with('/')
        && value.parse::<Uri>().is_ok_and(|uri| {
            uri.scheme().is_none()
                && uri.authority().is_none()
                && uri
                    .path_and_query()
                    .is_some_and(|path_and_query| path_and_query.as_str() == value)
        })
}

fn parse_server_request_target<'a>(
    value: &'a str,
    header_host: &'a str,
) -> Option<(Cow<'a, str>, &'a str)> {
    if is_valid_request_target(value) {
        return Some((Cow::Borrowed(value), header_host));
    }

    let uri = value.parse::<Uri>().ok()?;
    let scheme = uri.scheme_str()?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }

    let parsed_authority = uri.authority()?;
    if parsed_authority.as_str().contains('@') {
        return None;
    }

    let scheme_end = value.find("://")?;
    let authority_start = scheme_end + 3;
    let authority_end = authority_start.checked_add(parsed_authority.as_str().len())?;
    let authority = value.get(authority_start..authority_end)?;
    if authority != parsed_authority.as_str() {
        return None;
    }

    let resource = &value[authority_end..];
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

fn is_valid_header_text(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(is_header_value_byte)
}

fn is_header_name_token(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(is_header_name_byte)
}

pub(crate) fn validate_supported_protocols(protocols: &[String]) -> Result<()> {
    for protocol in protocols {
        if !is_header_name_token(protocol) {
            return Err(Error::InvalidHttp("invalid supported subprotocol"));
        }
    }
    Ok(())
}

pub(crate) fn select_subprotocol<'a>(
    offered: Option<&str>,
    supported: &'a [String],
) -> Option<&'a str> {
    let offered = offered?;
    supported
        .iter()
        .find(|supported| {
            offered
                .split(',')
                .map(str::trim)
                .any(|offered| offered == supported.as_str())
        })
        .map(String::as_str)
}

pub(crate) fn is_valid_protocol_list(value: &str) -> bool {
    let mut seen = std::collections::HashSet::new();
    // Recipients ignore empty HTTP list elements, but a required list must
    // still contain at least one protocol and must not repeat a protocol.
    value
        .split(',')
        .map(str::trim)
        .filter(|protocol| !protocol.is_empty())
        .all(|protocol| is_header_name_token(protocol) && seen.insert(protocol))
        && !seen.is_empty()
}

pub(crate) fn is_valid_extension_list(value: &str) -> bool {
    value
        .split(',')
        .any(|extension| !extension.trim().is_empty())
        && value
            .split(',')
            .map(str::trim)
            .filter(|extension| !extension.is_empty())
            .all(|extension| {
                let mut parts = extension.split(';').map(str::trim);
                parts.next().is_some_and(is_header_name_token)
                    && parts.all(|parameter| {
                        if let Some((name, value)) = parameter.split_once('=') {
                            is_header_name_token(name.trim())
                                && is_valid_extension_value(value.trim())
                        } else {
                            is_header_name_token(parameter)
                        }
                    })
            })
}

fn is_valid_extension_value(value: &str) -> bool {
    if is_header_name_token(value) {
        return true;
    }

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

pub(crate) fn is_zero_content_length(value: &str) -> bool {
    value.split(',').all(|length| {
        let length = length.trim();
        !length.is_empty() && length.bytes().all(|byte| byte == b'0')
    })
}

pub(crate) fn validate_selected_protocol(
    offered: Option<&str>,
    selected: Option<&str>,
) -> Result<()> {
    let Some(selected) = selected else {
        return Ok(());
    };
    let Some(offered) = offered else {
        return Err(Error::HandshakeFailed(
            "server returned an unoffered subprotocol",
        ));
    };
    if offered
        .split(',')
        .map(str::trim)
        .any(|candidate| candidate == selected)
    {
        Ok(())
    } else {
        Err(Error::HandshakeFailed(
            "server returned an unoffered subprotocol",
        ))
    }
}

pub(crate) fn validate_selected_extensions(
    offered: Option<&str>,
    selected: Option<&str>,
) -> Result<()> {
    let Some(selected) = selected else {
        return Ok(());
    };
    if !is_valid_extension_list(selected) {
        return Err(Error::HandshakeFailed("invalid selected extension"));
    }
    let Some(offered) = offered else {
        return Err(Error::HandshakeFailed(
            "server selected an unoffered extension",
        ));
    };

    let offered_names = offered
        .split(',')
        .filter_map(|extension| extension.split(';').next())
        .map(str::trim);
    for selected_extension in selected
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
    {
        let selected_name = selected_extension
            .split(';')
            .next()
            .expect("validated extension has a name")
            .trim();
        if !offered_names
            .clone()
            .any(|offered| offered == selected_name)
        {
            return Err(Error::HandshakeFailed(
                "server selected an unoffered extension",
            ));
        }
    }

    Ok(())
}

pub(crate) fn validate_server_selection(
    request: &HandshakeRequest<'_>,
    selection: &HandshakeSelection,
) -> Result<()> {
    validate_selected_protocol(request.protocol.as_deref(), selection.protocol.as_deref())?;
    validate_selected_extensions(
        request.extensions.as_deref(),
        selection.extensions.as_deref(),
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
    // Bytes after the HTTP header belong to the upgraded WebSocket stream.
    let header = &buf[..buf.len().min(MAX_HEADER_SIZE)];

    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut res = httparse::Response::new(&mut headers);

    match res.parse(header) {
        Ok(httparse::Status::Complete(len)) => {
            let status = res.code.unwrap_or(0);

            if res.version != Some(1) {
                return Err(Error::InvalidHttp("HTTP version must be 1.1"));
            }
            if status != 101 {
                return Err(Error::HandshakeFailed("expected 101 Switching Protocols"));
            }

            let mut accept = None;
            let mut protocol = None;
            let mut extensions = None;
            let mut upgrade = false;
            let mut connection_upgrade = false;

            for header in res.headers.iter() {
                let name = header.name;
                let value = std::str::from_utf8(header.value)
                    .map_err(|_| Error::InvalidHttp("invalid header value"))?
                    .trim();

                match name {
                    name if name.eq_ignore_ascii_case("sec-websocket-accept") => {
                        if accept.replace(value).is_some() {
                            return Err(Error::HandshakeFailed("duplicate Sec-WebSocket-Accept"));
                        }
                    }
                    name if name.eq_ignore_ascii_case("sec-websocket-protocol") => {
                        if protocol.replace(value).is_some() {
                            return Err(Error::HandshakeFailed("duplicate Sec-WebSocket-Protocol"));
                        }
                    }
                    name if name.eq_ignore_ascii_case("sec-websocket-extensions") => {
                        if extensions.replace(value).is_some() {
                            return Err(Error::HandshakeFailed(
                                "duplicate Sec-WebSocket-Extensions",
                            ));
                        }
                    }
                    name if name.eq_ignore_ascii_case("upgrade")
                        && (contains_header_token(value, "websocket")) =>
                    {
                        upgrade = true;
                    }
                    name if name.eq_ignore_ascii_case("connection")
                        && (contains_header_token(value, "upgrade")) =>
                    {
                        connection_upgrade = true;
                    }
                    _ => {}
                }
            }

            if !upgrade {
                return Err(Error::HandshakeFailed("missing Upgrade: websocket"));
            }
            if !connection_upgrade {
                return Err(Error::HandshakeFailed("missing Connection: Upgrade"));
            }
            if protocol.is_some_and(|value| !is_header_name_token(value)) {
                return Err(Error::HandshakeFailed("invalid Sec-WebSocket-Protocol"));
            }
            if extensions.is_some_and(|value| !is_valid_extension_list(value)) {
                return Err(Error::HandshakeFailed("invalid Sec-WebSocket-Extensions"));
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
        Ok(httparse::Status::Partial) if buf.len() >= MAX_HEADER_SIZE => {
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

/// Perform server-side handshake
#[cfg(feature = "tokio-runtime")]
pub async fn server_handshake<S>(stream: &mut S) -> Result<HandshakeResult>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    server_handshake_with(stream, |_| Ok(HandshakeSelection::default())).await
}

/// Perform a server-side handshake with request-aware selection.
///
/// The callback receives the validated request before a response is written.
/// Its selected protocol and extension names must have been offered by the
/// client.
#[cfg(feature = "tokio-runtime")]
pub async fn server_handshake_with<S, F>(stream: &mut S, select: F) -> Result<HandshakeResult>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    F: FnOnce(&HandshakeRequest<'_>) -> Result<HandshakeSelection>,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut buf = BytesMut::with_capacity(4096);

    // Read the HTTP request
    loop {
        if buf.len() > MAX_HEADER_SIZE {
            return Err(Error::InvalidHttp("request too large"));
        }

        let n = (&mut *stream)
            .take((MAX_HEADER_SIZE + 1 - buf.len()) as u64)
            .read_buf(&mut buf)
            .await?;
        if n == 0 {
            return Err(Error::ConnectionClosed);
        }

        // Try to parse the request
        if let Some((req, consumed)) = parse_request(&buf)? {
            let selection = select(&req)?;
            validate_server_selection(&req, &selection)?;

            // Extract values before mutably borrowing buf
            let path = req.path.to_string();

            // Generate accept key
            let accept_key = generate_accept_key(req.key);

            // Build and send response
            let response = build_response_inner(
                &accept_key,
                selection.protocol.as_deref(),
                selection.extensions.as_deref(),
            );
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
                protocol: selection.protocol,
                extensions: selection.extensions,
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

        let n = (&mut *stream)
            .take((MAX_HEADER_SIZE + 1 - buf.len()) as u64)
            .read_buf(&mut buf)
            .await?;
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
            if res.extensions.is_some() {
                return Err(Error::HandshakeFailed(
                    "server returned an unoffered extension",
                ));
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

    use super::*;

    #[test]
    fn test_generate_accept_key() {
        // Test vector from RFC 6455
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let accept = generate_accept_key(key);
        assert_eq!(accept, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    #[test]
    fn test_generate_key_contains_16_bytes() {
        let key = generate_key();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(key)
            .unwrap();

        assert_eq!(decoded.len(), 16);
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
    fn test_validate_accept_key() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let accept = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";
        assert!(validate_accept_key(key, accept));
        assert!(!validate_accept_key(key, "invalid"));
    }
}

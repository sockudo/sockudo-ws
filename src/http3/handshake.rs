//! HTTP/3 Extended CONNECT handshake (RFC 9220)
//!
//! This module handles the WebSocket upgrade handshake over HTTP/3 using
//! the Extended CONNECT method. The semantics are identical to HTTP/2
//! (RFC 8441), but adapted for HTTP/3's QPACK header compression.

use http::{HeaderMap, Method, Request, Response, StatusCode, Uri};

/// Parsed HTTP/3 WebSocket upgrade request (Extended CONNECT)
#[derive(Debug, Clone)]
pub struct H3HandshakeRequest {
    /// The `:path` pseudo-header (e.g., "/ws" or "/chat")
    pub path: String,
    /// The `:authority` pseudo-header (host:port)
    pub authority: String,
    /// The `:scheme` pseudo-header ("https" for QUIC)
    pub scheme: String,
    /// The `sec-websocket-protocol` header (optional subprotocol negotiation)
    pub protocol: Option<String>,
    /// The `sec-websocket-extensions` header (optional extensions)
    pub extensions: Option<String>,
    /// The `origin` header (optional, for CORS)
    pub origin: Option<String>,
    /// The `sec-websocket-version` header (should be "13")
    pub version: Option<String>,
}

impl H3HandshakeRequest {
    /// Parse an HTTP/3 request into a WebSocket handshake request
    ///
    /// Returns `Some` if this is a valid Extended CONNECT request for WebSocket,
    /// `None` otherwise.
    pub fn from_request<B>(req: &Request<B>) -> Option<Self> {
        // Must be CONNECT method
        if req.method() != Method::CONNECT {
            return None;
        }

        // Extract URI components
        let uri = req.uri();
        let path = uri
            .path_and_query()
            .map(|pq| pq.as_str().to_string())
            .unwrap_or_else(|| "/".to_string());

        let authority = uri
            .authority()
            .map(|a| a.to_string())
            .or_else(|| {
                req.headers()
                    .get("host")
                    .and_then(|v| v.to_str().ok())
                    .map(String::from)
            })
            .unwrap_or_default();

        // HTTP/3 always uses https scheme (QUIC requires TLS)
        let scheme = "https".to_string();

        // Extract WebSocket-specific headers
        let headers = req.headers();

        let protocol = get_header_string(headers, "sec-websocket-protocol");
        let extensions = get_header_string(headers, "sec-websocket-extensions");
        let origin = get_header_string(headers, "origin");
        let version = get_header_string(headers, "sec-websocket-version");

        Some(Self {
            path,
            authority,
            scheme,
            protocol,
            extensions,
            origin,
            version,
        })
    }

    /// Create a request from URI and optional parameters
    pub fn new(uri: &str, protocol: Option<&str>) -> Option<Self> {
        let uri: Uri = uri.parse().ok()?;

        let path = uri
            .path_and_query()
            .map(|pq| pq.as_str().to_string())
            .unwrap_or_else(|| "/".to_string());

        let authority = uri.authority()?.to_string();

        Some(Self {
            path,
            authority,
            scheme: "https".to_string(),
            protocol: protocol.map(String::from),
            extensions: None,
            origin: None,
            version: Some("13".to_string()),
        })
    }

    /// Check if the request specifies a particular subprotocol
    pub fn has_protocol(&self, proto: &str) -> bool {
        self.protocol
            .as_ref()
            .map(|p| p.split(',').any(|s| s.trim().eq_ignore_ascii_case(proto)))
            .unwrap_or(false)
    }

    /// Get the list of requested subprotocols
    pub fn protocols(&self) -> Vec<&str> {
        self.protocol
            .as_ref()
            .map(|p| p.split(',').map(|s| s.trim()).collect())
            .unwrap_or_default()
    }

    /// Get the full WebSocket URL
    pub fn url(&self) -> String {
        format!("wss://{}{}", self.authority, self.path)
    }
}

/// Build an HTTP/3 200 OK response for WebSocket upgrade
///
/// Per RFC 9220, the response is a 200 OK status with optional
/// WebSocket headers. This is identical to HTTP/2.
pub fn build_h3_response(protocol: Option<&str>, extensions: Option<&str>) -> Response<()> {
    let mut builder = Response::builder().status(StatusCode::OK);

    if let Some(proto) = protocol {
        builder = builder.header("sec-websocket-protocol", proto);
    }

    if let Some(ext) = extensions {
        builder = builder.header("sec-websocket-extensions", ext);
    }

    builder.body(()).expect("valid response")
}

/// Build an HTTP/3 error response for rejecting WebSocket upgrade
pub fn build_h3_error_response(status: StatusCode, reason: Option<&str>) -> Response<()> {
    let mut builder = Response::builder().status(status);

    if let Some(r) = reason {
        builder = builder.header("x-error-reason", r);
    }

    builder.body(()).expect("valid response")
}

/// Helper to extract a header as a String
fn get_header_string(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_h3_response() {
        let response = build_h3_response(None, None);
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn test_build_h3_response_with_protocol() {
        let response = build_h3_response(Some("graphql-ws"), None);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("sec-websocket-protocol").unwrap(),
            "graphql-ws"
        );
    }

    #[test]
    fn test_h3_handshake_request_new() {
        let req = H3HandshakeRequest::new("wss://example.com/ws", Some("graphql-ws")).unwrap();
        assert_eq!(req.path, "/ws");
        assert_eq!(req.authority, "example.com");
        assert_eq!(req.scheme, "https");
        assert!(req.has_protocol("graphql-ws"));
    }
}

//! HTTP/3 Extended CONNECT handshake (RFC 9220)
//!
//! This module handles the WebSocket upgrade handshake over HTTP/3 using
//! the Extended CONNECT method as specified in RFC 9220.
//!
//! # Protocol Overview (RFC 9220)
//!
//! 1. Server sends SETTINGS with SETTINGS_ENABLE_CONNECT_PROTOCOL=1
//! 2. Client sends Extended CONNECT request:
//!    - `:method` = CONNECT
//!    - `:protocol` = websocket
//!    - `:scheme` = https
//!    - `:authority` = host:port
//!    - `:path` = /path
//! 3. Server responds with 200 OK (success) or error status
//! 4. On 200 OK, stream transitions to WebSocket frame mode
//!
//! # Stream Closure Mapping (RFC 9220, Section 3)
//!
//! - Orderly TCP close (FIN) → QUIC stream FIN
//! - RST exception → Stream error H3_REQUEST_CANCELLED (0x10c)

use http::{HeaderMap, Method, Request, Response, StatusCode, Uri};

/// Parsed HTTP/3 WebSocket upgrade request (Extended CONNECT per RFC 9220)
///
/// This represents the parsed headers from an Extended CONNECT request
/// with `:protocol=websocket`.
#[derive(Debug, Clone)]
pub struct H3HandshakeRequest {
    /// The `:path` pseudo-header (e.g., "/ws" or "/chat")
    pub path: String,
    /// The `:authority` pseudo-header (host:port)
    pub authority: String,
    /// The `:scheme` pseudo-header ("https" for QUIC - always https per RFC 9220)
    pub scheme: String,
    /// The `:protocol` pseudo-header (should be "websocket" for WebSocket)
    pub protocol: Option<String>,
    /// The `sec-websocket-extensions` header (optional, for permessage-deflate etc.)
    pub extensions: Option<String>,
    /// The `origin` header (optional, for CORS validation)
    pub origin: Option<String>,
    /// The `sec-websocket-version` header (should be "13")
    pub version: Option<String>,
    /// The `sec-websocket-protocol` header (optional subprotocol negotiation)
    pub subprotocols: Option<String>,
}

impl H3HandshakeRequest {
    /// Parse an HTTP/3 request into a WebSocket handshake request
    ///
    /// Returns `Some` if this is a valid Extended CONNECT request,
    /// `None` if the request is not a CONNECT method.
    ///
    /// # RFC 9220 Validation
    ///
    /// The caller should verify:
    /// - `protocol` is Some("websocket") for WebSocket connections
    /// - `version` is "13" if present
    pub fn from_request<B>(req: &Request<B>) -> Option<Self> {
        // Per RFC 9220: Must be CONNECT method
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

        // HTTP/3 always uses https scheme (QUIC mandates TLS 1.3)
        let scheme = "https".to_string();

        // Extract the :protocol pseudo-header (critical for RFC 9220)
        // In the h3 crate, this comes through as a regular header or extension
        let headers = req.headers();
        let protocol = get_header_string(headers, ":protocol")
            .or_else(|| req.extensions().get::<Protocol>().map(|p| p.0.clone()));

        // Extract WebSocket-specific headers
        let extensions = get_header_string(headers, "sec-websocket-extensions");
        let origin = get_header_string(headers, "origin");
        let version = get_header_string(headers, "sec-websocket-version");
        let subprotocols = get_header_string(headers, "sec-websocket-protocol");

        Some(Self {
            path,
            authority,
            scheme,
            protocol,
            extensions,
            origin,
            version,
            subprotocols,
        })
    }

    /// Create a new handshake request for client use
    pub fn new(uri: &str, subprotocol: Option<&str>) -> Option<Self> {
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
            protocol: Some("websocket".to_string()),
            extensions: None,
            origin: None,
            version: Some("13".to_string()),
            subprotocols: subprotocol.map(String::from),
        })
    }

    /// Check if this is a valid WebSocket Extended CONNECT request
    ///
    /// Per RFC 9220, the `:protocol` pseudo-header must be "websocket".
    pub fn is_websocket(&self) -> bool {
        self.protocol
            .as_ref()
            .is_some_and(|p| p.eq_ignore_ascii_case("websocket"))
    }

    /// Check if the request specifies a particular subprotocol
    pub fn has_subprotocol(&self, proto: &str) -> bool {
        self.subprotocols
            .as_ref()
            .is_some_and(|p| p.split(',').any(|s| s.trim().eq_ignore_ascii_case(proto)))
    }

    /// Get the list of requested subprotocols
    pub fn subprotocol_list(&self) -> Vec<&str> {
        self.subprotocols
            .as_ref()
            .map(|p| p.split(',').map(|s| s.trim()).collect())
            .unwrap_or_default()
    }

    /// Get the full WebSocket URL
    pub fn url(&self) -> String {
        format!("wss://{}{}", self.authority, self.path)
    }

    /// Validate the request according to RFC 9220 requirements
    ///
    /// Returns `Ok(())` if valid, or `Err(status_code)` with the appropriate
    /// HTTP status code to return.
    pub fn validate(&self) -> Result<(), StatusCode> {
        // :protocol must be "websocket" for WebSocket
        if !self.is_websocket() {
            // RFC 9220 Section 3: "If a server receives an Extended CONNECT
            // request with a :protocol value that is unknown or not supported,
            // the server SHOULD respond with a 501 (Not Implemented) status code"
            return Err(StatusCode::NOT_IMPLEMENTED);
        }

        // sec-websocket-version should be 13 if present
        if let Some(ref version) = self.version {
            if version != "13" {
                return Err(StatusCode::BAD_REQUEST);
            }
        }

        Ok(())
    }
}

/// Extension type to carry :protocol pseudo-header through http::Request
#[derive(Debug, Clone)]
pub struct Protocol(pub String);

/// Build an HTTP/3 200 OK response for successful WebSocket upgrade
///
/// Per RFC 9220, the response is a 200 OK status. Optional headers:
/// - `sec-websocket-protocol`: Selected subprotocol
/// - `sec-websocket-extensions`: Negotiated extensions
pub fn build_h3_response(subprotocol: Option<&str>, extensions: Option<&str>) -> Response<()> {
    let mut builder = Response::builder().status(StatusCode::OK);

    if let Some(proto) = subprotocol {
        builder = builder.header("sec-websocket-protocol", proto);
    }

    if let Some(ext) = extensions {
        builder = builder.header("sec-websocket-extensions", ext);
    }

    builder.body(()).expect("valid response")
}

/// Build an HTTP/3 error response for rejecting WebSocket upgrade
///
/// Per RFC 9220 Section 3:
/// - 501 Not Implemented: Unknown or unsupported :protocol value
/// - 400 Bad Request: Malformed request
/// - 403 Forbidden: Origin not allowed
///
/// Optionally includes problem details per RFC 7807.
pub fn build_h3_error_response(status: StatusCode, reason: Option<&str>) -> Response<()> {
    let mut builder = Response::builder().status(status);

    if let Some(r) = reason {
        // Could use application/problem+json for RFC 7807 compliance
        builder = builder.header("x-error-reason", r);
    }

    builder.body(()).expect("valid response")
}

/// Build a 501 Not Implemented response for unsupported :protocol
///
/// Per RFC 9220 Section 3: "the server SHOULD respond to the request
/// with a 501 (Not Implemented) status code"
pub fn build_not_implemented_response(protocol: &str) -> Response<()> {
    build_h3_error_response(
        StatusCode::NOT_IMPLEMENTED,
        Some(&format!("Unsupported :protocol value: {}", protocol)),
    )
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
    fn test_build_h3_response_with_subprotocol() {
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
        assert!(req.is_websocket());
        assert!(req.has_subprotocol("graphql-ws"));
    }

    #[test]
    fn test_h3_handshake_validation() {
        let mut req = H3HandshakeRequest::new("wss://example.com/ws", None).unwrap();
        assert!(req.validate().is_ok());

        // Test with wrong protocol
        req.protocol = Some("unknown".to_string());
        assert_eq!(req.validate(), Err(StatusCode::NOT_IMPLEMENTED));

        // Test with missing protocol
        req.protocol = None;
        assert_eq!(req.validate(), Err(StatusCode::NOT_IMPLEMENTED));
    }

    #[test]
    fn test_build_not_implemented() {
        let response = build_not_implemented_response("unknown-protocol");
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
    }

    #[test]
    fn test_subprotocol_list() {
        let mut req = H3HandshakeRequest::new("wss://example.com/ws", None).unwrap();
        req.subprotocols = Some("graphql-ws, json, binary".to_string());

        let protos = req.subprotocol_list();
        assert_eq!(protos, vec!["graphql-ws", "json", "binary"]);
        assert!(req.has_subprotocol("json"));
        assert!(req.has_subprotocol("GRAPHQL-WS")); // case insensitive
        assert!(!req.has_subprotocol("xml"));
    }
}

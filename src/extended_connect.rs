//! Extended CONNECT handshake types for HTTP/2 and HTTP/3 WebSocket
//!
//! This module provides transport-agnostic handshake types that work
//! with both HTTP/2 (RFC 8441) and HTTP/3 (RFC 9220) Extended CONNECT.
//!
//! # Example
//!
//! ```ignore
//! use sockudo_ws::{WebSocketServer, Http2};
//! use sockudo_ws::extended_connect::ExtendedConnectRequest;
//!
//! let server = WebSocketServer::<Http2>::new(config);
//! server.serve(stream, |ws, req: ExtendedConnectRequest| async move {
//!     println!("Path: {}", req.path);
//!     println!("Subprotocols: {:?}", req.subprotocol_list());
//!     // handle connection
//! }).await?;
//! ```

#[cfg(any(feature = "http2", feature = "http3"))]
use http::{HeaderMap, Method, Request, Response, StatusCode, Uri};
use std::time::Duration;

#[cfg(not(any(feature = "http2", feature = "http3")))]
use http::StatusCode;

/// Default handshake timeout (30 seconds)
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Default connection timeout (60 seconds)
pub const DEFAULT_CONNECTION_TIMEOUT: Duration = Duration::from_secs(60);

/// Extended CONNECT request for WebSocket over HTTP/2 and HTTP/3
///
/// This struct represents a parsed Extended CONNECT request for WebSocket
/// upgrade, working with both HTTP/2 (RFC 8441) and HTTP/3 (RFC 9220).
///
/// # Fields
///
/// - `path`: The `:path` pseudo-header (e.g., "/ws" or "/chat")
/// - `authority`: The `:authority` pseudo-header (host:port)
/// - `scheme`: The `:scheme` pseudo-header ("https" for HTTP/3, "http" or "https" for HTTP/2)
/// - `protocol`: The `:protocol` pseudo-header (should be "websocket")
/// - `subprotocols`: The `sec-websocket-protocol` header (comma-separated list)
/// - `extensions`: The `sec-websocket-extensions` header
/// - `origin`: The `origin` header (for CORS validation)
/// - `version`: The `sec-websocket-version` header (should be "13")
#[derive(Debug, Clone)]
pub struct ExtendedConnectRequest {
    /// The `:path` pseudo-header (e.g., "/ws" or "/chat")
    pub path: String,
    /// The `:authority` pseudo-header (host:port)
    pub authority: String,
    /// The `:scheme` pseudo-header
    pub scheme: String,
    /// The `:protocol` pseudo-header (should be "websocket" for WebSocket)
    pub protocol: Option<String>,
    /// The `sec-websocket-protocol` header (optional subprotocol negotiation)
    pub subprotocols: Option<String>,
    /// The `sec-websocket-extensions` header (optional extensions)
    pub extensions: Option<String>,
    /// The `origin` header (optional, for CORS)
    pub origin: Option<String>,
    /// The `sec-websocket-version` header (should be "13")
    pub version: Option<String>,
}

impl ExtendedConnectRequest {
    /// Parse an HTTP request into an Extended CONNECT request
    ///
    /// Returns `Some` if this is a valid Extended CONNECT request,
    /// `None` if the request is not a CONNECT method.
    ///
    /// # Example
    ///
    /// ```ignore
    /// if let Some(ws_req) = ExtendedConnectRequest::from_request(&request) {
    ///     println!("WebSocket request to: {}", ws_req.path);
    /// }
    /// ```
    pub fn from_request<B>(req: &Request<B>) -> Option<Self> {
        // Must be CONNECT method for Extended CONNECT
        if req.method() != Method::CONNECT {
            return None;
        }

        let uri = req.uri();
        let path = uri
            .path_and_query()
            .map(|pq| pq.as_str().to_string())
            .unwrap_or_else(|| "/".to_string());

        let authority = uri
            .authority()
            .map(|a| a.to_string())
            .or_else(|| get_header_string(req.headers(), "host"))
            .unwrap_or_default();

        let scheme = uri.scheme_str().unwrap_or("https").to_string();

        let headers = req.headers();

        // Try to get :protocol from headers or extensions
        let protocol = get_header_string(headers, ":protocol");

        let subprotocols = get_header_string(headers, "sec-websocket-protocol");
        let extensions = get_header_string(headers, "sec-websocket-extensions");
        let origin = get_header_string(headers, "origin");
        let version = get_header_string(headers, "sec-websocket-version");

        Some(Self {
            path,
            authority,
            scheme,
            protocol,
            subprotocols,
            extensions,
            origin,
            version,
        })
    }

    /// Create a new Extended CONNECT request for client use
    ///
    /// # Example
    ///
    /// ```ignore
    /// let req = ExtendedConnectRequest::new("wss://example.com/ws", Some("graphql-ws"))?;
    /// ```
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
            subprotocols: subprotocol.map(String::from),
            extensions: None,
            origin: None,
            version: Some("13".to_string()),
        })
    }

    /// Check if this is a valid WebSocket Extended CONNECT request
    ///
    /// Per RFC 8441/9220, the `:protocol` pseudo-header must be "websocket".
    pub fn is_websocket(&self) -> bool {
        self.protocol
            .as_ref()
            .is_some_and(|p| p.eq_ignore_ascii_case("websocket"))
    }

    /// Check if the request specifies a particular subprotocol
    ///
    /// # Example
    ///
    /// ```ignore
    /// if req.has_subprotocol("graphql-ws") {
    ///     // Client supports GraphQL over WebSocket
    /// }
    /// ```
    pub fn has_subprotocol(&self, proto: &str) -> bool {
        self.subprotocols
            .as_ref()
            .is_some_and(|p| p.split(',').any(|s| s.trim().eq_ignore_ascii_case(proto)))
    }

    /// Get the list of requested subprotocols
    ///
    /// # Example
    ///
    /// ```ignore
    /// for proto in req.subprotocol_list() {
    ///     println!("Client supports: {}", proto);
    /// }
    /// ```
    pub fn subprotocol_list(&self) -> Vec<&str> {
        self.subprotocols
            .as_ref()
            .map(|p| p.split(',').map(|s| s.trim()).collect())
            .unwrap_or_default()
    }

    /// Get the list of requested extensions
    pub fn extension_list(&self) -> Vec<&str> {
        self.extensions
            .as_ref()
            .map(|e| e.split(',').map(|s| s.trim()).collect())
            .unwrap_or_default()
    }

    /// Get the full WebSocket URL
    ///
    /// # Example
    ///
    /// ```ignore
    /// let url = req.url(); // "wss://example.com/ws"
    /// ```
    pub fn url(&self) -> String {
        let scheme = if self.scheme == "https" { "wss" } else { "ws" };
        format!("{}://{}{}", scheme, self.authority, self.path)
    }

    /// Validate the request according to RFC 8441/9220 requirements
    ///
    /// Returns `Ok(())` if valid, or `Err(status_code)` with the appropriate
    /// HTTP status code to return.
    ///
    /// # Validation Rules
    ///
    /// - `:protocol` must be "websocket"
    /// - `sec-websocket-version` must be "13" if present
    pub fn validate(&self) -> Result<(), StatusCode> {
        // :protocol must be "websocket" for WebSocket
        if !self.is_websocket() {
            // RFC 9220 Section 3: Return 501 for unsupported :protocol
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

/// Extended CONNECT response
#[derive(Debug, Clone)]
pub struct ExtendedConnectResponse {
    /// HTTP status code (200 for success)
    pub status: StatusCode,
    /// Selected subprotocol (if any)
    pub protocol: Option<String>,
    /// Accepted extensions (if any)
    pub extensions: Option<String>,
}

impl ExtendedConnectResponse {
    /// Create a successful response
    pub fn ok() -> Self {
        Self {
            status: StatusCode::OK,
            protocol: None,
            extensions: None,
        }
    }

    /// Create a successful response with a selected subprotocol
    pub fn ok_with_protocol(protocol: impl Into<String>) -> Self {
        Self {
            status: StatusCode::OK,
            protocol: Some(protocol.into()),
            extensions: None,
        }
    }

    /// Create a successful response with a selected subprotocol and extensions
    pub fn ok_with_options(
        protocol: Option<impl Into<String>>,
        extensions: Option<impl Into<String>>,
    ) -> Self {
        Self {
            status: StatusCode::OK,
            protocol: protocol.map(Into::into),
            extensions: extensions.map(Into::into),
        }
    }

    /// Create an error response
    pub fn error(status: StatusCode) -> Self {
        Self {
            status,
            protocol: None,
            extensions: None,
        }
    }
}

/// Configuration for Extended CONNECT handshake with timeout support
#[derive(Debug, Clone)]
pub struct ExtendedConnectConfig {
    /// Timeout for the handshake operation
    pub handshake_timeout: Duration,
    /// Timeout for connection establishment (used by HTTP/3)
    pub connection_timeout: Duration,
    /// Required WebSocket version (typically "13")
    pub required_version: Option<&'static str>,
    /// Allowed origins (empty means all allowed)
    pub allowed_origins: Vec<String>,
}

impl Default for ExtendedConnectConfig {
    fn default() -> Self {
        Self {
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            connection_timeout: DEFAULT_CONNECTION_TIMEOUT,
            required_version: Some("13"),
            allowed_origins: Vec::new(),
        }
    }
}

impl ExtendedConnectConfig {
    /// Create a new config with custom handshake timeout
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            handshake_timeout: timeout,
            ..Default::default()
        }
    }

    /// Set the connection timeout (for HTTP/3)
    pub fn connection_timeout(mut self, timeout: Duration) -> Self {
        self.connection_timeout = timeout;
        self
    }

    /// Set allowed origins for CORS validation
    pub fn allowed_origins(mut self, origins: Vec<String>) -> Self {
        self.allowed_origins = origins;
        self
    }

    /// Validate an Extended CONNECT request against this configuration
    ///
    /// Returns `Ok(())` if valid, or `Err(StatusCode)` with the appropriate
    /// HTTP status code to return.
    pub fn validate(&self, request: &ExtendedConnectRequest) -> Result<(), StatusCode> {
        // First, validate the request per RFC 8441/9220
        request.validate()?;

        // Check WebSocket version if required
        if let Some(required_version) = self.required_version {
            if let Some(ref version) = request.version {
                if version != required_version {
                    return Err(StatusCode::BAD_REQUEST);
                }
            }
        }

        // Check origin if allowed_origins is not empty
        if !self.allowed_origins.is_empty() {
            if let Some(ref origin) = request.origin {
                if !self.allowed_origins.iter().any(|o| o == origin) {
                    return Err(StatusCode::FORBIDDEN);
                }
            } else {
                // No origin header when origins are required
                return Err(StatusCode::FORBIDDEN);
            }
        }

        Ok(())
    }
}

/// Build an HTTP 200 OK response for Extended CONNECT WebSocket upgrade
///
/// This creates the response that the server sends to accept an Extended CONNECT
/// request for WebSocket.
///
/// # Arguments
///
/// * `protocol` - Optional selected subprotocol to include in response
/// * `extensions` - Optional accepted extensions to include in response
///
/// # Example
///
/// ```ignore
/// use sockudo_ws::extended_connect::build_extended_connect_response;
///
/// let response = build_extended_connect_response(Some("graphql-ws"), None);
/// ```
pub fn build_extended_connect_response(
    protocol: Option<&str>,
    extensions: Option<&str>,
) -> Response<()> {
    let mut builder = Response::builder().status(StatusCode::OK);

    if let Some(proto) = protocol {
        builder = builder.header("sec-websocket-protocol", proto);
    }

    if let Some(ext) = extensions {
        builder = builder.header("sec-websocket-extensions", ext);
    }

    builder.body(()).expect("valid response")
}

/// Build an HTTP error response for rejecting Extended CONNECT
///
/// # Arguments
///
/// * `status` - HTTP status code (e.g., 400, 403, 501)
/// * `reason` - Optional reason phrase
///
/// # Status Codes
///
/// - 400 Bad Request: Malformed request
/// - 403 Forbidden: Origin not allowed
/// - 501 Not Implemented: Unsupported `:protocol` value (RFC 9220)
pub fn build_extended_connect_error(status: StatusCode, reason: Option<&str>) -> Response<()> {
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
    fn test_build_response() {
        let response = build_extended_connect_response(None, None);
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn test_build_response_with_protocol() {
        let response = build_extended_connect_response(Some("graphql-ws"), None);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("sec-websocket-protocol").unwrap(),
            "graphql-ws"
        );
    }

    #[test]
    fn test_extended_connect_request_new() {
        let req = ExtendedConnectRequest::new("wss://example.com/ws", Some("graphql-ws")).unwrap();
        assert_eq!(req.path, "/ws");
        assert_eq!(req.authority, "example.com");
        assert_eq!(req.scheme, "https");
        assert!(req.is_websocket());
        assert!(req.has_subprotocol("graphql-ws"));
    }

    #[test]
    fn test_validation() {
        let mut req = ExtendedConnectRequest::new("wss://example.com/ws", None).unwrap();
        assert!(req.validate().is_ok());

        // Test with wrong protocol
        req.protocol = Some("unknown".to_string());
        assert_eq!(req.validate(), Err(StatusCode::NOT_IMPLEMENTED));

        // Test with missing protocol
        req.protocol = None;
        assert_eq!(req.validate(), Err(StatusCode::NOT_IMPLEMENTED));
    }

    #[test]
    fn test_subprotocol_list() {
        let mut req = ExtendedConnectRequest::new("wss://example.com/ws", None).unwrap();
        req.subprotocols = Some("graphql-ws, json, binary".to_string());

        let protos = req.subprotocol_list();
        assert_eq!(protos, vec!["graphql-ws", "json", "binary"]);
        assert!(req.has_subprotocol("json"));
        assert!(req.has_subprotocol("GRAPHQL-WS")); // case insensitive
        assert!(!req.has_subprotocol("xml"));
    }

    #[test]
    fn test_url() {
        let req = ExtendedConnectRequest::new("wss://example.com:8443/ws/chat", None).unwrap();
        assert_eq!(req.url(), "wss://example.com:8443/ws/chat");
    }
}

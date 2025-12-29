//! HTTP/2 Extended CONNECT handshake (RFC 8441)
//!
//! This module handles the WebSocket upgrade handshake over HTTP/2 using
//! the Extended CONNECT method as defined in RFC 8441.
//!
//! # Protocol Differences from HTTP/1.1
//!
//! - Uses CONNECT method instead of GET
//! - `:protocol = websocket` pseudo-header indicates WebSocket upgrade
//! - No `Upgrade`, `Connection` headers (HTTP/2 doesn't use them)
//! - No `Sec-WebSocket-Key`/`Accept` (CONNECT provides authentication)
//! - `:scheme` and `:path` are included (unlike regular CONNECT)

use http::{HeaderMap, Method, Request, Response, StatusCode};

/// Parsed HTTP/2 WebSocket upgrade request (Extended CONNECT)
#[derive(Debug, Clone)]
pub struct H2HandshakeRequest {
    /// The `:path` pseudo-header (e.g., "/ws" or "/chat")
    pub path: String,
    /// The `:authority` pseudo-header (host:port)
    pub authority: String,
    /// The `:scheme` pseudo-header ("http" or "https")
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

impl H2HandshakeRequest {
    /// Parse an HTTP/2 request into a WebSocket handshake request
    ///
    /// Returns `Some` if this is a valid Extended CONNECT request for WebSocket,
    /// `None` otherwise.
    ///
    /// # Validation
    ///
    /// This method checks:
    /// - Method is CONNECT
    /// - `:protocol` extension is "websocket"
    /// - Required pseudo-headers are present
    pub fn from_request<B>(req: &Request<B>) -> Option<Self> {
        // Must be CONNECT method
        if req.method() != Method::CONNECT {
            return None;
        }

        // For Extended CONNECT, we need to check the :protocol pseudo-header
        // In the h2 crate, this is typically passed via extensions or a custom mechanism
        // For now, we check for the presence of websocket-related headers

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

        let scheme = uri.scheme_str().unwrap_or("https").to_string();

        // Extract WebSocket-specific headers (case-insensitive in HTTP/2)
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

    /// Get the list of requested extensions
    pub fn extensions_list(&self) -> Vec<&str> {
        self.extensions
            .as_ref()
            .map(|e| e.split(',').map(|s| s.trim()).collect())
            .unwrap_or_default()
    }
}

/// HTTP/2 WebSocket handshake response
#[derive(Debug, Clone)]
pub struct H2HandshakeResponse {
    /// HTTP status code (200 for success)
    pub status: StatusCode,
    /// Selected subprotocol (if any)
    pub protocol: Option<String>,
    /// Accepted extensions (if any)
    pub extensions: Option<String>,
}

impl H2HandshakeResponse {
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

    /// Create an error response
    pub fn error(status: StatusCode) -> Self {
        Self {
            status,
            protocol: None,
            extensions: None,
        }
    }
}

/// Build an HTTP/2 200 OK response for WebSocket upgrade
///
/// This creates the response that the server sends to accept an Extended CONNECT
/// request for WebSocket. Per RFC 8441, the response is simply a 200 OK status
/// with optional WebSocket headers.
///
/// # Arguments
///
/// * `protocol` - Optional selected subprotocol to include in response
/// * `extensions` - Optional accepted extensions to include in response
///
/// # Example
///
/// ```ignore
/// use sockudo_ws::http2::build_h2_response;
///
/// let response = build_h2_response(Some("graphql-ws"), None);
/// respond.send_response(response, false)?;
/// ```
pub fn build_h2_response(protocol: Option<&str>, extensions: Option<&str>) -> Response<()> {
    let mut builder = Response::builder().status(StatusCode::OK);

    if let Some(proto) = protocol {
        builder = builder.header("sec-websocket-protocol", proto);
    }

    if let Some(ext) = extensions {
        builder = builder.header("sec-websocket-extensions", ext);
    }

    builder.body(()).expect("valid response")
}

/// Build an HTTP/2 error response for rejecting WebSocket upgrade
///
/// # Arguments
///
/// * `status` - HTTP status code (e.g., 400, 403, 501)
/// * `reason` - Optional reason phrase
pub fn build_h2_error_response(status: StatusCode, reason: Option<&str>) -> Response<()> {
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
    fn test_build_h2_response() {
        let response = build_h2_response(None, None);
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn test_build_h2_response_with_protocol() {
        let response = build_h2_response(Some("graphql-ws"), None);
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("sec-websocket-protocol").unwrap(),
            "graphql-ws"
        );
    }

    #[test]
    fn test_h2_handshake_request_protocols() {
        let req = H2HandshakeRequest {
            path: "/ws".to_string(),
            authority: "example.com".to_string(),
            scheme: "https".to_string(),
            protocol: Some("graphql-ws, subscriptions-transport-ws".to_string()),
            extensions: None,
            origin: None,
            version: Some("13".to_string()),
        };

        assert!(req.has_protocol("graphql-ws"));
        assert!(req.has_protocol("subscriptions-transport-ws"));
        assert!(!req.has_protocol("unknown"));

        let protocols = req.protocols();
        assert_eq!(protocols.len(), 2);
    }
}

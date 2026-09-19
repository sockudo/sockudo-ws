//! JSON specs and explicit resource limits.
use crate::{
    Error, Result,
    catalog::{Case, matches},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

/// An echo server under test.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    /// WebSocket URL.
    #[serde(alias = "uri")]
    pub url: String,
    /// Label for reports; URL is used when absent.
    #[serde(alias = "name")]
    pub agent: Option<String>,
    /// Optional TLS SNI override.
    pub hostname: Option<String>,
    /// Additional HTTP request headers.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}
/// Fuzzing specification, accepting the upstream selection/report keys.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Spec {
    /// Listening URL or default target URL.
    pub url: String,
    /// Directory for HTML and JSON reports.
    pub outdir: String,
    /// Included case patterns.
    pub cases: Vec<String>,
    /// Excluded case patterns.
    #[serde(rename = "exclude-cases")]
    pub exclude_cases: Vec<String>,
    /// Exclusions keyed by agent glob.
    #[serde(rename = "exclude-agent-cases")]
    pub exclude_agent_cases: BTreeMap<String, Vec<String>>,
    /// Targets for fuzzingclient mode.
    pub servers: Vec<Target>,
    /// Maximum concurrent case connections.
    pub concurrency: usize,
    /// Maximum inbound frame size in bytes.
    pub max_frame_size: usize,
    /// Maximum reassembled/inflated message size in bytes.
    pub max_message_size: usize,
    /// Opening handshake deadline in milliseconds.
    pub handshake_timeout_ms: u64,
    /// Closing handshake deadline in milliseconds.
    pub close_timeout_ms: u64,
    /// Optional upper bound on each case's runtime in milliseconds.
    pub case_timeout_ms: Option<u64>,
    /// Optional maximum number of RTT/compression messages, useful for smoke tests.
    /// Reduced runs are labeled separately from upstream-conformance runs.
    pub message_count: Option<usize>,
    /// Server certificate chain (PEM) for WSS.
    pub cert: Option<String>,
    /// Server private key (PEM) for WSS.
    pub key: Option<String>,
    /// Additional trusted CA bundle for WSS clients.
    pub ca: Option<String>,
    /// Requested subprotocols.
    pub protocols: Vec<String>,
    /// Maximum accepted live connections for server modes.
    pub max_connections: usize,
    /// Number of clients in massconnect mode.
    pub connections: usize,
    /// Hold time in milliseconds for massconnect mode.
    pub hold_ms: u64,
    /// Optional HTTP port for browser tests and generated reports; zero disables it.
    pub webport: u16,
    /// Maximum stored agent/case results before new identities are rejected.
    pub max_results: usize,
    /// Delay between mass-connect batches, in milliseconds.
    pub batch_delay_ms: u64,
    /// Delay before retrying a failed mass-connect handshake.
    pub retry_delay_ms: u64,
    /// Retry count per connection; null retries until interrupted.
    pub connect_retries: Option<usize>,
}
impl Default for Spec {
    fn default() -> Self {
        Self {
            url: "ws://127.0.0.1:9001".into(),
            outdir: "reports".into(),
            cases: vec!["*".into()],
            exclude_cases: vec![],
            exclude_agent_cases: BTreeMap::new(),
            servers: vec![],
            concurrency: 1,
            max_frame_size: 64 * 1024 * 1024,
            max_message_size: 64 * 1024 * 1024,
            handshake_timeout_ms: 10000,
            close_timeout_ms: 1000,
            case_timeout_ms: None,
            message_count: None,
            cert: None,
            key: None,
            ca: None,
            protocols: vec![],
            max_connections: 256,
            connections: 100,
            hold_ms: 1000,
            webport: 0,
            max_results: 10000,
            batch_delay_ms: 0,
            retry_delay_ms: 1000,
            connect_retries: Some(0),
        }
    }
}
impl Spec {
    /// Load and validate a JSON specification.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let mut value: serde_json::Value = serde_json::from_slice(&std::fs::read(path)?)
            .map_err(|e| Error::Config(e.to_string()))?;
        normalize_legacy(&mut value)?;
        let spec: Self = serde_json::from_value(value).map_err(|e| Error::Config(e.to_string()))?;
        spec.validate()?;
        Ok(spec)
    }
    /// Reject invalid limits and unsupported URL schemes before opening sockets.
    pub fn validate(&self) -> Result<()> {
        if self.concurrency == 0
            || self.max_results == 0
            || self.max_connections == 0
            || self.max_frame_size == 0
            || self.max_message_size == 0
            || self.handshake_timeout_ms == 0
            || self.close_timeout_ms == 0
            || self.case_timeout_ms == Some(0)
            || self.message_count == Some(0)
        {
            return Err(Error::Config("limits must be positive".into()));
        }
        for protocol in &self.protocols {
            if protocol.is_empty()
                || !protocol
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
            {
                return Err(Error::Config("invalid subprotocol token".into()));
            }
        }
        for value in std::iter::once(&self.url).chain(self.servers.iter().map(|s| &s.url)) {
            let url = url::Url::parse(value).map_err(|e| Error::Config(e.to_string()))?;
            if !matches!(url.scheme(), "ws" | "wss")
                || url.host_str().is_none()
                || url.fragment().is_some()
                || !url.username().is_empty()
                || url.password().is_some()
            {
                return Err(Error::Config(format!("invalid WebSocket URL: {value}")));
            }
        }
        Ok(())
    }
    /// Select cases in upstream order.
    pub fn selected<'a>(&self, cases: &'a [Case]) -> Vec<&'a Case> {
        cases
            .iter()
            .filter(|case| {
                self.cases.iter().any(|p| matches(p, &case.id))
                    && !self.exclude_cases.iter().any(|p| matches(p, &case.id))
            })
            .collect()
    }
    /// Whether an agent-specific exclusion applies.
    pub fn excluded(&self, agent: &str, id: &str) -> bool {
        self.exclude_agent_cases
            .iter()
            .any(|(pattern, cases)| matches(pattern, agent) && cases.iter().any(|p| matches(p, id)))
    }
}

fn normalize_legacy(value: &mut serde_json::Value) -> Result<()> {
    let object = value
        .as_object_mut()
        .ok_or_else(|| Error::Config("spec must be an object".into()))?;
    if let Some(options) = object.remove("options") {
        let options = options
            .as_object()
            .ok_or_else(|| Error::Config("options must be an object".into()))?;
        for (name, value) in options {
            match name.as_str() {
                "failByDrop" if value == false => {}
                "connections" | "batchsize" | "batchdelay" | "retrydelay" => {
                    if value.as_u64().is_none() {
                        return Err(Error::Config(format!("{name} must be an unsigned integer")));
                    }
                    let key = match name.as_str() {
                        "connections" => "connections",
                        "batchsize" => "concurrency",
                        "batchdelay" => "batch_delay_ms",
                        _ => "retry_delay_ms",
                    };
                    object.entry(key).or_insert_with(|| value.clone());
                    if name == "retrydelay" {
                        object
                            .entry("connect_retries")
                            .or_insert(serde_json::Value::Null);
                    }
                }
                "openHandshakeTimeout" | "closeHandshakeTimeout" => {
                    let seconds = value
                        .as_f64()
                        .filter(|n| n.is_finite() && *n > 0.0 && *n <= 86400.0)
                        .ok_or_else(|| {
                            Error::Config(format!(
                                "{name} must be in (0, 86400]; use explicit finite deadlines"
                            ))
                        })?;
                    let key = if name == "openHandshakeTimeout" {
                        "handshake_timeout_ms"
                    } else {
                        "close_timeout_ms"
                    };
                    object
                        .entry(key)
                        .or_insert(serde_json::json!((seconds * 1000.0) as u64));
                }
                _ => {
                    return Err(Error::Config(format!(
                        "unsupported legacy option {name}; see README compatibility notes"
                    )));
                }
            }
        }
    }
    if let Some(servers) = object
        .get_mut("servers")
        .and_then(serde_json::Value::as_array_mut)
    {
        for server in servers {
            if let Some(server) = server.as_object_mut() {
                server.remove("desc");
                if let Some(options) = server.remove("options") {
                    let options = options
                        .as_object()
                        .ok_or_else(|| Error::Config("server options must be an object".into()))?;
                    for (name, value) in options {
                        if !(name == "version" && (value == 18 || value == 13)) {
                            return Err(Error::Config(format!("unsupported server option {name}")));
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

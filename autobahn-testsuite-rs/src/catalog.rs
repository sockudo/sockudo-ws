//! Pinned upstream test definitions and case selection.
use crate::{Error, Result};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Commit from which the complete active WebSocket catalog was imported.
pub const UPSTREAM_COMMIT: &str = "b8a5120d905e30470e4475785c48e4cedc35f6cd";

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub(crate) struct Payload {
    pub hex: String,
    pub len: usize,
}
impl Payload {
    pub fn bytes(&self) -> Result<Bytes> {
        if self.len > 64 * 1024 * 1024 {
            return Err(Error::Limit("catalog payload"));
        }
        let pattern = self
            .hex
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                let s =
                    std::str::from_utf8(pair).map_err(|_| Error::Config("non-ASCII hex".into()))?;
                u8::from_str_radix(s, 16).map_err(|_| Error::Config("invalid hex".into()))
            })
            .collect::<Result<Vec<_>>>()?;
        if !self.hex.len().is_multiple_of(2) || (pattern.is_empty() && self.len != 0) {
            return Err(Error::Config("invalid payload pattern".into()));
        }
        let mut out = Vec::with_capacity(self.len);
        out.extend_from_slice(&pattern[..pattern.len().min(self.len)]);
        while out.len() < self.len {
            let count = out.len().min(self.len - out.len());
            out.extend_from_within(..count);
        }
        Ok(Bytes::from(out))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ExpectedEvent {
    pub kind: String,
    pub payload: Payload,
    pub binary: bool,
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CloseExpectation {
    pub closed_by_me: bool,
    pub close_code: Vec<u16>,
    pub require_clean: bool,
    #[serde(default)]
    pub closed_by_wrong_endpoint_is_fatal: bool,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Action {
    pub at: u64,
    pub kind: String,
    #[serde(default)]
    pub opcode: u8,
    #[serde(default = "yes")]
    pub fin: bool,
    #[serde(default)]
    pub rsv: u8,
    #[serde(default)]
    pub payload: Payload,
    #[serde(default)]
    pub chop: usize,
    #[serde(default)]
    pub sync: bool,
    #[serde(default)]
    pub fragment: usize,
    #[serde(default)]
    pub length: usize,
    pub event: Option<ExpectedEvent>,
}
fn yes() -> bool {
    true
}

/// One registered upstream test, including its executable recipe.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Case {
    /// Stable dotted upstream identifier.
    pub id: String,
    /// Upstream case description (may contain HTML).
    pub description: String,
    /// Upstream expected behavior.
    pub expectation: String,
    pub(crate) engine: String,
    pub(crate) timeout_ms: u64,
    #[serde(default)]
    pub(crate) actions: Vec<Action>,
    #[serde(default)]
    pub(crate) expected: BTreeMap<String, Vec<ExpectedEvent>>,
    #[serde(default)]
    pub(crate) close: CloseExpectation,
    #[serde(default)]
    pub(crate) suppress_close: bool,
    #[serde(default)]
    pub(crate) informational: bool,
    #[serde(default)]
    pub(crate) wrong_code_fatal: bool,
    #[serde(default)]
    pub(crate) payload: Payload,
    #[serde(default)]
    pub(crate) binary: bool,
    #[serde(default)]
    pub(crate) count: usize,
    #[serde(default)]
    pub(crate) length: usize,
    #[serde(default)]
    pub(crate) fragment: usize,
    #[serde(default)]
    pub(crate) chop: usize,
    #[serde(default)]
    pub(crate) file: String,
    #[serde(default)]
    pub(crate) parameter: u8,
}
impl Case {
    /// Whether the case requires permessage-deflate negotiation.
    pub fn compression(&self) -> bool {
        self.engine == "compression"
    }
}
/// Load the checked-in catalog. No upstream checkout or Python runtime is needed.
pub fn load() -> Result<Vec<Case>> {
    serde_json::from_str(include_str!("../catalog/cases.json"))
        .map_err(|e| Error::Config(format!("embedded catalog: {e}")))
}
/// Match a case or agent against a glob containing `*` and `?`.
pub fn matches(pattern: &str, value: &str) -> bool {
    let (p, v) = (pattern.as_bytes(), value.as_bytes());
    let (mut i, mut j, mut star, mut retry) = (0, 0, None, 0);
    while j < v.len() {
        if i < p.len() && (p[i] == b'?' || p[i] == v[j]) {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == b'*' {
            star = Some(i);
            i += 1;
            retry = j;
        } else if let Some(s) = star {
            i = s + 1;
            retry += 1;
            j = retry;
        } else {
            return false;
        }
    }
    while i < p.len() && p[i] == b'*' {
        i += 1;
    }
    i == p.len()
}

pub(crate) fn corpus(file: &str) -> Result<&'static [u8]> {
    match file {
        "data1.json" => Ok(include_bytes!("../catalog/testdata/data1.json")),
        "data1.html" => Ok(include_bytes!("../catalog/testdata/data1.html")),
        "lena512.bmp" => Ok(include_bytes!("../catalog/testdata/lena512.bmp")),
        "pg2229.txt" => Ok(include_bytes!("../catalog/testdata/pg2229.txt")),
        "10.1.1.105.5439.pdf" => Ok(include_bytes!("../catalog/testdata/10.1.1.105.5439.pdf")),
        _ => Err(Error::Config(format!("unknown corpus {file}"))),
    }
}

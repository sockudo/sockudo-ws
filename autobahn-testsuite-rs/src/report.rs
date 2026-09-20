//! Deterministic JSON and escaped HTML reports.
use crate::{
    Error, Result,
    catalog::{Case, UPSTREAM_COMMIT},
};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

/// A measured result, with separate application and close-handshake outcomes.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaseResult {
    /// Agent label.
    pub agent: String,
    /// Stable dotted case ID.
    pub case: String,
    /// Application behavior: OK, NON-STRICT, FAILED, INFORMATIONAL, or UNIMPLEMENTED.
    pub behavior: String,
    /// Close-handshake behavior.
    pub behavior_close: String,
    /// Human-readable application verdict.
    pub result: String,
    /// Human-readable close verdict.
    pub result_close: String,
    /// Wall-clock duration in milliseconds.
    pub duration: f64,
    /// Peer close code, absent if no code was received.
    pub remote_close_code: Option<u16>,
    /// Whether this endpoint initiated closing.
    pub closed_by_me: bool,
    /// Whether closing exchanged frames and ended without a forced drop.
    pub was_clean: bool,
    /// Successful echoed messages.
    pub messages: usize,
    /// Bytes read from the peer, including frame headers.
    pub rx_bytes: u64,
    /// Bytes sent, including frame headers.
    pub tx_bytes: u64,
    /// Received frame count.
    pub rx_frames: u64,
    /// Sent frame count.
    pub tx_frames: u64,
    /// True when settings reduced the upstream workload or timeout.
    pub reduced: bool,
    /// Bounded event summaries for diagnosis.
    pub received: Vec<String>,
}
impl CaseResult {
    pub(crate) fn new(agent: &str, case: &Case, spec: &crate::config::Spec) -> Self {
        Self {
            agent: agent.into(),
            case: case.id.clone(),
            behavior: "FAILED".into(),
            behavior_close: "FAILED".into(),
            result: "No matching outcome".into(),
            result_close: "No closing handshake".into(),
            duration: 0.0,
            remote_close_code: None,
            closed_by_me: false,
            was_clean: false,
            messages: 0,
            rx_bytes: 0,
            tx_bytes: 0,
            rx_frames: 0,
            tx_frames: 0,
            reduced: spec.message_count.is_some_and(|n| n < case.count)
                || spec.case_timeout_ms.is_some_and(|n| n < case.timeout_ms),
            received: vec![],
        }
    }
    pub(crate) fn failure(
        agent: &str,
        case: &Case,
        spec: &crate::config::Spec,
        error: impl ToString,
    ) -> Self {
        let mut r = Self::new(agent, case, spec);
        r.result = error.to_string();
        r
    }
    /// Whether this result contains an application or close-handshake failure.
    pub fn failed(&self) -> bool {
        !matches!(
            self.behavior.as_str(),
            "OK" | "NON-STRICT" | "INFORMATIONAL" | "UNIMPLEMENTED"
        ) || !matches!(self.behavior_close.as_str(), "OK" | "INFORMATIONAL")
    }
}
fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
fn filename(agent: &str, case: &str) -> String {
    use sha1::{Digest, Sha1};
    let safe: String = agent
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .take(80)
        .collect();
    let digest = format!("{:x}", Sha1::digest(agent.as_bytes()));
    format!("{safe}_{digest}_case_{}", case.replace('.', "_"))
}
fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width\"><title>{}</title><style>body{{font:16px system-ui;max-width:1200px;margin:3rem auto;padding:0 1rem;background:#10151c;color:#e4eaf2}}a{{color:#8cc9ff}}table{{border-collapse:collapse;width:100%}}th,td{{padding:.7rem;text-align:left;border-bottom:1px solid #36404d}}pre{{white-space:pre-wrap;overflow-wrap:anywhere}}.OK{{color:#7ce3ac}}.FAILED{{color:#ff9393}}</style><h1>{}</h1>{body}</html>",
        escape(title),
        escape(title)
    )
}
/// Atomically replace report files. Agent names never become raw paths or HTML.
/// `index.json` preserves the upstream agent → case → behavior summary layout.
pub fn write(directory: impl AsRef<Path>, results: &[CaseResult], cases: &[Case]) -> Result<()> {
    let dir = directory.as_ref();
    std::fs::create_dir_all(dir)?;
    let mut index: BTreeMap<&str, BTreeMap<&str, serde_json::Value>> = BTreeMap::new();
    let mut ordered = results.iter().collect::<Vec<_>>();
    ordered.sort_by(|a, b| {
        a.agent
            .cmp(&b.agent)
            .then_with(|| numeric(&a.case).cmp(&numeric(&b.case)))
    });
    let mut rows = String::new();
    for r in ordered {
        let file = filename(&r.agent, &r.case);
        let json = serde_json::to_vec_pretty(r).map_err(|e| Error::Config(e.to_string()))?;
        atomic(dir, &format!("{file}.json"), &json)?;
        index.entry(&r.agent).or_default().insert(&r.case,serde_json::json!({"behavior":r.behavior,"behaviorClose":r.behavior_close,"duration":r.duration,"remoteCloseCode":r.remote_close_code,"reportfile":format!("{file}.json"),"reduced":r.reduced}));
        let desc = cases
            .iter()
            .find(|c| c.id == r.case)
            .map_or("", |c| c.description.as_str());
        let body = format!(
            "<p><a href=\"index.html\">All results</a></p><p>{}</p><pre>{}</pre>",
            escape(desc),
            escape(&String::from_utf8_lossy(&json))
        );
        atomic(
            dir,
            &format!("{file}.html"),
            page(&format!("{} · {}", r.agent, r.case), &body).as_bytes(),
        )?;
        rows.push_str(&format!("<tr><td>{}</td><td><a href=\"{file}.html\">{}</a></td><td class=\"{}\">{}</td><td>{}</td><td>{:.2} ms</td><td>{}</td></tr>",escape(&r.agent),escape(&r.case),if r.failed(){"FAILED"}else{"OK"},escape(&r.behavior),escape(&r.behavior_close),r.duration,if r.reduced{"reduced"}else{"upstream"}));
    }
    let summary = serde_json::to_vec_pretty(&index).map_err(|e| Error::Config(e.to_string()))?;
    atomic(dir, "index.json", &summary)?;
    let body = format!(
        "<p>Source: <code>{UPSTREAM_COMMIT}</code> · {} results · {} failures</p><table><thead><tr><th>Agent</th><th>Case</th><th>Behavior</th><th>Close</th><th>Duration</th><th>Workload</th></tr></thead><tbody>{rows}</tbody></table>",
        results.len(),
        results.iter().filter(|r| r.failed()).count()
    );
    atomic(
        dir,
        "index.html",
        page("Autobahn Rust conformance report", &body).as_bytes(),
    )
}
fn numeric(id: &str) -> Vec<u32> {
    id.split('.').filter_map(|p| p.parse().ok()).collect()
}
fn atomic(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let temp = dir.join(format!(".{name}.{}.tmp", rand::random::<u64>()));
    std::fs::write(&temp, bytes)?;
    std::fs::rename(temp, dir.join(name))?;
    Ok(())
}

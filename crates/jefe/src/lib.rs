//! jefe as a library: the host side of the supervision loop.

use serde::Serialize;

/// What this build supports, for `jefe capabilities --json`.
#[derive(Debug, Clone, Serialize)]
pub struct Capabilities {
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// Protocol major version.
    pub version: u32,
    pub jefe: &'static str,
    pub acc: &'static str,
    /// Harness names as jefe's config and `--harness` take them.
    pub harnesses: Vec<String>,
    pub brains: Vec<&'static str>,
}

pub fn capabilities() -> Capabilities {
    Capabilities {
        kind: "capabilities",
        version: jefe_core::PROTOCOL_VERSION,
        jefe: env!("CARGO_PKG_VERSION"),
        acc: acc_core::VERSION,
        harnesses: acc_core::Harness::ALL
            .iter()
            .filter_map(|h| serde_json::to_value(h).ok()?.as_str().map(str::to_owned))
            .collect(),
        brains: vec!["rules", "harness"],
    }
}

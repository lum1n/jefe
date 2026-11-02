//! The worker and chief prompts. Defaults are built in; a host may pass
//! overrides (for example from `<repo>/.jefe/prompts/`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const DEFAULT_WORKER: &str = include_str!("../prompts/worker.md");
pub const DEFAULT_CHIEF: &str = include_str!("../prompts/chief.md");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prompts {
    pub worker: String,
    pub chief: String,
}

impl Default for Prompts {
    fn default() -> Self {
        Self {
            worker: DEFAULT_WORKER.to_owned(),
            chief: DEFAULT_CHIEF.to_owned(),
        }
    }
}

/// Replaces every `{{name}}` with its value; unknown names become empty.
pub fn render(template: &str, vars: &BTreeMap<&str, String>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                let name = after[..end].trim();
                if let Some(v) = vars.get(name) {
                    out.push_str(v);
                }
                rest = &after[end + 2..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes_and_drops_unknowns() {
        let vars = BTreeMap::from([("goal", "ship it".to_owned())]);
        assert_eq!(render("a {{goal}} b {{ nope }} c {{", &vars), "a ship it b  c {{");
    }
}

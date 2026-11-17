//! The worker, role and chief prompts. Defaults are built in; a host may
//! pass overrides (for example from `<repo>/.jefe/prompts/`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const DEFAULT_WORKER: &str = include_str!("../prompts/worker.md");
pub const DEFAULT_CHIEF: &str = include_str!("../prompts/chief.md");
/// A later turn of a persistent brain: the delta since its last one.
pub const DEFAULT_CHIEF_TURN: &str = include_str!("../prompts/chief_turn.md");

/// The built-in roles' instructions, by role id.
pub const ROLE_PROMPTS: &[(&str, &str)] = &[
    ("explorer", include_str!("../prompts/roles/explorer.md")),
    ("researcher", include_str!("../prompts/roles/researcher.md")),
    ("implementor", include_str!("../prompts/roles/implementor.md")),
    ("tester", include_str!("../prompts/roles/tester.md")),
    ("reviewer", include_str!("../prompts/roles/reviewer.md")),
];

/// The built-in instructions of role `id`.
pub fn builtin_role_prompt(id: &str) -> Option<&'static str> {
    ROLE_PROMPTS.iter().find(|(r, _)| *r == id).map(|(_, p)| *p)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prompts {
    pub worker: String,
    pub chief: String,
    pub chief_turn: String,
    /// Role instructions overriding the built-in ones, by role id
    /// (`.jefe/prompts/roles/<role>.md`).
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub roles: BTreeMap<String, String>,
}

impl Default for Prompts {
    fn default() -> Self {
        Self {
            worker: DEFAULT_WORKER.to_owned(),
            chief: DEFAULT_CHIEF.to_owned(),
            chief_turn: DEFAULT_CHIEF_TURN.to_owned(),
            roles: BTreeMap::new(),
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

    #[test]
    fn every_builtin_role_has_a_prompt() {
        for id in crate::job::builtin_roles().keys() {
            assert!(builtin_role_prompt(id).is_some(), "{id}");
        }
    }
}

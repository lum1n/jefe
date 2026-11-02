//! `docs/PROTOCOL.md` lists every event, action and trigger: adding one
//! without documenting it fails here.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;

use jefe_core::ActionKind;

fn read(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn snake(camel: &str) -> String {
    let mut s = String::new();
    for (i, c) in camel.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                s.push('_');
            }
            s.push(c.to_ascii_lowercase());
        } else {
            s.push(c);
        }
    }
    s
}

/// Variant names of `pub enum <name>` in `source`, snake_cased.
fn variants(source: &str, name: &str) -> Vec<String> {
    let start = source
        .find(&format!("pub enum {name} {{"))
        .unwrap_or_else(|| panic!("no enum {name}"));
    let body = &source[start..];
    let end = body.find("\n}").unwrap();
    body[..end]
        .lines()
        .skip(1)
        .filter(|l| l.starts_with("    ") && !l.starts_with("     "))
        .map(str::trim)
        .filter(|l| l.starts_with(|c: char| c.is_ascii_uppercase()))
        .map(|l| {
            let n: String = l.chars().take_while(char::is_ascii_alphanumeric).collect();
            snake(&n)
        })
        .collect()
}

fn assert_listed(doc: &str, what: &str, names: &[String]) {
    assert!(!names.is_empty(), "found no {what}");
    let missing: Vec<&String> = names
        .iter()
        .filter(|n| !doc.contains(&format!("`{n}`")))
        .collect();
    assert!(
        missing.is_empty(),
        "docs/PROTOCOL.md doesn't list these {what}: {missing:?}"
    );
}

#[test]
fn every_event_action_and_trigger_is_documented() {
    let doc = read("../../docs/PROTOCOL.md");
    assert!(doc.starts_with(&format!(
        "# jefe protocol, version {}",
        jefe_core::PROTOCOL_VERSION
    )));
    let events = variants(&read("src/event.rs"), "Event");
    assert!(events.contains(&"job_created".to_owned()), "{events:?}");
    assert_listed(&doc, "events", &events);
    let triggers = variants(&read("src/brain/mod.rs"), "Trigger");
    assert_listed(&doc, "triggers", &triggers);
    let actions = variants(&read("src/brain/mod.rs"), "ActionKind");
    assert_eq!(
        actions,
        [
            ActionKind::Plan,
            ActionKind::Split,
            ActionKind::Prompt,
            ActionKind::Answer,
            ActionKind::Allow,
            ActionKind::Deny,
            ActionKind::Interrupt,
            ActionKind::Handoff,
            ActionKind::Respawn,
            ActionKind::Accept,
            ActionKind::Reject,
            ActionKind::Wait,
            ActionKind::AskHuman,
            ActionKind::GiveUpTask,
            ActionKind::GiveUp,
            ActionKind::Done,
        ]
        .map(|a| a.name().to_owned())
    );
    assert_listed(&doc, "actions", &actions);
}

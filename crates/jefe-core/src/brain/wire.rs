//! Talking to a brain that is a model: the prompt it gets and the decision
//! block it answers with.

use std::collections::BTreeMap;

use super::{Action, ActionKind, Decision, DecisionSource, Situation};
use crate::prompts::{Prompts, render};

const FENCE: &str = "```jefe-decision";

/// One line per action: its JSON shape.
pub fn schema_line(kind: ActionKind) -> &'static str {
    match kind {
        ActionKind::Plan => {
            r#"{"action":"plan","tasks":[{"title":"..","brief":"what the worker must do, self-contained","depends_on":["t1"],"harness":"claude|codex|.."}]}"#
        }
        ActionKind::Split => r#"{"action":"split","task":"t1","into":[{"title":"..","brief":".."}]}"#,
        ActionKind::Prompt => r#"{"action":"prompt","worker":"<worker id>","text":".."}"#,
        ActionKind::Answer => {
            r#"{"action":"answer","worker":"<worker id>","answers":[["answer to question 1"],["a","b"]]}"#
        }
        ActionKind::Allow => r#"{"action":"allow","worker":"<worker id>"}"#,
        ActionKind::Deny => r#"{"action":"deny","worker":"<worker id>"}"#,
        ActionKind::Interrupt => r#"{"action":"interrupt","worker":"<worker id>"}"#,
        ActionKind::Handoff => {
            r#"{"action":"handoff","worker":"<worker id>","harness":"codex","prompt":"optional next message"}"#
        }
        ActionKind::Respawn => r#"{"action":"respawn","worker":"<worker id>"}"#,
        ActionKind::Accept => r#"{"action":"accept","task":"t1"}"#,
        ActionKind::Reject => r#"{"action":"reject","task":"t1","feedback":"what to change"}"#,
        ActionKind::Wait => r#"{"action":"wait","ms":60000}"#,
        ActionKind::AskHuman => r#"{"action":"ask_human","question":"..","options":["..",".."]}"#,
        ActionKind::GiveUpTask => r#"{"action":"give_up_task","task":"t1","reason":".."}"#,
        ActionKind::GiveUp => r#"{"action":"give_up","reason":".."}"#,
        ActionKind::Done => r#"{"action":"done","summary":"what was achieved"}"#,
    }
}

/// The prompt for one consultation.
pub fn render_prompt(s: &Situation, prompts: &Prompts) -> String {
    let allowed: Vec<_> = s.allowed.iter().map(|k| k.name()).collect();
    let actions: Vec<_> = s.allowed.iter().map(|k| schema_line(*k)).collect();
    let vars = BTreeMap::from([
        ("goal", s.goal.clone()),
        ("repo", s.repo.clone()),
        ("trigger", s.trigger.name().replace('_', " ")),
        ("allowed", allowed.join(", ")),
        ("actions", actions.join("\n")),
        (
            "situation",
            serde_json::to_string_pretty(s).unwrap_or_else(|_| "{}".to_owned()),
        ),
    ]);
    render(&prompts.chief, &vars)
}

/// The last decision block in `text` that parses as an action.
pub fn parse_decision(text: &str, source: DecisionSource) -> Option<Decision> {
    let value: serde_json::Value = crate::result::last_block_where(text, FENCE, |v: &serde_json::Value| {
        serde_json::from_value::<Action>(v.clone()).is_ok()
    })?;
    let action = serde_json::from_value::<Action>(value.clone()).ok()?;
    let rationale = value
        .get("rationale")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    let confidence = value
        .get("confidence")
        .and_then(serde_json::Value::as_f64)
        .map_or(0.5, |c| c.clamp(0.0, 1.0) as f32);
    Some(Decision {
        action,
        rationale,
        confidence,
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::TaskId;

    fn brain() -> DecisionSource {
        DecisionSource::Brain {
            harness: None,
            model: None,
        }
    }

    #[test]
    fn parses_the_last_valid_block() {
        let text = "Looks fine.\n```jefe-decision\n{\"action\":\"reject\",\"task\":\"t1\",\"feedback\":\"x\"}\n```\n\
                    Actually:\n```jefe-decision\n{\"action\":\"accept\",\"task\":\"t2\",\"rationale\":\"ok\",\"confidence\":0.8}\n```\n\
                    ```jefe-decision\n{\"action\":\"fly\"}\n```";
        let d = parse_decision(text, brain()).unwrap();
        assert_eq!(
            d.action,
            Action::Accept {
                task: TaskId::from("t2")
            }
        );
        assert_eq!(d.rationale, "ok");
        assert!((d.confidence - 0.8).abs() < 1e-6);
    }

    #[test]
    fn nothing_without_a_block() {
        assert_eq!(parse_decision("accept t1", brain()), None);
    }

    #[test]
    fn every_schema_line_parses() {
        for kind in super::super::rules::ALL_KINDS {
            let mut v: serde_json::Value = serde_json::from_str(schema_line(*kind)).unwrap();
            // Placeholders that aren't valid values in every slot.
            if let Some(h) = v.get_mut("harness") {
                *h = "codex".into();
            }
            if let Some(tasks) = v.get_mut("tasks").and_then(|t| t.as_array_mut()) {
                tasks[0]["harness"] = "codex".into();
            }
            let a: Action = serde_json::from_value(v).unwrap();
            assert_eq!(a.kind(), *kind);
        }
    }
}

//! What a worker reports at the end of its turn: the last fenced
//! ```` ```jefe-result ```` block in its final message.

use serde::{Deserialize, Serialize};

const FENCE: &str = "```jefe-result";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultStatus {
    /// The task is done and committed.
    Done,
    /// The worker can't go on (missing access, broken environment, ...).
    Blocked,
    /// Nothing needed changing.
    NoChange,
    /// The worker needs answers to its questions first.
    NeedsInput,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultQuestion {
    pub question: String,
    #[serde(default)]
    pub options: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerResult {
    pub status: ResultStatus,
    #[serde(default)]
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub questions: Vec<ResultQuestion>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub notes: String,
}

/// The last result block in `text` that parses. Harnesses that wrap
/// subagent output (opencode) still work: the whole message is scanned.
pub fn parse_result(text: &str) -> Option<WorkerResult> {
    last_block(text, FENCE)
}

/// The last block after `fence` whose content parses as `T`. The JSON is
/// read as a stream, so a closing fence inside a string doesn't end it.
pub(crate) fn last_block<T: serde::de::DeserializeOwned>(text: &str, fence: &str) -> Option<T> {
    text.match_indices(fence)
        .filter_map(|(i, _)| {
            let after = &text[i + fence.len()..];
            serde_json::Deserializer::from_str(after)
                .into_iter::<T>()
                .next()?
                .ok()
        })
        .last()
}

/// Like [`last_block`], keeping only blocks `ok` accepts.
pub(crate) fn last_block_where<T: serde::de::DeserializeOwned>(
    text: &str,
    fence: &str,
    ok: impl Fn(&T) -> bool,
) -> Option<T> {
    text.match_indices(fence)
        .filter_map(|(i, _)| {
            let after = &text[i + fence.len()..];
            serde_json::Deserializer::from_str(after)
                .into_iter::<T>()
                .next()?
                .ok()
        })
        .filter(|v| ok(v))
        .last()
}

/// The message without its result block, for summaries.
pub fn strip_result(text: &str) -> String {
    match text.rfind(FENCE) {
        Some(i) => text[..i].trim_end().to_owned(),
        None => text.trim().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_parsable_block_wins() {
        let text = "Did it.\n```jefe-result\n{\"status\":\"blocked\",\"summary\":\"first\"}\n```\nMore.\n\
                    ```jefe-result\n{\"status\":\"done\",\"summary\":\"Added X\"}\n```\n```jefe-result\nnot json\n```";
        let r = parse_result(text).unwrap();
        assert_eq!(r.status, ResultStatus::Done);
        assert_eq!(r.summary, "Added X");
    }

    #[test]
    fn questions_and_fences_inside_strings() {
        let text = "```jefe-result\n{\"status\":\"needs_input\",\"summary\":\"```\\nx\\n```\",\
                    \"questions\":[{\"question\":\"Which DB?\",\"options\":[\"Postgres\",\"SQLite\"]}]}\n```";
        let r = parse_result(text).unwrap();
        assert_eq!(r.status, ResultStatus::NeedsInput);
        assert_eq!(r.questions[0].options, ["Postgres", "SQLite"]);
        assert!(r.summary.contains("```"));
    }

    #[test]
    fn none_without_a_block() {
        assert_eq!(parse_result("All done!"), None);
        assert_eq!(strip_result("Hi.\n```jefe-result\n{}\n```"), "Hi.");
    }
}

//! What goes in and out of the loop.

use acc_core::Exec;
use serde::{Deserialize, Serialize};

use crate::brain::{Decision, Situation};
use crate::event::Stamped;
use crate::ids::{AskId, ExecId, HostId, TaskId, WorkerId};
use crate::job::{JobStatus, Report};

/// What happened, for [`crate::Jefe::step`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "input", rename_all = "snake_case")]
pub enum Input {
    /// Time passed. Send one at least every few seconds, or at
    /// [`crate::Jefe::next_deadline`].
    Tick,
    /// An [`Effect::Run`] finished. `code` is -1 when it didn't run.
    ExecDone {
        id: ExecId,
        code: i32,
        stdout: String,
        stderr: String,
    },
    /// Bytes a worker's log gained, from the offset of its [`Effect::Follow`].
    /// Serialized as a string when they are UTF-8 (logs are NDJSON), else as
    /// an array of numbers.
    Bytes {
        worker: WorkerId,
        #[serde(with = "text_or_bytes")]
        bytes: Vec<u8>,
    },
    /// A worker's log ended: the harness exited.
    LogEnded {
        worker: WorkerId,
        code: Option<i32>,
    },
    /// The brain's answer to an [`Effect::Consult`].
    Decided {
        ask: AskId,
        decision: Decision,
    },
    /// The brain couldn't answer; the rules will.
    BrainFailed {
        ask: AskId,
        why: String,
    },
    Human(HumanReply),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum HumanReply {
    /// An answer to the open [`Effect::AskHuman`] (`ask` may be omitted).
    Answer { ask: Option<AskId>, text: String },
    /// Stop the job, keeping every branch.
    Cancel,
    /// A note for the brain.
    Nudge { text: String },
}

/// What the host must do.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "effect", rename_all = "snake_case")]
pub enum Effect {
    /// Run `exec` on `host`, then send [`Input::ExecDone`] with `id`.
    Run {
        id: ExecId,
        host: HostId,
        exec: Exec,
        purpose: Purpose,
    },
    /// Follow the worker's log from `offset`: send each chunk as
    /// [`Input::Bytes`] and the end as [`Input::LogEnded`].
    Follow {
        worker: WorkerId,
        host: HostId,
        offset: u64,
    },
    Unfollow {
        worker: WorkerId,
    },
    /// Ask the brain; answer with [`Input::Decided`] or [`Input::BrainFailed`].
    Consult {
        ask: AskId,
        situation: Box<Situation>,
    },
    /// Ask the human; answer with [`HumanReply::Answer`].
    AskHuman {
        ask: AskId,
        question: String,
        options: Vec<String>,
    },
    /// Show or record an event.
    Emit(Stamped),
    /// Send a [`Input::Tick`] in this long.
    WakeIn {
        ms: u64,
    },
    /// The job is over and nothing is left to run. Persist and exit.
    Finished {
        status: JobStatus,
        report: Report,
    },
}

/// Why an exec runs; the loop reads its result accordingly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "purpose", content = "of", rename_all = "snake_case")]
pub enum Purpose {
    Preflight,
    WorktreeAdd(WorkerId),
    Launch(WorkerId),
    Input(WorkerId),
    Status(WorkerId),
    SpecWrite(WorkerId),
    Diff(TaskId),
    Merge(TaskId),
    ConflictFiles(TaskId),
    Discard(WorkerId),
    Kill(WorkerId),
    Remove(WorkerId),
}

/// Log bytes as a JSON string when they are valid UTF-8, which they are
/// unless a chunk splits a character; an array of numbers otherwise.
mod text_or_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        match std::str::from_utf8(bytes) {
            Ok(text) => s.serialize_str(text),
            Err(_) => s.collect_seq(bytes),
        }
    }

    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Repr {
        Text(String),
        Bytes(Vec<u8>),
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        Ok(match Repr::deserialize(d)? {
            Repr::Text(t) => t.into_bytes(),
            Repr::Bytes(b) => b,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn bytes_are_text_when_they_can_be() {
        let worker = WorkerId("w".to_owned());
        for bytes in [b"{\"a\":1}\n".to_vec(), vec![0xe2, 0x82]] {
            let input = Input::Bytes {
                worker: worker.clone(),
                bytes,
            };
            let json = serde_json::to_string(&input).unwrap();
            assert_eq!(serde_json::from_str::<Input>(&json).unwrap(), input);
        }
        let json = serde_json::to_string(&Input::Bytes {
            worker,
            bytes: b"hi".to_vec(),
        })
        .unwrap();
        assert!(json.contains("\"bytes\":\"hi\""), "{json}");
    }
}

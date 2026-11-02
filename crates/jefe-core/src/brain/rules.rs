//! The rules brain: a pure fallback that always has an answer. It runs when
//! no brain is configured, when the brain fails or is refused twice, and
//! once the job has used up its consultations.

use super::{Action, ActionKind, Brain, Decision, Focus, Situation, Trigger, WorkerView};
use crate::ids::{TaskId, WorkerId};
use crate::job::{TaskSpec, TaskStatus};
use crate::result::ResultStatus;

/// What a worker is told when nobody can answer its questions.
pub const DECIDE_YOURSELF: &str =
    "No one is available to answer. Decide yourself, go on, and note the choice in your result.";

#[derive(Debug, Default, Clone, Copy)]
pub struct RulesBrain;

impl Brain for RulesBrain {
    fn decide(&mut self, s: &Situation) -> Decision {
        let (candidates, why) = candidates(s);
        let action = candidates
            .into_iter()
            .find(|a| s.allowed.contains(&a.kind()))
            .unwrap_or_else(|| Action::GiveUp {
                reason: format!("no allowed action for {}", s.trigger.name()),
            });
        Decision::rules(action, why)
    }
}

fn worker<'a>(s: &'a Situation, id: &WorkerId) -> Option<&'a WorkerView> {
    s.workers.iter().find(|w| &w.id == id)
}

fn task_of(s: &Situation, id: &WorkerId) -> Option<TaskId> {
    worker(s, id).and_then(|w| w.task.clone())
}

fn alternate(s: &Situation, id: &WorkerId) -> Option<acc_core::Harness> {
    let current = worker(s, id).map(|w| w.harness);
    s.alternates.iter().copied().find(|h| Some(*h) != current)
}

/// The first line of the goal, short enough for a title.
pub fn title_of(goal: &str) -> String {
    let line = goal
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("Do the goal");
    let mut title: String = line.chars().take(72).collect();
    if title.len() < line.len() {
        title.push('…');
    }
    title
}

/// Actions in order of preference; the first allowed one wins.
fn candidates(s: &Situation) -> (Vec<Action>, &'static str) {
    let give_up_task = |w: &WorkerId, reason: &str| {
        task_of(s, w).map(|task| Action::GiveUpTask {
            task,
            reason: reason.to_owned(),
        })
    };
    match &s.trigger {
        Trigger::Plan => (
            vec![Action::Plan {
                tasks: vec![TaskSpec {
                    title: title_of(&s.goal),
                    brief: s.goal.clone(),
                    depends_on: Vec::new(),
                    harness: None,
                }],
            }],
            "one task: the whole goal",
        ),
        Trigger::Review { task, .. } => (
            vec![Action::Accept { task: task.clone() }],
            "accept what reports done",
        ),
        Trigger::Question { worker, .. } => {
            let answers = match &s.focus {
                Some(Focus::Question { questions, .. }) => questions
                    .iter()
                    .map(|q| {
                        vec![
                            q.options
                                .first()
                                .cloned()
                                .unwrap_or_else(|| DECIDE_YOURSELF.to_owned()),
                        ]
                    })
                    .collect(),
                _ => vec![vec![DECIDE_YOURSELF.to_owned()]],
            };
            let mut out = Vec::new();
            if s.attended {
                out.push(Action::AskHuman {
                    question: question_text(s),
                    options: question_options(s),
                });
            }
            out.push(Action::Answer {
                worker: worker.clone(),
                answers,
            });
            (out, "the human if there is one, else the first option")
        }
        Trigger::Report { task, worker } => {
            let status = match &s.focus {
                Some(Focus::Report { result, .. }) => Some(result.status),
                _ => None,
            };
            let mut out = Vec::new();
            if s.attended {
                out.push(Action::AskHuman {
                    question: question_text(s),
                    options: Vec::new(),
                });
            }
            if status == Some(ResultStatus::NeedsInput) {
                out.push(Action::Prompt {
                    worker: worker.clone(),
                    text: DECIDE_YOURSELF.to_owned(),
                });
            }
            out.push(Action::GiveUpTask {
                task: task.clone(),
                reason: "the worker is blocked".to_owned(),
            });
            (out, "ask, else tell it to decide, else give up the task")
        }
        Trigger::WorkerError { worker: w } | Trigger::Exited { worker: w } => {
            let mut out = Vec::new();
            if worker(s, w).is_some_and(|v| v.respawns == 0) {
                out.push(Action::Respawn { worker: w.clone() });
            }
            if let Some(harness) = alternate(s, w) {
                out.push(Action::Handoff {
                    worker: w.clone(),
                    harness,
                    prompt: None,
                });
            }
            out.extend(give_up_task(w, "the worker kept failing"));
            (out, "respawn once, hand off once, then give up the task")
        }
        Trigger::Auth { worker: w } => {
            let mut out = vec![Action::AskHuman {
                question: format!("{w} needs its harness signed in. Sign in, then answer to continue."),
                options: Vec::new(),
            }];
            if let Some(harness) = alternate(s, w) {
                out.push(Action::Handoff {
                    worker: w.clone(),
                    harness,
                    prompt: None,
                });
            }
            out.extend(give_up_task(w, "the harness needs a login"));
            (out, "a login needs a human")
        }
        Trigger::Stalled { worker: w } => {
            let stalls = worker(s, w).map_or(1, |v| v.stalls);
            let mut out = Vec::new();
            match stalls {
                0 | 1 => out.push(Action::Wait { ms: 5 * 60_000 }),
                2 => out.push(Action::Interrupt { worker: w.clone() }),
                _ => {}
            }
            out.extend(give_up_task(w, "the worker stopped making progress"));
            (out, "wait, interrupt, then give up the task")
        }
        Trigger::Quota { worker: w } => {
            let mut out = Vec::new();
            if let Some(harness) = alternate(s, w) {
                out.push(Action::Handoff {
                    worker: w.clone(),
                    harness,
                    prompt: None,
                });
            }
            out.push(Action::Wait { ms: 15 * 60_000 });
            (out, "hand off to a harness with quota left, else wait")
        }
        Trigger::Wrap => {
            let merged: Vec<_> = s
                .tasks
                .iter()
                .filter(|t| t.status == TaskStatus::Merged)
                .collect();
            if merged.is_empty() {
                (
                    vec![Action::GiveUp {
                        reason: "no task was merged".to_owned(),
                    }],
                    "nothing landed",
                )
            } else {
                let summary = merged
                    .iter()
                    .map(|t| match &t.summary {
                        Some(sum) if !sum.is_empty() => format!("{}: {sum}", t.title),
                        _ => t.title.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                (vec![Action::Done { summary }], "every task is finished")
            }
        }
    }
}

fn question_text(s: &Situation) -> String {
    match &s.focus {
        Some(Focus::Question { worker, questions }) => {
            let qs: Vec<_> = questions.iter().map(|q| q.question.as_str()).collect();
            format!("{worker} asks: {}", qs.join(" / "))
        }
        Some(Focus::Report { task, result }) => {
            let mut text = format!("{task} is {}: {}", status_word(result.status), result.summary);
            for q in &result.questions {
                text.push_str(&format!("\n- {}", q.question));
                if !q.options.is_empty() {
                    text.push_str(&format!(" ({})", q.options.join(" / ")));
                }
            }
            text
        }
        _ => format!("jefe needs a decision ({})", s.trigger.name()),
    }
}

fn question_options(s: &Situation) -> Vec<String> {
    match &s.focus {
        Some(Focus::Question { questions, .. }) if questions.len() == 1 => questions[0].options.clone(),
        _ => Vec::new(),
    }
}

fn status_word(status: ResultStatus) -> &'static str {
    match status {
        ResultStatus::Done => "done",
        ResultStatus::Blocked => "blocked",
        ResultStatus::NoChange => "unchanged",
        ResultStatus::NeedsInput => "waiting for input",
    }
}

/// Every action kind, for tests and the prompt.
pub const ALL_KINDS: &[ActionKind] = &[
    ActionKind::Plan,
    ActionKind::Split,
    ActionKind::Prompt,
    ActionKind::Answer,
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
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_are_short_first_lines() {
        assert_eq!(title_of("\n  Add retries  \nand more"), "Add retries");
        assert_eq!(title_of(&"x".repeat(100)).chars().count(), 73);
    }
}

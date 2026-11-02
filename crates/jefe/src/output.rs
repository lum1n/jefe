//! What jefe prints: one prose line per event when a person watches, or the
//! versioned NDJSON protocol (`--json`) when a program does. Diagnostics go
//! to stderr in both modes.

use std::io::Write as _;

use jefe_core::brain::DecisionSource;
use jefe_core::result::ResultStatus;
use jefe_core::{Event, JobId, JobStatus, Report, Stamped};
use serde_json::json;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Prose,
    Json,
}

#[derive(Debug)]
pub struct Output {
    pub mode: Mode,
    job: JobId,
    /// The open question's options, so a prose answer can be a number.
    options: Vec<String>,
}

impl Output {
    pub fn new(mode: Mode, job: JobId) -> Self {
        Self {
            mode,
            job,
            options: Vec::new(),
        }
    }

    fn line(&self, text: &str) {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{text}");
        let _ = out.flush();
    }

    fn json(&self, value: &serde_json::Value) {
        self.line(&value.to_string());
    }

    pub fn hello(&self) {
        if self.mode == Mode::Json {
            self.json(&json!({
                "type": "hello",
                "version": jefe_core::PROTOCOL_VERSION,
                "job": self.job,
                "jefe": env!("CARGO_PKG_VERSION"),
                "acc": acc_core::VERSION,
            }));
        }
    }

    pub fn event(&self, s: &Stamped) {
        match self.mode {
            Mode::Json => self.json(&json!({
                "type": "event",
                "seq": s.seq,
                "at_ms": s.at_ms,
                "job": s.job,
                "event": s.event,
            })),
            Mode::Prose => {
                if let Some(text) = describe(&s.event, &self.job) {
                    self.line(&text);
                }
            }
        }
    }

    pub fn ask(&mut self, ask: &jefe_core::AskId, question: &str, options: &[String]) {
        self.options = options.to_vec();
        match self.mode {
            Mode::Json => self.json(&json!({
                "type": "ask",
                "ask": ask,
                "question": question,
                "options": options,
            })),
            Mode::Prose => {
                let mut text = format!("needs you: {question}");
                for (i, o) in options.iter().enumerate() {
                    text.push_str(&format!("\n  [{}] {o}", i + 1));
                }
                self.line(&text);
            }
        }
    }

    /// A prose answer: an option's number stands for the option.
    pub fn answer_text(&self, text: &str) -> String {
        match text.trim().parse::<usize>() {
            Ok(n) if (1..=self.options.len()).contains(&n) => self.options[n - 1].clone(),
            _ => text.trim().to_owned(),
        }
    }

    pub fn snapshot(&self, job: &serde_json::Value) {
        self.json(&json!({ "type": "snapshot", "job": job }));
    }

    pub fn finished(&self, report: &Report) {
        match self.mode {
            Mode::Json => self.json(&json!({ "type": "bye", "status": report.status })),
            Mode::Prose => self.line(report_md(report).trim_end()),
        }
    }

    pub fn error(&self, code: &str, message: &str) {
        match self.mode {
            Mode::Json => self.json(&json!({ "type": "error", "code": code, "message": message })),
            Mode::Prose => eprintln!("jefe: {message}"),
        }
    }
}

/// `jefe-<job>-w1` → `w1`.
fn short<'a>(job: &JobId, id: &'a str) -> &'a str {
    id.strip_prefix("jefe-")
        .and_then(|r| r.strip_prefix(job.as_str()))
        .and_then(|r| r.strip_prefix('-'))
        .unwrap_or(id)
}

fn status_word(s: JobStatus) -> &'static str {
    match s {
        JobStatus::Planning => "planning",
        JobStatus::Running => "running",
        JobStatus::NeedsHuman => "needs a human",
        JobStatus::Done => "done",
        JobStatus::Failed => "failed",
        JobStatus::Cancelled => "cancelled",
    }
}

/// A harness as the config and `--harness` name it.
fn harness_name(h: acc_core::Harness) -> String {
    serde_json::to_value(h)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| h.id().to_owned())
}

fn action_name(action: &jefe_core::Action) -> String {
    serde_json::to_value(action)
        .ok()
        .and_then(|v| v.get("action")?.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// One line for a person, or nothing for bookkeeping they don't need.
pub fn describe(e: &Event, job: &JobId) -> Option<String> {
    let w = |id: &jefe_core::WorkerId| short(job, id.as_str()).to_owned();
    Some(match e {
        Event::JobCreated { goal, repo, .. } => format!("job {job}: {goal} (in {repo})"),
        Event::Planned { tasks } => match tasks.len() {
            1 => "planned 1 task".to_owned(),
            n => format!("planned {n} tasks"),
        },
        Event::TaskAdded { task, title, .. } => format!("  {task}: {title}"),
        Event::TaskDropped { task, reason } => format!("dropped {task}: {reason}"),
        Event::TaskFailed { task, reason } => format!("{task} failed: {reason}"),
        Event::WorkerSpawned {
            worker,
            task,
            harness,
            branch,
        } => format!(
            "{} ({}) started on {task} → {branch}",
            w(worker),
            harness_name(*harness)
        ),
        Event::WorkerPrompted { worker, text } => format!("told {}: {}", w(worker), first_line(text)),
        Event::WorkerBlocked { worker, title, .. } => format!("{} is waiting: {title}", w(worker)),
        Event::WorkerAnswered { worker, answers } => format!(
            "answered {}: {}",
            w(worker),
            answers
                .iter()
                .map(|a| a.join(", "))
                .collect::<Vec<_>>()
                .join("; ")
        ),
        Event::PermissionDenied { worker, title } => format!("denied {}: {title}", w(worker)),
        Event::WorkerReported { worker, task, result } => {
            let what = match result.status {
                ResultStatus::Done => "done",
                ResultStatus::Blocked => "blocked",
                ResultStatus::NoChange => "no change",
                ResultStatus::NeedsInput => "needs input",
            };
            format!(
                "{} reported {task} {what}: {}",
                w(worker),
                first_line(&result.summary)
            )
        }
        Event::WorkerStalled { worker, idle_ms } => {
            format!("{} has been quiet for {} min", w(worker), idle_ms / 60_000)
        }
        Event::WorkerErrored { worker, message } => format!("{} errored: {}", w(worker), first_line(message)),
        Event::WorkerExited { worker, code } => match code {
            Some(c) => format!("{} exited ({c})", w(worker)),
            None => format!("{} exited", w(worker)),
        },
        Event::WorkerReplaced {
            from,
            to,
            harness,
            handoff,
        } => format!(
            "{} {} {} ({})",
            w(to),
            if *handoff { "takes over from" } else { "restarts" },
            w(from),
            harness_name(*harness)
        ),
        Event::Decided { decision, .. } => {
            let who = match &decision.source {
                DecisionSource::Rules => return None,
                DecisionSource::Human => "you".to_owned(),
                DecisionSource::Brain { harness, model } => model
                    .clone()
                    .or_else(|| harness.map(harness_name))
                    .unwrap_or_else(|| "brain".to_owned()),
            };
            format!(
                "decided {} ({who}, {:.1}): {}",
                action_name(&decision.action),
                decision.confidence,
                first_line(&decision.rationale)
            )
        }
        Event::DecisionRejected { action, why, .. } => {
            format!("refused {}: {why}", action_name(action))
        }
        Event::Reviewed {
            task,
            accepted,
            feedback,
        } => match (accepted, feedback) {
            (true, _) => format!("accepted {task}"),
            (false, Some(f)) => format!("sent {task} back: {}", first_line(f)),
            (false, None) => format!("sent {task} back"),
        },
        Event::Merged { task, commit } => format!("merged {task} as {}", &commit[..commit.len().min(7)]),
        Event::MergeConflict { task, files } => {
            format!("conflict merging {task} ({})", files.join(", "))
        }
        Event::HumanAnswered { text, .. } => format!("you said: {}", first_line(text)),
        Event::JobFailed { reason, .. } => format!("failed: {reason}"),
        Event::Notice { message } => format!("note: {message}"),
        Event::Consulted { .. }
        | Event::Usage { .. }
        | Event::Quota { .. }
        | Event::WorkerDiscarded { .. }
        | Event::HumanAsked { .. }
        | Event::JobDone { .. }
        | Event::JobCancelled { .. } => return None,
    })
}

fn first_line(text: &str) -> &str {
    text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim()
}

/// The final report, as Markdown.
pub fn report_md(r: &Report) -> String {
    let mut s = format!("jefe {}", status_word(r.status));
    if !r.summary.is_empty() {
        s.push_str(&format!(": {}", r.summary.trim()));
    }
    s.push('\n');
    if !r.merged.is_empty() {
        s.push_str("\nmerged:\n");
        for m in &r.merged {
            let c = &m.commit[..m.commit.len().min(7)];
            s.push_str(&format!("- {} {} ({c})\n", m.task, m.title));
        }
    }
    if !r.branches.is_empty() {
        s.push_str("\nbranches kept:\n");
        for b in &r.branches {
            s.push_str(&format!("- {b}\n"));
        }
    }
    s
}

pub fn exit_code(status: JobStatus) -> i32 {
    match status {
        JobStatus::Done => 0,
        JobStatus::Failed => 2,
        JobStatus::Cancelled => 3,
        JobStatus::NeedsHuman | JobStatus::Planning | JobStatus::Running => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_names_drop_the_job_prefix() {
        let job = JobId("abc".to_owned());
        assert_eq!(short(&job, "jefe-abc-w2"), "w2");
        assert_eq!(short(&job, "other"), "other");
    }
}

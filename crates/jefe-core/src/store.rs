//! A job's state on its host, so any client (a desktop, a phone, the CLI)
//! can pick it up: `jobs/<id>/{job.json,events.ndjson,meta,lease,workers/<id>/final.md}`
//! under `$JEFE_STATE_DIR` (default `$XDG_STATE_HOME/jefe`, else
//! `~/.local/state/jefe`). These are script builders and parsers; running
//! them is the host's job ([`crate::hosted::Hosted`] adds them to the loop's
//! effects).

use acc_core::Exec;
use acc_core::shell::quote;
use serde::{Deserialize, Serialize};

use crate::event::Stamped;
use crate::ids::JobId;
use crate::job::JobStatus;

/// How long a lease lasts without a renewal.
pub const LEASE_TTL_MS: u64 = 30_000;
/// How often the holder renews it.
pub const LEASE_RENEW_MS: u64 = 10_000;
/// How long saves are batched.
pub const SAVE_DEBOUNCE_MS: u64 = 1_000;
/// Events `load_exec` brings back by default.
pub const LOAD_TAIL: usize = 200;

/// The state directory, as a shell expression.
pub fn state_dir_expr() -> &'static str {
    "${JEFE_STATE_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/jefe}"
}

/// Job `id`'s directory, as a shell expression.
pub fn job_dir_expr(id: &JobId) -> String {
    format!("{}/jobs/{}", state_dir_expr(), quote(id.as_str()))
}

/// One line, tabs and newlines included nowhere, for `meta`.
fn one_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .replace('\t', " ")
}

/// Writes the snapshot atomically, appends `events` to the log and updates
/// `meta` (status, time, goal) for listings. stdin: the snapshot's line,
/// then the events as NDJSON.
pub fn save_exec(id: &JobId, snapshot: &str, status: JobStatus, goal: &str, events: &[Stamped]) -> Exec {
    let mut stdin = snapshot.replace('\n', " ");
    stdin.push('\n');
    for e in events {
        if let Ok(line) = serde_json::to_string(e) {
            stdin.push_str(&line);
            stdin.push('\n');
        }
    }
    let script = format!(
        "d={d}\n\
         mkdir -p \"$d\" || exit 6\n\
         IFS= read -r snap || exit 6\n\
         printf '%s\\n' \"$snap\" > \"$d/job.json.tmp\" && mv -f \"$d/job.json.tmp\" \"$d/job.json\" || exit 6\n\
         cat >> \"$d/events.ndjson\"\n\
         printf '%s\\t%s\\t%s\\n' {status} \"$(date +%s)\" {goal} > \"$d/meta.tmp\" && mv -f \"$d/meta.tmp\" \"$d/meta\"\n",
        d = job_dir_expr(id),
        status = quote(status.name()),
        goal = quote(&one_line(goal)),
    );
    Exec {
        script,
        stdin: stdin.into_bytes(),
    }
}

/// Saves a worker's last message as `workers/<worker>/final.md` (stdin).
pub fn worker_final_exec(id: &JobId, worker: &str, text: &str) -> Exec {
    let script = format!(
        "d={d}/workers/{w}\nmkdir -p \"$d\" || exit 6\ncat > \"$d/final.md.tmp\" && mv -f \"$d/final.md.tmp\" \"$d/final.md\"\n",
        d = job_dir_expr(id),
        w = quote(worker),
    );
    Exec {
        script,
        stdin: text.as_bytes().to_vec(),
    }
}

/// Takes or renews the job's lease for `client`: free, expired or already
/// `client`'s, it is written anew. Prints `ok\t<client>\t<at>\t<ttl>` or
/// `held\t<holder>\t<at>\t<ttl>` (seconds); see [`parse_lease`].
pub fn lease_exec(id: &JobId, client: &str, ttl_ms: u64) -> Exec {
    let ttl = ttl_ms.div_ceil(1000).max(1);
    let script = format!(
        "d={d}\n\
         mkdir -p \"$d\" || exit 6\n\
         i=0\n\
         while ! mkdir \"$d/lease.lock\" 2>/dev/null; do\n\
         i=$((i + 1)); if [ $i -gt 30 ]; then rm -rf \"$d/lease.lock\"; break; fi; sleep 0.1\n\
         done\n\
         now=$(date +%s); holder=; at=0; ttl=0\n\
         if [ -f \"$d/lease\" ]; then IFS=\"$(printf '\\t')\" read -r holder at ttl < \"$d/lease\"; fi\n\
         if [ -z \"$holder\" ] || [ \"$holder\" = {client} ] || [ $((now - at)) -gt \"${{ttl:-0}}\" ]; then\n\
         printf '%s\\t%s\\t%s\\n' {client} \"$now\" {ttl} > \"$d/lease.tmp\" && mv -f \"$d/lease.tmp\" \"$d/lease\"\n\
         printf 'ok\\t%s\\t%s\\t%s\\n' {client} \"$now\" {ttl}\n\
         else\n\
         printf 'held\\t%s\\t%s\\t%s\\n' \"$holder\" \"$at\" \"$ttl\"\n\
         fi\n\
         rmdir \"$d/lease.lock\" 2>/dev/null; true\n",
        d = job_dir_expr(id),
        client = quote(client),
        ttl = quote(&ttl.to_string()),
    );
    Exec::new(script)
}

/// Gives the lease up, if `client` holds it.
pub fn release_exec(id: &JobId, client: &str) -> Exec {
    let script = format!(
        "d={d}\n\
         if [ -f \"$d/lease\" ]; then IFS=\"$(printf '\\t')\" read -r holder at ttl < \"$d/lease\"; \
         [ \"$holder\" = {client} ] && rm -f \"$d/lease\"; fi; true\n",
        d = job_dir_expr(id),
        client = quote(client),
    );
    Exec::new(script)
}

/// Who holds the job's lease after [`lease_exec`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    /// The caller holds it now.
    pub ok: bool,
    pub holder: String,
    /// When it was taken or renewed, in seconds since the Unix epoch (the host's clock).
    pub at_secs: u64,
    pub ttl_secs: u64,
}

pub fn parse_lease(stdout: &str) -> Option<Lease> {
    let line = stdout
        .lines()
        .find(|l| l.starts_with("ok\t") || l.starts_with("held\t"))?;
    let mut f = line.split('\t');
    let ok = f.next()? == "ok";
    Some(Lease {
        ok,
        holder: f.next()?.to_owned(),
        at_secs: f.next()?.trim().parse().ok()?,
        ttl_secs: f.next()?.trim().parse().ok()?,
    })
}

/// Lists every job on the host: one `J\t<id>\t<holder or ->\t<status>\t<updated>\t<goal>` line each.
pub fn list_exec() -> Exec {
    let script = format!(
        "now=$(date +%s)\n\
         for d in {s}/jobs/*/; do\n\
         [ -f \"$d/job.json\" ] || continue\n\
         d=${{d%/}}; id=${{d##*/}}\n\
         meta=$(cat \"$d/meta\" 2>/dev/null || printf 'unknown\\t0\\t')\n\
         holder=-\n\
         if [ -f \"$d/lease\" ]; then IFS=\"$(printf '\\t')\" read -r h at ttl < \"$d/lease\"; \
         [ $((now - ${{at:-0}})) -le \"${{ttl:-0}}\" ] && holder=$h; fi\n\
         printf 'J\\t%s\\t%s\\t%s\\n' \"$id\" \"$holder\" \"$meta\"\n\
         done; true\n",
        s = state_dir_expr(),
    );
    Exec::new(script)
}

/// A job on the host, from [`list_exec`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobListing {
    pub id: JobId,
    pub status: String,
    /// The last save, in seconds since the Unix epoch (the host's clock).
    pub updated_secs: u64,
    /// The client driving it now, if any.
    pub holder: Option<String>,
    pub goal: String,
}

pub fn parse_list(stdout: &str) -> Vec<JobListing> {
    stdout
        .lines()
        .filter_map(|l| {
            let rest = l.strip_prefix("J\t")?;
            let mut f = rest.splitn(5, '\t');
            let id = f.next()?;
            if !crate::ids::valid_job_id(id) {
                return None;
            }
            let holder = f.next()?;
            Some(JobListing {
                id: JobId(id.to_owned()),
                status: f.next().unwrap_or("unknown").to_owned(),
                updated_secs: f.next().and_then(|u| u.trim().parse().ok()).unwrap_or(0),
                holder: (holder != "-" && !holder.is_empty()).then(|| holder.to_owned()),
                goal: f.next().unwrap_or_default().to_owned(),
            })
        })
        .collect()
}

/// Reads the job's snapshot and the last `tail` events. Exit 6 when there is
/// no such job.
pub fn load_exec(id: &JobId, tail: usize) -> Exec {
    let script = format!(
        "d={d}\n\
         [ -f \"$d/job.json\" ] || {{ echo 'jefe: no such job' >&2; exit 6; }}\n\
         head -n 1 \"$d/job.json\"\n\
         [ -f \"$d/events.ndjson\" ] && tail -n {tail} \"$d/events.ndjson\"; true\n",
        d = job_dir_expr(id),
    );
    Exec::new(script)
}

/// What [`load_exec`] brought back.
#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    pub snapshot: String,
    pub events: Vec<Stamped>,
}

pub fn parse_load(stdout: &str) -> Option<Loaded> {
    let mut lines = stdout.lines();
    let snapshot = lines.next()?.trim().to_owned();
    if !snapshot.starts_with('{') {
        return None;
    }
    let events = lines
        .filter_map(|l| serde_json::from_str::<Stamped>(l.trim()).ok())
        .collect();
    Some(Loaded { snapshot, events })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn sh(script: &str, stdin: &[u8], env: &[(&str, &str)]) -> (i32, String) {
        use std::io::Write as _;
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(script);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(stdin).unwrap();
        let out = child.wait_with_output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    }

    #[test]
    fn save_load_list_and_lease_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let env = [("JEFE_STATE_DIR", tmp.path().to_str().unwrap())];
        let id = JobId::from("j-1");
        let event = Stamped {
            seq: 1,
            at_ms: 5,
            job: id.clone(),
            event: crate::event::Event::Notice {
                message: "hi".to_owned(),
            },
        };
        let save = save_exec(
            &id,
            "{\"v\":1}",
            JobStatus::Running,
            "Add\tthings\nmore",
            std::slice::from_ref(&event),
        );
        assert_eq!(sh(&save.script, &save.stdin, &env).0, 0);
        let save2 = save_exec(&id, "{\"v\":2}", JobStatus::Done, "Add things", &[]);
        assert_eq!(sh(&save2.script, &save2.stdin, &env).0, 0);

        let load = load_exec(&id, 10);
        let (code, out) = sh(&load.script, &load.stdin, &env);
        assert_eq!(code, 0, "{out}");
        let loaded = parse_load(&out).unwrap();
        assert_eq!(loaded.snapshot, "{\"v\":2}");
        assert_eq!(loaded.events, vec![event]);
        let missing = load_exec(&JobId::from("nope"), 10);
        assert_eq!(sh(&missing.script, &[], &env).0, 6);

        let lease = lease_exec(&id, "desk", 30_000);
        let (_, out) = sh(&lease.script, &[], &env);
        let l = parse_lease(&out).unwrap();
        assert!(l.ok && l.holder == "desk" && l.ttl_secs == 30, "{out}");
        let other = lease_exec(&id, "phone", 30_000);
        let l = parse_lease(&sh(&other.script, &[], &env).1).unwrap();
        assert!(!l.ok && l.holder == "desk", "{l:?}");
        // Renewing one's own lease is fine.
        assert!(parse_lease(&sh(&lease.script, &[], &env).1).unwrap().ok);

        let list = list_exec();
        let (_, out) = sh(&list.script, &[], &env);
        let jobs = parse_list(&out);
        assert_eq!(jobs.len(), 1, "{out}");
        assert_eq!(jobs[0].id, id);
        assert_eq!(jobs[0].status, "done");
        assert_eq!(jobs[0].holder.as_deref(), Some("desk"));
        assert_eq!(jobs[0].goal, "Add things");

        let release = release_exec(&id, "phone");
        sh(&release.script, &[], &env);
        assert_eq!(
            parse_list(&sh(&list.script, &[], &env).1)[0].holder.as_deref(),
            Some("desk")
        );
        let release = release_exec(&id, "desk");
        sh(&release.script, &[], &env);
        assert_eq!(parse_list(&sh(&list.script, &[], &env).1)[0].holder, None);
        let l = parse_lease(&sh(&other.script, &[], &env).1).unwrap();
        assert!(l.ok && l.holder == "phone");

        let fin = worker_final_exec(&id, "jefe-j-1-w1", "done\n");
        assert_eq!(sh(&fin.script, &fin.stdin, &env).0, 0);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("jobs/j-1/workers/jefe-j-1-w1/final.md")).unwrap(),
            "done\n"
        );
    }

    #[test]
    fn an_expired_lease_can_be_taken() {
        let tmp = tempfile::tempdir().unwrap();
        let env = [("JEFE_STATE_DIR", tmp.path().to_str().unwrap())];
        let id = JobId::from("j");
        let dir = tmp.path().join("jobs/j");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("lease"), "old\t1\t30\n").unwrap();
        let l = parse_lease(&sh(&lease_exec(&id, "new", 30_000).script, &[], &env).1).unwrap();
        assert!(l.ok && l.holder == "new");
    }
}

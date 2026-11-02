//! End to end: the real `jefe` binary runs a job through acc with a fake
//! Claude Code, in a temporary `$HOME` with a fresh repository.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead as _, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Speaks just enough of Claude Code's stream-json: on each prompt it waits
/// for `$HOME/go` (when `$HOME/hold` exists), commits a file and ends the
/// turn with a `jefe-result` block.
const FAKE_CLAUDE: &str = r##"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"type":"user"'*)
      touch "$HOME/started"
      if [ -e "$HOME/hold" ]; then
        while [ ! -e "$HOME/go" ]; do sleep 0.1; done
      fi
      printf '%s\n' '{"type":"system","subtype":"init","session_id":"fake-1","cwd":"/","model":"fake","tools":[],"permissionMode":"acceptEdits"}'
      echo hello > hello.txt
      git add hello.txt && git commit -qm "Add hello.txt"
      printf '%s\n' '{"type":"assistant","message":{"id":"msg_1","type":"message","role":"assistant","model":"fake","content":[{"type":"text","text":"Added it.\n\n```jefe-result\n{\"status\":\"done\",\"summary\":\"Added hello.txt\"}\n```"}]},"session_id":"fake-1"}'
      printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"Added it.","session_id":"fake-1","num_turns":1,"total_cost_usd":0,"usage":{"input_tokens":10,"output_tokens":5}}'
      ;;
  esac
done
"##;

fn sh(dir: &Path, script: &str) -> String {
    let out = Command::new("sh")
        .arg("-c")
        .arg(script)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{script}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

struct Env {
    _tmp: tempfile::TempDir,
    root: PathBuf,
}

impl Env {
    /// A temporary `$HOME` with a git identity, a repository on `main`, the
    /// fake harness and a config pointing at it.
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(
            home.join(".gitconfig"),
            "[user]\n\tname = t\n\temail = t@t\n[init]\n\tdefaultBranch = main\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("repo")).unwrap();
        sh(
            &root.join("repo"),
            &format!(
                "export HOME={home}; git init -q -b main \
                 && echo '# proj' > README.md && git add README.md && git commit -qm init",
                home = home.display()
            ),
        );
        let fake = root.join("bin/claude");
        std::fs::create_dir_all(fake.parent().unwrap()).unwrap();
        std::fs::write(&fake, FAKE_CLAUDE).unwrap();
        sh(&root, &format!("chmod +x {}", fake.display()));
        std::fs::write(
            root.join("jefe.toml"),
            format!(
                "[workers]\nharness = \"claude\"\nlauncher = {{ program = \"{}\" }}\n",
                fake.display()
            ),
        )
        .unwrap();
        Self { _tmp: tmp, root }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn repo(&self) -> PathBuf {
        self.root.join("repo")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_jefe"));
        c.args(args)
            .current_dir(self.repo())
            .env("HOME", self.home())
            .env("JEFE_CONFIG", self.root.join("jefe.toml"))
            .env("JEFE_STATE_DIR", self.root.join("state"))
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_STATE_HOME")
            .stdin(Stdio::null());
        c
    }

    fn jefe(&self, args: &[&str]) -> Output {
        let mut child = self
            .command(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let start = Instant::now();
        while child.try_wait().unwrap().is_none() {
            if start.elapsed() > Duration::from_secs(120) {
                let _ = child.kill();
                panic!("jefe {args:?} took too long");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        child.wait_with_output().unwrap()
    }

    fn acc_dir_is_empty(&self, sub: &str) -> bool {
        std::fs::read_dir(self.home().join(".acc").join(sub)).map_or(true, |mut d| d.next().is_none())
    }
}

fn events(stdout: &[u8]) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}")))
        .collect()
}

fn event_types(lines: &[serde_json::Value]) -> Vec<String> {
    lines
        .iter()
        .filter(|l| l["type"] == "event")
        .map(|l| l["event"]["type"].as_str().unwrap().to_owned())
        .collect()
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn a_goal_becomes_a_merged_commit() {
    let env = Env::new();
    let out = env.jefe(&["add hello", "--json", "--unattended", "--id", "e1"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let lines = events(&out.stdout);
    assert_eq!(lines[0]["type"], "hello");
    assert_eq!(lines[0]["version"], 1);
    assert_eq!(lines.last().unwrap()["type"], "bye");
    assert_eq!(lines.last().unwrap()["status"], "done");
    let types = event_types(&lines);
    for t in [
        "job_created",
        "planned",
        "worker_spawned",
        "worker_reported",
        "merged",
        "job_done",
    ] {
        assert!(types.iter().any(|x| x == t), "no {t} in {types:?}");
    }
    assert_eq!(
        std::fs::read_to_string(env.repo().join("hello.txt")).unwrap(),
        "hello\n"
    );
    let log = sh(&env.repo(), "git log --format=%s main");
    assert!(log.contains("Add hello.txt"), "{log}");
    assert!(env.acc_dir_is_empty("w"), "worktree left behind");
    assert!(env.acc_dir_is_empty("s"), "session left behind");

    let dir = env.root.join("state/jobs/e1");
    for f in ["job.json", "events.ndjson", "inputs.ndjson", "report.md"] {
        assert!(dir.join(f).is_file(), "no {f}");
    }
    assert!(!dir.join("pid").exists(), "pid file left behind");

    let status = text(&env.jefe(&["status", "e1"]));
    assert!(status.contains("job e1: done"), "{status}");
    let jobs = text(&env.jefe(&["jobs", "--json"]));
    assert!(jobs.contains("\"job\":\"e1\""), "{jobs}");
}

#[test]
fn prose_mode_reads_like_a_report() {
    let env = Env::new();
    let out = env.jefe(&["add hello", "--unattended", "--id", "p1"]);
    let t = text(&out);
    assert_eq!(out.status.code(), Some(0), "{t}");
    assert!(t.contains("w1 (claude) started on t1"), "{t}");
    assert!(t.contains("merged t1 as"), "{t}");
    assert!(t.contains("jefe done"), "{t}");
}

#[test]
fn a_dirty_checkout_fails_before_any_worker() {
    let env = Env::new();
    std::fs::write(env.repo().join("README.md"), "changed\n").unwrap();
    let out = env.jefe(&["add hello", "--json", "--unattended", "--id", "d1"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    let types = event_types(&events(&out.stdout));
    assert!(types.iter().any(|t| t == "job_failed"), "{types:?}");
    assert!(!types.iter().any(|t| t == "worker_spawned"), "{types:?}");
}

/// Kill jefe while its worker works, then pick the job up with `attach`:
/// the worker's session is restored and its report still lands.
#[test]
fn attach_after_kill_resumes_the_worker() {
    let env = Env::new();
    std::fs::write(env.home().join("hold"), "").unwrap();
    let mut child = env
        .command(&["add hello", "--json", "--unattended", "--id", "k1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || BufReader::new(stdout).lines().map_while(Result::ok).count());
    let start = Instant::now();
    while !env.home().join("started").exists() {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "the worker never started"
        );
        assert!(child.try_wait().unwrap().is_none(), "jefe exited early");
        std::thread::sleep(Duration::from_millis(50));
    }
    // Let the loop save the prompt's exec result.
    std::thread::sleep(Duration::from_millis(300));
    child.kill().unwrap(); // SIGKILL
    child.wait().unwrap();
    reader.join().unwrap();

    let attach = env
        .command(&["attach", "k1", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Release the worker only once the new jefe is following it.
    let start = Instant::now();
    while !env.root.join("state/jobs/k1/pid").exists() {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "attach never claimed the job"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    std::fs::write(env.home().join("go"), "").unwrap();
    let out = attach.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let lines = events(&out.stdout);
    let types = event_types(&lines);
    assert!(types.iter().any(|t| t == "notice"), "{types:?}");
    assert!(types.iter().any(|t| t == "merged"), "{types:?}");
    assert!(
        !types.iter().any(|t| t == "worker_spawned"),
        "a second worker: {types:?}"
    );
    assert_eq!(
        std::fs::read_to_string(env.repo().join("hello.txt")).unwrap(),
        "hello\n"
    );
    assert!(env.acc_dir_is_empty("w"), "worktree left behind");
}

/// `jefe stop` while a worker works cancels the job and keeps its branch.
#[test]
fn stop_cancels_and_keeps_the_branch() {
    let env = Env::new();
    std::fs::write(env.home().join("hold"), "").unwrap();
    let child = env
        .command(&["add hello", "--json", "--unattended", "--id", "s1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while !env.home().join("started").exists() {
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "the worker never started"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let stop = env.jefe(&["stop", "s1"]);
    assert_eq!(stop.status.code(), Some(0), "{}", text(&stop));
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    let lines = events(&out.stdout);
    let cancelled = lines
        .iter()
        .find(|l| l["event"]["type"] == "job_cancelled")
        .unwrap_or_else(|| panic!("no job_cancelled: {lines:?}"));
    assert_eq!(cancelled["event"]["branches"][0], "acc/jefe-s1-w1");
    let branches = sh(&env.repo(), "git branch --list 'acc/*'");
    assert!(branches.contains("acc/jefe-s1-w1"), "{branches}");
    assert!(!env.repo().join("hello.txt").exists());
}

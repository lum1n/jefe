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
      branch=$(printf '%s' "$line" | grep -o 'Run `git merge acc/[A-Za-z0-9_-]*' | head -1 | sed 's/.*merge //')
      case "$line" in
        *'Run `git merge acc/'*)
          git merge -q "$branch" >/dev/null 2>&1
          printf 'a\nb\n' > hello.txt
          git add hello.txt && git commit -qm "Merge $branch keeping both" ;;
        *'Your task: Write A'*) echo a > hello.txt; git add hello.txt && git commit -qm "Write a" ;;
        *'Your task: Write B'*) echo b > hello.txt; git add hello.txt && git commit -qm "Write b" ;;
        *) echo hello > hello.txt; git add hello.txt && git commit -qm "Add hello.txt" ;;
      esac
      printf '%s\n' '{"type":"assistant","message":{"id":"msg_1","type":"message","role":"assistant","model":"fake","content":[{"type":"text","text":"Added it.\n\n```jefe-result\n{\"status\":\"done\",\"summary\":\"Added hello.txt\"}\n```"}]},"session_id":"fake-1"}'
      printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"Added it.","session_id":"fake-1","num_turns":1,"total_cost_usd":0,"usage":{"input_tokens":10,"output_tokens":5}}'
      ;;
  esac
done
"##;

/// A brain on Claude's stream-json: plans one task, accepts it and wraps
/// up, by the trigger named in its prompt. With `$HOME/brain-garbage` it
/// never gives a decision block.
const FAKE_BRAIN: &str = r##"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"type":"user"'*)
      printf '%s\n' "$line" >> "$HOME/brain-prompts"
      text='Looks fine to me.'
      if [ ! -e "$HOME/brain-garbage" ]; then
        task=$(printf '%s' "$line" | grep -o 'kind\\": \\"review\\",\\n *\\"task\\": \\"t[0-9]*' | head -1 | grep -o 't[0-9]*$')
        case "$line" in
          *'Why you are asked: plan'*'write two'*|*'write two'*'Why you are asked: plan'*)
            text='Two tasks.\n\n```jefe-decision\n{\"action\":\"plan\",\"tasks\":[{\"title\":\"Write A\",\"brief\":\"Write a to hello.txt.\"},{\"title\":\"Write B\",\"brief\":\"Write b to hello.txt.\"}],\"confidence\":0.9}\n```' ;;
          *'Why you are asked: plan'*)
            text='One task.\n\n```jefe-decision\n{\"action\":\"plan\",\"tasks\":[{\"title\":\"Add hello.txt\",\"brief\":\"Create hello.txt saying hello and commit it.\"}],\"rationale\":\"one file\",\"confidence\":0.9}\n```' ;;
          *'Why you are asked: review'*)
            text='Good.\n\n```jefe-decision\n{\"action\":\"accept\",\"task\":\"'"$task"'\",\"rationale\":\"matches the brief\",\"confidence\":0.8}\n```' ;;
          *'Why you are asked: wrap'*)
            text='```jefe-decision\n{\"action\":\"done\",\"summary\":\"hello.txt added\",\"confidence\":0.9}\n```' ;;
        esac
      fi
      printf '%s\n' '{"type":"system","subtype":"init","session_id":"brain-1","cwd":"/","model":"fake-brain","tools":[],"permissionMode":"default"}'
      printf '%s%s%s\n' '{"type":"assistant","message":{"id":"msg_b","type":"message","role":"assistant","model":"fake-brain","content":[{"type":"text","text":"' "$text" '"}]},"session_id":"brain-1"}'
      printf '%s\n' '{"type":"result","subtype":"success","is_error":false,"result":"ok","session_id":"brain-1","num_turns":1,"total_cost_usd":0,"usage":{"input_tokens":10,"output_tokens":5}}'
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
        let brain = root.join("bin/brain");
        std::fs::write(&brain, FAKE_BRAIN).unwrap();
        sh(&root, &format!("chmod +x {} {}", fake.display(), brain.display()));
        std::fs::write(
            root.join("jefe.toml"),
            format!(
                "[workers]\nharness = \"claude\"\nlauncher = {{ program = \"{}\" }}\n\n\
                 [brain]\nsandbox = false\ntimeout_secs = 30\nlauncher = {{ program = \"{}\" }}\n",
                fake.display(),
                brain.display()
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
    let last = std::fs::read_to_string(dir.join("workers/jefe-e1-w1/final.md")).unwrap();
    assert!(last.contains("```jefe-result"), "{last}");
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

fn decisions_by(lines: &[serde_json::Value], by: &str) -> Vec<String> {
    lines
        .iter()
        .filter(|l| l["event"]["type"] == "decided" && l["event"]["decision"]["source"]["by"] == by)
        .map(|l| l["event"]["decision"]["action"].as_str().unwrap().to_owned())
        .collect()
}

/// The harness brain plans, reviews and wraps up; each consultation is a
/// session of its own that is gone afterwards.
#[test]
fn a_harness_brain_drives_the_job() {
    let env = Env::new();
    let out = env.jefe(&[
        "add hello",
        "--json",
        "--unattended",
        "--brain",
        "harness",
        "--id",
        "b1",
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let lines = events(&out.stdout);
    assert_eq!(
        decisions_by(&lines, "brain"),
        ["plan", "accept", "done"],
        "{lines:?}"
    );
    let added = lines.iter().find(|l| l["event"]["type"] == "task_added").unwrap();
    assert_eq!(added["event"]["title"], "Add hello.txt");
    let done = lines.iter().find(|l| l["event"]["type"] == "job_done").unwrap();
    assert_eq!(done["event"]["summary"], "hello.txt added");
    assert!(env.repo().join("hello.txt").exists());
    assert!(env.acc_dir_is_empty("s"), "a session was left behind");
    let prompts = std::fs::read_to_string(env.home().join("brain-prompts")).unwrap();
    assert!(prompts.contains("You are the brain of jefe"), "{prompts}");
    let log = std::fs::read_to_string(env.root.join("state/jobs/b1/brain/a1.md")).unwrap();
    assert!(
        log.contains("## Reply 1") && log.contains("\"action\":\"plan\""),
        "{log}"
    );
}

/// A brain that never answers with a block is asked once more, then the
/// rules decide, and the job still gets done.
#[test]
fn a_brain_without_decisions_falls_back_to_the_rules() {
    let env = Env::new();
    std::fs::write(env.home().join("brain-garbage"), "").unwrap();
    let out = env.jefe(&[
        "add hello",
        "--json",
        "--unattended",
        "--brain",
        "harness",
        "--id",
        "g1",
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let lines = events(&out.stdout);
    assert!(decisions_by(&lines, "brain").is_empty(), "{lines:?}");
    assert!(!decisions_by(&lines, "rules").is_empty(), "{lines:?}");
    let notices: Vec<&str> = lines
        .iter()
        .filter(|l| l["event"]["type"] == "notice")
        .filter_map(|l| l["event"]["message"].as_str())
        .collect();
    assert!(
        notices.iter().any(|n| n.contains("the brain failed")),
        "{notices:?}"
    );
    let prompts = std::fs::read_to_string(env.home().join("brain-prompts")).unwrap();
    assert!(
        prompts.contains("no valid ```jefe-decision block"),
        "never asked again: {prompts}"
    );
    assert!(env.repo().join("hello.txt").exists());
    assert!(env.acc_dir_is_empty("s"), "a session was left behind");
}

/// Two workers write the same file: the second merge conflicts, a third
/// worker resolves it, and both sides land.
#[test]
fn a_conflict_becomes_a_task_and_both_sides_land() {
    let env = Env::new();
    let out = env.jefe(&[
        "write two lines",
        "--json",
        "--unattended",
        "--brain",
        "harness",
        "--workers",
        "2",
        "--id",
        "c1",
    ]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let lines = events(&out.stdout);
    let types = event_types(&lines);
    let count = |t: &str| types.iter().filter(|x| *x == t).count();
    assert_eq!(count("worker_spawned"), 3, "{types:?}");
    assert_eq!(count("merge_conflict"), 1, "{types:?}");
    assert_eq!(count("merged"), 2, "{types:?}");
    let resolve = lines
        .iter()
        .filter(|l| l["event"]["type"] == "task_added")
        .find(|l| l["event"]["kind"]["kind"] == "resolve_conflict")
        .unwrap_or_else(|| panic!("no resolve task: {types:?}"));
    assert_eq!(resolve["event"]["task"], "t3");
    assert_eq!(
        decisions_by(&lines, "brain")
            .iter()
            .filter(|a| *a == "accept")
            .count(),
        3
    );
    let hello = std::fs::read_to_string(env.repo().join("hello.txt")).unwrap();
    assert_eq!(hello, "a\nb\n");
    let done = lines.iter().find(|l| l["event"]["type"] == "job_done").unwrap();
    assert_eq!(done["event"]["merged"].as_array().unwrap().len(), 2, "{done}");
    assert!(env.acc_dir_is_empty("w"), "worktree left behind");
    assert!(env.acc_dir_is_empty("s"), "session left behind");
}

/// An unprivileged `sshd` on a loopback port that logs in only the user
/// running the tests, with a key of its own, and sets `$HOME` to the test's
/// home so acc's state on "the host" is the test's.
struct Sshd {
    child: std::process::Child,
    port: u16,
    fingerprint: String,
    identity: PathBuf,
}

impl Sshd {
    /// `None` when this machine has no sshd.
    fn start(env: &Env) -> Option<Self> {
        let bin = ["/usr/sbin/sshd", "/usr/bin/sshd"]
            .into_iter()
            .find(|p| Path::new(p).is_file())?;
        let dir = env.root.join("sshd");
        std::fs::create_dir_all(&dir).unwrap();
        sh(
            &dir,
            "ssh-keygen -q -t ed25519 -N '' -f hostkey && ssh-keygen -q -t ed25519 -N '' -f client \
             && cp client.pub authorized_keys",
        );
        let fingerprint = sh(&dir, "ssh-keygen -lf hostkey.pub")
            .split_whitespace()
            .nth(1)
            .unwrap()
            .to_owned();
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        std::fs::write(
            dir.join("sshd_config"),
            format!(
                "Port {port}\nListenAddress 127.0.0.1\nHostKey {d}/hostkey\n\
                 AuthorizedKeysFile {d}/authorized_keys\nPidFile none\nUsePAM no\nStrictModes no\n\
                 PasswordAuthentication no\nKbdInteractiveAuthentication no\nSetEnv HOME={home}\n",
                d = dir.display(),
                home = env.home().display()
            ),
        )
        .unwrap();
        let child = Command::new(bin)
            .args(["-D", "-e", "-f"])
            .arg(dir.join("sshd_config"))
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(dir.join("sshd.log")).unwrap())
            .spawn()
            .unwrap();
        let start = Instant::now();
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "sshd never listened: {}",
                std::fs::read_to_string(dir.join("sshd.log")).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        Some(Self {
            child,
            port,
            fingerprint,
            identity: dir.join("client"),
        })
    }
}

impl Drop for Sshd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The job runs on an SSH host: preflight, workers, the harness brain and
/// merges all go over SSH, and `attach` after `kill -9` reaches the same
/// host from what the job saved.
#[test]
fn a_job_runs_on_an_ssh_host_and_attach_finds_it_again() {
    let env = Env::new();
    let Some(sshd) = Sshd::start(&env) else {
        eprintln!("skipped: no sshd on this machine");
        return;
    };
    let user = std::env::var("USER").unwrap();
    let mut config = std::fs::read_to_string(env.root.join("jefe.toml")).unwrap();
    config.push_str(&format!(
        "\n[hosts.loop]\nhost = \"127.0.0.1\"\nport = {}\nuser = \"{user}\"\nidentity = \"{}\"\n\
         fingerprints = [\"{}\"]\n",
        sshd.port,
        sshd.identity.display(),
        sshd.fingerprint
    ));
    std::fs::write(env.root.join("jefe.toml"), config).unwrap();
    let repo = env.repo().to_string_lossy().into_owned();

    let out = env.jefe(&["add hello", "--host", "nope", "--repo", &repo]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(text(&out).contains("no [hosts.nope]"), "{}", text(&out));
    let out = env.jefe(&["add hello", "--host", "loop"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    assert!(text(&out).contains("--repo"), "{}", text(&out));

    std::fs::write(env.home().join("hold"), "").unwrap();
    let mut child = env
        .command(&[
            "add hello",
            "--json",
            "--unattended",
            "--brain",
            "harness",
            "--host",
            "loop",
            "--repo",
            &repo,
            "--id",
            "r1",
        ])
        .current_dir(&env.root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while !env.home().join("started").exists() {
        if let Some(status) = child.try_wait().unwrap() {
            let out = child.wait_with_output().unwrap();
            panic!("jefe exited early ({status}): {}", text(&out));
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "the worker never started"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(300));
    child.kill().unwrap();
    let first = child.wait_with_output().unwrap();
    let saved = std::fs::read_to_string(env.root.join("state/jobs/r1/host.json")).unwrap();
    assert!(saved.contains("\"name\":\"loop\""), "{saved}");
    let job = std::fs::read_to_string(env.root.join("state/jobs/r1/job.json")).unwrap();
    assert!(job.contains("\"host\":\"loop\""), "{job}");

    let attach = env
        .command(&["attach", "r1", "--json"])
        .current_dir(&env.root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while !env.root.join("state/jobs/r1/pid").exists() {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "attach never claimed the job"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    std::fs::write(env.home().join("go"), "").unwrap();
    let out = attach.wait_with_output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "first run: {}\nattach: {}",
        text(&first),
        text(&out)
    );
    let lines = events(&out.stdout);
    let types = event_types(&lines);
    assert!(types.iter().any(|t| t == "merged"), "{types:?}");
    assert!(!types.iter().any(|t| t == "worker_spawned"), "{types:?}");
    assert_eq!(decisions_by(&lines, "brain"), ["accept", "done"], "{lines:?}");
    assert_eq!(
        std::fs::read_to_string(env.repo().join("hello.txt")).unwrap(),
        "hello\n"
    );
    assert!(env.acc_dir_is_empty("w"), "worktree left behind");
    assert!(env.acc_dir_is_empty("s"), "session left behind");
    let log = std::fs::read_to_string(env.root.join("sshd/sshd.log")).unwrap();
    assert!(log.contains("Accepted publickey"), "{log}");
}

/// Protocol v1 as an embedder sees it: the envelope, every control command,
/// both error codes, and `capabilities`.
#[test]
fn json_mode_speaks_protocol_v1() {
    use std::io::Write as _;

    let env = Env::new();
    let caps = env.jefe(&["capabilities", "--json"]);
    assert_eq!(caps.status.code(), Some(0));
    let caps: serde_json::Value = serde_json::from_slice(&caps.stdout).unwrap();
    assert_eq!(caps["type"], "capabilities");
    assert_eq!(caps["version"], 1);
    assert!(caps["jefe"].is_string() && caps["acc"].is_string(), "{caps}");
    assert!(
        caps["harnesses"].as_array().unwrap().contains(&"claude".into()),
        "{caps}"
    );
    assert_eq!(caps["brains"], serde_json::json!(["rules", "harness"]));

    let bad = env.jefe(&["add hello", "--json", "--repo", "/nonexistent/repo"]);
    assert_eq!(bad.status.code(), Some(1));
    let lines = events(&bad.stdout);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["type"], "error");
    assert_eq!(lines[0]["code"], "fatal");
    assert!(
        lines[0]["message"]
            .as_str()
            .unwrap()
            .contains("/nonexistent/repo")
    );

    std::fs::write(env.home().join("hold"), "").unwrap();
    let mut child = env
        .command(&["add hello", "--json", "--id", "j1"])
        .stdin(Stdio::piped())
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
    let mut stdin = child.stdin.take().unwrap();
    for line in [
        r#"{"cmd":"status"}"#,
        "not json",
        r#"{"cmd":"nudge","text":"keep it short"}"#,
        r#"{"cmd":"cancel"}"#,
    ] {
        writeln!(stdin, "{line}").unwrap();
    }
    stdin.flush().unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    let lines = events(&out.stdout);

    let hello = &lines[0];
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["version"], 1);
    assert_eq!(hello["job"], "j1");
    assert_eq!(hello["jefe"], caps["jefe"]);
    assert_eq!(hello["acc"], caps["acc"]);
    let bye = lines.last().unwrap();
    assert_eq!(*bye, serde_json::json!({"type": "bye", "status": "cancelled"}));

    let mut seq = 0;
    for l in &lines {
        let kind = l["type"].as_str().unwrap();
        assert!(
            ["hello", "event", "ask", "snapshot", "error", "bye"].contains(&kind),
            "unknown envelope {l}"
        );
        if kind == "event" {
            let s = l["seq"].as_u64().unwrap();
            assert!(s > seq, "seq went from {seq} to {s}");
            seq = s;
            assert_eq!(l["job"], "j1");
            assert!(l["at_ms"].as_u64().unwrap() > 0);
            assert!(l["event"]["type"].is_string(), "{l}");
        }
    }
    let snapshot = lines.iter().find(|l| l["type"] == "snapshot").unwrap();
    assert_eq!(snapshot["job"]["id"], "j1");
    assert_eq!(snapshot["job"]["goal"], "add hello");
    let error = lines.iter().find(|l| l["type"] == "error").unwrap();
    assert_eq!(error["code"], "bad_control");
    let types = event_types(&lines);
    assert_eq!(types.first().map(String::as_str), Some("job_created"));
    assert_eq!(types.last().map(String::as_str), Some("job_cancelled"));
}

/// `jefe status --live` follows a running job to its end.
#[test]
fn status_live_follows_a_job_to_its_end() {
    let env = Env::new();
    std::fs::write(env.home().join("hold"), "").unwrap();
    let job = env
        .command(&["add hello", "--json", "--unattended", "--id", "l1"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
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
    let live = env
        .command(&["status", "l1", "--live", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    std::fs::write(env.home().join("go"), "").unwrap();
    let out = live.wait_with_output().unwrap();
    let mut job = job;
    assert!(job.wait().unwrap().success());
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let lines = events(&out.stdout);
    assert_eq!(lines[0]["type"], "snapshot", "{lines:?}");
    let types = event_types(&lines);
    assert!(types.iter().any(|t| t == "merged"), "{types:?}");
    assert_eq!(types.last().map(String::as_str), Some("job_done"), "{types:?}");
    assert!(
        !types.iter().any(|t| t == "job_created"),
        "replayed old events: {types:?}"
    );
}

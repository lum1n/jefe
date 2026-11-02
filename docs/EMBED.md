# Embedding jefe

There are three ways to embed jefe, from the loosest coupling to the tightest. Pick the loosest one that does the job.

## 1. Run the binary (recommended)

Spawn `jefe` and use the [protocol](PROTOCOL.md):

```sh
jefe "add a --version flag" --repo /src/proj --json [--unattended] [--host box] [--id my-job-17]
```

- Read stdout line by line. Check that `hello.version` is a major you support, act on `event`s, answer `ask`s, and stop at `bye` or at an `error` with code `fatal`. The exit code agrees with `bye.status`.
- Write control commands to stdin. You can keep stdin open for the whole job: jefe exits after `bye` either way. With `--unattended`, jefe never asks and never reads stdin.
- Pass `--id` so you know the job id before `hello`. Ids are up to 40 characters: letters, digits, `-` and `_`.
- Isolate state with `JEFE_STATE_DIR` (default `~/.local/state/jefe`) and config with `JEFE_CONFIG` (default `~/.config/jefe/jefe.toml`). `jefe config example` prints every setting.
- If your process or jefe dies, the workers keep running in their acc sessions. `jefe attach <id> --json` picks the job up and goes on with `seq` where it left off. `jefe stop <id>` cancels it and keeps its branches.
- Files are in `<state>/jobs/<id>/`: `events.ndjson` holds the same events as the stream, `report.md` is the final report, and `job.json` is the latest snapshot.
- `jefe capabilities --json` tells you the protocol version, the harnesses and the brains before you start anything.

For tests, point `[workers] launcher` and `[brain] launcher` at scripts that speak a harness's stream format. jefe's own e2e tests (`crates/jefe/tests/e2e.rs`) do this with a few lines of shell.

## 2. The `jefe` crate

`jefe::start_embedded(spec, host, brain)` runs the supervisor in your tokio runtime. It prints nothing and reads neither stdin nor signals. You get back an `Embedded`:

- `messages`: an unbounded receiver of `jefe::Message`. These are the protocol's envelopes as values (`Hello`, `Event(Stamped)`, `Ask`, `Snapshot`, `Error`, `Bye`), and they serialize to exactly the lines `--json` prints.
- `control`: a `jefe::Control` with `answer(ask, text)`, `nudge(text)`, `cancel()` and `status()`. It is cheap to clone.
- `done`: a `JoinHandle` that resolves to the job's `Report`.

```rust
let mut spec = jefe_core::JobSpec::new("my-job-17", "add a --version flag", "/src/proj");
spec.workers = jefe::config::Config::load(&path)?.worker_config();
let mut job = jefe::start_embedded(spec, jefe::JobHost::local(), None).await?;
while let Some(m) = job.messages.recv().await {
    if let jefe::Message::Ask { ask, .. } = &m { job.control.answer(Some(ask.clone()), "yes"); }
}
let report = job.done.await??;
```

Pass `Some(HarnessBrain { … })` for a harness brain or `None` for the rules. For an SSH host, use `Config::job_host(name)`. `jefe::resume_embedded(id)` picks up a job whose driver died. Job state still goes to `JEFE_STATE_DIR` (default `~/.local/state/jefe`), so the CLI's `status`, `attach` and `stop` work on embedded jobs too.

`jefe::start` and `jefe::resume` are the CLI's versions: they print prose or protocol lines to stdout.

## 3. The sans-IO core (`jefe-core`)

`jefe_core::Jefe` is the whole supervision loop with no I/O. It builds for `wasm32-unknown-unknown`. You drive it:

```rust
let (mut jefe, effects) = Jefe::new(spec, now_ms);
// run `effects`, then for each result:
let effects = jefe.step(input, now_ms);
```

| `Effect` | your job |
|---|---|
| `Run { id, host, exec, purpose }` | run `exec.script` with `exec.stdin` on `host` (an acc-ssh `Host::exec`, or anything that runs POSIX `sh`), then `Input::ExecDone { id, code, stdout, stderr }`. Use `code: -1` if it couldn't run. |
| `Follow { worker, host, offset }` | stream the worker's acc log from `offset` as `Input::Bytes`, and its end as `Input::LogEnded` (acc-ssh's `Detached::attach`) |
| `Unfollow { worker }` | stop that stream |
| `Consult { ask, situation }` | ask a brain, then send `Input::Decided { ask, decision }` or `Input::BrainFailed { ask, why }`. `RulesBrain.decide(&situation)` answers at once. |
| `AskHuman { ask, question, options }` | eventually send `Input::Human(HumanReply::Answer { … })` |
| `Emit(stamped)` | show or store the event |
| `WakeIn { ms }` | send `Input::Tick` after `ms` |
| `Finished { status, report }` | done; nothing is left running |

Also send `Input::Tick` at `jefe.next_deadline()` (or every few seconds) so stalls and the wall-clock limit fire. For a crash-safe host, save `jefe.snapshot()` after every step. To come back, call `Jefe::restore(&snapshot, &specs, now_ms)`, where `specs` are the `SessionSpec`s acc lists on the host. It returns the effects that resume following.

The loop is deterministic for a given sequence of inputs and clock readings. jefe's binary logs every input to `inputs.ndjson`, so a job can be replayed.

The brain never runs anything. It returns one `Decision` naming one action, and `rules::validate` checks it against `situation.allowed`. A refused decision is retried once, and then the rules decide.

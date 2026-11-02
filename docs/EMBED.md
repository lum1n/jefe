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

`jefe::start(spec, host, brain, Mode::Json, control)` and `jefe::resume(id, mode, control, cancel)` run the same supervisor in your tokio runtime and return the `Report`. Build the `JobSpec` yourself (`JobSpec::new(id, goal, repo)` plus the fields you need), get the host from `Config::job_host(name)` or `JobHost::local()`, and pass `Some(HarnessBrain { … })` for a harness brain or `None` for the rules.

Limits as of 0.1.0: output goes to the process's stdout (prose or protocol lines), job state goes to `JEFE_STATE_DIR`, and stdin is read when `control` is true. If you need events as values instead, use the core.

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

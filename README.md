# jefe

jefe is a chief orchestrator. You give it a goal and a repository; it plans the work, hands each piece to a coding agent in its own git worktree, watches them, answers what it can, reviews what they report and merges what it accepts. It does no coding itself.

It runs on [acc](https://github.com/lum1n/acc), so the agents can be Claude Code, Codex, Cursor, Copilot, opencode or pi, on this machine or over SSH.

jefe talks prose when you run it yourself. When another program embeds it, it speaks NDJSON (`--json`, protocol version 1, see [docs/PROTOCOL.md](docs/PROTOCOL.md)). The supervision loop is a sans-IO crate, `jefe-core`, that other programs can drive directly (see [docs/EMBED.md](docs/EMBED.md)).

**Status:** 0.1.0, early. Jobs run on this machine or on an SSH host. See `CHANGELOG.md`.

```
jefe "add a --version flag"            # in the repository, on its base branch
jefe "..." --json --unattended         # NDJSON for programs; never waits on a human
jefe "..." --brain harness             # a model decides at judgment points, not the rules
jefe "..." --host box --repo /src/proj # workers on [hosts.box], over SSH
jefe status [JOB] [--json]             # the latest job by default
jefe jobs [--json]
jefe attach JOB                        # resume a job whose jefe process died
jefe stop JOB                          # cancel, keeping branches
jefe config example                    # every setting with its default
```

While it runs, type an answer when it asks (a number picks an option) or `q` to cancel. In `--json` mode stdin takes `{"cmd":"answer","text":".."}`, `{"cmd":"nudge","text":".."}`, `{"cmd":"cancel"}` and `{"cmd":"status"}`.

Exit codes: 0 done, 1 jefe failed (bad input, config, unreachable host), 2 failed or gave up, 3 cancelled.

With `--host <name>`, the workers, the brain and the merges run on a host from `[hosts.<name>]` in the config. jefe reaches it over SSH, using the SSH agent or an unencrypted `identity` file, and checks its key against `~/.ssh/known_hosts` or pinned `fingerprints`. Nothing needs to be installed there except git and the harness. `--repo` is then an absolute path on that host.

With `--brain harness` (or `[brain] kind = "harness"`), each judgment point (plan, review, questions, failures, wrap-up) is a one-shot session on your own harness in the repository, under a read-only policy, inside acc's read-only sandbox unless `[brain] sandbox = false`. It answers with one action from the allowed set, which jefe checks before acting; when it fails, times out or gives no valid decision twice, the rules decide. Each exchange is saved to `brain/<ask>.md` in the job's directory.

Config lives at `~/.config/jefe/jefe.toml` (`$JEFE_CONFIG`), job state at `~/.local/state/jefe/jobs/<id>/` (`$JEFE_STATE_DIR`): `job.json`, `events.ndjson`, `inputs.ndjson`, `report.md`, `brain.json` and `host.json`. You can override prompts per local repository in `.jefe/prompts/{worker,chief}.md`.

## Layout

- `crates/jefe-core`: jobs, tasks, workers, the supervision loop and the `Brain` interface. No I/O; builds for `wasm32-unknown-unknown`.
- `crates/jefe`: the host. It runs the loop's effects through acc and provides the `jefe` binary.

## License

MIT

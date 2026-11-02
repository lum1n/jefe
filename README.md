# jefe

jefe is a chief orchestrator. You give it a goal and a repository; it plans the work, hands each piece to a coding agent in its own git worktree, watches them, answers what it can, reviews what they report and merges what it accepts. It does no coding itself.

It runs on [acc](https://github.com/lum1n/acc), so the agents can be Claude Code, Codex, Cursor, Copilot, opencode or pi, on this machine or over SSH.

jefe talks prose when you run it yourself and NDJSON (`--json`) when another program embeds it. The supervision loop is a sans-IO crate, `jefe-core`, that other programs can drive directly.

**Status:** early. The binary runs jobs on this machine with the built-in rules as the brain; the harness brain and SSH hosts come next. See `CHANGELOG.md`.

```
jefe "add a --version flag"            # in the repository, on its base branch
jefe "..." --json --unattended         # NDJSON for programs; never waits on a human
jefe status [JOB] [--json]             # the latest job by default
jefe jobs [--json]
jefe attach JOB                        # resume a job whose jefe process died
jefe stop JOB                          # cancel, keeping branches
jefe config example                    # every setting with its default
```

While it runs, type an answer when it asks (a number picks an option) or `q` to cancel. In `--json` mode stdin takes `{"cmd":"answer","text":".."}`, `{"cmd":"nudge","text":".."}`, `{"cmd":"cancel"}` and `{"cmd":"status"}`.

Exit codes: 0 done, 1 bad input or config, 2 failed or gave up, 3 cancelled.

Config lives at `~/.config/jefe/jefe.toml` (`$JEFE_CONFIG`), job state at `~/.local/state/jefe/jobs/<id>/` (`$JEFE_STATE_DIR`): `job.json`, `events.ndjson`, `inputs.ndjson` and `report.md`. Prompts can be overridden per repository in `.jefe/prompts/{worker,chief}.md`.

## Layout

- `crates/jefe-core`: jobs, tasks, workers, the supervision loop and the `Brain` interface. No I/O; builds for `wasm32-unknown-unknown`.
- `crates/jefe`: the host. It runs the loop's effects through acc and provides the `jefe` binary.

## License

MIT

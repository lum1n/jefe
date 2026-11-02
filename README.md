# jefe

jefe is a chief orchestrator. You give it a goal and a repository; it plans the work, hands each piece to a coding agent in its own git worktree, watches them, answers what it can, reviews what they report and merges what it accepts. It does no coding itself.

It runs on [acc](https://github.com/lum1n/acc), so the agents can be Claude Code, Codex, Cursor, Copilot, opencode or pi, on this machine or over SSH.

jefe talks prose when you run it yourself and NDJSON (`--json`) when another program embeds it. The supervision loop is a sans-IO crate, `jefe-core`, that other programs can drive directly.

**Status:** early. The workspace builds; the loop is being written. See `CHANGELOG.md`.

```
jefe capabilities --json
```

## Layout

- `crates/jefe-core`: jobs, tasks, workers, the supervision loop and the `Brain` interface. No I/O; builds for `wasm32-unknown-unknown`.
- `crates/jefe`: the host. It runs the loop's effects through acc and provides the `jefe` binary.

## License

MIT

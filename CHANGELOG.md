# Changelog

## Unreleased

- `[workers] unmatched = "deny" | "brain" | "human"`: who decides approvals no policy rule matches. `brain` adds the `approval` trigger and the `allow` and `deny` actions (the rules brain always denies); `human` asks, and denies when nobody is attending. New event `permission_granted`.
- A stalled worker can be respawned or handed off.
- Library: `jefe::start_embedded` / `resume_embedded` run a job in the caller's runtime with protocol messages as typed `jefe::Message` values and a `jefe::Control` handle. Nothing is printed, and stdin and signals are left alone.
- `jefe status --live` (`-f`) prints a job's events as they happen until it ends.
- Each worker's last message when it reports is saved as `workers/<id>/final.md` in the job's directory.
- Workers may run `git remote` (list, `-v`, `get-url`) and `git rev-parse` without asking; changing remotes stays denied.
- Restarts lose nothing: writes to a worker that were running when jefe died are written again if the worker stays silent for 30 s, and a replacement waiting for its predecessor to stop still launches.

## 0.1.0 (2026-11-02)

- Protocol version 1 is frozen: [docs/PROTOCOL.md](docs/PROTOCOL.md) lists every envelope, event, trigger, action and control command, and a test fails when one is added without docs. [docs/EMBED.md](docs/EMBED.md) covers the binary, the crate and the sans-IO core.
- `--json` failures are `{"type":"error","code":"fatal"}` lines, and ignored control lines are `{"type":"error","code":"bad_control"}` (both also on stderr).
- Fix: jefe no longer hangs after `bye` when its stdin is still open.

- Workspace with `jefe-core` (sans-IO) and `jefe` (host and binary), on acc 22f3466.
- `jefe capabilities [--json]`.
- jefe-core: the supervision loop (`Jefe::new/step/snapshot/restore`) over jobs, tasks and workers on acc sessions; preflight of the repository; worktree per worker; policy-held approvals with unmatched ones denied; questions, errors, exits, stalls, quota and auth routed to the brain; result blocks (`jefe-result`) with one nudge when missing; diff review; sequential merges with conflicts turned into resolve tasks; respawn and handoff; wall-clock limit; cancel keeps branches.
- jefe-core: the `Brain` interface with a bounded action set, `rules::allowed`/`validate` (refused decisions are retried once, then the rules decide), `RulesBrain`, and the chief prompt and `jefe-decision` parser.
- `jefe "<goal>"`, `status`, `jobs`, `attach`, `stop` and `config example|check`: one local host, the rules brain, prose or `--json` output, stdin answers and control, Ctrl-C/SIGTERM cancel, `jefe.toml`, and job state under `~/.local/state/jefe/jobs/<id>/`. `attach` restores the workers' acc sessions after jefe dies.
- `Input::Bytes` serializes as a string when the bytes are UTF-8.
- `--brain harness`: each consultation is a one-shot acc session on the user's harness (`--brain-harness`, `[brain]`), read-only policy, optional read-only sandbox, one retry when the reply has no decision block, a timeout, then the rules; exchanges saved to `brain/<ask>.md`, the choice to `brain.json` so `attach` keeps it.
- The report's `merged` list names the tasks whose work landed; a conflict task's merge is listed under the task it resolved.
- `--host <name>`: run a job's workers, brain and merges on a `[hosts.<name>]` from `jefe.toml` over SSH (acc-ssh): SSH agent or an unencrypted `identity` file, host keys from `known_hosts` (default `~/.ssh/known_hosts`) or pinned `fingerprints`. `--repo` is then an absolute path on that host. The host is saved as `host.json` so `attach` and `stop` reach it again.

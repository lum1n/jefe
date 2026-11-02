# Changelog

## Unreleased

- Workspace with `jefe-core` (sans-IO) and `jefe` (host and binary), on acc 22f3466.
- `jefe capabilities [--json]`.
- jefe-core: the supervision loop (`Jefe::new/step/snapshot/restore`) over jobs, tasks and workers on acc sessions; preflight of the repository; worktree per worker; policy-held approvals with unmatched ones denied; questions, errors, exits, stalls, quota and auth routed to the brain; result blocks (`jefe-result`) with one nudge when missing; diff review; sequential merges with conflicts turned into resolve tasks; respawn and handoff; wall-clock limit; cancel keeps branches.
- jefe-core: the `Brain` interface with a bounded action set, `rules::allowed`/`validate` (refused decisions are retried once, then the rules decide), `RulesBrain`, and the chief prompt and `jefe-decision` parser.
- `jefe "<goal>"`, `status`, `jobs`, `attach`, `stop` and `config example|check`: one local host, the rules brain, prose or `--json` output, stdin answers and control, Ctrl-C/SIGTERM cancel, `jefe.toml`, and job state under `~/.local/state/jefe/jobs/<id>/`. `attach` restores the workers' acc sessions after jefe dies.
- `Input::Bytes` serializes as a string when the bytes are UTF-8.
- `--brain harness`: each consultation is a one-shot acc session on the user's harness (`--brain-harness`, `[brain]`), read-only policy, optional read-only sandbox, one retry when the reply has no decision block, a timeout, then the rules; exchanges saved to `brain/<ask>.md`, the choice to `brain.json` so `attach` keeps it.
- The report's `merged` list names the tasks whose work landed; a conflict task's merge is listed under the task it resolved.

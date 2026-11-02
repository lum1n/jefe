# Changelog

## Unreleased

- Workspace with `jefe-core` (sans-IO) and `jefe` (host and binary), on acc 22f3466.
- `jefe capabilities [--json]`.
- jefe-core: the supervision loop (`Jefe::new/step/snapshot/restore`) over jobs, tasks and workers on acc sessions; preflight of the repository; worktree per worker; policy-held approvals with unmatched ones denied; questions, errors, exits, stalls, quota and auth routed to the brain; result blocks (`jefe-result`) with one nudge when missing; diff review; sequential merges with conflicts turned into resolve tasks; respawn and handoff; wall-clock limit; cancel keeps branches.
- jefe-core: the `Brain` interface with a bounded action set, `rules::allowed`/`validate` (refused decisions are retried once, then the rules decide), `RulesBrain`, and the chief prompt and `jefe-decision` parser.

# jefe protocol, version 1

`jefe --json` (and `jefe attach --json`) speaks newline-delimited JSON: one object per line on stdout, and control commands one per line on stdin. Diagnostics go to stderr and are not part of the protocol.

## Compatibility

- `version` in `hello` is the protocol's major version. Reject majors you don't support.
- Within a major, jefe may add envelope types, event types, fields and enum values. Ignore what you don't know. Fields are never removed or renamed, and their meaning doesn't change.
- `snapshot.job` is jefe's internal job state. It is useful for display, but it isn't frozen: read events if you need stability.
- Times are milliseconds since the Unix epoch. Ids are strings: jobs as given or generated, tasks `t1`, `t2`, …, asks `a1`, `a2`, …, and workers `jefe-<job>-w<n>` (which are also the acc session id and the worktree name). Each worker's branch is `acc/<worker>`.

## Output envelopes

Every line has a `type`.

| `type` | fields | when |
|---|---|---|
| `hello` | `version`, `job`, `jefe` (jefe's version), `acc` (acc's version) | first line of a run or attach |
| `event` | `seq`, `at_ms`, `job`, `event` | each thing that happened; `seq` increases by one per job, across attaches |
| `ask` | `ask`, `question`, `options` (may be empty) | jefe needs a human answer (attended jobs only) |
| `snapshot` | `job` | answer to `{"cmd":"status"}` |
| `error` | `code`, `message` | see below |
| `bye` | `status` (`done`, `failed` or `cancelled`) | last line of a job that ended |

The stream ends with `bye`, or with an `error` whose code is `fatal`.

Error codes:

- `bad_control`: a stdin line wasn't a valid control command. It is ignored and the job goes on.
- `fatal`: jefe itself failed (bad arguments or config, no repository, unreachable host, I/O). Nothing follows.

Exit codes: `0` done, `1` jefe failed (`fatal`), `2` failed or gave up, `3` cancelled.

## Control commands (stdin)

Control is read when the job is attended, which is the default with `--json`. `--unattended` turns it off.

| command | effect |
|---|---|
| `{"cmd":"answer","ask":"a3","text":"yes"}` | answers the open `ask`. `ask` may be omitted or null. |
| `{"cmd":"nudge","text":"…"}` | a note for the brain, shown in every later consultation |
| `{"cmd":"cancel"}` | stops the job and keeps every branch; ends with `job_cancelled` and `bye` |
| `{"cmd":"status"}` | prints a `snapshot` |

How an answer is read depends on what was asked. On a review, `y`, `yes`, `ok`, `accept`, `lgtm` or `merge` accept the task, and anything else rejects it with the text as feedback. A worker's question gets the text as its answer. On an approval, `y`, `yes`, `ok` or `allow` grant it, and anything else denies it. On a login or exit problem, any answer respawns the worker. At planning and wrap-up, the text becomes a note and the brain is asked again.

## Events

Each `event` object has a `type`. Below are its fields. `?` marks a field that may be null or missing.

### Job

| `type` | fields |
|---|---|
| `job_created` | `goal`, `repo`, `base?`, `host` (`local` or a `[hosts.<name>]`) |
| `planned` | `tasks`: task ids, in the plan's order |
| `job_done` | `summary`, `merged`: `[{task, title, commit}]`, `branches`: branches left for a human |
| `job_failed` | `reason`, `branches` |
| `job_cancelled` | `branches` |
| `notice` | `message`: anything worth showing (fallbacks, human notes, retries) |

### Tasks

| `type` | fields |
|---|---|
| `task_added` | `task`, `title`, `kind`, `depends_on?` (task ids) |
| `task_dropped` | `task`, `reason` |
| `task_failed` | `task`, `reason` |
| `reviewed` | `task`, `accepted`, `feedback?` |
| `merged` | `task`, `commit` |
| `merge_conflict` | `task`, `files` (each conflict becomes a `resolve_conflict` task) |

`kind` is `{"kind":"implement"}` or `{"kind":"resolve_conflict","of":"t2","branch":"acc/…"}`.

### Workers

| `type` | fields |
|---|---|
| `worker_spawned` | `worker`, `task`, `harness`, `branch` |
| `worker_prompted` | `worker`, `text` |
| `worker_blocked` | `worker`, `kind` (`permission`, `question`, `auth`), `title` |
| `worker_answered` | `worker`, `answers` (one list of answers per question) |
| `permission_denied` | `worker`, `title`: an approval was refused (by default, every one no policy rule allows) |
| `permission_granted` | `worker`, `title`: an approval no policy rule matched was granted by the brain or the human (`[workers] unmatched`) |
| `worker_reported` | `worker`, `task`, `result` |
| `worker_stalled` | `worker`, `idle_ms` |
| `worker_errored` | `worker`, `message` |
| `worker_exited` | `worker`, `code?` |
| `worker_replaced` | `from`, `to`, `harness`, `handoff` (true: another harness primed with the conversation; false: respawn) |
| `worker_discarded` | `worker`: its session and worktree are gone |
| `usage` | `worker`, `usage`: acc's `Usage` (`input_tokens?`, `output_tokens?`, `cache_read_tokens?`, `cache_write_tokens?`, `cost_usd?`, `context_used?`, `context_window?`) at each report |
| `quota` | `worker`, `quota`: acc's `Quota` (`account?`, `plan?`, `windows`: `[{id, used_percent?, window_minutes?, resets_at?, status?}]`) |

A worker's `result` is what it reported in its `jefe-result` block: `status` (`done`, `blocked`, `no_change` or `needs_input`), `summary`, `questions?` (`[{question, options}]`) and `notes?`.

Harness names: `claude`, `codex`, `cursor`, `copilot`, `opencode`, `pi` (see `jefe capabilities --json`).

### Decisions

| `type` | fields |
|---|---|
| `consulted` | `ask`, `trigger` |
| `decided` | `ask`, `decision` |
| `decision_rejected` | `ask`, `action`, `why`: the decision failed validation, and the brain is asked once more before the rules decide |
| `human_asked` | `ask`, `question`, `options` |
| `human_answered` | `ask`, `text` |

`trigger` is `{"kind": …}` plus the ids it is about:

| `kind` | ids | asked when |
|---|---|---|
| `plan` | | the goal needs tasks |
| `report` | `task`, `worker` | a worker reported something other than done |
| `review` | `task`, `worker` | a worker reported done and its diff is ready |
| `question` | `worker`, `request_id` | a worker asked a question |
| `approval` | `worker`, `request_id` | a worker wants an approval no policy rule matches, and `[workers] unmatched = "brain"` |
| `worker_error` | `worker` | a worker's turn failed |
| `exited` | `worker` | a worker's process ended before it reported |
| `auth` | `worker` | a worker's harness needs a login |
| `stalled` | `worker` | a worker went quiet |
| `quota` | `worker` | a worker's subscription is nearly or fully used |
| `wrap` | | every task is finished |

A `decision` is one action with its `action` name flattened in, plus `rationale`, `confidence` (0–1) and `source`. `source` is `{"by":"brain","harness?","model?"}`, `{"by":"rules"}` or `{"by":"human"}`.

## Actions

These are the only things a brain can choose. For each consultation the loop works out which are allowed, and `rules::validate` checks the choice before anything happens.

| `action` | fields | does |
|---|---|---|
| `plan` | `tasks`: `[{title, brief, depends_on?, harness?}]` | sets the plan, or adds tasks at wrap-up. `depends_on` takes titles or ids. |
| `split` | `task`, `into` (tasks as for `plan`) | drops `task` and plans these instead |
| `prompt` | `worker`, `text` | sends a worker a new turn |
| `answer` | `worker`, `answers` | answers a worker's question |
| `allow` | `worker` | grants the approval the worker is waiting on |
| `deny` | `worker` | refuses the approval the worker is waiting on |
| `interrupt` | `worker` | stops a worker's current turn |
| `handoff` | `worker`, `harness`, `prompt?` | continues the task on another harness, primed with the conversation |
| `respawn` | `worker` | starts the harness again, resuming its conversation |
| `accept` | `task` | the diff is good: merge it |
| `reject` | `task`, `feedback` | sends the task back to its worker |
| `wait` | `ms` | does nothing for a while |
| `ask_human` | `question`, `options` | asks the human (attended jobs only) |
| `give_up_task` | `task`, `reason` | fails one task and keeps its branch |
| `give_up` | `reason` | fails the job |
| `done` | `summary` | ends the job |

Spawning workers and merging belong to the machine, not the brain.

## Other JSON commands

| command | output |
|---|---|
| `jefe capabilities --json` | `{"type":"capabilities","version","jefe","acc","harnesses","brains"}` |
| `jefe status [JOB] --json` | `{"type":"snapshot","job","driver_pid?"}` |
| `jefe jobs --json` | one `{"type":"job","job","status","goal","driver_pid?"}` per job, newest first |

## Example

```
{"type":"hello","version":1,"job":"tnw4th","jefe":"0.1.0","acc":"0.3.0"}
{"type":"event","seq":1,"at_ms":1793651455100,"job":"tnw4th","event":{"type":"job_created","goal":"add hello","repo":"/src/proj","base":null,"host":"local"}}
{"type":"event","seq":2,"at_ms":1793651455101,"job":"tnw4th","event":{"type":"consulted","ask":"a1","trigger":{"kind":"plan"}}}
{"type":"event","seq":3,"at_ms":1793651455138,"job":"tnw4th","event":{"type":"decided","ask":"a1","decision":{"action":"plan","tasks":[{"title":"add hello","brief":"add hello"}],"rationale":"one task: the whole goal","confidence":1.0,"source":{"by":"rules"}}}}
…
{"type":"event","seq":14,"at_ms":1793651461002,"job":"tnw4th","event":{"type":"job_done","summary":"…","merged":[{"task":"t1","title":"add hello","commit":"3f2a…"}],"branches":[]}}
{"type":"bye","status":"done"}
```

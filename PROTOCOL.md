# Shadow protocol v1

`shadow serve` is one process that serves many chats at once. A host starts
it and talks to it over its stdin and stdout.

## Transport

- UTF-8 JSON, one object per line, both ways. stdout carries protocol lines
  only; logs go to stderr.
- Every line the daemon writes carries `"seq"`: 1, 2, 3 …
- Request: `{"id", "method", "params"}` → reply `{"id", "result"}` or
  `{"id", "error": {"code", "message"}}`. The id is echoed as given (string
  or number). The host sends requests only; a line without `id` is ignored
  (logged on stderr).
- Notification (daemon → host): `{"method", "params"}`.
- Requests from the daemon to the host have integer ids 1, 2, 3 … The host
  replies `{"id", "result"}` (or `{"id", "error"}`).
- Everything about a chat carries `session`, and `turn` while a turn runs.
  Everything a branch sends (notifications and requests) also carries
  `branch`: the id of its `Branch` call.
- When stdin closes, the daemon does what `shutdown` does, then exits 0.

Error codes: `bad_request`, `busy`, `not_initialized`, `unknown_method`,
`not_running` (`turn.steer` with no running turn), `compaction_failed`,
`io_error` (`session.info` could not read the wire log).

## Host → daemon requests

| Method | Params | Result |
|---|---|---|
| `initialize` | `protocol: 1`, `data_dir` (absolute), `client: {name, version}` | `{protocol: 1, version, pid}`. Must come first; any other request before it gets `not_initialized`. Another protocol number, or a second `initialize`, is `bad_request`. |
| `session.info` | `session` | `{exists, cursor, messages, last_model_call_at, running, tasks}` — `messages`: messages in the context (after the last compaction); `last_model_call_at`: ms since the epoch or null; `tasks`: running background tasks. |
| `turn.start` | see below | `{turn}` at once (a 16-hex-digit id), before any notification of the turn. `busy` while a turn or a compaction runs in that chat; `bad_request` for bad params. |
| `turn.steer` | `session`, `id` (string or number), `input` (content blocks) | `{}`. Later `steer.delivered {id}`, or the id in `turn.end.undelivered`. `not_running` when no turn runs. |
| `turn.stop` | `session` | `{}`. The stream is cancelled, the waiting foreground command's process group gets TERM (KILL 0.5 s later), `turn.end` with reason `stopped` follows within 1 s whatever native tool runs (a Grep stops reading; a TaskStop's TERM → KILL goes on). Background commands keep running. |
| `task.list` | `session` | `{tasks: [{id, command, state, exit_code, started_at, output_path}]}` — background tasks; `started_at` in ms since the epoch. |
| `task.stop` | `session`, `task` | `{stopped}` — true when the task was running and has now ended (TERM, KILL after 2 s). |
| `session.compact` | `session`, `model`, `endpoint`, `request?`, `system?`, `prompt?` | `{summary, messages_before, messages_after}`. Notifications `compaction {state: start}`, then `done` or `failed`. On failure: error `compaction_failed`, the wire log is unchanged. |
| `shutdown` | — | `{}`; then running turns are stopped, every running command is ended (TERM, KILL after 2 s), stdout is flushed, exit 0. |

`session` ids name a folder: letters, digits, `.`, `_`, `-`, not starting
with `.`, at most 200 characters.

### `turn.start` params

| Field | Meaning |
|---|---|
| `session` | chat id |
| `cwd` | working folder (absolute, must exist); every command starts there |
| `env?` | `{NAME: value}` added over the daemon's own environment for every command |
| `model` | model id, sent as is |
| `request?` | object whose fields are added to the model request body (`max_tokens`, `thinking`, `metadata` …). Setting `model`, `system`, `tools`, `messages` or `stream` is `bad_request`. The daemon adds no `max_tokens` of its own. |
| `system` | array of text blocks, sent as is |
| `host_tools` | `[{name, description?, input_schema, parallel?, keep_latest?}]` — tools the host runs. Sent to the model as `{name, description, input_schema}`. A name used twice, or the name of an offered native tool, is `bad_request`. `keep_latest`: the newest call of this tool is never faded and is kept word for word at a compaction (the ledger). |
| `native_tools?` | names of native tools to offer (default all eight); an unknown name is `bad_request` |
| `endpoint` | `{url, headers?}` — `url` is `http://…` or `https://…` (TLS with the bundled web root certificates); `headers` are added to each model request |
| `limits?` | `max_steps` (50, ≥ 1), `bash_wait_ms` (120000), `result_cap_chars` (30000, ≥ 200), `retry_max` (6), `context_tokens` (≥ 1): the size of the model's window; without it there is no 60% fade, no gauge and no auto-compaction |
| `cache?` | `{mode: "auto" or "off", ttl?: "5m" or "1h"}`, default auto. The cache's lifetime is `ttl` (5 minutes when absent); with `off` every turn starts cold |
| `auto_compact?` | `true`: the turn compacts by itself at 85% of `context_tokens`. Anything else, or absent: it does not |
| `catchup?` | messages (`{role: user or assistant, content}`) appended to the log before `input` |
| `cursor?` | text tied to this turn; logged; the newest is in `session.info` and `turn.end` |
| `input` | content blocks of the user's message (non-empty) |

## Daemon → host notifications

| Method | Params |
|---|---|
| `step.start` | `step` (1, 2 … within the turn) |
| `model.event` | `step`, `event` — every event of the model's stream as received, except `ping`. After a `retry` the step's events start again from `message_start`. |
| `message.end` | `step`, `message` (the assistant message as logged), `usage`, `stop_reason`. After a Stop mid-stream it carries the complete blocks kept, `usage: {}` and `stop_reason: null`. |
| `tool.start` | `step`, `call: {id, name, input}`, `native` — when a call starts to run (for a native tool, after `tool.check` allowed it; `input` as it runs) |
| `task.update` | `task: {id, command, state, exit_code?, output_path}` — when a command becomes a background task (`running`) and when a background task ends (`exited` or `stopped`), also outside turns |
| `steer.delivered` | `id` |
| `retry` | `step`, `attempt`, `wait_ms`, `status` (null for a connection error or a broken stream) |
| `compaction` | `state: start, done or failed` — for `session.compact`, and with `turn` for a turn's auto-compaction |
| `turn.end` | `reason: done, stopped, max_steps or error`, `error?: {message, status?, body?}`, `steps`, `usage` (sums of the steps' numeric usage fields), `undelivered` (ids of every accepted steer not delivered, whatever the reason), `cursor` |

## Daemon → host requests

| Method | Params | Reply |
|---|---|---|
| `tool.check` | `step`, `call: {id, name, input}` | `{decision: "allow", input?}` or `{decision: "deny", message}`. Sent for every native tool call before it runs; with `input` (an object) the tool runs with it. An error reply or a reply without a decision gives the call an error result. |
| `tool.call` | `step`, `call: {id, name, input}` | `{content, is_error?}` — `content` a string or content blocks. For host tools; no `tool.check` before it. |
| `tool.end` | `step`, `call_id`, `name`, `input` (as run), `content`, `is_error`, `meta: {duration_ms, exit_code?, task_id?, truncated?, overflow_path?}` | `{}`. For native tools that ran, before the next model call. During a Stop it is sent but not waited for. |

A denied call gets an error result with the host's message and no
`tool.start` / `tool.end`. A tool name that is not offered gets the error
result `No such tool: <name>` and no request at all.

## The loop

1. One step is one model call plus its tool calls.
2. Request body, in this order: `model`, `system`, `tools` (native
   definitions in the fixed order Read, Write, Edit, Bash, TaskStop, Grep,
   Glob, Branch — those offered — then `host_tools`), `messages` (from the
   wire log, spliced in byte for byte, with fades applied), `stream: true`,
   then the fields of `request`.
   `POST` to `endpoint.url` with `content-type: application/json`,
   `accept: text/event-stream` and `endpoint.headers`.
3. Cache mode `auto`: `cache_control: {type: "ephemeral", ttl?}` on the last
   system block, the last tool definition and the last content block of the
   last message (not on a thinking block or string content). Only in what is
   sent; never logged.
4. Neighbouring calls to Read, Grep, Glob, Branch and host tools marked
   `parallel` run at the same time (their first request goes out in call order); every
   other call runs alone, in call order. Results go into one user message,
   in call order.
5. After the results come, in this order: the gauge line (see below), notes
   of background tasks that ended, then waiting steer messages in arrival
   order (each followed by `steer.delivered`).
6. A foreground command moves to the background at once when a steer is
   waiting. From the moment a steer is waiting, no further call that runs
   alone (Write, Edit, Bash, TaskStop, host tools not marked `parallel`) is
   started in that step: each gets the error result `Not run: a message from
   the user arrived first. Read it, then decide again.` and no `tool.check`
   or `tool.start` (or, when its `tool.check` was already out, no
   `tool.start`). Read, Grep, Glob and `parallel` host tools still run. Task
   notes do not count.
7. The turn ends when the model's message has no tool call (`done`), on Stop
   (`stopped`), after the results of step `max_steps` (`max_steps`), or on an
   error retries cannot fix (`error`). If steers or task notes wait when the
   model's message has no tool call (and `max_steps` is not reached), they
   become a new user message and the turn goes on. Steers not delivered when
   the turn ends — still waiting, or taken for a write that failed — are
   listed in `undelivered`. An assistant message with no content
   block is not logged.
8. Retries: status 429, 500, 502, 503, 504, 529, connection errors, and a
   stream that breaks, reports an `error` event, or carries tool input that
   is not valid JSON (the partial message is dropped; no tool runs), up to
   `retry_max` times; waits 1 s, 2 s, 4 s … up to 30 s, or the response's
   `retry-after` (seconds). Any other status ends the turn with
   `error: {message, status, body}`.
9. Stop mid-stream: complete blocks are kept and logged, incomplete ones
   dropped; tool calls that did not run get the error result
   `Stopped by the user.`
10. The same call (tool and input) in each of the two steps before, with the
   same result as theirs: the result sent is `Unchanged. You already have
   this result (step N).`, N being the latest step whose result holds it in
   full. The tool still runs (`tool.end` has its real result). A different
   result is sent in full and starts the count again. Counted within a turn
   or a branch.

## The context engine

Everything it adds to a message (a stub, the state delta, a gauge line) is
logged with it, so later requests repeat it unchanged. N in "step N" counts
the chat's assistant messages since the last compaction.

- **Size.** The context's size is what the model last reported (input,
  cache-read and cache-write tokens plus the output tokens of its message),
  plus or minus the characters changed since then at 4 per token.
- **Fade.** A candidate is a tool result older than the last 10 steps, of at
  least 2,000 characters, not yet faded, not the newest Read of its file that
  shows the file (an `Unchanged since` answer does not count), and not the
  newest call of a `keep_latest` host tool. Order: superseded results first
  (a Read of a file read again later, a call repeated later with the same
  tool and input), then the rest; oldest first, larger before smaller. A
  faded result is sent as one line:
  `[Faded to save context: <tool and main argument> returned <N lines (M characters) | an image> at step N. Full output: <overflow file>. Run it again if you need it.]`
  (the overflow file only when there is one). Its Read lines are no longer
  "above": reading them again returns the text. Fades happen in one batch,
  logged as one marker, at two moments only:
  - **cold wake**: at `turn.start` when more than the cache's lifetime has
    passed since the chat's last model call (always with cache `off`): every
    candidate, before the first model call;
  - **60%**: at a step boundary when the context is at 60% of
    `context_tokens` or more: candidates in order until it is at 40%, or
    none is left. When that leaves it at 60% or more, the next 60% fade
    waits until a boundary finds it under 60% (or the next turn), so fades
    never come a few at a time. A cold wake counts as such a fade.
- **State delta.** At `turn.start`, the user's message starts with one text
  block `What changed since the last model call:` and one line each for: a
  file in the version table that changed since the model saw it (lines
  added and removed) or was deleted, each new version named once; a
  background command that ended and was not announced (id, command, exit
  code, output); the time since the last model call when over 30 minutes.
  No block when nothing applies. A named file still counts as not seen.
- **Gauge.** When the context first crosses 50, 75 or 90% of
  `context_tokens` since the last compaction (measured after a fade at the
  same boundary): one line after the step's results,
  `Context: 52% used (104k of 200k tokens).`
- **Auto-compaction.** With `auto_compact: true`, at a step boundary at 85%
  or more: the turn compacts with its own model, endpoint, request, system
  and tools (`compaction` start, then done or failed), then goes on. Steers
  and notes of that boundary come after the summary. A failed compaction
  leaves the log unchanged and is not tried again in that turn.

## Branch

`Branch({task})` runs `task` on a copy of the context and returns only its
final text, within the result cap (`tool.check` is asked for it like any
native tool).

- Its first request: the same `model`, `system`, `tools`, `request`, cache
  marks and endpoint as the turn, byte for byte; `messages` are the turn's
  messages before the current step, then one user message: the task, and
  `You are a branch of the conversation above. Only your final answer goes back to it, and you cannot change files here (Edit, Write and Branch are not available).`
- It runs the turn's loop with its own steps (1, 2 …) up to `max_steps`. In
  it, Edit, Write and Branch calls are not run: `Not available in a branch.`
  (no `tool.check`). Other calls run as in the turn (`tool.check`,
  `tool.call`, `tool.end` carry `branch`). Its Reads use a copy of the
  version table.
- Nothing of it is logged; the turn's log gains the call and its result. Its
  usage is added to the turn's `usage`; `steps` counts the turn's steps only.
- Its first message without a tool call is the answer. Errors:
  `The branch failed: …`, `Not finished: the branch used all its N steps.`
- Stop ends every branch (`Stopped by the user.`). A waiting steer ends every
  running branch at once: `Not finished: a message from the user arrived first.`

## Tools

`shadow tools` prints the eight definitions as one JSON array.

- Paths are absolute or relative to `cwd`.
- A text result longer than `result_cap_chars` is cut to whole first and last
  lines around one line `[… N lines omitted. Full output: <path>]`, the
  whole within the cap; the full text is in
  `<data_dir>/sessions/<session>/scratch/<call id>.txt` (`meta.truncated`,
  `meta.overflow_path`).
- The version table remembers per chat the content hash of every file the
  model has read in full (all its lines returned, over one or more Reads) or
  written.

| Tool | Arguments | Notes |
|---|---|---|
| `Read` | `file_path`, `offset?` (1-based), `limit?` (2000) | `cat -n` lines (number right-aligned to 6, a tab, the line); a line over 2000 characters is cut and marked; `(Lines a–b of N. …)` when not all lines are shown. PNG, JPEG, GIF, WebP (by content) come back as an image block (at most 3,932,160 bytes). Other binary files (a NUL byte in the first 8 KB): an error with the size and a type. A missing file: an error with up to 5 similar names from its folder. Lines already returned at an earlier step of this chat for the same content, since the last compaction: `Unchanged since step N (lines a–b are above).` where N counts the chat's assistant messages. |
| `Write` | `file_path`, `content` | Creates parent folders. Overwriting needs the current version in the version table; a changed file gives the stale error of Edit. |
| `Edit` | `file_path` + `old_string`, `new_string`, `replace_all?` — or `edits: [{old_string, new_string, replace_all?}]` | Needs the current version in the version table. Changed since seen: the unified diff seen → now (at most 200 lines); if it all fitted, the new version counts as seen. Not found: the nearest near-match (similarity ≥ 0.6) with its line number. Not unique: the line of every match (at most 20). `edits` apply in order, all or nothing. Temp file + rename; keeps mode, CRLF line endings (when every line has one) and whether the file ends with a newline. Result: changed lines with new line numbers and 3 lines around. |
| `Bash` | `command`, `description?`, `timeout?` (ms, max 600000), `run_in_background?` | `/bin/bash -c` in `cwd`, own process group, stdin `/dev/null`, stdout+stderr in `<scratch>/tasks/<task id>.log`. Waits `timeout` or `bash_wait_ms`. Ended: bounded output, `Exit code N` when not 0 (128+signal when killed), `(no output)` when empty. At the limit, with `run_in_background`, or when a steer waits: a background task — output so far plus `Still running in the background as task <id>. Output: <path>. You will be told when it ends.` Never killed by the limit. When a background task ends: `task.update`, and while a turn runs a note `Background task <id> (<command>) ended: exit N. Output: <path>` (none when TaskStop stopped it). |
| `TaskStop` | `task_id` | TERM to the task's process group, KILL after 2 s; result: its last 20 lines. |
| `Grep` | `pattern`, `path?`, `glob?`, `output_mode?` (`files_with_matches`, `content`, `count`), `-i?`, `-n?`, `-A?`, `-B?`, `-C?`, `head_limit?`, `multiline?` | ripgrep's crates. `.gitignore` respected, hidden files searched, `.git` skipped, binary files skipped, files in name order. Entries within `head_limit`, then `(N more not shown)`; the whole result is cut to the cap like every tool (a long list goes to the overflow file, not into memory). |
| `Glob` | `pattern`, `path?` | Same walk as Grep; `*` stays within one folder, `**` crosses folders, `{a,b}` alternatives. Newest first, at most 100, then `(N more not shown)`; cut to the cap like every tool. |
| `Branch` | `task` | See "Branch" above. |

## Files

`<data_dir>/sessions/<session>/`:

- `wire.jsonl` — append-only, one object per line, written and synced after
  the user's input, each complete model message and each step's results:
  `{"kind":"message","message":{…}}`, `{"kind":"compaction","message":{…},"ledger"?:{files, latest}}`
  (the summary message; the context restarts from it),
  `{"kind":"cursor","cursor":"…"}`, `{"kind":"model_call","at":<ms>,"tokens"?:<n>}`
  (after the model's message; `tokens`: the size it reported),
  `{"kind":"fade","stubs":{"<call id>":"<stub>"}}` (those results are sent
  as their stub from here on; the earlier lines are not rewritten).
  On load a torn last line is ignored (and cut off at the next write); a
  model message whose tool calls have no results gets the error result
  `Interrupted: the harness restarted.` for each.
- `versions.json` — the version table: `{"files": {"<path>": {hash, size, full, lines, text?, shown?, told?}}}` (FNV-1a 64 hashes; text kept up to 4 MB; `told`: the version a state delta named).
- `scratch/` — overflow files (`<call id>.txt`) and command logs (`tasks/<id>.log`).

## Compaction

`session.compact` sends the chat (with the latest turn's tool definitions,
`system` if given, and the fields of `request`) plus one user message with
the built-in summary prompt (or `prompt`). The text of the reply becomes the
summary. A compaction marker is logged with the message
`This chat was compacted. Summary of the conversation before this point:`
followed by the summary; from then on requests hold that message plus later
messages. Read's "above" lines are forgotten.

The ledger, written by Shadow without a model, is a second text block of that
message, starting `Kept word for word at this compaction (not summarized):`.
It lists every file the chat created or changed with Write or Edit (as the
call named it, with the files of earlier ledgers), every background command
still running (id, command, output file), and for each `keep_latest` host
tool its newest call's input and result (from an earlier ledger when it was
not called since), within the result cap. `session.compact` uses the latest
turn's `keep_latest` tools and result cap (none and 30000 after a restart).

## Test only

`SHADOW_TEST_CACHE_TTL_MS`: **tests only**. When set, the prompt cache's
lifetime is that many milliseconds instead of 5 minutes or 1 hour. Never set
it in normal use.

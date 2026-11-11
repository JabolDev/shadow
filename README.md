# Shadow

Shadow is a small, fast coding agent. It runs a model in a loop with eight
tools: Read, Write, Edit, Bash, TaskStop, Grep, Glob and Branch. It can run in two ways:

- `shadow run "<prompt>"` runs one prompt in the current folder from a terminal.
- `shadow serve` lets another program drive it over stdin/stdout (see `PROTOCOL.md`).

It calls Anthropic-format Messages APIs (Anthropic, OpenRouter, or any other
Anthropic-format endpoint) with your own API key.

## Download

Each release has a static x86_64 Linux binary,
`shadow-x86_64-unknown-linux-musl`, and its SHA-256 next to it:

```sh
sha256sum -c shadow-x86_64-unknown-linux-musl.sha256
```

Releases are built by `.github/workflows/release.yml` when a tag
`v<version>` (the version in `Cargo.toml`) is pushed. Other platforms are not
built yet.

## Build

You need Rust 1.99 (`rust-toolchain.toml`).

```sh
cargo build --release                                      # target/release/shadow
cargo build --release --target x86_64-unknown-linux-musl   # a static Linux binary (needs musl-gcc)
```

## Run

```sh
export ANTHROPIC_API_KEY=...        # or OPENROUTER_API_KEY=...
shadow run --model <model id> "why does the test in src/parse.rs fail?"
```

The model's text goes to stdout. Each tool call, a short line for each result,
and errors go to stderr. The exit code is 0 when the model finished, 130 after
Ctrl-C, and not 0 for any other ending, including when stderr or stdout can
no longer be written. When the run ends, every process it started is ended
too, including processes its commands left behind.

Reading and searching are always allowed. Changing files and running commands
need permission:

| Flag | Meaning |
|---|---|
| `--model NAME` | the model (or `SHADOW_MODEL`); there is no default |
| `--max-tokens N` | `max_tokens` of each request (default 16000) |
| `--context-tokens N` | the size of the model's window (default 200000); the chat compacts itself at 85% of it |
| `--allow edit` | allow Write and Edit |
| `--allow shell` | allow Bash |
| `--yes` | allow both |
| `--session NAME` | continue the chat NAME (without it, every run is a new chat) |
| `--data-dir DIR` | where chats are kept (default `~/.shadow`) |
| `--base-url URL` | another Anthropic-format endpoint (`http://` or `https://`) |

A refused call is answered to the model with the reason, for example
`Not allowed: this run was started without --allow shell.` The calls of a
`Branch` follow the same rules; their lines on stderr are indented by two
more spaces, and a branch's own text is not printed.

The prompt is built in, and names the working folder. If the folder has an
`AGENTS.md`, its text is added after it.

### Which endpoint and key

1. If `ANTHROPIC_API_KEY` is set, Shadow uses `https://api.anthropic.com/v1/messages`
   and sends the key as `x-api-key`.
2. Otherwise, if `OPENROUTER_API_KEY` is set, it uses
   `https://openrouter.ai/api/v1/messages` and sends `Authorization: Bearer <key>`.
3. `--base-url` replaces only the URL. The key and its header still follow 1 and 2.

A key is always needed. Without one, Shadow stops with a one-line error. Every
request also sends `anthropic-version: 2023-06-01`. The key is read from the
environment only, and the commands the model runs do not see it. Shadow never
prints, logs or writes the key itself, and it removes the key from every error
and tool line it prints. Text an endpoint sends back as the model's own answer
is shown and stored as received.

### Variables

| Variable | Meaning |
|---|---|
| `ANTHROPIC_API_KEY`, `OPENROUTER_API_KEY` | the API key (see above) |
| `SHADOW_MODEL` | the model, when `--model` is not given |
| `SHADOW_TEST_CA` | **tests only**: a PEM file of extra certificates to trust for `https://`. Never set it in normal use. |
| `SHADOW_TEST_CACHE_TTL_MS` | **tests only**: the prompt cache's lifetime in ms. Never set it in normal use. |

## Serve

```sh
shadow serve
```

One process serves many chats. It speaks protocol v1: one JSON object per
line on stdin and stdout. The host sends the model, the endpoint (URL and
headers), the system prompt and its own tools with each turn. `PROTOCOL.md`
describes every message.

## Long chats

Shadow keeps a long chat small without breaking the model's prompt cache
more than needed (`PROTOCOL.md` has the details):

- Old bulky tool results fade to a one-line note saying how to get them back,
  in one batch: when a chat starts again after its cache has expired, or
  when the context reaches 60% of the window (down to 40%).
- At 85% the chat is compacted: the model writes a summary, and Shadow keeps
  the changed files, the running commands and the newest result of chosen
  host tools word for word.
- A turn starts with a short note of what changed meanwhile (files, ended
  commands, time); the model sees one line at 50, 75 and 90% of the window;
  the third identical call in a row with an identical result gets a one-line
  answer.
- `Branch` runs a task on a copy of the conversation and returns only its
  answer, so looking things up does not fill the conversation.

## Data

Each chat lives in `<data dir>/sessions/<name>/`:

- `wire.jsonl` — the chat, append-only
- `versions.json` — which file versions the model has seen
- `scratch/` — the full text of long tool results, and command output

The data dir is `~/.shadow` for `run` (or `--data-dir`). For `serve`, the host
names it.

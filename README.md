# fast-rlm-agent

A terminal harness for [FastRLM](https://github.com/avbiswas/fast-rlm), built
with Rust and Ratatui. It gives FastRLM structured source context and renders
its recursive agents, generated Python, REPL output, final result, and usage as
the run happens.

## Features

- Live FastRLM agent and REPL-step rendering
- Recursive sub-agent execution with generated Python and captured output
- Structured dictionary input with `prompt`, `links`, and `files`
- A bounded `fetch_url` REPL tool for external sources
- Reviewed workspace read, write, exact-edit, and shell tools over MCP
- Diff previews and explicit approval before mutations or commands
- Persistent FastRLM REPL memory across follow-up turns and `/resume`
- Syntax-highlighted Python with compact output and error previews
- Token, cache, reasoning-token, and cost accounting
- Local JSON chat sessions and FastRLM run logs

## Setup

You need a recent Rust toolchain, Python 3.10+, Deno 2+, `uv`, and an
OpenAI-compatible model API. FastRLM is pinned to the latest tested GitHub
revision in `requirements-fast-rlm.txt`.

Clone or download this repository, then enter its directory:

```sh
cd fast-rlm-agent
```

Set the model configuration in your shell:

```sh
export BASE_URL="https://api.openai.com/v1"
export API_KEY="your-api-key"
export MODEL_NAME="your-model-name"
```

Install the FastRLM Python backend:

```sh
./scripts/setup-fast-rlm.sh
```

Then build and run the harness from the project you want the agent to work in:

```sh
cargo build --release
cd /path/to/your/project
/path/to/fast-rlm-agent/target/release/fast-rlm-agent
```

For development, run it directly from this repository with:

```sh
cargo run
```

## Usage

- `Enter` sends a message.
- `Alt+Enter` or `Shift+Enter` inserts a newline.
- `Esc` cancels the active turn or exits when idle.
- `/resume` loads a saved conversation.
- `/undo` restores the local transcript/filesystem checkpoint. Rewinding
  FastRLM's persisted REPL memory is not implemented yet.

Chat sessions, FastRLM REPL state, run logs, and undo snapshots are stored under
`~/.fast-rlm-agent/`.

Before each model turn, the harness extracts HTTP(S) URLs and referenced UTF-8
workspace files from the prompt. The model receives a structured context object:

```json
{
  "prompt": "Compare docs/design.md with https://example.com/spec",
  "links": ["https://example.com/spec"],
  "files": [{"path": "docs/design.md", "content": "..."}]
}
```

Unlike the previous direct-chat implementation, this object is passed to
FastRLM as a real Python dictionary. The root agent can inspect and transform it
inside its REPL without first asking the model to parse a giant JSON string.

Files can be referenced as ordinary paths, backtick paths, `@paths`, or local
Markdown links. Missing, binary, and out-of-workspace files are not loaded.
Links remain structured strings; FastRLM can retrieve them through `fetch_url`.

## Current limitations

FastRLM can now read, write, and edit workspace files and run commands through
the Rust approval boundary. Directory listing, globbing, text search, patch
editing, process-group cancellation, and rewinding FastRLM's REPL state during
`/undo` remain on the roadmap.

## Launch demo

The included launch prompt mixes several URLs and local source files. Run the
harness from this repository, paste the contents of `demo/PROMPT.md`, and watch
FastRLM recursively inspect the sources while the TUI renders its generated
Python, REPL output, child agents, tokens, and cost:

```sh
cargo run --release
```

The repeatable fixtures under `harness-tests/` validate structured context and
coding behavior with hidden verifiers.

## Acknowledgements

FastRLM is the backend engine for this harness. Huge shoutout to its recursive
REPL design, structured I/O, resumable memory, and live event API.

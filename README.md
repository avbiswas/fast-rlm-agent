# fast-rlm-agent

A small terminal coding harness built with Rust and Ratatui. It connects to an
OpenAI-compatible model endpoint, streams responses into the terminal, and lets
the model inspect and modify the current project through reviewed tool calls.

## Features

- Streaming terminal chat interface
- Read, write, and exact-string edit tools
- Shell commands with an approval prompt
- Inline, syntax-highlighted diffs before file changes are applied
- Workspace boundary checks for file operations
- Web search through Exa and direct URL fetching
- Multiple-choice questions from the agent
- Saved sessions with `/resume`
- Per-turn filesystem and conversation checkpoints with `/undo`
- Prompt-cache usage displayed in the UI when the provider reports it
- Structured input preprocessing into `prompt`, `links`, and `files`

## Setup

You need a recent Rust toolchain and an OpenAI-compatible model API.

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

Web search is optional and uses Exa:

```sh
export EXA_API_KEY="your-exa-api-key"
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
- `/undo` restores files and conversation state to an earlier turn.

File writes, edits, and shell commands require approval. File tools are confined
to the directory where the harness was started. Shell commands start in that
directory but are not yet OS-sandboxed, so review commands carefully.

Sessions and undo snapshots are stored under `~/.fast-rlm-agent/`.

Before each model turn, the harness extracts HTTP(S) URLs and referenced UTF-8
workspace files from the prompt. The model receives a structured context object:

```json
{
  "prompt": "Compare docs/design.md with https://example.com/spec",
  "links": ["https://example.com/spec"],
  "files": [{"path": "docs/design.md", "content": "..."}]
}
```

Files can be referenced as ordinary paths, backtick paths, `@paths`, or local
Markdown links. Missing, binary, and out-of-workspace files are not loaded.

## Demo cases

The repository includes small, repeatable coding tasks with hidden verifiers:

```sh
./harness-tests/run.sh list
./harness-tests/run.sh run fix-discount
```

See [harness-tests/README.md](harness-tests/README.md) for the full workflow.

## Acknowledgements

Huge shoutout to [FastRLM](https://github.com/avbiswas/fast-rlm), which provides
the main backend-engine foundation and inspiration for this coding harness.

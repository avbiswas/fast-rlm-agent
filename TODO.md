# fast-rlm-agent TODO

This is the production-readiness backlog for turning the current prototype into
a dependable coding harness. Items are roughly ordered by risk and dependency.

## Safety and containment

- [x] Confine `read`, `write`, and `edit` paths to the startup workspace,
  including canonical symlink checks.
- [x] Start shell commands in the startup workspace.
- [ ] Run shell commands in an OS sandbox that prevents filesystem access
  outside the workspace unless the user grants broader access.
- [ ] Block or require approval for fetches to loopback, link-local, private,
  and otherwise sensitive network addresses.
- [ ] Bind approval to the exact command or file contents executed and clearly
  report when the target changes while approval is pending.
- [ ] Define limits and handling for binary files, large files, and large tool
  results.

## Process execution and cancellation

- [ ] Stream command stdout and stderr into the transcript while commands run.
- [ ] Add command timeouts and configurable output-size limits.
- [ ] Start commands in their own process group and terminate the entire group
  when a turn is cancelled.
- [ ] Track every active tool under its owning turn so cancellation cannot leave
  detached mutations running.
- [ ] Preserve exit status, timeout, cancellation, and signal information in
  both the transcript and model-facing tool result.

## Context and model loop

- [x] Replace the direct response engine with FastRLM and render its live REPL
  steps.
- [x] Persist FastRLM REPL memory across chat turns and resumed sessions.
- [x] Preprocess prompts into structured `prompt`, `links`, and preloaded `files`
  fields compatible with FastRLM dictionary input.
- [x] Bridge reviewed host read, write, exact-edit, and shell tools into FastRLM.
- [ ] Bridge structured user questions into FastRLM.
- [x] Grant sub-agents workspace MCP access by default, via the FastRLM
  `inherit_mcp` / `inherit_tools` config flags (FastRLM 0.4.2).
- [ ] Make `/undo` restore FastRLM REPL state as well as files and transcript.
- [ ] Estimate context usage before each request and show remaining capacity.
- [ ] Compact old conversation history at explicit checkpoints while retaining
  recent raw model/tool rounds.
- [ ] Recover gracefully from provider context-length errors.
- [ ] Validate tool-call arguments strictly instead of replacing missing or
  incorrectly typed values with empty defaults.
- [ ] Decide whether independent parallel tool calls should execute concurrently.
- [ ] Make the maximum tool-round count and request deadlines configurable.

## Provider compatibility

- [ ] Expose FastRLM provider, recursive depth, call, and cost limits through
  CLI/configuration instead of bridge defaults.
- [ ] Add an explicit upgrade check for the pinned FastRLM revision and run the
  bridge contract tests before changing it.
- [ ] Surface FastRLM retry, rate-limit, and provider errors with actionable
  terminal messages.

## Sessions and recovery

- [ ] Filter resumable sessions by workspace and reject or explicitly confirm
  sessions created in a different directory.
- [ ] Version the on-disk session format and add migrations.
- [ ] Surface session save/load failures instead of silently ignoring them.
- [ ] Persist enough turn state to explain interrupted or cancelled work after
  restart.
- [ ] Add retention and cleanup policies for sessions and snapshots.

## Undo reliability

- [ ] Document exactly which files `/undo` captures and restores.
- [ ] Handle ignored files, symlinks, permissions, directories, unusual file
  names, nested repositories, and large binaries explicitly.
- [ ] Verify the restored worktree after checkout and report partial failures.
- [ ] Prevent secrets or unsuitable files from being retained indefinitely in
  shadow repositories.
- [ ] Test undo behavior after cancellation and process crashes.

## Developer and user experience

- [x] Add a README with installation, configuration, key bindings, tool safety,
  session storage, and undo semantics.
- [ ] Add CLI flags and a documented configuration file.
- [ ] Add configurable approval policies.
- [ ] Add directory listing, glob, and text-search tools.
- [ ] Add a patch-based editing tool for changes that do not fit exact-string
  replacement.
- [x] Add non-interactive/headless operation (`--headless`).
- [ ] Add structured diagnostic logging and an opt-in debug view.
- [ ] Add packaging, release, and installation workflows.
- [x] Keep the project clean under `cargo fmt`, `cargo test`, and strict Clippy.

## End-to-end verification

- [x] Add simple repeatable demo cases with isolated workspaces and hidden
  verifiers.
- [ ] Build a local fake OpenAI-compatible SSE server for deterministic tests.
- [ ] Test the complete tool-call, approval, execution, result, and follow-up
  response flow.
- [ ] Test cancellation of long-running commands and their child processes.
- [ ] Test workspace escape attempts through relative paths, absolute paths,
  symlinks, shell commands, and network fetches.
- [ ] Test session resume and filesystem undo across process restarts.
- [ ] Run opt-in compatibility tests against each supported provider.

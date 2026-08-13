# Harness test cases

These cases are small coding tasks for demos and manual quality checks. Each
case contains:

- a task copied into the workspace as `TASK.md`;
- a minimal starter project;
- a verifier that stays outside the agent workspace;
- a reference solution used only to test the case itself.

List the available cases:

```sh
./harness-tests/run.sh list
```

Run a case interactively:

```sh
./harness-tests/run.sh run fix-discount
```

The runner builds the harness, creates a clean temporary workspace, prints the
prompt to paste, and opens the TUI there. Simple coding cases use:

```text
Complete the task in TASK.md. Run the tests and fix any failures.
```

Exit the harness when it is done. The hidden verifier runs automatically and
prints `PASS` or `FAIL`. The temporary workspace is retained so you can inspect
the result.

You can also prepare and verify workspaces separately:

```sh
workspace=$(./harness-tests/run.sh prepare implement-slugify)
./harness-tests/run.sh verify implement-slugify "$workspace"
```

Validate the definitions, baseline failures, and reference solutions for every
case with:

```sh
./harness-tests/run.sh self-test
```

The cases require Python 3 but no third-party Python packages.

`research-rust-urls` requires network access because the agent fetches its
sources. `summarize-release-files` is fully local and demonstrates several file
contents arriving in structured context.

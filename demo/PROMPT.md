Act as a skeptical release engineer. Inspect all of these local files:

- `README.md`
- `TODO.md`
- `src/agent.rs`
- `scripts/fast_rlm_bridge.py`

Then fetch and inspect these upstream FastRLM sources:

- https://github.com/avbiswas/fast-rlm
- https://raw.githubusercontent.com/avbiswas/fast-rlm/main/pyproject.toml
- https://raw.githubusercontent.com/avbiswas/fast-rlm/main/README.md

Produce an eye-catching launch brief in the terminal. Include:

1. a compact architecture map showing how this Rust TUI uses FastRLM;
2. a “real today vs. next” table grounded in the local code and TODO;
3. three concrete examples of how structured input and persistent REPL memory
   improve a multi-turn investigation;
4. the top three launch risks, ordered by severity; and
5. a final SHIP / SHIP WITH CAVEATS / DO NOT SHIP verdict with one-sentence
   justification.

Use citations as URLs beside claims about upstream FastRLM. Do not modify files.

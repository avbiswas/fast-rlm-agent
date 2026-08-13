#!/usr/bin/env bash
set -eu

repo_dir=$(cd "$(dirname "$0")/.." && pwd)

command -v uv >/dev/null 2>&1 || {
  echo "uv is required: https://docs.astral.sh/uv/" >&2
  exit 1
}
command -v deno >/dev/null 2>&1 || {
  echo "Deno 2+ is required: https://deno.com/" >&2
  exit 1
}

if [[ ! -x "$repo_dir/.venv/bin/python" ]]; then
  uv venv "$repo_dir/.venv" --python 3.13
fi
uv pip install \
  --python "$repo_dir/.venv/bin/python" \
  -r "$repo_dir/requirements-fast-rlm.txt"

"$repo_dir/.venv/bin/python" -c \
  'import importlib.metadata as m; print("FastRLM", m.version("fast-rlm"), "installed")'

#!/usr/bin/env bash
set -u

suite_dir=$(cd "$(dirname "$0")" && pwd)
repo_dir=$(cd "$suite_dir/.." && pwd)
cases_dir="$suite_dir/cases"

usage() {
  echo "usage: $0 {list|prepare|run|verify|self-test} [case] [workspace]" >&2
  exit 2
}

case_dir() {
  local name=$1
  local dir="$cases_dir/$name"
  if [[ ! -d "$dir/fixture" || ! -f "$dir/task.md" || ! -f "$dir/verify.py" ]]; then
    echo "unknown or incomplete case: $name" >&2
    exit 2
  fi
  printf '%s\n' "$dir"
}

prepare_case() {
  local name=$1
  local destination=${2:-}
  local source
  source=$(case_dir "$name")
  if [[ -z "$destination" ]]; then
    destination=$(mktemp -d "${TMPDIR:-/tmp}/fast-rlm-${name}.XXXXXX")
  elif [[ -e "$destination" ]]; then
    echo "destination already exists: $destination" >&2
    exit 2
  else
    mkdir -p "$destination"
  fi
  cp -R "$source/fixture/." "$destination/"
  cp "$source/task.md" "$destination/TASK.md"
  if [[ -f "$source/prompt.txt" ]]; then
    cp "$source/prompt.txt" "$destination/PROMPT.txt"
  fi
  printf '%s\n' "$destination"
}

verify_case() {
  local name=$1
  local workspace=$2
  local source
  source=$(case_dir "$name")
  python3 "$source/verify.py" "$workspace"
}

command=${1:-}
case "$command" in
  list)
    for dir in "$cases_dir"/*; do
      [[ -d "$dir" ]] || continue
      name=$(basename "$dir")
      summary=$(head -n 1 "$dir/task.md" | sed 's/^# *//')
      printf '%-22s %s\n' "$name" "$summary"
    done
    ;;
  prepare)
    [[ $# -ge 2 && $# -le 3 ]] || usage
    prepare_case "$2" "${3:-}"
    ;;
  verify)
    [[ $# -eq 3 ]] || usage
    verify_case "$2" "$3"
    ;;
  run)
    [[ $# -eq 2 ]] || usage
    workspace=$(prepare_case "$2")
    cargo build --quiet --manifest-path "$repo_dir/Cargo.toml" || exit 1
    echo "Workspace: $workspace"
    source=$(case_dir "$2")
    echo "Paste this prompt into the harness:"
    echo
    if [[ -f "$source/prompt.txt" ]]; then
      sed -n '1,240p' "$source/prompt.txt"
    else
      echo "Complete the task in TASK.md. Run the tests and fix any failures."
    fi
    echo
    echo "Press Enter to open the harness."
    read -r
    (
      cd "$workspace" || exit 1
      "$repo_dir/target/debug/fast-rlm-agent"
    )
    status=$?
    echo
    if [[ $status -ne 0 ]]; then
      echo "Harness exited with status $status" >&2
    fi
    verify_case "$2" "$workspace"
    ;;
  self-test)
    [[ $# -eq 1 ]] || usage
    failed=0
    for dir in "$cases_dir"/*; do
      [[ -d "$dir" ]] || continue
      name=$(basename "$dir")
      workspace=$(prepare_case "$name")
      if verify_case "$name" "$workspace" >/dev/null 2>&1; then
        echo "FAIL $name: starter fixture unexpectedly passes" >&2
        failed=1
      fi
      cp -R "$dir/solution/." "$workspace/"
      if verify_case "$name" "$workspace" >/dev/null; then
        echo "PASS $name"
      else
        echo "FAIL $name: reference solution failed" >&2
        failed=1
      fi
      rm -rf "$workspace"
    done
    exit "$failed"
    ;;
  *)
    usage
    ;;
esac

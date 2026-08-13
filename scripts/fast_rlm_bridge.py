"""NDJSON bridge between the Rust TUI and FastRLM's Python API.

The request is read as one JSON object from stdin. Protocol events are written
one-per-line to the original stdout; incidental FastRLM output is redirected to
stderr so it cannot corrupt the stream.
"""

from __future__ import annotations

import contextlib
import json
import os
import sys
import traceback
from typing import Any, Callable


def emit(stream, kind: str, **payload: Any) -> None:
    stream.write(json.dumps({"kind": kind, **payload}, ensure_ascii=False) + "\n")
    stream.flush()


def fetch_url(url: str, max_chars: int = 50_000) -> str:
    """Fetch an HTTP(S) URL and return at most max_chars characters."""
    import urllib.request

    request = urllib.request.Request(
        url,
        headers={"User-Agent": "fast-rlm-agent/0.1"},
    )
    with urllib.request.urlopen(request, timeout=20) as response:
        charset = response.headers.get_content_charset() or "utf-8"
        return response.read().decode(charset, errors="replace")[:max_chars]


def run_request(
    request: dict[str, Any],
    fast_rlm_module,
    send: Callable[..., None],
) -> None:
    model = request["model"]
    config = {
        "primary_agent": model,
        "sub_agent": request.get("sub_agent") or model,
        "max_depth": request.get("max_depth", 3),
        "max_calls_per_subagent": request.get("max_calls", 20),
        "max_money_spent": request.get("max_money_spent", 0.2),
    }

    def on_step(event: dict[str, Any]) -> None:
        send("rlm_event", event=event)

    result = fast_rlm_module.run(
        request["context"],
        config=config,
        verbosity="silent",
        on_step=on_step,
        tools=[fetch_url],
        mcp_servers={
            "workspace": {
                "command": "deno",
                "args": [
                    "run",
                    "--quiet",
                    "--allow-env=FAST_RLM_AGENT_BROKER_URL,FAST_RLM_AGENT_BROKER_TOKEN",
                    "--allow-net=127.0.0.1",
                    request["workspace_mcp_script"],
                ],
                "env": {
                    "FAST_RLM_AGENT_BROKER_URL": request["broker_url"],
                    "FAST_RLM_AGENT_BROKER_TOKEN": request["broker_token"],
                },
            },
        },
        log_dir=request["log_dir"],
        session_dir=request["session_dir"],
        session_id=request["session_id"],
        instruction=request.get("instruction"),
    )
    send(
        "complete",
        result=result.get("results"),
        usage=result.get("usage") or {},
        log_file=result.get("log_file"),
    )


def main() -> int:
    protocol = os.fdopen(os.dup(sys.stdout.fileno()), "w", encoding="utf-8")

    def send(kind: str, **payload: Any) -> None:
        emit(protocol, kind, **payload)

    try:
        request = json.load(sys.stdin)
        import fast_rlm

        with contextlib.redirect_stdout(sys.stderr):
            run_request(request, fast_rlm, send)
        return 0
    except Exception as error:
        traceback.print_exc(file=sys.stderr)
        send("error", message=f"{type(error).__name__}: {error}")
        return 1
    finally:
        protocol.close()


if __name__ == "__main__":
    raise SystemExit(main())

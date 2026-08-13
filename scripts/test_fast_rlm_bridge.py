import importlib.util
import pathlib
import unittest


BRIDGE_PATH = pathlib.Path(__file__).with_name("fast_rlm_bridge.py")
SPEC = importlib.util.spec_from_file_location("fast_rlm_bridge", BRIDGE_PATH)
BRIDGE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(BRIDGE)


class FakeFastRlm:
    def __init__(self):
        self.query = None
        self.kwargs = None

    def run(self, query, **kwargs):
        self.query = query
        self.kwargs = kwargs
        kwargs["on_step"](
            {
                "event_type": "execution_result",
                "run_id": "root",
                "depth": 0,
                "step": 1,
                "code": "print(len(context['files']))",
                "output": "2",
                "hasError": False,
                "usage": {"total_tokens": 10},
            }
        )
        return {
            "results": {"answer": 2},
            "usage": {"total_tokens": 10, "cost": 0.01},
            "log_file": "/tmp/run.jsonl",
        }


class BridgeTests(unittest.TestCase):
    def test_passes_real_dictionary_and_streams_events(self):
        fake = FakeFastRlm()
        sent = []
        request = {
            "context": {
                "prompt": "compare",
                "links": ["https://example.com"],
                "files": [
                    {"path": "a.txt", "content": "a"},
                    {"path": "b.txt", "content": "b"},
                ],
            },
            "model": "test-model",
            "session_dir": "/tmp/sessions",
            "session_id": "demo",
            "log_dir": "/tmp/logs",
            "broker_url": "http://127.0.0.1:1234/tool",
            "broker_token": "test-token",
            "workspace_mcp_script": "/tmp/workspace_mcp.ts",
        }

        BRIDGE.run_request(
            request,
            fake,
            lambda kind, **payload: sent.append({"kind": kind, **payload}),
        )

        self.assertIs(fake.query, request["context"])
        self.assertEqual(fake.kwargs["config"]["primary_agent"], "test-model")
        self.assertEqual(fake.kwargs["session_id"], "demo")
        self.assertEqual(fake.kwargs["tools"][0].__name__, "fetch_url")
        self.assertEqual([tool.__name__ for tool in fake.kwargs["tools"]], ["fetch_url"])
        self.assertEqual(
            fake.kwargs["mcp_servers"]["workspace"]["env"]["FAST_RLM_AGENT_BROKER_TOKEN"],
            "test-token",
        )
        self.assertEqual(sent[0]["kind"], "rlm_event")
        self.assertEqual(sent[1]["kind"], "complete")
        self.assertEqual(sent[1]["result"], {"answer": 2})


if __name__ == "__main__":
    unittest.main()

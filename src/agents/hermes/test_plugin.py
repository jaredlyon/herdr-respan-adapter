"""Synthetic Hermes observer contract checks; no Hermes/provider/config access."""
import json
import os
from pathlib import Path
import threading
import types
import unittest
from unittest.mock import patch, Mock

ROOT = Path(__file__).parent


class PluginTests(unittest.TestCase):
    def setUp(self):
        self.module = types.ModuleType("herdr_respan_test")
        source = (ROOT / "assets/herdr-respan/__init__.py").read_text()
        for name, value in (("BIN", "/synthetic/bin/herdr-respan"), ("STATE", "/synthetic/state"), ("CONFIG", "/synthetic/config")):
            source = source.replace(f"__HERDR_RESPAN_{name}__", repr(value))
        exec(compile(source, "herdr-respan/__init__.py", "exec"), self.module.__dict__)
        self.hooks = {}
        self.env = patch.dict(os.environ, {"HERDR_ENV": "1", "HERDR_RESPAN": "on", "HERDR_RESPAN_PROXY": "0"})
        self.env.start()
        self.addCleanup(self.env.stop)
        self.module.register(types.SimpleNamespace(register_hook=lambda n, f: self.hooks.__setitem__(n, f)))
        self.module._VERSION = None
        self.sent = []
        self.dispatch = patch.object(self.module, "_dispatch", side_effect=lambda b: self.sent.append(json.loads(json.dumps(b))))
        self.dispatch.start()
        self.addCleanup(self.dispatch.stop)
        self.clock = patch.object(self.module.time, "time_ns", return_value=1_000_000_000)
        self.clock.start()
        self.addCleanup(self.clock.stop)
        self.cwd = patch.object(self.module.os, "getcwd", return_value="/synthetic/repo")
        self.cwd.start()
        self.addCleanup(self.cwd.stop)

    def call(self, name, **kwargs):
        kwargs.setdefault("telemetry_schema_version", "hermes.observer.v1")
        self.assertIsNone(self.hooks[name](**kwargs))

    def start(self, **overrides):
        self.call("pre_llm_call", **{"session_id": "s", "turn_id": "t", "platform": "cli", **overrides})

    def end(self, **overrides):
        self.call("on_session_end", **{"session_id": "s", "turn_id": "t", "platform": "cli", "completed": True, **overrides})

    def test_exact_emitted_batch_and_registration(self):
        for name, kwargs in json.loads((ROOT / "fixtures/hooks.json").read_text()):
            self.call(name, **kwargs)
        expected = json.loads((ROOT / "fixtures/batch.json").read_text())
        self.assertEqual(self.sent, [expected])
        self.call("on_session_end", session_id="parent-session", turn_id="turn-1", platform="cli", completed=True)
        self.assertEqual(len(self.sent), 1)
        manifest_hooks = [line.strip()[2:] for line in (ROOT / "assets/herdr-respan/plugin.yaml").read_text().splitlines() if line.strip().startswith("- ")]
        self.assertEqual(set(manifest_hooks), set(self.hooks))
        self.assertNotIn("on_session_start", self.hooks)

    def test_gate_and_foreign_platforms(self):
        for platform in ("discord", "gateway", "telegram", "subagent", "", None):
            self.start(platform=platform)
        self.start(telemetry_schema_version="other")
        self.assertFalse(self.module._TURNS)
        with patch.dict(os.environ, {"HERDR_ENV": "0"}):
            self.start()
            ctx = Mock()
            self.module.register(ctx)
            ctx.register_hook.assert_not_called()
        with patch.dict(os.environ, {"HERDR_RESPAN": "off"}):
            self.start()
        self.assertFalse(self.module._TURNS)
        self.call("subagent_start", parent_session_id="missing", parent_turn_id="t", child_session_id="child")
        self.assertFalse(self.module._CHILDREN)
        self.start()
        self.call("post_tool_call", session_id="s", turn_id="t", platform="discord", tool_call_id="foreign", tool_name="bash")
        self.end()
        self.assertEqual([e["type"] for e in self.sent[0]["events"]], ["turn_start", "turn_end"])

    def test_threaded_tools_without_platform_and_child_after_parent_end(self):
        self.start()
        self.call("pre_tool_call", session_id="s", turn_id="t", tool_call_id="spawn", tool_name="delegate_task")
        self.call("subagent_start", parent_session_id="s", parent_turn_id="t", child_session_id="c", child_subagent_id="sub")
        self.end()
        self.assertFalse(self.sent)
        def tool(i):
            self.call("pre_tool_call", session_id="c", turn_id="ct", tool_call_id=str(i), tool_name="bash", args={"command": "echo example"})
            self.call("post_tool_call", session_id="c", turn_id="ct", tool_call_id=str(i), tool_name="bash", result="example", duration_ms=0, status="ok")
        workers = [threading.Thread(target=tool, args=(i,)) for i in range(20)]
        for worker in workers:
            worker.start()
        for worker in workers:
            worker.join()
        self.call("subagent_stop", parent_session_id="s", parent_turn_id="t", child_session_id="c", child_status="ok")
        self.assertEqual(len(self.sent), 1)
        tools = [e for e in self.sent[0]["events"] if e["type"] == "tool"]
        self.assertEqual(len(tools), 20)
        self.assertTrue(all(e["parent_id"] == "sub" and e["turn_id"] == "t" for e in tools))
        self.assertFalse(self.module._TURNS)
        self.assertFalse(self.module._CHILDREN)

    def test_failed_auxiliary_unknown_usage_and_proxy_suppression(self):
        self.start()
        self.call("post_auxiliary_call", session_id="s", turn_id="t", platform="cli", api_request_id="aux", aux_task="summary", started_at=1, ended_at=1, error="TimeoutError: example", usage=None)
        for request, env, base_url in (("proxy", {"HERDR_RESPAN_PROXY": "1"}, ""), ("respan", {}, "https://api.respan.ai/v1")):
            with patch.dict(os.environ, env):
                self.call("post_api_request", session_id="s", turn_id="t", platform="cli", api_request_id=request, base_url=base_url, started_at=1, ended_at=1)
        self.end(interrupted=True, completed=False)
        chats = [e for e in self.sent[0]["events"] if e["type"] == "chat"]
        self.assertEqual(len(chats), 1)
        self.assertIsNone(chats[0]["usage"])
        self.assertEqual(chats[0]["status"], "error")
        self.assertEqual(self.sent[0]["events"][-1]["status"], "error")

    def test_tower_no_auxiliary_and_sparse_shutdown(self):
        hooks = json.loads((ROOT / "fixtures/hooks.json").read_text())
        for name, kwargs in hooks:
            if "auxiliary" not in name and name not in ("on_session_end", "post_llm_call"):
                self.call(name, **kwargs)
        self.call("on_session_end", session_id="parent-session", platform="cli", completed=False, interrupted=True, reason="shutdown")
        self.assertEqual(len(self.sent), 1)
        self.assertFalse(any(e.get("aux_task") for e in self.sent[0]["events"]))
        self.assertEqual(self.sent[0]["events"][-1]["error"], "shutdown")
        self.assertEqual(self.sent[0]["events"][-1]["status"], "error")
        self.call("on_session_finalize", session_id="parent-session", platform="cli")
        self.assertEqual(len(self.sent), 1)

    def test_sanitized_requests_and_truncation_markers(self):
        self.start()
        marker = {"_truncated": True, "preview": "example"}
        self.call("pre_api_request", session_id="s", turn_id="t", platform="cli", api_request_id="trunc", request=marker)
        self.call("post_api_request", session_id="s", turn_id="t", platform="cli", api_request_id="trunc", response=marker)
        self.call("pre_api_request", session_id="s", turn_id="t", platform="cli", api_request_id="responses", request={"body": {"input": "example", "instructions": "system"}})
        self.call("post_api_request", session_id="s", turn_id="t", platform="cli", api_request_id="responses")
        self.end()
        chats = [e for e in self.sent[0]["events"] if e["type"] == "chat"]
        self.assertEqual(chats[0]["input"], marker)
        self.assertEqual(chats[0]["output"], marker)
        self.assertEqual(chats[1]["input"], [{"role": "system", "content": "system"}, {"role": "user", "content": "example"}])

    def test_handoff_is_background_and_windows_uses_no_window(self):
        self.dispatch.stop()
        entered, release, done = threading.Event(), threading.Event(), threading.Event()
        def communicate(data):
            entered.set()
            release.wait(2)
            self.assertEqual(json.loads(data)["agent"], "hermes")
            done.set()
        process = Mock(communicate=Mock(side_effect=communicate))
        with patch.object(self.module.subprocess, "Popen", return_value=process) as popen:
            self.start()
            self.end()  # Returns while communicate remains blocked on another thread.
            self.assertTrue(entered.wait(2))
            self.assertFalse(done.is_set())
            release.set()
            self.assertTrue(done.wait(2))
            self.assertEqual(popen.call_count, 1)
            self.assertEqual(popen.call_args.args[0], ["/synthetic/bin/herdr-respan", "hook", "hermes", "--state-dir", "/synthetic/state", "--config-dir", "/synthetic/config"])
        with patch.object(self.module.os, "name", "nt"), patch.object(self.module.subprocess, "CREATE_NO_WINDOW", 0x08000000, create=True), patch.object(self.module.subprocess, "Popen", return_value=Mock()) as popen:
            self.module._send({"v": 1, "agent": "hermes"})
            self.assertEqual(popen.call_args.kwargs["creationflags"], 0x08000000)
            self.assertNotIn("start_new_session", popen.call_args.kwargs)


if __name__ == "__main__":
    unittest.main()

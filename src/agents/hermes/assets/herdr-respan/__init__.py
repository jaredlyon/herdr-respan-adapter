"""Hermes observers translate kwargs only; Rust owns redaction and delivery."""
import atexit
import json
import os
import subprocess
import threading
import time

_BIN = __HERDR_RESPAN_BIN__
_STATE = __HERDR_RESPAN_STATE__
_CONFIG = __HERDR_RESPAN_CONFIG__
_INTERACTIVE = {"cli", "tui", "desktop", "acp"}
_LOCK = threading.RLock()
_TURNS = {}
_CHILDREN = {}
_CLOSED = {}  # Last completed turn per native session, for late auxiliary observers.
_AUX_PENDING = {}


def _enabled():
    return os.environ.get("HERDR_ENV") == "1" and os.environ.get("HERDR_RESPAN", "").lower() != "off"


def _ns(seconds=None):
    return time.time_ns() if seconds is None else max(0, int(float(seconds) * 1_000_000_000))


def _send(batch):
    # Serialization, pipe backpressure and child reaping never run on Hermes' hook thread.
    try:
        data = json.dumps(batch, ensure_ascii=False).encode("utf-8")
        kwargs = {"stdin": subprocess.PIPE, "stdout": subprocess.DEVNULL,
                  "stderr": subprocess.DEVNULL}
        if os.name == "nt":
            kwargs["creationflags"] = subprocess.CREATE_NO_WINDOW
        else:
            kwargs["start_new_session"] = True
        child = subprocess.Popen([_BIN, "hook", "hermes", "--state-dir", _STATE,
                                  "--config-dir", _CONFIG], **kwargs)
        child.communicate(data)
    except Exception:
        pass


def _take_aux():
    with _LOCK:
        batches = list(_AUX_PENDING.values())
        _AUX_PENDING.clear()
        return batches


def _send_all(batches):
    for batch in batches:
        _send(batch)


def _dispatch(batch):
    batches = [batch] + _take_aux()
    threading.Thread(target=_send_all, args=(batches,), name="herdr-respan-handoff", daemon=True).start()


def _exit_handoff():
    if _enabled():
        _send_all(_take_aux())


atexit.register(_exit_handoff)


def _location(k):
    sid, tid = k.get("session_id"), k.get("turn_id")
    platform = k.get("platform")
    if sid in _CHILDREN:
        if platform not in (None, "", "subagent"):
            return None
        root, sub = _CHILDREN[sid]
        return (root, sub) if root in _TURNS else None
    key = (sid, tid)
    if not sid or not tid or key not in _TURNS:
        return None
    if platform is not None and platform not in _INTERACTIVE:
        return None
    return key, None


def _append(turn, kind, **fields):
    turn["batch"]["events"].append({"type": kind, "turn_id": turn.get("turn_id") or turn["batch"]["events"][0]["turn_id"], **fields})


def _finish(key, turn):
    if not turn["ended"] or turn["children"]:
        return None
    _append(turn, "turn_end", ts_ns=turn["end_ns"], output=turn.get("output"),
            status=turn["status"], error=turn.get("error"))
    _CLOSED[key[0]] = {"turn_id": key[1], "requests": turn["requests"],
                       "batch": dict(turn["batch"], events=[])}
    del _TURNS[key]
    for sid, (root, _) in list(_CHILDREN.items()):
        if root == key:
            del _CHILDREN[sid]
    return turn["batch"]


def _request_input(k):
    request = k.get("request")
    if not isinstance(request, dict):
        return request
    body = request.get("body")
    if not isinstance(body, dict):
        return request  # Keep Hermes' explicit truncation markers.
    messages = body.get("messages", body.get("input"))
    system = body.get("system", body.get("instructions", k.get("system_prompt")))
    if messages is None:
        return request
    if not isinstance(messages, list):
        messages = [{"role": "user", "content": messages}]
    if system and not any(isinstance(m, dict) and m.get("role") in ("system", "developer") for m in messages):
        messages = [{"role": "system", "content": system}] + messages
    return messages


def _response_output(k):
    response = k.get("response")
    if isinstance(response, dict) and "assistant_message" in response:
        return [response["assistant_message"]]
    return response


def _observe(name, k):
    batch = None
    with _LOCK:
        if name == "pre_llm_call":
            sid, tid = k.get("session_id"), k.get("turn_id")
            if not sid or not tid or sid in _CHILDREN or k.get("platform") not in _INTERACTIVE:
                return
            key = (sid, tid)
            if key not in _TURNS:
                _TURNS[key] = {"batch": {"v": 1, "agent": "hermes", "session_id": sid,
                    "cwd": os.getcwd(), "agent_version": _VERSION, "events": [
                    {"type": "turn_start", "turn_id": tid, "ts_ns": _ns(), "prompt": k.get("user_message")}]},
                    "requests": {}, "tools": {}, "children": set(), "ended": False, "status": "ok"}
            return
        if name in ("subagent_start", "subagent_stop"):
            parent = {"session_id": k.get("parent_session_id"), "turn_id": k.get("parent_turn_id")}
            location = _location(parent)
            if location is None:
                return
            key, _ = location
            turn = _TURNS[key]
            sid = k.get("child_session_id")
            if not sid:
                return
            if name == "subagent_start":
                if sid in _CHILDREN:
                    return
                sub = k.get("child_subagent_id") or sid
                # Observer payload lacks the spawning tool id. Use it only when unambiguous.
                tools = [i for (s, _t, i), v in turn["tools"].items()
                         if s == parent["session_id"] and _t == parent["turn_id"] and v.get("tool_name") == "delegate_task"]
                _CHILDREN[sid] = (key, sub)
                turn["children"].add(sid)
                _append(turn, "subagent_start", id=sub, parent_id=tools[0] if len(tools) == 1 else None,
                        ts_ns=_ns(), name=k.get("child_role"))
            elif sid in turn["children"]:
                sub = _CHILDREN[sid][1]
                _append(turn, "subagent_end", id=sub, ts_ns=_ns(), output=k.get("child_summary"),
                        status="ok" if k.get("child_status") in (None, "ok", "completed", "success") else "error")
                turn["children"].remove(sid)
                batch = _finish(key, turn)
        elif name in ("on_session_finalize", "on_session_reset") or (name == "on_session_end" and not k.get("turn_id")):
            if k.get("platform") not in _INTERACTIVE:
                return
            for key in list(_TURNS):
                if key[0] == k.get("session_id"):
                    turn = _TURNS[key]
                    turn.update(ended=True, end_ns=_ns(), status="error", error=k.get("reason") or "session ended without turn completion")
                    # Sparse shutdown is authoritative: close unfinished child lifetimes too.
                    for sid in list(turn["children"]):
                        _append(turn, "subagent_end", id=_CHILDREN[sid][1], ts_ns=_ns(), status="error")
                    turn["children"].clear()
                    finished = _finish(key, turn)
                    if finished:
                        _dispatch(finished)
        else:
            location = _location(k)
            late_aux = False
            if location is None:
                closed = _CLOSED.get(k.get("session_id"))
                if (name not in ("pre_auxiliary_call", "post_auxiliary_call") or not closed
                        or k.get("turn_id") != closed["turn_id"] or k.get("platform") not in _INTERACTIVE):
                    return
                key, parent_id = (k["session_id"], closed["turn_id"]), None
                turn, late_aux = closed, True
            else:
                key, parent_id = location
                turn = _TURNS[key]
            if name == "post_llm_call":
                if parent_id is None:
                    turn["output"] = k.get("assistant_response")
                # on_session_end follows even failed/interrupted turns and includes final status.
            elif name == "on_session_end":
                if parent_id is not None:
                    return  # Child completion is subagent_stop, not a second trace/turn.
                failed = k.get("failed") or k.get("interrupted") or k.get("completed") is False
                turn.update(ended=True, end_ns=_ns(), status="error" if failed else "ok",
                            error=k.get("turn_exit_reason") if failed else None)
                batch = _finish(key, turn)
            elif name in ("pre_api_request", "pre_auxiliary_call"):
                rid = k.get("api_request_id")
                if rid:
                    turn["requests"][(k.get("session_id"), rid)] = {
                        "input": _request_input(k), "start_ns": _ns(k.get("started_at")), "base_url": k.get("base_url")}
            elif name in ("post_api_request", "api_request_error", "post_auxiliary_call"):
                rid = k.get("api_request_id")
                if not rid:
                    return
                pre = turn["requests"].pop((k.get("session_id"), rid), {})
                if os.environ.get("HERDR_RESPAN_PROXY") == "1" or "api.respan.ai" in str(k.get("base_url") or pre.get("base_url") or ""):
                    return
                end = _ns(k.get("ended_at"))
                start = pre.get("start_ns", _ns(k.get("started_at")))
                usage = k.get("usage")
                if isinstance(usage, dict):
                    total_input = usage.get("prompt_tokens")
                    if total_input is None:
                        buckets = [usage.get(n) for n in ("input_tokens", "cache_read_tokens", "cache_write_tokens")]
                        total_input = sum(buckets) if all(isinstance(v, int) for v in buckets) else usage.get("input_tokens")
                    usage = {"input_tokens": total_input, "output_tokens": usage.get("output_tokens"),
                             "cache_read_input_tokens": usage.get("cache_read_tokens"),
                             "cache_creation_input_tokens": usage.get("cache_write_tokens"),
                             "reasoning_tokens": usage.get("reasoning_tokens")}
                error = k.get("error")
                if isinstance(error, dict):
                    error = ": ".join(str(error[n]) for n in ("type", "message") if error.get(n))
                _append(turn, "chat", id=rid, parent_id=parent_id, start_ns=start, end_ns=max(start, end),
                        input=pre.get("input", _request_input(k)), output=_response_output(k),
                        model=k.get("response_model") or k.get("model"), provider=k.get("provider"), usage=usage,
                        aux_task=k.get("aux_task") if name == "post_auxiliary_call" else None,
                        status="error" if error or name == "api_request_error" else "ok", error=error)
                if late_aux:
                    pending = _AUX_PENDING.setdefault(key, dict(turn["batch"], events=[]))
                    pending["events"].extend(turn["batch"]["events"])
                    turn["batch"]["events"] = []
            elif name == "pre_tool_call":
                tid = k.get("tool_call_id")
                if tid:
                    turn["tools"][(k.get("session_id"), k.get("turn_id"), tid)] = {"start_ns": _ns(), "tool_name": k.get("tool_name")}
            elif name == "post_tool_call":
                tid = k.get("tool_call_id")
                if not tid:
                    return
                end = _ns()
                pre = turn["tools"].pop((k.get("session_id"), k.get("turn_id"), tid), {})
                duration = k.get("duration_ms")
                start = pre.get("start_ns", end - int(float(duration) * 1_000_000) if duration is not None else end)
                _append(turn, "tool", id=tid, parent_id=parent_id, name=k.get("tool_name"), start_ns=max(0, start),
                        end_ns=max(start, end), input=k.get("args"), output=k.get("result"),
                        status="ok" if k.get("status") in (None, "ok") else "error", error=k.get("error_message"))
    if batch:
        _dispatch(batch)


_VERSION = None

def register(ctx):
    global _VERSION
    if not _enabled():
        return
    try:
        from hermes_cli import __version__
        _VERSION = __version__
    except ImportError:
        pass
    for name in ("pre_llm_call", "post_llm_call", "on_session_end", "on_session_finalize", "on_session_reset",
                 "pre_api_request", "post_api_request", "api_request_error", "pre_tool_call", "post_tool_call",
                 "subagent_start", "subagent_stop", "pre_auxiliary_call", "post_auxiliary_call"):
        def callback(_name=name, **kwargs):
            try:
                if _enabled() and kwargs.get("telemetry_schema_version") == "hermes.observer.v1":
                    _observe(_name, kwargs)
            except Exception:
                pass  # Including pre_tool_call: never return a permission/block directive.
            return None
        ctx.register_hook(name, callback)

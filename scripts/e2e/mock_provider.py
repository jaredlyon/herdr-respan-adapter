#!/usr/bin/env python3
"""Loopback-only deterministic provider; no upstream requests or dependencies."""
import argparse
import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlsplit

TEXT = "hra-e2e mock response"
USAGE = {"input_tokens": 11, "output_tokens": 7, "cache_read_input_tokens": 3, "cache_creation_input_tokens": 0}


def scenario(body):
    conversation = body.get("messages", body.get("input", body.get("contents", [])))
    text = json.dumps(conversation)
    tool_result = any(marker in text for marker in ('"tool_result"', '"function_call_output"', '"functionResponse"', '"role": "tool"'))
    return "RUN_TOOL" in text and not tool_result


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def record(self, body):
        headers = {k: "[REDACTED]" if any(word in k.lower() for word in ("auth", "key", "cookie")) else v for k, v in self.headers.items()}
        # Query API keys must not leak through Gemini URLs either.
        path = urlsplit(self.path).path
        with self.server.lock, self.server.log.open("a") as log:
            log.write(json.dumps({"method": self.command, "path": path, "headers": headers, "body": body}) + "\n")
            self.server.sequence += 1
            return str(self.server.sequence)

    def reply(self, body, status=200):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def stream(self, events):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        for event, data in events:
            if event:
                self.wfile.write(f"event: {event}\n".encode())
            self.wfile.write(("data: " + (data if isinstance(data, str) else json.dumps(data)) + "\n\n").encode())
            self.wfile.flush()
        self.close_connection = True

    def do_GET(self):
        self.record(None)
        if self.path == "/healthz":
            self.reply({"ok": True})
        elif "models" in self.path:
            self.reply({"object": "list", "data": [{"id": "claude-haiku-4-5-20251001", "type": "model", "object": "model", "display_name": "Mock Haiku", "created_at": "2025-10-01T00:00:00Z"}, {"id": "mock-model", "object": "model"}], "has_more": False, "models": [{"name": "models/mock-model", "supportedGenerationMethods": ["generateContent"]}]})
        else:
            self.reply({"error": "unsupported mock route"}, 404)

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))) or b"{}")
        number = self.record(body)
        path = urlsplit(self.path).path
        tool = scenario(body)
        model = body.get("model", "mock-model")
        if "count_tokens" in path or "countTokens" in path:
            self.reply({"input_tokens": 11, "totalTokens": 11})
        elif path.endswith("/messages"):
            content = {"type": "tool_use", "id": "toolu_hra_" + number, "name": "Bash", "input": {"command": "echo hra-e2e", "description": "Harmless mock E2E echo"}} if tool else {"type": "text", "text": TEXT}
            message = {"id": "msg_hra_" + number, "type": "message", "role": "assistant", "model": model, "content": [content], "stop_reason": "tool_use" if tool else "end_turn", "stop_sequence": None, "usage": USAGE}
            if not body.get("stream"):
                self.reply(message)
                return
            start = dict(message, content=[], stop_reason=None, usage=dict(USAGE, output_tokens=0))
            block = dict(content, input={}) if tool else dict(content, text="")
            delta = {"type": "input_json_delta", "partial_json": json.dumps(content["input"])} if tool else {"type": "text_delta", "text": TEXT}
            events = [("message_start", {"type": "message_start", "message": start}), ("content_block_start", {"type": "content_block_start", "index": 0, "content_block": block}), ("content_block_delta", {"type": "content_block_delta", "index": 0, "delta": delta}), ("content_block_stop", {"type": "content_block_stop", "index": 0}), ("message_delta", {"type": "message_delta", "delta": {"stop_reason": message["stop_reason"], "stop_sequence": None}, "usage": USAGE}), ("message_stop", {"type": "message_stop"})]
            self.stream(events)
        elif path.endswith("/chat/completions"):
            usage = {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18, "prompt_tokens_details": {"cached_tokens": 3}}
            delta = {"role": "assistant", "tool_calls": [{"index": 0, "id": "call_hra_" + number, "type": "function", "function": {"name": tool_name(body), "arguments": json.dumps({"command": "echo hra-e2e"})}}]} if tool else {"role": "assistant", "content": TEXT}
            base = {"id": "chatcmpl_hra_" + number, "object": "chat.completion.chunk", "created": 1791500000, "model": model}
            if not body.get("stream"):
                self.reply(dict(base, object="chat.completion", choices=[{"index": 0, "message": delta, "finish_reason": "tool_calls" if tool else "stop"}], usage=usage))
                return
            self.stream([(None, dict(base, choices=[{"index": 0, "delta": delta, "finish_reason": None}])), (None, dict(base, choices=[{"index": 0, "delta": {}, "finish_reason": "tool_calls" if tool else "stop"}])), (None, dict(base, choices=[], usage=usage)), (None, "[DONE]")])
        elif path.endswith("/responses"):
            item = {"id": "fc_hra_" + number, "type": "function_call", "call_id": "call_hra_" + number, "name": tool_name(body), "arguments": json.dumps({"command": "echo hra-e2e"}), "status": "completed"} if tool else {"id": "item_hra_" + number, "type": "message", "role": "assistant", "status": "completed", "content": [{"type": "output_text", "text": TEXT, "annotations": []}]}
            response = {"id": "resp_hra_" + number, "object": "response", "created_at": 1791500000, "status": "completed", "model": model, "output": [item], "usage": {"input_tokens": 11, "output_tokens": 7, "total_tokens": 18, "input_tokens_details": {"cached_tokens": 3}, "output_tokens_details": {"reasoning_tokens": 0}}}
            if not body.get("stream"):
                self.reply(response)
                return
            events = [("response.created", {"response": dict(response, status="in_progress", output=[], usage=None)}), ("response.output_item.added", {"output_index": 0, "item": dict(item, status="in_progress", **({"arguments": ""} if tool else {"content": []}))})]
            if tool:
                events += [("response.function_call_arguments.delta", {"item_id": item["id"], "output_index": 0, "delta": item["arguments"]}), ("response.function_call_arguments.done", {"item_id": item["id"], "output_index": 0, "arguments": item["arguments"]})]
            else:
                part = item["content"][0]
                events += [("response.content_part.added", {"item_id": item["id"], "output_index": 0, "content_index": 0, "part": dict(part, text="")}), ("response.output_text.delta", {"item_id": item["id"], "output_index": 0, "content_index": 0, "delta": TEXT}), ("response.output_text.done", {"item_id": item["id"], "output_index": 0, "content_index": 0, "text": TEXT}), ("response.content_part.done", {"item_id": item["id"], "output_index": 0, "content_index": 0, "part": part})]
            events += [("response.output_item.done", {"output_index": 0, "item": item}), ("response.completed", {"response": response})]
            self.stream([(event, dict(data, type=event, sequence_number=i)) for i, (event, data) in enumerate(events)])
        elif "GenerateContent" in path or "generateContent" in path:
            part = {"functionCall": {"name": tool_name(body), "args": {"command": "echo hra-e2e"}}} if tool else {"text": TEXT}
            result = {"candidates": [{"content": {"role": "model", "parts": [part]}, "finishReason": "STOP", "index": 0}], "usageMetadata": {"promptTokenCount": 11, "candidatesTokenCount": 7, "totalTokenCount": 18, "cachedContentTokenCount": 3}, "modelVersion": model, "responseId": "gemini_hra_" + number}
            if "streamGenerateContent" in path:
                self.stream([(None, result)])
            else:
                self.reply(result)
        else:
            self.reply({"error": "unsupported mock route", "path": path}, 404)


def tool_name(body):
    tools = body.get("tools", [])
    names = [t.get("name", t.get("function", {}).get("name", "")) for t in tools]
    names += [f.get("name", "") for t in tools for f in t.get("functionDeclarations", [])]
    return next((n for n in names if n.lower() in ("bash", "shell", "run_shell_command", "execute", "terminal")), "bash")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=0)
    parser.add_argument("--port-file", type=Path, required=True)
    parser.add_argument("--log", type=Path, required=True)
    args = parser.parse_args()
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    server.log = args.log
    server.lock = threading.Lock()
    server.sequence = 0
    args.log.parent.mkdir(parents=True, exist_ok=True)
    args.port_file.write_text(str(server.server_port))
    server.serve_forever()


if __name__ == "__main__":
    main()

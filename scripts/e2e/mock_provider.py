#!/usr/bin/env python3
"""Loopback-only deterministic provider; no upstream requests or dependencies."""
import argparse
import base64
import hashlib
import struct
import time
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


def responses(body, number):
    tool = scenario(body)
    item = {"id": "fc_hra_" + number, "type": "function_call", "call_id": "call_hra_" + number, "name": tool_name(body), "arguments": json.dumps(tool_arguments(body)), "status": "completed"} if tool else {"id": "item_hra_" + number, "type": "message", "role": "assistant", "status": "completed", "content": [{"type": "output_text", "text": TEXT, "annotations": []}]}
    response = {"id": "resp_hra_" + number, "object": "response", "created_at": 1791500000, "status": "completed", "model": body.get("model", "mock-model"), "output": [item], "usage": {"input_tokens": 11, "output_tokens": 7, "total_tokens": 18, "input_tokens_details": {"cached_tokens": 3}, "output_tokens_details": {"reasoning_tokens": 0}}}
    events = [("response.created", {"response": dict(response, status="in_progress", output=[], usage=None)})]
    if body.get("generate") is False:
        response.update(output=[], usage=None)
    else:
        events.append(("response.output_item.added", {"output_index": 0, "item": dict(item, status="in_progress", **({"arguments": ""} if tool else {"content": []}))}))
        if tool:
            events += [("response.function_call_arguments.delta", {"item_id": item["id"], "output_index": 0, "delta": item["arguments"]}), ("response.function_call_arguments.done", {"item_id": item["id"], "output_index": 0, "arguments": item["arguments"]})]
        else:
            part = item["content"][0]
            events += [("response.content_part.added", {"item_id": item["id"], "output_index": 0, "content_index": 0, "part": dict(part, text="")}), ("response.output_text.delta", {"item_id": item["id"], "output_index": 0, "content_index": 0, "delta": TEXT}), ("response.output_text.done", {"item_id": item["id"], "output_index": 0, "content_index": 0, "text": TEXT}), ("response.content_part.done", {"item_id": item["id"], "output_index": 0, "content_index": 0, "part": part})]
        events.append(("response.output_item.done", {"output_index": 0, "item": item}))
    events.append(("response.completed", {"response": response}))
    return response, [(event, dict(data, type=event, sequence_number=i)) for i, (event, data) in enumerate(events)]


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *_):
        pass

    def record(self, body, raw=b"", transport="http"):
        headers = {k: "[REDACTED]" if any(word in k.lower() for word in ("auth", "key", "cookie")) else v for k, v in self.headers.items()}
        hashes = {}
        for key, value in self.headers.raw_items():
            hashes.setdefault(key.lower(), []).append(hashlib.sha256(value.encode("latin-1")).hexdigest())
        # Query API keys must not leak through Gemini URLs either.
        path = urlsplit(self.path).path
        with self.server.lock, self.server.log.open("a") as log:
            self.server.sequence += 1
            number = str(self.server.sequence)
            log.write(json.dumps({"request_id": number, "received_ns": time.time_ns(), "transport": transport, "method": self.command, "path": path, "headers": headers, "header_value_sha256": hashes, "body_sha256": hashlib.sha256(raw).hexdigest(), "body": body}) + "\n")
            self.request_id = number
            return number

    def evidence(self, transport, index, data):
        with self.server.lock, self.server.events_log.open("a") as log:
            log.write(json.dumps({"request_id": self.request_id, "transport": transport, "index": index, "sent_ns": time.time_ns(), "body_sha256": hashlib.sha256(data).hexdigest()}) + "\n")

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
        for index, (event, data) in enumerate(events):
            if index:
                time.sleep(self.server.chunk_delay)
            chunk = ((f"event: {event}\n" if event else "") + "data: " + (data if isinstance(data, str) else json.dumps(data)) + "\n\n").encode()
            self.wfile.write(chunk)
            self.wfile.flush()
            self.evidence("sse", index, chunk)
        self.close_connection = True

    def do_GET(self):
        self.record(None)
        if self.headers.get("Upgrade", "").lower() == "websocket" and urlsplit(self.path).path.endswith("/responses"):
            self.websocket()
        elif self.path == "/healthz":
            self.reply({"ok": True})
        elif "models" in self.path:
            self.reply({"object": "list", "data": [{"id": "claude-haiku-4-5-20251001", "type": "model", "object": "model", "display_name": "Mock Haiku", "created_at": "2025-10-01T00:00:00Z"}, {"id": "mock-model", "object": "model"}], "has_more": False, "models": [{"name": "models/mock-model", "supportedGenerationMethods": ["generateContent"]}]})
        else:
            self.reply({"error": "unsupported mock route"}, 404)

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        body = json.loads(raw or b"{}")
        number = self.record(body, raw)
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
            delta = {"role": "assistant", "tool_calls": [{"index": 0, "id": "call_hra_" + number, "type": "function", "function": {"name": tool_name(body), "arguments": json.dumps(tool_arguments(body))}}]} if tool else {"role": "assistant", "content": TEXT}
            base = {"id": "chatcmpl_hra_" + number, "object": "chat.completion.chunk", "created": 1791500000, "model": model}
            if not body.get("stream"):
                self.reply(dict(base, object="chat.completion", choices=[{"index": 0, "message": delta, "finish_reason": "tool_calls" if tool else "stop"}], usage=usage))
                return
            self.stream([(None, dict(base, choices=[{"index": 0, "delta": delta, "finish_reason": None}])), (None, dict(base, choices=[{"index": 0, "delta": {}, "finish_reason": "tool_calls" if tool else "stop"}])), (None, dict(base, choices=[], usage=usage)), (None, "[DONE]")])
        elif path.endswith("/responses"):
            response, events = responses(body, number)
            if body.get("stream"):
                self.stream(events)
            else:
                self.reply(response)
        elif "GenerateContent" in path or "generateContent" in path:
            part = {"functionCall": {"name": tool_name(body), "args": tool_arguments(body)}} if tool else {"text": TEXT}
            result = {"candidates": [{"content": {"role": "model", "parts": [part]}, "finishReason": "STOP", "index": 0}], "usageMetadata": {"promptTokenCount": 11, "candidatesTokenCount": 7, "totalTokenCount": 18, "cachedContentTokenCount": 3}, "modelVersion": model, "responseId": "gemini_hra_" + number}
            if "streamGenerateContent" in path:
                # Real Gemini emits multiple AfterModel hooks before final usage.
                chunks = []
                parts = [part] if tool else [{"text": "hra-e2e "}, {"text": "mock response"}]
                for chunk_part in parts:
                    chunks.append((None, dict(result, candidates=[{"content": {"role": "model", "parts": [chunk_part]}, "index": 0}], usageMetadata=None)))
                chunks.append((None, dict(result, candidates=[{"content": {"role": "model", "parts": []}, "finishReason": "STOP", "index": 0}])))
                self.stream(chunks)
            else:
                self.reply(result)
        else:
            self.reply({"error": "unsupported mock route", "path": path}, 404)


    def ws_send(self, opcode, payload):
        length = len(payload)
        header = bytes([0x80 | opcode])
        header += bytes([length]) if length < 126 else (b"\x7e" + struct.pack("!H", length) if length <= 65535 else b"\x7f" + struct.pack("!Q", length))
        self.wfile.write(header + payload)
        self.wfile.flush()

    def ws_read(self, count):
        data = self.rfile.read(count)
        if len(data) != count:
            raise EOFError
        return data

    def websocket(self):
        key = self.headers.get("Sec-WebSocket-Key", "")
        try:
            if len(base64.b64decode(key, validate=True)) != 16 or self.headers.get("Sec-WebSocket-Version") != "13":
                raise ValueError
        except ValueError:
            self.reply({"error": "invalid websocket handshake"}, 400)
            return
        accept = base64.b64encode(hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
        self.send_response(101)
        self.send_header("Upgrade", "websocket")
        self.send_header("Connection", "Upgrade")
        self.send_header("Sec-WebSocket-Accept", accept)
        self.end_headers()
        self.wfile.flush()
        self.close_connection = True
        pending = bytearray()
        message_opcode = None
        try:
            while True:
                first, second = self.ws_read(2)
                final, opcode = bool(first & 0x80), first & 15
                if first & 0x70 or not second & 0x80:
                    raise ValueError("invalid frame")
                size = second & 127
                if size == 126:
                    size = struct.unpack("!H", self.ws_read(2))[0]
                elif size == 127:
                    size = struct.unpack("!Q", self.ws_read(8))[0]
                if size > 8 * 1024 * 1024 or len(pending) + size > 8 * 1024 * 1024:
                    raise ValueError("frame too large")
                mask = self.ws_read(4)
                payload = bytes(b ^ mask[i % 4] for i, b in enumerate(self.ws_read(size)))
                if opcode >= 8:
                    if not final or size > 125:
                        raise ValueError("invalid control frame")
                    if opcode == 8:
                        self.ws_send(8, payload)
                        return
                    if opcode == 9:
                        self.ws_send(10, payload)
                    elif opcode != 10:
                        raise ValueError("unsupported control frame")
                    continue
                if opcode in (1, 2):
                    if message_opcode is not None:
                        raise ValueError("overlapping message")
                    message_opcode = opcode
                elif opcode != 0 or message_opcode is None:
                    raise ValueError("unexpected continuation")
                pending.extend(payload)
                if not final:
                    continue
                if message_opcode != 1:
                    raise ValueError("Responses requires text frames")
                raw = bytes(pending)
                pending.clear()
                message_opcode = None
                body = json.loads(raw)
                number = self.record(body, raw, "websocket")
                if body.get("type") != "response.create":
                    raise ValueError("expected response.create")
                _, events = responses(body, number)
                for index, (_, data) in enumerate(events):
                    if index:
                        time.sleep(self.server.chunk_delay)
                    encoded = json.dumps(data).encode()
                    self.ws_send(1, encoded)
                    self.evidence("websocket", index, encoded)
        except (EOFError, ConnectionError):
            pass
        except (ValueError, UnicodeError):
            self.ws_send(8, struct.pack("!H", 1002))


def tool_name(body):
    tools = body.get("tools", [])
    names = [t.get("name", t.get("function", {}).get("name", "")) for t in tools]
    names += [f.get("name", "") for t in tools for f in t.get("functionDeclarations", [])]
    return next((n for n in names if n.lower() in ("bash", "shell", "run_shell_command", "execute", "terminal", "exec_command", "shell_command")), "bash")


def tool_arguments(body):
    name = tool_name(body)
    tools = [t.get("function", t) for t in body.get("tools", [])]
    tools += [f for t in body.get("tools", []) for f in t.get("functionDeclarations", [])]
    tool = next((t for t in tools if t.get("name") == name), {})
    properties = tool.get("parameters", {}).get("properties", {})
    key = "cmd" if "cmd" in properties else "command"
    command = ["sh", "-c", "echo hra-e2e"] if properties.get(key, {}).get("type") == "array" else "echo hra-e2e"
    arguments = {key: command}
    if "login" in properties:
        arguments["login"] = False
    if "description" in properties:
        arguments["description"] = "Harmless mock E2E echo"
    return arguments


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=0)
    parser.add_argument("--port-file", type=Path, required=True)
    parser.add_argument("--log", type=Path, required=True)
    parser.add_argument("--events-log", type=Path)
    parser.add_argument("--chunk-delay-ms", type=float, default=0)
    args = parser.parse_args()
    if args.chunk_delay_ms < 0:
        parser.error("--chunk-delay-ms must be nonnegative")
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    server.log = args.log
    server.events_log = args.events_log or args.log.with_suffix(".events.jsonl")
    server.events_log.parent.mkdir(parents=True, exist_ok=True)
    server.chunk_delay = args.chunk_delay_ms / 1000
    server.lock = threading.Lock()
    server.sequence = 0
    args.log.parent.mkdir(parents=True, exist_ok=True)
    args.port_file.write_text(str(server.server_port))
    server.serve_forever()


if __name__ == "__main__":
    main()

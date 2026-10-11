import { afterEach, beforeEach, expect, spyOn, test } from "bun:test";
import type { Mock } from "bun:test";
import extension from "./assets/herdr-respan.ts";
import type { Batch, Context, NativeEvent } from "./assets/herdr-respan.ts";

const source = await Bun.file(new URL("./assets/herdr-respan.ts", import.meta.url)).text();
const fixture: NativeEvent[] = await Bun.file(new URL("./fixtures/turn.json", import.meta.url)).json();
const golden: Batch = await Bun.file(new URL("./fixtures/batch.json", import.meta.url)).json();
const keys = ["HERDR_ENV", "HERDR_SOCKET_PATH", "HERDR_PANE_ID", "HERDR_RESPAN", "HERDR_RESPAN_PROXY"];
const original = Object.fromEntries(keys.map(key => [key, process.env[key]]));
let clock: Mock<() => number>;

beforeEach(() => {
  process.env.HERDR_ENV = "1";
  process.env.HERDR_SOCKET_PATH = "/tmp/synthetic-herdr.sock";
  process.env.HERDR_PANE_ID = "synthetic-pane";
  delete process.env.HERDR_RESPAN;
  delete process.env.HERDR_RESPAN_PROXY;
  clock = spyOn(Date, "now").mockReturnValue(1000);
});
afterEach(() => {
  clock.mockRestore();
  for (const key of keys) {
    if (original[key] === undefined) delete process.env[key];
    else process.env[key] = original[key];
  }
});
function harness(mode = "tui", baseUrl = "https://api.anthropic.com") {
  const handlers: Record<string, (event: NativeEvent, ctx: Context) => void> = {};
  const batches: Batch[] = [];
  const ctx = { mode, cwd: "/tmp/pi-example", model: { baseUrl }, sessionManager: {
    getSessionId: () => "synthetic-session", getSessionFile: () => "/tmp/pi-synthetic-session.jsonl",
  } };
  extension({ on: (name, handler) => { handlers[name] = handler; } }, (batch) => {
    batches.push(JSON.parse(JSON.stringify(batch)));
  });
  function emit(event: NativeEvent) {
    if (event.at_ms !== undefined) clock.mockReturnValue(event.at_ms);
    if (event.type) handlers[event.type]?.(event, ctx);
  }
  return { emit, batches, handlers, ctx };
}

test("configured pi asset passes Bun TS syntax checking", () => {
  const configured = source.replaceAll('"__HERDR_RESPAN_BIN__"', JSON.stringify("/tmp/bin with spaces/herdr-respan"))
    .replaceAll('"__HERDR_RESPAN_STATE__"', JSON.stringify("/tmp/synthetic-state"))
    .replaceAll('"__HERDR_RESPAN_CONFIG__"', JSON.stringify("/tmp/synthetic-config"));
  expect(() => new Bun.Transpiler({ loader: "ts", target: "node" }).transformSync(configured)).not.toThrow();
});

test("native fixture emits exactly the golden bridge batch once at settlement", () => {
  const h = harness();
  for (const event of fixture) {
    h.emit(event);
    if (event.type !== "agent_settled") expect(h.batches).toHaveLength(0);
  }
  expect(h.batches).toHaveLength(1);
  const batch = h.batches[0];
  for (const event of batch.events) {
    event.turn_id = "t1";
    if (event.name === "compaction") event.id = "c1";
  }
  expect(batch).toEqual(golden);
  h.emit({ type: "agent_settled", aborted: false });
  expect(h.batches).toHaveLength(1);
});

test("herdr and RPC gates leave no batch; opt-out before delivery drops buffered content", () => {
  for (const key of ["HERDR_ENV", "HERDR_SOCKET_PATH", "HERDR_PANE_ID"]) {
    const saved = process.env[key];
    delete process.env[key];
    expect(Object.keys(harness().handlers)).toHaveLength(0);
    process.env[key] = saved;
  }
  process.env.HERDR_RESPAN = "off";
  expect(Object.keys(harness().handlers)).toHaveLength(0);
  delete process.env.HERDR_RESPAN;
  for (const mode of ["rpc"]) {
    const h = harness(mode);
    for (const event of fixture) h.emit(event);
    expect(h.batches).toHaveLength(0);
  }
  const h = harness();
  h.emit(fixture[0]);
  process.env.HERDR_RESPAN = "off";
  h.emit({ type: "agent_settled", aborted: false });
  expect(h.batches).toHaveLength(0);
});

test("proxy and Respan gateway suppress only model spans", () => {
  for (const proxy of [false, true]) {
    if (proxy) process.env.HERDR_RESPAN_PROXY = "1";
    const h = harness("tui", proxy ? "http://127.0.0.1:9000" : "https://api.respan.ai/v1");
    for (const event of fixture) h.emit(event);
    expect(h.batches[0].events.map(event => event.type)).toEqual(["turn_start", "tool", "tool", "turn_end"]);
  }
});

test("unknown usage stays unknown; errors and unfinished tools close safely on shutdown", () => {
  const h = harness();
  h.ctx.sessionManager.getSessionFile = () => "relative-session.jsonl";
  h.emit(fixture[0]);
  h.emit({ type: "message_start", message: { role: "assistant" } });
  h.emit({ type: "message_end", message: { role: "assistant", content: [], stopReason: "error", errorMessage: "synthetic error" } });
  h.emit({ type: "tool_execution_start", toolCallId: "unfinished", toolName: "bash", args: { command: "example" } });
  h.emit({ type: "session_shutdown", reason: "quit" });
  expect(h.batches[0].session_path).toBeNull();
  const chat = h.batches[0].events.find(event => event.type === "chat");
  expect(chat?.usage).toBeUndefined();
  expect(chat?.cost_usd).toBeUndefined();
  expect(chat?.status).toBe("error");
  expect(h.batches[0].events.at(-2)?.error).toBe("tool not completed");
  expect(h.batches[0].events.at(-1)?.status).toBe("error");
});

test("manual compaction gets one standalone turn; failure is not reported as success", () => {
  const h = harness();
  h.emit({ type: "session_before_compact", reason: "manual", preparation: { tokensBefore: 10 } });
  h.emit({ type: "session_compact_failed", reason: "manual", aborted: true });
  expect(h.batches).toHaveLength(1);
  expect(h.batches[0].events.map(event => event.type)).toEqual(["turn_start", "tool", "turn_end"]);
  expect(h.batches[0].events[1].name).toBe("compaction");
  expect(h.batches[0].events[1].error).toBe("compaction aborted");
  expect(h.batches[0].events[2].status).toBe("error");
});

test("observer failures never escape into pi", () => {
  const h = harness();
  h.ctx.sessionManager.getSessionId = () => { throw new Error("synthetic session failure"); };
  expect(() => h.emit(fixture[0])).not.toThrow();
  expect(h.batches).toHaveLength(0);
});


test("captured pi 1.1.0 print-mode tool turn retains completed usage at settlement", async () => {
  const live: NativeEvent[] = await Bun.file(new URL("./fixtures/live-print-tool.json", import.meta.url)).json();
  for (const mode of ["tui", "print"]) {
    const h = harness(mode, "http://127.0.0.1:18080/v1");
    for (const event of live) {
      h.emit(event);
      if (event.type !== "agent_settled") expect(h.batches).toHaveLength(0);
    }
    expect(h.batches).toHaveLength(1);
    const events = h.batches[0].events;
    expect(events.map(event => event.type)).toEqual(["turn_start", "chat", "tool", "chat", "turn_end"]);
    const chats = events.filter(event => event.type === "chat");
    for (const chat of chats) {
      expect(chat.model).toBe("mock-model");
      expect(chat.provider).toBe("mock");
      expect(chat.usage).toEqual({ input_tokens: 11, output_tokens: 7, cache_read_input_tokens: 3,
        cache_creation_input_tokens: 0, reasoning_tokens: 0 });
      expect(chat.cost_usd).toBe(0);
      expect(chat.status).toBe("ok");
    }
    expect(chats[1].output).toEqual([{ role: "assistant", content: [{ type: "text", text: "hra-e2e mock response" }] }]);
    const tool = events.find(event => event.type === "tool")!;
    expect(tool.name).toBe("bash");
    expect(tool.input).toEqual({ command: "echo hra-e2e" });
    expect(JSON.stringify(tool.output)).toContain("hra-e2e");
    expect(tool.status).toBe("ok");
  }
});

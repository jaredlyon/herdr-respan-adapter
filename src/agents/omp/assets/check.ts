import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const dir = dirname(fileURLToPath(import.meta.url));
Object.assign(globalThis, { __HERDR_RESPAN_BIN__: "/synthetic/herdr-respan", __HERDR_RESPAN_STATE__: "/synthetic/state", __HERDR_RESPAN_CONFIG__: "/synthetic/config" });
// Loading boundary test: placeholders must receive synthetic globals before module evaluation.
const { collect, enabled } = await import("./herdr-respan.ts");
type Api = Parameters<typeof collect>[0];
type Ctx = Parameters<Parameters<Api["on"]>[1]>[1];
type Native = Parameters<Parameters<Api["on"]>[1]>[0];
const ctx: Ctx = { hasUI: true, agent: { kind: "main" }, cwd: "/synthetic/repo", sessionManager: {
  getSessionId: () => "synthetic-session", getSessionFile: () => "/synthetic/session.jsonl" } };
function harness(context = ctx, gate = () => true) {
  const handlers: Record<string, (event: Native, ctx: Ctx) => void> = {};
  const bus: Record<string, (event: unknown) => void> = {};
  const batches: unknown[] = [];
  let time = 0;
  collect({ on: (name, fn) => { handlers[name] = fn; }, events: { on: (name, fn) => { bus[name] = fn; } } },
    batch => { batches.push(batch); }, () => time += 10, () => "t1", gate);
  return { batches, handlers, bus, emit: (event: Native, contextOverride = context) => handlers[event.type]?.(event, contextOverride) };
}
const h = harness();
h.emit({ type: "before_agent_start", prompt: "Find an example", systemPrompt: ["Be concise"] });
h.emit({ type: "context", messages: [{ role: "user", content: "Find an example" }] });
h.emit({ type: "message_start", message: { role: "assistant", content: [] } });
h.emit({ type: "message_end", message: { role: "assistant", content: [{ type: "text", text: "Checking" }], responseId: "response-one",
  model: "router", provider: "gateway", upstreamModel: "tiny", upstreamProvider: "direct", stopReason: "toolUse",
  usage: { input: 11, output: 3, cacheRead: 0, cacheWrite: 2, reasoningTokens: 1, cost: { total: 0.001 } } } });
h.emit({ type: "agent_end", willContinue: true });
assert.equal(h.batches.length, 0);
h.emit({ type: "before_agent_start", prompt: "Automatic continuation" });
h.emit({ type: "tool_execution_start", toolCallId: "spawn", toolName: "task", args: { agent: "explore" } });
h.bus["task:subagent:lifecycle"]({ id: "foreign-child", status: "started", parentToolCallId: "foreign" });
h.bus["task:subagent:lifecycle"]({ id: "child", status: "started", parentToolCallId: "spawn", agent: "explore" });
h.bus["task:subagent:event"]({ id: "child", event: { type: "message_start", message: { role: "assistant", content: [] } } });
h.emit({ type: "message_end", message: { role: "assistant", content: "must not duplicate" } }, { ...ctx, agent: { kind: "sub" } });
h.bus["task:subagent:event"]({ id: "child", event: { type: "message_end", message: { role: "assistant", responseId: "response-child",
  content: [{ type: "text", text: "Example found" }], model: "tiny", provider: "direct", stopReason: "stop" } } });
h.bus["task:subagent:lifecycle"]({ id: "child", status: "completed" });
h.emit({ type: "tool_execution_end", toolCallId: "spawn", toolName: "task", result: { content: [{ type: "text", text: "Example found" }] }, isError: false });
h.emit({ type: "message_start", message: { role: "assistant", content: [] } });
h.emit({ type: "message_end", message: { role: "assistant", responseId: "response-final", content: [{ type: "text", text: "Done" }], model: "tiny", provider: "direct", stopReason: "stop" } });
assert.equal(h.handlers.session_stop, undefined);
h.emit({ type: "agent_end" });
h.emit({ type: "agent_end" });
assert.deepEqual(h.batches, [JSON.parse(readFileSync(resolve(dir, "../fixtures/turn.json"), "utf8"))]);

const good = { HERDR_ENV: "1", HERDR_SOCKET_PATH: "socket", HERDR_PANE_ID: "pane" };
assert(enabled(good));
for (const env of [{}, { ...good, OMPCODE: "1" }, { ...good, HERDR_RESPAN: "off" }, { ...good, HERDR_SOCKET_PATH: "" }, { ...good, HERDR_ENV: "true" }]) assert(!enabled(env));
for (const context of [{ ...ctx, hasUI: false }, { ...ctx, agent: { kind: "sub" } }]) {
  const h = harness(context); h.emit({ type: "agent_start" }); h.emit({ type: "agent_end" }); assert.deepEqual(h.batches, []);
}
const paused = harness(ctx, () => false); paused.emit({ type: "agent_start" }); paused.emit({ type: "agent_end" }); assert.deepEqual(paused.batches, []);
const proxy = harness({ ...ctx, model: { baseUrl: "https://api.respan.ai/v1" } });
proxy.emit({ type: "agent_start" }); proxy.emit({ type: "message_end", message: { role: "assistant", content: "done" } }); proxy.emit({ type: "agent_end" });
assert(!JSON.stringify(proxy.batches).includes('"type":"chat"'));
const failure = harness(); failure.emit({ type: "agent_start" });
failure.emit({ type: "message_end", message: { role: "assistant", content: [], stopReason: "aborted", errorMessage: "cancelled" } }); failure.emit({ type: "agent_end" });
assert(JSON.stringify(failure.batches).includes('"status":"error"'));
const detached = harness(); detached.emit({ type: "agent_start" }); detached.emit({ type: "tool_execution_start", toolCallId: "spawn", toolName: "task" });
detached.bus["task:subagent:lifecycle"]({ id: "later", agent: "explore", parentToolCallId: "spawn", status: "started", detached: true });
detached.emit({ type: "agent_end" }); assert.equal(detached.batches.length, 0);
detached.bus["task:subagent:lifecycle"]({ id: "later", status: "completed" }); assert.equal(detached.batches.length, 1);
const shutdown = harness(); shutdown.emit({ type: "agent_start" }); shutdown.emit({ type: "session_shutdown" });
assert(JSON.stringify(shutdown.batches).includes("session ended before turn settled"));
let openGate = true;
const midPause = harness(ctx, () => openGate); midPause.emit({ type: "agent_start" });
openGate = false; midPause.emit({ type: "message_end", message: { role: "assistant", content: "paused private content" } });
openGate = true; midPause.emit({ type: "agent_end" }); assert.deepEqual(midPause.batches, []);
console.log("omp native mapping, gates, continuation, subagent parentage, and batch golden passed");
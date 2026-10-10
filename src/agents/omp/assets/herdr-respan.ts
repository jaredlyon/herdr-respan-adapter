// Managed by herdr-respan; install beside herdr-agent-state.ts, never replace it.
import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { readFileSync } from "node:fs";
import path from "node:path";

declare const __HERDR_RESPAN_BIN__: string;
declare const __HERDR_RESPAN_STATE__: string;
declare const __HERDR_RESPAN_CONFIG__: string;
const binary = __HERDR_RESPAN_BIN__;
const state = __HERDR_RESPAN_STATE__;
const config = __HERDR_RESPAN_CONFIG__;
const now = () => Date.now() * 1_000_000;
const observed: Record<string, true> = { context: true, message_start: true, message_end: true, tool_execution_start: true, tool_execution_end: true };
const number = (v: unknown) => typeof v === "number" && Number.isFinite(v) && v >= 0 ? v : null;

type Message = { role: string; content: unknown; responseId?: string; model?: string; provider?: string;
  upstreamModel?: string; upstreamProvider?: string; stopReason?: string; errorMessage?: string;
  usage?: { input?: number; output?: number; cacheRead?: number; cacheWrite?: number; reasoningTokens?: number; cost?: { total?: number } } };
type NativeEvent = { type: string; prompt?: string; systemPrompt?: string[]; messages?: Message[]; message?: Message;
  toolCallId?: string; toolName?: string; args?: unknown; result?: unknown; isError?: boolean; willContinue?: boolean };
type Context = { hasUI: boolean; agent?: { kind: string }; cwd?: string; model?: { baseUrl?: string };
  sessionManager: { getSessionId(): string; getSessionFile(): string | undefined } };
type Api = { on(name: string, handler: (event: NativeEvent, ctx: Context) => void): void;
  events: { on(name: string, handler: (data: unknown) => void): unknown } };
type BridgeEvent = Record<string, unknown> & { type: string; turn_id: string };
type Batch = { v: number; agent: string; session_id: string; session_path: string | null; cwd: string | null;
  agent_version: string | null; events: BridgeEvent[] };
type Tool = { start: number; input: unknown; name?: string; parent: string | null };
type Turn = { id: string; closed: boolean; end: number; pending: Set<string>; tools: Map<string, Tool>;
  calls: Map<string, { start: number; input: unknown }>; input: { role: string; content: unknown }[] | null;
  system?: string[]; output: unknown; status: string; error: string | null; skipChat: boolean; batch: Batch };

export function enabled(env = process.env): boolean {
  return env.HERDR_ENV === "1" && !!env.HERDR_SOCKET_PATH && !!env.HERDR_PANE_ID && env.OMPCODE !== "1" && env.HERDR_RESPAN !== "off";
}
function allowed(): boolean {
  if (!enabled()) return false;
  try {
    const paused: unknown = JSON.parse(readFileSync(path.join(state, "paused.json"), "utf8"));
    if (!paused || typeof paused !== "object" || !("workspaces" in paused) || !Array.isArray(paused.workspaces)
      || !paused.workspaces.every(w => typeof w === "string")) return false;
    return !paused.workspaces.includes(process.env.HERDR_WORKSPACE_ID);
  } catch (e: unknown) { return !!e && typeof e === "object" && "code" in e && e.code === "ENOENT"; }
}
function send(batch: Batch): void {
  try {
    const child = spawn(binary, ["hook", "omp", "--state-dir", state, "--config-dir", config],
      { stdio: ["pipe", "ignore", "ignore"], windowsHide: true });
    child.on("error", () => {});
    child.stdin.on("error", () => {});
    child.stdin.end(JSON.stringify(batch));
    child.unref();
  } catch { /* Observability must never affect the agent. */ }
}

// Test seams exercise the actual mapper without launching an agent or provider.
export function collect(api: Api, deliver = send, clock = now, id: () => string = randomUUID, gate = allowed): void {
  let active: Turn | undefined;
  const turns = new Set<Turn>();
  const children = new Map<string, Turn>();
  const root = (ctx: Context) => ctx.hasUI === true && ctx.agent?.kind !== "sub";
  const safe = <Args extends unknown[]>(fn: (...args: Args) => void) => (...args: Args) => {
    try { fn(...args); } catch { /* No stdout, replacements, or agent failures. */ }
  };
  const add = (t: Turn, event: Record<string, unknown> & { type: string }) => t.batch.events.push({ ...event, turn_id: t.id });
  function start(ctx: Context, prompt?: string, system?: string[]): void {
    if (!root(ctx) || !gate() || active) return;
    const session = ctx.sessionManager.getSessionId();
    if (typeof session !== "string" || !session) return;
    const file = ctx.sessionManager.getSessionFile();
    const t: Turn = { id: id(), closed: false, end: 0, pending: new Set(), tools: new Map(), calls: new Map(),
      input: null, system, output: null, status: "ok", error: null,
      skipChat: process.env.HERDR_RESPAN_PROXY === "1" || String(ctx.model?.baseUrl ?? "").includes("api.respan.ai"),
      batch: { v: 1, agent: "omp", session_id: session,
        session_path: typeof file === "string" && (path.posix.isAbsolute(file) || path.win32.isAbsolute(file)) ? file : null,
        cwd: ctx.cwd ?? null, agent_version: null, events: [] } };
    add(t, { type: "turn_start", ts_ns: clock(), prompt: prompt ?? null });
    turns.add(t); active = t;
  }
  function flush(t: Turn): void {
    if (!t.closed || t.pending.size) return;
    add(t, { type: "turn_end", ts_ns: t.end, output: t.output, status: t.status, error: t.error });
    turns.delete(t);
    if (gate()) deliver(t.batch);
  }
  function close(t: Turn | undefined, error?: string): void {
    if (!t || t.closed) return;
    t.closed = true; t.end = clock();
    if (error) { t.status = "error"; t.error = error; }
    if (active === t) active = undefined;
    flush(t);
  }
  function event(t: Turn, e: NativeEvent, parent: string | null = null): void {
    if (!observed[e.type]) return;
    if (!gate()) { active = undefined; turns.clear(); children.clear(); return; }
    const key = `${parent ?? "root"}:assistant`;
    if (e.type === "context" && e.messages) {
      if (parent) return;
      t.input = e.messages.map(m => ({ role: m.role, content: m.content }));
      if (t.system) t.input.unshift({ role: "system", content: t.system.join("\n") });
    } else if (e.type === "message_start" && e.message?.role === "assistant") {
      t.calls.set(key, { start: clock(), input: parent ? null : t.input });
    } else if (e.type === "message_end" && e.message?.role === "assistant") {
      const m = e.message;
      const call = t.calls.get(key);
      t.calls.delete(key);
      const end = clock();
      const failed = m.stopReason === "error" || m.stopReason === "aborted";
      if (!parent) { t.output = [{ role: "assistant", content: m.content }]; t.status = failed ? "error" : "ok"; t.error = m.errorMessage ?? null; }
      if (t.skipChat) return;
      const u = m.usage;
      add(t, { type: "chat", id: m.responseId ?? id(), parent_id: parent, start_ns: call?.start ?? end, end_ns: end,
        model: m.upstreamModel ?? m.model ?? null, provider: m.upstreamProvider ?? m.provider ?? null,
        input: call?.input ?? null, output: [{ role: "assistant", content: m.content }],
        usage: { input_tokens: number(u?.input), output_tokens: number(u?.output),
          cache_read_input_tokens: number(u?.cacheRead), cache_creation_input_tokens: number(u?.cacheWrite),
          reasoning_tokens: number(u?.reasoningTokens) }, cost_usd: number(u?.cost?.total),
        status: failed ? "error" : "ok", error: m.errorMessage ?? null });
    } else if (e.type === "tool_execution_start" && e.toolCallId) {
      t.tools.set(e.toolCallId, { start: clock(), input: e.args, name: e.toolName, parent });
    } else if (e.type === "tool_execution_end" && e.toolCallId) {
      const tool = t.tools.get(e.toolCallId);
      const end = clock();
      add(t, { type: "tool", id: e.toolCallId, parent_id: tool?.parent ?? parent, name: e.toolName ?? tool?.name ?? null,
        start_ns: tool?.start ?? end, end_ns: end, input: tool?.input ?? null, output: e.result ?? null,
        status: e.isError ? "error" : "ok", error: null });
      // Retain the id until delivery: lifecycle starts correlate to executed spawning tools.
    }
  }
  api.on("before_agent_start", safe((e, ctx) => start(ctx, e.prompt, e.systemPrompt)));
  api.on("agent_start", safe((_e, ctx) => start(ctx)));
  for (const name of Object.keys(observed)) {
    api.on(name, safe((e, ctx) => { if (root(ctx) && active) event(active, e); }));
  }
  api.on("agent_end", safe((e, ctx) => { if (root(ctx) && !e.willContinue) close(active); }));
  // session_stop can request continuation; it is intentionally not a terminal boundary.
  function stop(ctx: Context): void {
    if (!root(ctx)) return;
    for (const t of turns) {
      for (const child of t.pending) add(t, { type: "subagent_end", id: child, ts_ns: clock(), status: "error", output: null });
      t.pending.clear();
      if (t.closed) flush(t); else close(t, "session ended before turn settled");
    }
    children.clear();
  }
  api.on("session_switch", safe((_e, ctx) => stop(ctx)));
  api.on("session_shutdown", safe((_e, ctx) => stop(ctx)));
  api.events.on("task:subagent:lifecycle", safe((data: unknown) => {
    if (!data || typeof data !== "object" || !("id" in data) || typeof data.id !== "string" || !("status" in data)) return;
    if (!gate()) { active = undefined; turns.clear(); children.clear(); return; }
    // This in-process event bus uses OMP's documented SubagentLifecyclePayload.
    const e = data as { id: string; status: string; parentToolCallId?: string; agent?: string };
    if (e.status === "started") {
      const t = e.parentToolCallId ? [...turns].find(t => t.tools.has(e.parentToolCallId!)) : undefined;
      if (!t || children.has(e.id)) return;
      children.set(e.id, t); t.pending.add(e.id);
      add(t, { type: "subagent_start", id: e.id, parent_id: e.parentToolCallId ?? null, ts_ns: clock(), name: e.agent });
    } else if (["completed", "failed", "aborted"].includes(e.status)) {
      const t = children.get(e.id);
      if (!t) return;
      add(t, { type: "subagent_end", id: e.id, ts_ns: clock(), status: e.status === "completed" ? "ok" : "error", output: null });
      t.pending.delete(e.id); children.delete(e.id); flush(t);
    }
  }));
  api.events.on("task:subagent:event", safe((data: unknown) => {
    if (!data || typeof data !== "object" || !("id" in data) || typeof data.id !== "string" || !("event" in data)) return;
    const t = children.get(data.id);
    // Native AgentSessionEvent has the same completed-message/execution fields as extension events.
    const e = data.event as NativeEvent;
    if (t) event(t, e, data.id);
  }));
}

export default function (api: Api): void {
  if (enabled()) collect(api);
}
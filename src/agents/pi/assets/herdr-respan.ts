// Managed by herdr-respan; install beside (never replace) herdr-agent-state.ts.
import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { isAbsolute } from "node:path";
import type { Socket } from "node:net";

const binary = "__HERDR_RESPAN_BIN__";
const stateDir = "__HERDR_RESPAN_STATE__";
const configDir = "__HERDR_RESPAN_CONFIG__";

type Message = {
  role: string; content?: unknown; provider?: string; model?: string; responseModel?: string;
  responseId?: string; stopReason?: string; errorMessage?: string;
  usage?: { input?: number; output?: number; cacheRead?: number; cacheWrite?: number;
    reasoning?: number; cost?: { total?: number } };
};
export type NativeEvent = {
  type?: string; at_ms?: number; prompt?: string; systemPrompt?: string; messages?: Message[];
  message?: Message; toolCallId?: string; toolName?: string; args?: unknown; result?: unknown;
  isError?: boolean; reason?: string; preparation?: { tokensBefore?: number };
  customInstructions?: string; compactionEntry?: { summary?: string; [key: string]: unknown };
  errorMessage?: string; aborted?: boolean;
};
export type Context = {
  mode?: string; cwd?: string; model?: { baseUrl?: string };
  sessionManager?: { getSessionId?(): string; getSessionFile?(): string | undefined };
};
export type BridgeEvent = Record<string, unknown>;
export type Batch = {
  v: 1; agent: "pi"; session_id: string; session_path: string | null;
  cwd: string | null; events: BridgeEvent[];
};
type Pi = { on(name: string, handler: (event: NativeEvent, ctx: Context) => void): void };

function enabled() {
  return process.env.HERDR_ENV === "1" && !!process.env.HERDR_SOCKET_PATH &&
    !!process.env.HERDR_PANE_ID && process.env.HERDR_RESPAN !== "off";
}
function ns() { return Date.now() * 1_000_000; }
function counter(value: unknown): number | undefined {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= 0 ? value : undefined;
}
function send(batch: Batch) {
  // No shell: quoted paths, spaces and Windows .exe paths remain single arguments.
  const child = spawn(binary, ["hook", "pi", "--state-dir", stateDir, "--config-dir", configDir],
    { stdio: ["pipe", "ignore", "ignore"], windowsHide: true });
  child.on("error", () => {});
  child.stdin?.on("error", () => {});
  child.stdin?.end(JSON.stringify(batch));
  // Child-process pipe streams are net.Socket instances; unref only affects loop ownership.
  const stdin = child.stdin as Socket | null;
  child.unref();
  stdin?.unref();
}

// The optional delivery function lets synthetic replay check the exact emitted batch.
export default function extension(pi: Pi, deliver: (batch: Batch) => void = send) {
  if (!enabled()) return;
  let batch: Batch | undefined;
  let turn = "";
  let systemPrompt: string | undefined;
  let input: unknown;
  let assistantStart: number | undefined;
  let chatIndex = 0;
  let finalOutput: unknown;
  let finalError: string | undefined;
  const tools = new Map<string, { name?: string; start: number; input: unknown }>();
  let compact: { id: string; start: number; input: unknown; standalone: boolean } | undefined;

  function add(event: BridgeEvent) {
    batch?.events.push({ ...event, turn_id: turn });
  }
  function open(ctx: Context, prompt?: unknown) {
    if (batch || !enabled() || (ctx?.mode !== "tui" && ctx?.mode !== "print")) return;
    const session = ctx.sessionManager?.getSessionId?.();
    if (typeof session !== "string" || !session) return;
    const path = ctx.sessionManager?.getSessionFile?.();
    batch = { v: 1, agent: "pi", session_id: session,
      session_path: typeof path === "string" && isAbsolute(path) ? path : null,
      cwd: typeof ctx.cwd === "string" ? ctx.cwd : null, events: [] };
    turn = randomUUID();
    chatIndex = 0;
    finalOutput = undefined;
    finalError = undefined;
    input = undefined;
    assistantStart = undefined;
    tools.clear();
    add({ type: "turn_start", ts_ns: ns(), prompt });
  }
  function close(error?: string) {
    if (!batch) return;
    const end = ns();
    for (const [id, tool] of tools) {
      add({ type: "tool", id, name: tool.name, start_ns: tool.start, end_ns: end,
        input: tool.input, status: "error", error: "tool not completed" });
    }
    tools.clear();
    add({ type: "turn_end", ts_ns: end, output: finalOutput,
      status: error || finalError ? "error" : "ok", error: error ?? finalError });
    const completed = batch;
    batch = undefined;
    compact = undefined;
    systemPrompt = undefined;
    if (enabled()) deliver(completed);
  }
  function on(name: string, handler: (event: NativeEvent, ctx: Context) => void) {
    // Observers never throw, replace messages, or return protocol decisions.
    pi.on(name, (event, ctx) => { try { handler(event, ctx); } catch {} });
  }
  on("before_agent_start", (event, ctx) => {
    open(ctx, event.prompt);
    systemPrompt = typeof event.systemPrompt === "string" ? event.systemPrompt : undefined;
  });
  on("agent_start", (_event, ctx) => open(ctx));
  on("context", (event) => {
    if (!batch) return;
    const messages = Array.isArray(event.messages) ? event.messages : [];
    input = systemPrompt ? [{ role: "system", content: systemPrompt }, ...messages] : messages.slice();
  });
  on("message_start", (event) => {
    if (batch && event.message?.role === "assistant") assistantStart = ns();
  });
  on("message_end", (event, ctx) => {
    const message = event.message;
    if (!batch || message?.role !== "assistant") return;
    const end = ns();
    const usage = message.usage;
    const error = message.stopReason === "error" || message.stopReason === "aborted";
    finalOutput = message.content;
    finalError = error ? message.errorMessage || message.stopReason : undefined;
    const reportedCost = usage?.cost?.total;
    // Core also applies proxy/gateway suppression; avoid collecting duplicate model content here.
    if (process.env.HERDR_RESPAN_PROXY !== "1" && !String(ctx.model?.baseUrl ?? "").includes("api.respan.ai")) {
      add({ type: "chat", id: message.responseId || `message-${++chatIndex}`, parent_id: null,
        start_ns: assistantStart ?? end, end_ns: end,
        model: message.responseModel ?? message.model, provider: message.provider,
        input, output: [{ role: "assistant", content: message.content }],
        usage: usage ? { input_tokens: counter(usage.input), output_tokens: counter(usage.output),
          cache_read_input_tokens: counter(usage.cacheRead), cache_creation_input_tokens: counter(usage.cacheWrite),
          reasoning_tokens: counter(usage.reasoning) } : undefined,
        cost_usd: typeof reportedCost === "number" && Number.isFinite(reportedCost) && reportedCost >= 0 ? reportedCost : undefined,
        status: error ? "error" : "ok", error: finalError });
    }
    assistantStart = undefined;
  });
  on("tool_execution_start", (event) => {
    if (batch && typeof event.toolCallId === "string" && event.toolCallId) {
      tools.set(event.toolCallId, { name: event.toolName, start: ns(), input: event.args });
    }
  });
  on("tool_execution_end", (event) => {
    if (!batch || typeof event.toolCallId !== "string" || !event.toolCallId) return;
    const end = ns();
    const tool = tools.get(event.toolCallId);
    add({ type: "tool", id: event.toolCallId, parent_id: null, name: tool?.name ?? event.toolName,
      start_ns: tool?.start ?? end, end_ns: end, input: tool?.input, output: event.result,
      status: event.isError ? "error" : "ok" });
    tools.delete(event.toolCallId);
    // parentToolCallId is another tool, not a child agent id; bridge parent_id cannot represent it.
  });
  on("session_before_compact", (event, ctx) => {
    const standalone = !batch;
    open(ctx, "Context compaction");
    if (batch) compact = { id: `compaction-${randomUUID()}`, start: ns(), standalone,
      input: { reason: event.reason, tokensBefore: event.preparation?.tokensBefore,
        customInstructions: event.customInstructions } };
  });
  function compactEnd(event: NativeEvent, failed: boolean) {
    if (!batch || !compact) return;
    const operation = compact;
    compact = undefined;
    const error = failed ? event.errorMessage || (event.aborted ? "compaction aborted" : "compaction failed") : undefined;
    add({ type: "tool", id: operation.id, name: "compaction", parent_id: null,
      start_ns: operation.start, end_ns: ns(), input: operation.input,
      output: event.compactionEntry, status: failed ? "error" : "ok", error });
    if (operation.standalone) { finalOutput = event.compactionEntry?.summary; close(error); }
  }
  on("session_compact", (event) => compactEnd(event, false));
  on("session_compact_failed", (event) => compactEnd(event, true));
  // agent_end/turn_end may precede retry, compaction or queued continuation; only settlement closes.
  on("agent_settled", (event) => close(event.aborted ? "agent aborted" : undefined));
  on("session_shutdown", () => close("session shutdown before settlement"));
}

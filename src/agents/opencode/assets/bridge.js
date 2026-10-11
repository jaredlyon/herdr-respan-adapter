// herdr-respan: shared V1/V2 translation; Rust alone assembles, redacts and spools.
import { spawn } from "node:child_process";

export function enabled() {
  return process.env.HERDR_ENV === "1" && process.env.HERDR_RESPAN !== "off" &&
    !!process.env.HERDR_SOCKET_PATH && !!process.env.HERDR_PANE_ID;
}

export function deliver(batch) {
  if (!enabled()) return;
  try {
    const payload = JSON.stringify(batch);
    const child = spawn(__HERDR_RESPAN_BIN__, ["hook", "opencode", "--state-dir",
      __HERDR_RESPAN_STATE__, "--config-dir", __HERDR_RESPAN_CONFIG__],
      { detached: true, windowsHide: true, stdio: ["pipe", "ignore", "ignore"] });
    child.on("error", () => {});
    child.stdin.on("error", () => {});
    child.stdin.end(payload);
    child.unref();
  } catch { /* Observability must not break the agent. */ }
}

const ns = (ms) => Math.trunc((typeof ms === "number" ? ms : Date.now()) * 1e6);
const errorText = (error) => error == null ? null : typeof error === "string" ? error :
  error.data?.message ?? error.message ?? error.name ?? JSON.stringify(error);
const number = (value) => Number.isFinite(value) && value >= 0 ? Math.trunc(value) : null;
// Native input excludes cache; Core's usage contract is the total prompt.
const usage = (tokens) => ({ input_tokens: number(tokens?.input) === null ? null : number(tokens.input) + (number(tokens?.cache?.read) ?? 0) + (number(tokens?.cache?.write) ?? 0),
  output_tokens: number(tokens?.output),
  cache_read_input_tokens: number(tokens?.cache?.read), cache_creation_input_tokens: number(tokens?.cache?.write),
  reasoning_tokens: number(tokens?.reasoning) });
const text = (parts) => [...parts.values()].filter((p) => p.type === "text" || p.type === "reasoning")
  .map((p) => ({ type: p.type, text: p.text }));

export function collector({ owns, cwd, send = deliver }) {
  const turns = new Map();
  const active = new Map();
  const children = new Map();
  const completed = new Set();
  let suppressModels = process.env.HERDR_RESPAN_PROXY === "1";

  function root(id) {
    const visited = new Set();
    while (children.has(id) && !visited.has(id)) { visited.add(id); id = children.get(id).root; }
    return id;
  }
  function turn(id, messageID) {
    const owner = root(id);
    if (messageID) {
      for (const value of turns.values()) if (value.sessionID === owner &&
        (value.id === messageID || value.messages.has(`${id}:${messageID}`))) return value;
    }
    const child = children.get(id);
    return turns.get(child ? child.turnKey : active.get(owner));
  }
  function start(sessionID, id, timestamp, prompt) {
    const child = children.get(sessionID);
    if (child) {
      const value = turn(sessionID);
      if (id) value?.roles.set(`${sessionID}:${id}`, "user");
      return value;
    }
    if (!owns(sessionID) || !id || completed.has(`${sessionID}:${id}`)) return;
    const key = `${sessionID}:${id}`;
    let value = turns.get(key);
    if (value?.id === id) { if (prompt != null) value.prompt = prompt; return value; }
    value = { id, sessionID, start: ns(timestamp), prompt: prompt ?? null,
      roles: new Map([[key, "user"]]), messages: new Map(), tools: new Map(), subs: new Map(), error: null };
    turns.set(key, value);
    active.set(sessionID, key);
    return value;
  }
  function message(value, sessionID, id, timestamp) {
    if (!value || !id) return;
    const key = `${sessionID}:${id}`;
    if (!value.messages.has(key)) value.messages.set(key, { id, sessionID, start: ns(timestamp),
      parts: new Map(), model: null, provider: null, settled: false });
    return value.messages.get(key);
  }
  function tool(value, sessionID, id, timestamp) {
    if (!value || !id) return;
    const key = `${sessionID}:${id}`;
    if (!value.tools.has(key)) value.tools.set(key, { id, sessionID, start: ns(timestamp), name: null });
    return value.tools.get(key);
  }
  function link(value, parentSessionID, childID, callID, name, timestamp) {
    if (!value || !childID) return;
    children.set(childID, { root: value.sessionID, turnKey: `${value.sessionID}:${value.id}`, parentSessionID });
    const sub = value.subs.get(childID) ?? { id: childID, start: ns(timestamp) };
    if (callID) sub.parent = `${parentSessionID}:${callID}`;
    if (name) sub.name = name;
    value.subs.set(childID, sub);
  }
  function close(sessionID, timestamp, error) {
    const value = turn(sessionID);
    if (!value) return;
    const end = ns(timestamp);
    if (sessionID !== value.sessionID) {
      const sub = value.subs.get(sessionID);
      if (sub) Object.assign(sub, { end, error: errorText(error) });
      return;
    }
    for (const pending of [...turns.values()]) if (pending.sessionID === sessionID) closeTurn(pending, end, error);
    active.delete(sessionID);
  }
  function closeTurn(value, end, error) {
    const sessionID = value.sessionID;
    const key = `${sessionID}:${value.id}`;
    if (!owns(sessionID)) { turns.delete(key); return; }
    const events = [{ type: "turn_start", turn_id: value.id, ts_ns: value.start, prompt: value.prompt }];
    let finalOutput = null;
    for (const m of value.messages.values()) {
      const output = text(m.parts);
      if (m.sessionID === sessionID && output.length) finalOutput = output;
      if (suppressModels) continue;
      const failure = m.error ?? (!m.settled ? "message not completed" : null);
      events.push({ type: "chat", turn_id: value.id, id: `${m.sessionID}:${m.id}`,
        parent_id: m.sessionID === sessionID ? null : m.sessionID, start_ns: m.start,
        end_ns: Math.max(m.start, m.end ?? end), model: m.model, provider: m.provider,
        input: value.prompt == null ? null : [{ role: "user", content: value.prompt }],
        output: [{ role: "assistant", content: output }], usage: m.usage ?? usage(null),
        cost_usd: m.cost ?? null, aux_task: null, status: failure ? "error" : "ok", error: failure });
    }
    for (const t of value.tools.values()) {
      const failure = t.error ?? (t.end == null ? "tool not completed" : null);
      events.push({ type: "tool", turn_id: value.id, id: `${t.sessionID}:${t.id}`,
        parent_id: t.sessionID === sessionID ? null : t.sessionID, name: t.name,
        start_ns: t.start, end_ns: Math.max(t.start, t.end ?? end), input: t.input ?? null,
        output: t.output ?? null, status: failure ? "error" : "ok", error: failure });
    }
    for (const sub of value.subs.values()) {
      events.push({ type: "subagent_start", turn_id: value.id, id: sub.id, parent_id: sub.parent ?? null,
        ts_ns: sub.start, name: sub.name ?? null });
      const failure = sub.error ?? (sub.end == null ? "subagent not completed" : null);
      events.push({ type: "subagent_end", turn_id: value.id, id: sub.id, ts_ns: Math.max(sub.start, sub.end ?? end),
        status: failure ? "error" : "ok", output: sub.output ?? null });
    }
    const failure = errorText(error) ?? value.error;
    events.push({ type: "turn_end", turn_id: value.id, ts_ns: Math.max(value.start, end), output: finalOutput,
      status: failure ? "error" : "ok", error: failure });
    turns.delete(key);
    for (const [id, child] of children) if (child.turnKey === key) children.delete(id);
    completed.add(`${sessionID}:${value.id}`);
    if (completed.size > 1024) completed.delete(completed.values().next().value);
    send({ v: 1, agent: "opencode", session_id: sessionID, session_path: null,
      cwd: cwd ?? null, agent_version: process.env.OPENCODE_VERSION ?? null, events });
  }

  function v1(event) {
    const d = event?.properties;
    if (!d) return;
    const id = d.sessionID ?? d.info?.sessionID ?? d.part?.sessionID;
    if (event.type === "session.created" || event.type === "session.updated") {
      const info = d.info;
      if (info?.parentID) link(turn(info.parentID), info.parentID, info.id, null, info.title, info.time?.created);
      return;
    }
    if (!owns(root(id))) return;
    if (event.type === "message.updated") {
      const info = d.info;
      if (info?.role === "user") {
        start(id, info.id, info.time?.created, null);
        return;
      }
      if (info?.role !== "assistant") return;
      const value = turn(id, info.parentID);
      const m = message(value, id, info.id, info.time?.created);
      if (!m) return;
      value.roles.set(`${id}:${info.id}`, "assistant");
      Object.assign(m, { start: ns(info.time?.created), model: info.modelID ?? null, provider: info.providerID ?? null,
        error: errorText(info.error), usage: usage(info.tokens), cost: info.cost });
      if (m.error && id === value.sessionID) value.error = m.error;
      if (info.time?.completed != null) Object.assign(m, { end: ns(info.time.completed), settled: true });
    } else if (event.type === "message.part.updated") {
      const p = d.part;
      const value = turn(id, p?.messageID);
      if (!value || !p) return;
      const role = value.roles.get(`${id}:${p.messageID}`);
      if (role === "user") {
        if (id === value.sessionID && p.messageID === value.id && p.type === "text") {
          value.promptParts ??= new Map(); value.promptParts.set(p.id, p);
          value.prompt = text(value.promptParts);
        }
      } else if (role === "assistant" && (p.type === "text" || p.type === "reasoning")) {
        message(value, id, p.messageID, p.time?.start)?.parts.set(p.id, p);
      } else if (p.type === "tool") {
        const t = tool(value, id, p.callID, p.state?.time?.start);
        Object.assign(t, { name: p.tool, input: p.state?.input, output: p.state?.output,
          error: errorText(p.state?.error) });
        if (p.state?.time?.start != null) t.start = ns(p.state.time.start);
        if (["completed", "error"].includes(p.state?.status)) t.end = ns(p.state.time?.end);
        const childID = p.state?.metadata?.sessionId;
        if (childID) link(value, id, childID, p.callID, p.state.input?.subagent_type, p.state.time?.start);
      }
    } else if (event.type === "message.part.removed") {
      turn(id, d.messageID)?.messages.get(`${id}:${d.messageID}`)?.parts.delete(d.partID);
    } else if (event.type === "session.error") {
      const value = turn(id); if (value) value.error = errorText(d.error);
    } else if (event.type === "session.idle" ||
      (event.type === "session.status" && d.status?.type === "idle")) close(id, undefined);
    else if (event.type === "session.deleted") close(id, undefined, "session deleted");
  }

  function v2(event) {
    const d = event?.data;
    if (!d) return;
    const id = d.sessionID;
    if (event.type === "session.created" && d.parentID) {
      link(turn(d.parentID), d.parentID, id, null, d.title, d.timestamp); return;
    }
    if (!owns(root(id))) return;
    const type = event.type;
    if (type === "session.next.prompt.admitted") {
      start(id, d.messageID, d.timestamp, d.prompt); return;
    }
    const value = turn(id, d.assistantMessageID);
    if (!value) return;
    if (type === "session.next.step.started") {
      const m = message(value, id, d.assistantMessageID, d.timestamp);
      Object.assign(m, { model: d.model?.id ?? null, provider: d.model?.providerID ?? null });
    } else if (type === "session.next.text.ended" || type === "session.next.reasoning.ended") {
      message(value, id, d.assistantMessageID, d.timestamp)?.parts.set(d.textID ?? d.reasoningID,
        { type: type.includes("reasoning") ? "reasoning" : "text", text: d.text });
    } else if (type === "session.next.step.ended" || type === "session.next.step.failed") {
      const m = message(value, id, d.assistantMessageID, d.timestamp);
      Object.assign(m, { end: ns(d.timestamp), settled: true, usage: usage(d.tokens), cost: d.cost,
        error: errorText(d.error) });
    } else if (type === "session.next.tool.called") {
      Object.assign(tool(value, id, d.callID, d.timestamp), { name: d.tool, input: d.input });
    } else if (type === "session.next.tool.success" || type === "session.next.tool.failed") {
      const t = tool(value, id, d.callID, d.timestamp);
      Object.assign(t, { end: ns(d.timestamp), output: d.result ?? d.content, error: errorText(d.error) });
    } else if (type === "session.next.shell.started") {
      Object.assign(tool(value, id, d.callID, d.timestamp), { name: "shell", input: d.command });
    } else if (type === "session.next.shell.ended") {
      Object.assign(tool(value, id, d.callID, d.timestamp), { end: ns(d.timestamp), output: d.output });
    } else if (["session.execution.succeeded", "session.execution.interrupted", "session.execution.failed"].includes(type)) {
      close(id, d.timestamp, d.error ?? (type.endsWith("interrupted") ? "execution interrupted" : type.endsWith("failed") ? "execution failed" : null));
    }
  }
  return { v1, v2,
    prompt(input, output) { start(input.sessionID, output.message.id, output.message.time?.created, output.parts); },
    configure(config) { suppressModels ||= JSON.stringify(config).includes("api.respan.ai"); },
  };
}

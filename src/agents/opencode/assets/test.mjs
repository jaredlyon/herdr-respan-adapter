import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { collector, enabled } from "./bridge.js";

const fixture = async (name) => JSON.parse(await readFile(new URL(`../fixtures/${name}.json`, import.meta.url), "utf8"));
const v1 = await fixture("v1");
const v2 = await fixture("v2");
const expected = await fixture("batch");
const originalNow = Date.now;
Date.now = () => 1200;
delete process.env.OPENCODE_VERSION;
delete process.env.HERDR_RESPAN_PROXY;

function harness() {
  const batches = [];
  return { batches, capture: collector({ owns: (id) => id === "s1", cwd: "/repo", send: (b) => batches.push(b) }) };
}
try {
  const first = harness();
  for (const event of v1.slice(0, -1)) first.capture.v1(event);
  assert.equal(first.batches.length, 0, "no per-chunk or per-message subprocess delivery");
  first.capture.v1(v1.at(-1));
  assert.deepEqual(first.batches, [expected]);
  for (const event of v1) first.capture.v1(event);
  assert.equal(first.batches.length, 1, "duplicate settlement/replay does not duplicate a turn");

  const second = harness();
  for (const event of v2.slice(0, -1)) second.capture.v2(event);
  assert.equal(second.batches.length, 0);
  second.capture.v2(v2.at(-1));
  const expectedV2 = structuredClone(expected);
  expectedV2.events[0].prompt = { text: "List the files" };
  expectedV2.events[1].input = [{ role: "user", content: { text: "List the files" } }];
  assert.deepEqual(second.batches, [expectedV2]);
  for (const event of v2) second.capture.v2(event);
  assert.equal(second.batches.length, 1);

  const foreign = harness();
  for (const event of v1) foreign.capture.v1(JSON.parse(JSON.stringify(event).replaceAll('"s1"', '"other-pane"')));
  for (const event of v2) foreign.capture.v2(JSON.parse(JSON.stringify(event).replaceAll('"s1"', '"other-pane"')));
  assert.equal(foreign.batches.length, 0, "server-global events are not pane ownership");

  const partial = harness();
  for (const event of v1.slice(0, 5)) partial.capture.v1(event);
  partial.capture.v1({ type: "session.error", properties: { sessionID: "s1", error: { name: "APIError", data: { message: "request failed" } } } });
  partial.capture.v1(v1.at(-1));
  const failed = partial.batches[0];
  assert.equal(failed.events.at(-1).status, "error");
  assert.equal(failed.events.at(-1).error, "request failed");
  assert.equal(failed.events.find((e) => e.type === "chat").status, "error");
  assert.equal(failed.events.find((e) => e.type === "tool").status, "error");

  const missing = harness();
  const unknown = structuredClone(v2);
  delete unknown[7].data.tokens;
  for (const event of unknown) missing.capture.v2(event);
  assert.deepEqual(missing.batches[0].events[1].usage, { input_tokens: null, output_tokens: null,
    cache_read_input_tokens: null, cache_creation_input_tokens: null, reasoning_tokens: null });

  const suppressed = harness();
  suppressed.capture.configure({ provider: { custom: { options: { baseURL: "https://api.respan.ai" } } } });
  for (const event of v1) suppressed.capture.v1(event);
  assert.deepEqual(suppressed.batches[0].events.map((e) => e.type), ["turn_start", "tool", "turn_end"]);
  process.env.HERDR_RESPAN_PROXY = "1";
  const proxied = harness();
  for (const event of v2) proxied.capture.v2(event);
  assert.deepEqual(proxied.batches[0].events.map((e) => e.type), ["turn_start", "tool", "turn_end"]);
  delete process.env.HERDR_RESPAN_PROXY;

  const subagent = harness();
  for (const event of v1.slice(0, 3)) subagent.capture.v1(event);
  subagent.capture.v1({ type: "session.created", properties: { sessionID: "child", info: { id: "child", parentID: "s1", title: "explore", time: { created: 1030 } } } });
  subagent.capture.v1({ type: "message.part.updated", properties: { sessionID: "s1", part: { id: "task-part", sessionID: "s1", messageID: "a1", type: "tool", callID: "task-call", tool: "task", state: { status: "running", input: { subagent_type: "explore" }, metadata: { sessionId: "child" }, time: { start: 1030 } } } } });
  // Captured V1 child admission: prompt hook, user message, then its text part (IDs normalized).
  const childUser = { id: "child-user", role: "user", sessionID: "child", time: { created: 1031 },
    agent: "explore", model: { providerID: "mock", modelID: "mock-model" } };
  const childPrompt = { type: "text", text: "Say the deterministic mock text. Do not use any tools.",
    messageID: "child-user", sessionID: "child", id: "child-prompt" };
  subagent.capture.prompt({ sessionID: "child", agent: "explore", messageID: "child-user" },
    { message: childUser, parts: [childPrompt] });
  subagent.capture.v1({ type: "message.updated", properties: { sessionID: "child", info: childUser } });
  subagent.capture.v1({ type: "message.part.updated", properties: { sessionID: "child", part: childPrompt, time: 1032 } });
  const childMessage = structuredClone(v1[9]);
  childMessage.properties.sessionID = "child";
  childMessage.properties.info.sessionID = "child";
  childMessage.properties.info.parentID = "child-user";
  subagent.capture.v1(childMessage);
  subagent.capture.v1({ type: "session.idle", properties: { sessionID: "child" } });
  assert.equal(subagent.batches.length, 0, "child idle does not close the parent turn");
  subagent.capture.v1(v1[9]);
  subagent.capture.v1(v1.at(-1));
  const subEvents = subagent.batches[0].events;
  assert.equal(subEvents.find((e) => e.type === "subagent_start").parent_id, "s1:task-call");
  assert.equal(subEvents.find((e) => e.type === "chat" && e.id === "child:a1").parent_id, "child");
  const childChats = subEvents.filter((e) => e.type === "chat" && e.parent_id === "child");
  assert.equal(childChats.length, 1, "child user text is not an assistant chat");
  assert.equal(childChats[0].id, "child:a1");
  assert.ok(!subEvents.some((e) => e.error === "message not completed"), "no phantom failed chat");
  assert.deepEqual(subEvents[0].prompt, [{ type: "text", text: "List the files" }], "child prompt does not replace root prompt");

  const queued = harness();
  for (const event of v1.slice(0, 10)) queued.capture.v1(event);
  const nextTurn = JSON.parse(JSON.stringify(v1.slice(0, 10)).replaceAll('"p1"', '"p2"').replaceAll('"a1"', '"a2"'));
  for (const event of nextTurn) queued.capture.v1(event);
  queued.capture.v1(v1.at(-1));
  assert.deepEqual(queued.batches.map((b) => b.events[0].turn_id), ["p1", "p2"]);
  assert.deepEqual(queued.batches.map((b) => b.events.find((e) => e.type === "chat").id), ["s1:a1", "s1:a2"]);

  delete process.env.HERDR_ENV;
  assert.equal(enabled(), false);
  Object.assign(process.env, { HERDR_ENV: "1", HERDR_PANE_ID: "pane", HERDR_SOCKET_PATH: "/tmp/synthetic-herdr" });
  assert.equal(enabled(), true);
  process.env.HERDR_RESPAN = "off";
  assert.equal(enabled(), false);
  console.log("OpenCode synthetic V1/V2 collector checks passed");
} finally { Date.now = originalNow; }

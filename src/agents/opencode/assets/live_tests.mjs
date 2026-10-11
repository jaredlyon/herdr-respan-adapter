import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { collector } from "./bridge.js";

const fixtures = JSON.parse(await readFile(new URL("../fixtures/live-v1.json", import.meta.url), "utf8"));
for (const fixture of fixtures) {
  const batches = [];
  const capture = collector({ owns: (id) => id === fixture.session_id, cwd: "/mock/work", send: (batch) => batches.push(batch) });
  for (const event of fixture.events) {
    // The run server receives chat.message; the TUI only consumes native events.
    if (event.type === "chat.message") {
      if (fixture.client === "run") capture.prompt(event.input, event.output);
    } else capture.v1(event);
  }
  assert.equal(batches.length, fixture.client === "tui" ? 2 : 1);
  const counts = [];
  for (const batch of batches) {
    const chats = batch.events.filter((e) => e.type === "chat");
    const tools = batch.events.filter((e) => e.type === "tool");
    assert.equal(batch.events[0].type, "turn_start");
    assert.equal(batch.events.at(-1).type, "turn_end");
    assert.equal(batch.events.at(-1).status, "ok");
    for (const chat of chats) {
      assert.equal(chat.status, "ok");
      assert.equal(chat.model, "mock-model");
      assert.equal(chat.provider, "mock");
      assert.deepEqual(chat.usage, { input_tokens: 11, output_tokens: 7, cache_read_input_tokens: 3,
        cache_creation_input_tokens: 0, reasoning_tokens: 0 });
    }
    for (const tool of tools) {
      assert.equal(tool.name, "bash");
      assert.equal(tool.status, "ok");
      assert.equal(tool.input.command, "echo hra-e2e");
      assert.match(tool.output, /hra-e2e/);
    }
    counts.push([chats.length, tools.length]);
  }
  assert.deepEqual(counts, fixture.counts);
  for (const event of fixture.events) if (event.type !== "chat.message") capture.v1(event);
  assert.equal(batches.length, fixture.counts.length, "replayed native events must not duplicate turns");
}
console.log("OpenCode 1.18.35 captured V1 run/full-TUI replay checks passed");

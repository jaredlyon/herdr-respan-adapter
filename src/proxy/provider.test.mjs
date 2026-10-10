import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
const directory = fs.mkdtempSync(path.join(os.tmpdir(), "respan-provider-fixture-"));
try {
  const registration = path.join(directory, "run.json");
  fs.writeFileSync(registration, JSON.stringify({ upstream: {} }));
  const base = "http://127.0.0.1:123/r/synthetic";
  const source = fs.readFileSync(new URL("./provider.js", import.meta.url), "utf8")
    .replaceAll("__BASE__", JSON.stringify(base))
    .replaceAll("__REGISTRATION__", JSON.stringify(registration));
  const { default: factory } = await import("data:text/javascript;base64," + Buffer.from(source).toString("base64"));
  const handlers = new Map();
  const providers = [];
  factory({ on(event, fn) { handlers.set(event, fn); }, registerProvider(name, options) { providers.push({ name, options }); } });
  const model = { provider: "custom-openai", api: "openai-responses", baseUrl: "https://example.invalid/v1", apiKey: "untouched-auth" };
  handlers.get("session_start")({}, { model });
  assert.equal(model.baseUrl, base + "/openai/__provider/custom-openai");
  assert.equal(model.apiKey, "untouched-auth");
  assert.deepEqual(providers, [{ name: "custom-openai", options: { baseUrl: model.baseUrl } }]);
  assert.equal(JSON.parse(fs.readFileSync(registration)).upstream["openai/custom-openai"], "https://example.invalid/v1");
  handlers.get("before_agent_start")({}, { model });
  assert.equal(providers.length, 1);
  const next = { provider: "anthropic", api: "anthropic-messages", baseUrl: "https://example.invalid" };
  handlers.get("before_agent_start")({}, { model: next });
  assert.equal(next.baseUrl, base + "/anthropic/__provider/anthropic");
  assert.throws(() => handlers.get("session_start")({}, { model: { provider: "unsafe", api: "openai-responses", baseUrl: "https://user:secret@example.invalid" } }), /credential-free/);
  assert.equal(JSON.stringify(JSON.parse(fs.readFileSync(registration))).includes("secret"), false);
  console.log("provider asset synthetic check passed: in-memory URL change, auth unchanged, fixed per-provider registration, no config writes");
} finally {
  fs.rmSync(directory, { recursive: true, force: true });
}

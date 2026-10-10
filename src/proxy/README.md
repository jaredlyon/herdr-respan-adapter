# Local request proxy

`herdr-respan run <agent> -- <args>` launches an agent with invocation-local endpoint overrides. `run --protocol anthropic|openai|gemini -- <command> <args>` supplies the common SDK environment variables (best-effort for arbitrary commands). The loopback daemon is started only on demand; Tokio's runtime exists only inside `proxy serve`, never in hooks or the launch wrapper. `HERDR_RESPAN=off` launches without interception. Proxy startup failure launches the original command without interception. Cursor mode refuses because its model calls are server-side. Hermes uses its native adapter rather than redundant interception.

No agent setup edits are made by this slice. No key variable, OAuth store, or user config is written. The wrapper sets `HERDR_RESPAN_RUN_ID` and sets `HERDR_RESPAN_PROXY=1` only when model traffic is actually routed through the proxy. In particular, subscription Copilot uses its native metadata-only OTel file exporter and does **not** set the proxy flag. Child stdio and foreground console/process group are inherited; the wrapper survives Ctrl-C so it can remove the run registration and return the child's exit status (128 + signal on Unix).

| Agent | Invocation override |
|---|---|
| Claude | `ANTHROPIC_BASE_URL`; original upstream preserved |
| Codex | prepend `-c openai_base_url="<local>"`; API-key `/v1` or ChatGPT `/backend-api/codex`, retaining bearer/account/beta headers |
| Gemini | `GOOGLE_GEMINI_BASE_URL` plus separate `CODE_ASSIST_ENDPOINT` route to Code Assist |
| pi / omp | temporary `-e <state>/proxy/runs/<run>/provider.js`; `registerProvider(provider, {baseUrl})` only, plus the current in-memory model object's URL; no `setModel` or auth override |
| OpenCode | merge V1 `provider.*.options.baseURL` and V2 `providers.*.settings.baseURL` into invocation-local `OPENCODE_CONFIG_CONTENT`, preserving its other fields |
| Qwen | `OPENAI_BASE_URL`, `OPENAI_API_BASE`; documented OpenAI-compatible/API-key route (legacy OAuth is discontinued) |
| Kimi | `KIMI_BASE_URL`, `OPENAI_BASE_URL`, `OPENAI_API_BASE` |
| Grok | `GROK_MODELS_BASE_URL`, `GROK_XAI_API_BASE_URL` (not the login/session-service endpoint) |
| Copilot | `COPILOT_OTEL_FILE_EXPORTER_PATH=<state>/agents/copilot/otel.jsonl`; supported legacy BYOK env routing additionally overrides `COPILOT_PROVIDER_BASE_URL` without changing provider type/auth; registry-based BYOK remains native OTel |

`$STATE/proxy.json` contains PID, port, start timestamp, and binary version. `$STATE/proxy/runs/<128-bit-random-run>.json` contains the wrapper PID, agent, non-secret upstream URLs and inherited metadata. Unknown/expired run capabilities are rejected. The pi/omp asset registers fixed upstreams under internal `/<protocol>/__provider/<provider>/` routes; clients cannot name arbitrary upstream URLs. Upstream URLs containing credentials, query strings, or fragments are rejected rather than persisted. Run JSON and temporary extensions are removed on normal/interrupt exit; stale registrations are reclaimed by the daemon. The daemon exits after its configured idle timeout only when no requests or live runs remain.

HTTP methods, request bodies, streaming response chunks, status and end-to-end header values pass through unchanged. Host is changed to the fixed upstream authority, as required by HTTP. Transport framing is managed by Hyper. WebSocket Upgrade handshakes retain keys/auth/beta/account headers, and the upgraded stream is copied byte-for-byte, including original masked, fragmented, binary and control frames. Observation never controls forwarding.

One `source=proxy`, `log_type=chat` record is emitted per model request, with run ID for Core's flush-time parenting. Parsers handle Anthropic Messages, OpenAI Chat Completions, OpenAI Responses (SSE, JSON and WebSocket), and Gemini/Code Assist JSON and SSE. Codex `generate=false` prewarms are not model spans. Usage preserves unknown versus zero and final cache/reasoning breakdowns. Request input is the first full request and subsequently the newly added message/input/content suffix plus changed system instructions. Status, latency and observed first-token latency are metadata. Only Core caps, scrubs and writes records; request/response authentication headers are never captured.

## Limits

- This is local mock/source verification, **not** live provider or subscription authentication verification. No providers, Respan endpoints, real agent runs or user configs are used in tests.
- Model protocols outside Anthropic/OpenAI/Gemini are not translated. Provider policy/config precedence can defeat an environment override (especially Qwen custom registries, OpenCode OAuth rewrites, Grok policy pins); use native adapters for those paths rather than claiming complete request capture.
- pi/omp routing observes the selected model at session start and before each user agent run. Auxiliary calls using unrelated, independently cached model objects may require another native adapter source. The extension changes only the live model URL and provider registry in memory.
- Capture parsing is bounded to 8 MiB per JSON/SSE event/WebSocket message and request copy; content text is bounded to the capture ceiling, and Core alone applies the configurable field cap and scrubbing. Oversized/malformed observations lose details, not traffic. Full upstream request/response bytes remain untouched. WebSocket compressed messages are forwarded but not parsed; current Responses traffic does not require negotiated compression.
- Timings are local proxy wall-clock observations, not provider server timings. No tokens or model identity are invented for malformed/unknown bodies. No count-token, model discovery, health or OAuth helper request is mislabeled as model generation.

## Synthetic fixture provenance

Fixtures live inline in `parse.rs` and `tests.rs`, contain synthetic model/token/account/key strings only, and are derived from:

- Anthropic Messages and Claude endpoint/header/SSE schemas: <https://code.claude.com/docs/en/llm-gateway-protocol>.
- OpenAI Codex `4bad6d78e9b50f9fa8bd941f1db012ed491ad2da`: `codex-rs/codex-api/src/sse/responses.rs:118-155,846-870`, `core/src/client.rs:11-26,175,1384-1387`, `model-provider-info/src/lib.rs:427-445`; `response.create`, `response.*`, usage and `generate=false`.
- Gemini CLI `9b6e0265d16bbd29ca51e33c9e0c01dc4cec5e83`: `packages/core/src/code_assist/server.ts:475-529` and `contentGenerator.ts:87-89,351-355`; Code Assist response wrapper and generation endpoints.
- Qwen Code `c3968ace2f54af27b626c3f75ae72cf889533ef9`: `openaiContentGenerator/pipeline.ts:1061-1064,1140-1141` and `converter.ts:1430-1478`; final empty-choice usage chunk and cache/reasoning breakdown.
- OpenCode `055d95bb7e278c94baf06235a52cac79dd13ba67`: `provider/provider.ts:1785-1840`, `config/config.ts:482-489`; V1 invocation-local config. V2 shape: <https://opencode.ai/v2/docs/providers/#endpoint>.
- pi `packages/coding-agent/src/core/extensions/types.ts`, documented `registerProvider(name,{baseUrl})`: <https://github.com/earendil-works/pi/blob/main/packages/coding-agent/src/core/extensions/types.ts>; omp source `0af0f96b62c218c3d058a203c394885e39be477f`, `extensibility/extensions/types.ts:1654-1684` and `runner.ts:794-798`.
- Copilot BYOK/exporter: <https://docs.github.com/en/copilot/how-tos/copilot-cli/customize-copilot/use-byok-models> and <https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-command-reference#opentelemetry-monitoring>.
- MoonshotAI Kimi `9ab1286b8fe4e6bcd116949a27ce5e0ac3389c82`: `src/kimi_cli/llm.py:263-341`; Grok Build `2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8`: `docs/user-guide/26-config-reference.md#endpoints`.

Runnable checks: `node src/proxy/provider.test.mjs` (synthetic host API, no agent), `cargo test proxy`, `cargo test run::tests`, `cargo test`, `cargo clippy -- -D warnings`. Mock upstream tests assert incremental SSE delivery for all protocols, unchanged auth/custom header values and request body bytes, actual panic-injected forwarding, unknown-run rejection, two-way WebSocket binary/text frames, final usage, and warmup exclusion. Core's `tests::fake_hook_fail_open_and_wall_time` measures hook p95 independently of the detached sender; compare release executable sizes before/after the proxy dependency change.

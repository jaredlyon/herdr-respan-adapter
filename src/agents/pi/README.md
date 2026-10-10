# pi

An owned TypeScript observer, loaded by pi's jiti extension loader. It sends one
bridge-v1 batch to `herdr-respan hook pi` when the user run settles. The Rust
shared bridge owns all span keys, redaction, toggles, spool and export. No Respan
SDK, API key, network exporter or additional npm package is installed in pi.

## Setup and removal

`setup pi` drops **only** `herdr-respan.ts` into
`<PI_CODING_AGENT_DIR or ~/.pi/agent>/extensions/`. A `~/` relocation is expanded.
With `--project <dir>`, it uses `<dir>/.pi/extensions/` instead. The existing
`herdr-agent-state.ts`, settings, model configuration and authentication files
are untouched. The asset contains JSON-escaped absolute binary/state/config
paths substituted for `__HERDR_RESPAN_BIN__`, `__HERDR_RESPAN_STATE__` and
`__HERDR_RESPAN_CONFIG__`; it spawns without a shell. Ownership id:
`herdr-respan-pi-extension`. Shared removal deletes only the unchanged owned
file; shared startup refresh updates the owned asset.

Requires the pi **1.1.0 event contract** inspected below: `agent_settled` and
`ctx.mode` must exist. Earlier versions were not qualified. Setup prints this
requirement; it does not install or upgrade pi. Restart/reload pi after setup.

## Captured spans

- **Turn:** `before_agent_start`/`agent_start` through `agent_settled`, spanning
  internal model turns, tool loops, retries and automatic continuation. Native
  session id is the thread id; absolute session-file path is optional. Shutdown
  closes an interrupted turn as an error.
- **Chat:** each completed assistant message, with its request context, complete
  response content, serving-model override, provider and per-message
  input/output/cache-read/cache-write/reasoning tokens when provided. Missing
  counters remain unknown, while actual zeroes are retained. `usage.cost.total`
  is a local model-rate estimate, sent as `estimated_cost_usd` by Core.
- **Tool:** execution start/end keyed by native `toolCallId`; failures and
  unfinished calls have error status. Subagent tools are ordinary tool spans.
- **Compaction:** a tool span named `compaction` with reason, tokens-before,
  summary/result and failure/abort status. Manual compaction outside a user run
  receives its own turn and one delivery process.

The gate matches herdr's pi asset: `HERDR_ENV=1`, socket/pane ids present and
`ctx.mode=tui|print` (not RPC). Print-mode invocations inside a real herdr
pane are traced; herdr's interactive state-reporting restriction is not a tracing
restriction. `HERDR_RESPAN=off` drops collection at
startup/delivery; workspace pause is enforced by Rust before ingestion.
`HERDR_RESPAN_PROXY=1` or a selected model base URL containing `api.respan.ai`
suppresses native chat spans, leaving turn/tool spans. Core repeats suppression.
All callbacks are observational, catch errors and return no protocol decisions.
No process is launched for chunks or intermediate `agent_end` events.

## Limits

pi exposes **no native subagent lifecycle/correlation events** in the inspected
contract. Consequently no invented child-agent spans or child model usage are
reported. This limitation was explicitly accepted by the implementation owner.
`parentToolCallId` represents nested tools, not a subagent id; it is not sent as
bridge `parent_id`. These nested tool spans are siblings under the turn.
Native message completion measures logical requests, not every provider HTTP
retry or all auxiliary/compaction model calls; use the proxy for that fidelity.
The extension buffers content in process until settlement. Abrupt process death
before delivery loses that batch. The detached child ignores stdout/stderr and
fails open if the binary is missing. The loopback E2E below qualifies real print-mode runtime behavior, not
hook latency or real-provider behavior. Agent version is unknown in the batch because
the event context has no documented version field.

## Fixtures and source provenance

All samples are synthetic, with no real user content, session files or secrets:

- `fixtures/turn.json`: synthetic from
  [`earendil-works/pi@42a3497d03ad17e308a2299fa824727894f2c0ec`,
  `packages/coding-agent/src/core/extensions/types.ts:777-817,881-920,1004-1095`](https://github.com/earendil-works/pi/blob/42a3497d03ad17e308a2299fa824727894f2c0ec/packages/coding-agent/src/core/extensions/types.ts#L777-L817)
  and [`packages/ai/src/types.ts:455-479,565-602`](https://github.com/earendil-works/pi/blob/42a3497d03ad17e308a2299fa824727894f2c0ec/packages/ai/src/types.ts#L455-L479).
  Only fields consumed by the observer and replay timing are included; this is
  not represented as a captured complete native event stream.
- `fixtures/batch.json`: golden bridge-v1 translation of that fixture. Random
  turn/compaction ids are normalized to `t1`/`c1` only in the replay assertion.
- Extension gates: existing herdr source
  `src/integration/assets/pi/herdr-agent-state.ts:11-20,229-239`; version notice
  from the pinned pi `packages/coding-agent/package.json` (`1.1.0`).

Checks: `cargo test agents::pi`; `cargo clippy -- -D warnings`;
`~/.local/bin/bun test src/agents/pi/extension.test.ts`. Bun's transpiler performs
an asset syntax check (not a full TypeScript type check). Rust parses the golden
batch through the real bridge, checks the resulting spool tree, duplicate and
foreign-payload rejection, proxy suppression, relocation, non-destructive setup
and removal. The Bun replay checks batch shape, once-at-settlement delivery,
usage/cost, gates, manual compaction failure and fail-open callbacks.


## Real binary E2E

Qualified pi 1.1.0 on Linux aarch64 with
`HRA_BIN=target/release/herdr-respan bash scripts/e2e/run_pi.sh` (use an absolute
binary path when overriding `HRA_BIN`). The driver isolates HOME/XDG and
`PI_CODING_AGENT_DIR`, uses an explicit dummy-key OpenAI Chat mock provider,
`--offline --no-approve -p --tools bash`, and retains evidence under
`~/.hra-e2e/pi/run-*`. It creates/stops only `hra-e2e-pi` and its loopback mocks.

The received tree is one turn + one chat for text, and one turn + two chats +
one bash tool for the harmless echo. Assertions bind chats to all three native
completed-assistant messages, check parentage, exact usage, thread ids and
herdr pane/workspace/machine metadata, and reject duplicate/orphan spans. pi
reports **input=8, output=7, cacheRead=3, cacheWrite=0, reasoning=0**: its OpenAI
adapter subtracts the 3 cached tokens from mock prompt_tokens=11. These are the
native exposed counters, not invented total-input=11. Model-rate cost is zero
for the mock model. The same tool run outside herdr makes two local inference
requests but no OTLP delivery. Removal retains provider/settings JSON values
and the foreign capture extension, deleting only the owned observer.

`fixtures/live-print-tool.json` is **captured live 2026-10-09** from pi 1.1.0
against `scripts/e2e/mock_provider.py`, artifacts
`~/.hra-e2e/pi/run-dgQCKkas/native.jsonl`. It retains only observer-consumed event
and message fields for the tool turn; generated system instructions, session
paths and machine-specific context are omitted/scrubbed. No real keys or user
content are present. The Bun regression replays these events in both print and
TUI modes, proving zero-start-usage is not emitted and only settlement delivers
the completed usage + echo result.

Mocks do not qualify subscription auth, real provider retries/auxiliary calls,
real Respan ingestion, latency, native subagent lifecycles, or abrupt-kill
recovery. TUI is fixture-replayed, not interactively driven by this print-mode
E2E.

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
`ctx.mode=tui` (not headless print/RPC). `HERDR_RESPAN=off` drops collection at
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
fails open if the binary is missing. No live-runtime or hook-latency claim is
made by synthetic replay tests. Agent version is unknown in the batch because
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

# OpenCode

## Mechanism and ownership

A dependency-free JavaScript plugin buffers native user messages, settled assistant messages/steps, full text/reasoning parts and tool results. Streaming deltas never spawn a process. On the root session's idle/execution completion it sends one bridge batch per turn to the stable binary using a shell-free argv array and JSON stdin:

```text
herdr-respan hook opencode --state-dir <state> --config-dir <config>
```

The Rust adapter is `common::bridge::hook`; bridge identity validation rejects other agents, native hook payloads and unsupported versions. Rust owns keys, parenting, toggles, redaction, caps, persistence and delivery. No provider/Respan traffic is made by the JavaScript assets.

The same pane gates as herdr's OpenCode integration apply: `HERDR_ENV=1`, `HERDR_PANE_ID` and `HERDR_SOCKET_PATH`. `HERDR_RESPAN=off` also disables the assets; the Rust dispatcher applies workspace pause state. Spawn failures and native callback errors are fail-open and never write to the agent's stdout.

The server entrypoint deliberately handles **only local V1 run/Mini**, with root ownership established by that client's `chat.message`, not server-global `session.created`. V1 full TUI and V2 use separate pane-local lifecycle entrypoints (`tui(api)` and `setup(api)`) subscribed to their own UI event stream. The selected route bounds ownership, including sessions on shared/remote servers. Browsing a descendant retains the selected subtree; selecting another root stops attributing the former root's events to this pane. Historical turns completed before plugin subscription are not backfilled.

## Setup and removal

Global setup honors `OPENCODE_CONFIG_DIR`, otherwise uses `~/.config/opencode`. This follows the installed agent's native config relocation; XDG directories separately relocate state/cache. `--project <dir>` uses `<dir>/.opencode`.

Owned assets:

| File | Purpose |
| --- | --- |
| `plugins/herdr-respan.js` | V1 local server plugin; V2 server entrypoint is intentionally inert |
| `herdr-respan/bridge.js` | Shared native-event buffering and bridge delivery |
| `herdr-respan-tui.js` | Pane-local V1 and V2 TUI implementations |
| `herdr-respan/tui.js` | V2 package-directory TUI entrypoint |

Setup substitutes `__HERDR_RESPAN_BIN__`, `__HERDR_RESPAN_STATE__`, `__HERDR_RESPAN_CONFIG__` with JSON-escaped absolute path literals, so quotes, backslashes and spaces never pass through a shell. Local server files auto-load: **no `plugin` entry in opencode.json is added**.

TUI registration is additive and marker-owned:

- `tui.jsonc` (or existing `tui.json` when no JSONC file exists): append `"./herdr-respan-tui.js"` to singular `plugin`.
- `cli.json`: append `"./herdr-respan"` to plural `plugins`.

The actual textual insertion precedes existing array elements without modifying/reordering those elements. JSONC comments, strings, trailing commas and herdr entries remain byte-identical. Existing registrations are not adopted as owned entries. Removal deletes only our marker-owned array element and owned files. A property created by setup is removed only while empty; plugins appended by the user survive uninstall; modified owned assets are protected by Core's removal checks.

Like herdr, setup **defers creating cli.json** when V1 `tui.json` or the OpenCode state directory's `kv.json` would otherwise be migrated on first V2 startup. State directory is `$XDG_STATE_HOME/opencode`, otherwise `~/.local/state/opencode`. The setup notice instructs the user to start V2 once and rerun setup; an existing cli.json is always eligible for additive registration.

## Spans and limits

- Turn: native user message ID, prompt, final root text/reasoning, time window and terminal error.
- Chat: once per completed V1 assistant message or settled V2 step, model/provider when exposed, full-value text/reasoning, five optional request usage counters and agent-reported cost. Unknown counters stay null, not zero. V1 `step-finish` usage is not emitted a second time. Inputs contain the observed turn prompt, **not an invented complete provider conversation**.
- Tool: native call ID, name, input, final result, timestamps and errors; V2 shell events also produce tools. IDs are session-qualified to avoid collisions across children.
- Subagent: V1/V2 child sessions under their root turn; child chat/tools use the child session as parent. V1 task metadata `sessionId` identifies the exact spawning tool. Child idle does not close the root turn. Unknown V2 spawning-call correlation stays null rather than guessing between concurrent tasks.

`HERDR_RESPAN_PROXY=1` suppresses native model spans without suppressing turn/tool/subagent spans. The V1 local server's native config callback also suppresses chats when a resolved config contains `api.respan.ai`. The pane-local TUI API's resolved provider/baseURL config is not established by the cited API; gateway-only double-count suppression in that topology needs installed-release evidence. No provider or model is inferred from a model name.

V1 compatibility requires **1.18.29+**, which accepts the object-style plugin entrypoint. V2 lifecycle wiring follows herdr integration **v13 / beta 19242**; settlement translation follows the pinned schema below. Hosted V2 plugin APIs are still evolving and are **not claimed as live-verified**. Selecting a session mid-turn cannot recover a missed user/admission event; only turns observed from their beginning are captured. Buffers are in-memory until completion, so abrupt client termination loses an unfinished turn. This is intentionally not a second spool implementation.

## Provenance and checks

The original fixtures are **synthetic**; the live fixture below is captured against loopback mocks. Neither contains personal data, credentials or real provider calls:

- `fixtures/v1.json`: `anomalyco/opencode@055d95bb7e278c94baf06235a52cac79dd13ba67`, [`packages/schema/src/v1/session.ts:210-495,571-642`](https://github.com/anomalyco/opencode/blob/055d95bb7e278c94baf06235a52cac79dd13ba67/packages/schema/src/v1/session.ts); final completion semantics from [`packages/opencode/src/session/processor.ts:435-470,608-610`](https://github.com/anomalyco/opencode/blob/055d95bb7e278c94baf06235a52cac79dd13ba67/packages/opencode/src/session/processor.ts).
- `fixtures/v2.json`: the same commit's [`packages/schema/src/session-event.ts:25-390`](https://github.com/anomalyco/opencode/blob/055d95bb7e278c94baf06235a52cac79dd13ba67/packages/schema/src/session-event.ts), [`model.ts:15-21`](https://github.com/anomalyco/opencode/blob/055d95bb7e278c94baf06235a52cac79dd13ba67/packages/schema/src/model.ts), and [`session-delivery.ts:7-8`](https://github.com/anomalyco/opencode/blob/055d95bb7e278c94baf06235a52cac79dd13ba67/packages/schema/src/session-delivery.ts). Execution completion envelope follows herdr's V2 `api.data.listen` consumer below.
- `fixtures/batch.json`: exact golden output produced by the V1 asset from `v1.json`, with a fixed synthetic clock of 1200 ms. A Rust test ingests these exact bytes and asserts the spooled turn→chat/tool tree, usage, cost, identity rejection, idempotency and proxy suppression. JavaScript checks compare both V1 and V2 asset output against this golden (only native prompt representation differs).
- V1 task-to-child correlation derives from [`packages/opencode/src/tool/task.ts:181-206`](https://github.com/anomalyco/opencode/blob/055d95bb7e278c94baf06235a52cac79dd13ba67/packages/opencode/src/tool/task.ts).
- Gates, server-local lifecycle distinction and pane-local API/registration are based on `herdrdev/herdr@2563803dca97c040beaf3dc3acdcb5a3221b4238`: [`herdr-agent-state.js:118-160,218-224`](https://github.com/herdrdev/herdr/blob/2563803dca97c040beaf3dc3acdcb5a3221b4238/src/integration/assets/opencode/herdr-agent-state.js), [`herdr-tui-session.js:67-126,402-610`](https://github.com/herdrdev/herdr/blob/2563803dca97c040beaf3dc3acdcb5a3221b4238/src/integration/assets/opencode/herdr-tui-session.js), [`opencode_config.rs:64-90`](https://github.com/herdrdev/herdr/blob/2563803dca97c040beaf3dc3acdcb5a3221b4238/src/integration/opencode_config.rs), [`env.rs:135-145`](https://github.com/herdrdev/herdr/blob/2563803dca97c040beaf3dc3acdcb5a3221b4238/src/integration/env.rs).

Runnable checks (no installed agents or external endpoints required):

```sh
~/.cargo/bin/cargo test agents::opencode
~/.cargo/bin/cargo clippy -- -D warnings
~/.local/bin/node src/agents/opencode/assets/test.mjs
# Run node --check on each assets/*.js / assets/*.mjs file as well.
```

## Real V1 mock-provider E2E

`bash scripts/e2e/run_opencode.sh` uses the installed OpenCode **1.18.35** on Spark, two loopback-only shared mocks and a dedicated `hra-e2e-opencode` herdr session. Build with `~/.cargo/bin/cargo build --release`; the driver defaults to this worktree's `target/release/herdr-respan` (`HRA_BIN` may override it). HOME, every XDG directory and `OPENCODE_CONFIG_DIR` are isolated; the launcher constructs a credential-free environment, disables share/model fetching/updating, and exposes only the mock provider. A preseeded native `$schema` preference avoids mistaking OpenCode's own first-run config update for a removal defect. Only `echo hra-e2e` is permitted: deny-all rules precede the exact allow rule because OpenCode uses last-match permission precedence.

The driver exercises the **V1 local server entrypoint** using two `opencode run` calls, then the **pane-local full TUI entrypoint** using two prompts in one interactive client. The expected received tree is four turn traces: twice `turn → chat`, twice `turn → chat, bash, chat` (12 spans, three native sessions). Each adapter chat exposes **input=11, output=7, cache.read=3, cache.write=0, reasoning=0**. Input is the total prompt including cached tokens: the adapter adds OpenCode's three cached tokens to its eight uncached input tokens, matching the mock's OpenAI prompt total of 11. Settlement is per-message, not a cumulative session total or a second step-finish span. The tool input/output must prove `echo hra-e2e → hra-e2e`. Herdr pane/workspace/machine IDs, native thread IDs, OK statuses, correct parentage, unique delivery, no outside-herdr OTLP and semantic setup/remove restoration are asserted. The outside run uses a fresh non-herdr PTY: the tested redirected-stdin process completed responses but did not exit, while terminal transport exits normally. Cleanup stops only the test's own herdr session and mocks.

`fixtures/live-v1.json` was **captured live 2026-10-10 UTC** (2026-10-09 operator date) with OpenCode 1.18.35 against the loopback provider, from `~/.hra-e2e/opencode/run-bNsUDrxC/native.jsonl`. It retains relevant native user/message/part/status/idle events for the two run clients and one full TUI client; unrelated plugin/session-list events and the outside run are excluded. Native identifiers are consistently pseudonymized and local paths become `/mock/work`. No adapter events are invented. `assets/live_tests.mjs` replays both routes, asserts exact message usage/tool result/cardinality and duplicate-idle/replay suppression:

```sh
node src/agents/opencode/assets/live_tests.mjs
bash scripts/e2e/run_opencode.sh
```

The E2E found and fixed **setup ignoring `OPENCODE_CONFIG_DIR`**, which previously installed plugins in a different directory from the real isolated agent. The relocation/project-precedence regression is in `setup.rs`. No event translation fix was needed. Windows spawning, attached/shared-server ownership, V2 installed-release compatibility, provider authentication/proxy/WebSocket behavior and real Respan ingestion remain unverified by these mocks.

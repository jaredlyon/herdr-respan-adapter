# Grok Build

Targets the official [xai-org/grok-build](https://github.com/xai-org/grok-build), not third-party similarly named CLIs. Source contract pinned to `2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8`; an earliest release supporting every field is not established. Native runtime is qualified on Linux aarch64 with Grok Build 1.0.50 (`c58f321264ba`) and herdr 0.9.3 against loopback-only providers; real provider authentication remains unverified.

## Setup

`setup grok` non-destructively inserts nested matcher groups into `<GROK_HOME or ~/.grok>/hooks/herdr-respan.json`, or `<project>/.grok/hooks/herdr-respan.json` with `--project`. Each group contains a command produced by the shared shell-quoting helper, with timeout 5 seconds. Events: SessionStart, UserPromptSubmit, PreToolUse, PostToolUse, PostToolUseFailure, Stop, StopFailure, StopCancelled, SubagentStart, SubagentStop, SessionEnd. No permission hooks are registered. The adapter prints nothing; the shared CLI always exits zero on hook failures.

Existing `herdr.json`, settings, credentials and authentication are untouched. Removal deletes only owned entries. Project hooks require folder trust; managed policy can disable hooks. Windows uses PowerShell by default, honoring `GROK_SHELL=cmd|bash|gitbash|git-bash|pwsh|powershell` at setup. Keep that shell choice stable after setup; changing it requires `remove grok` followed by `setup grok` so quoted owned commands are replaced. Native Unix hooks always run through `sh -c`.

## Coverage

- **Turn spans:** native `promptId` and `sessionId`, prompt and final output; stop failure/cancellation recorded as error. Missing promptId uses the source hook timestamp, not a shared pane identifier.
- **Chat spans:** complete assistant records incrementally read from `chat_history.jsonl` beside the hook's `transcriptPath` (`updates.jsonl`). Models come from each assistant's `model_id`, never the current session summary. Input contains available transcript deltas; output includes assistant content/tool calls. No usage or cost is fabricated. Transcript rows lack timing: chat timestamps are observation time and carry `grok.timing_source=hook_observed`.
- **Tool spans:** native `toolUseId`, successful/failed output, exact pre-hook window where available, otherwise native durationMs. Upstream truncation flags are retained alongside content.
- **Subagent spans:** subagentId/type and completed observe phase only, parented to the spawning task tool. The join uses `toolInput.task_id` or typed tool result `subagent_id`/`task_id`; gate-phase SubagentStop does not complete a child. Start/end may precede the spawn result, so they are retained until that result arrives. No concurrent child is matched by name/time.

Request usage comes from the opt-in proxy under `herdr-respan run grok`. When that proxy is active, this adapter emits no chat/model spans, avoiding double counting, but retains turns/tools/subagents. Per-turn/session usage totals, diagnostic sampling logs, and current_model_id are not read. External Grok OTel requires a separate logs receiver and is intentionally not enabled.

Native cursor/correlation state lives only in `$STATE/agents/grok/native-<session-hash>.json`; shared state persistence scrubs content. Per-session shared locks protect concurrent hook read/modify/write. SessionStart starts at the existing transcript tail so resumed historical messages are not mislabeled. Complete lines only; shared cursor handles truncation/rotation. Flush settlement retries late transcript tails via Core's bounded settlement schedule. Rows appearing after the last bounded retry need a later hook. Only the raw conversation cache is parsed. If no assistant row appears by the bounded retries, lastAssistantMessage supplies one chat span without model/usage; missing cache still permits turn/tool/subagent coverage. Child-session model/tool streams are not joined across independent session files without an authoritative parent-session mapping. A child whose spawning identity was clipped or absent cannot be safely parented and is not emitted until an exact join is available.

## Imported Claude/Cursor hooks

Grok imports compatible Claude/Cursor configuration. Its command runner sets `GROK_HOOK_EVENT`, `GROK_HOOK_NAME`, `GROK_SESSION_ID`, and `GROK_WORKSPACE_ROOT` after handler-specified env, so handlers cannot override the identity. Claude and Cursor adapters must reject those Grok env signals. Defensively reject a Grok-shaped envelope (`hookEventName`, `sessionId`, `workspaceRoot`, `timestamp`) even though it also includes Claude aliases such as hook_event_name/session_id. This adapter accepts that native camelCase shape and validates env/session/event agreement; it always feeds bridge batches as agent `grok`.

## Source evidence

All links pin the inspected commit:
- [Hook envelope, aliases and payloads](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-hooks/src/event.rs#L365-L580)
- [Protected identity env and execution](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-hooks/src/runner/command.rs#L145-L260)
- [Windows shell detection](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-config/src/shell.rs#L1-L94)
- [Conversation serialization](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-sampling-types/src/conversation.rs#L71-L331)
- [Session storage layout](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-pager/docs/user-guide/17-sessions.md#L24-L54)
- [Spawning task identity and typed result](https://github.com/xai-org/grok-build/blob/2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8/crates/codegen/xai-grok-tools/src/implementations/grok_build/task/mod.rs#L450-L770)

See [fixture provenance](fixtures/README.md). Source-fixture tests make no provider calls. The real-agent E2E runs isolated Grok sessions against loopback mocks only; it never edits the user's configs or calls Respan/providers.

## Real-agent mock qualification

Run `HRA_BIN="$PWD/target/release/herdr-respan" bash scripts/e2e/run_grok.sh` after `cargo build --release`. The driver isolates HOME/XDG/GROK_HOME, installs capture hooks alongside the adapter, and owns only the `hra-e2e-grok` herdr session and its loopback mocks/proxy daemon. Native endpoint overrides include **all three** `GROK_MODELS_BASE_URL`, `GROK_XAI_API_BASE_URL`, and `GROK_CLI_CHAT_PROXY_BASE_URL`. The primary custom model uses the recipe's explicit mock `base_url`; auxiliary title Responses and turn-summary Chat calls remain local too. Only `run_terminal_command` is advertised; canned responses execute `echo hra-e2e`.

Native acceptance asserts two traces: text → one turn/one chat; tool → one turn/two chats/one successful echo tool. Every native chat lacks usage rather than fabricating zero tokens. IDs are unique, all children are parented, native thread IDs and pane/workspace/machine metadata are present, an outside-herdr tool run delivers nothing, and `remove` restores hook JSON semantically. Grok itself adds `[marketplace] default_skills_installs_purged = true` on first launch; model-config removal comparison snapshots immediately before `remove` to isolate plugin effects from this native mutation. Runtime evidence stays under `~/.hra-e2e/grok/run-*`.

The proxy phase uses the same custom Chat model **without** an explicit per-model `base_url`, making it inherit the launcher's endpoint env. This leaves the built-in title model on Responses, so both OpenAI protocols are exercised. The driver's clean wrapper environment unsets the three original endpoint variables and uses `[proxy.upstream]` pointing only at the mock; the launcher must route primary, title and post-Stop summary requests locally. It requires two prompt traces (three proxy chats for text, four chats plus echo tool for the tool prompt), exact input=11/output=7/cache_read=3 including usage aliases, and one proxy chat per received inference request with no duplicate adapter chats.

**Wrapper limit:** Grok 1.0.50's invocation-only config overlay does not allow the `model` table. An explicit `[model.*].base_url` bypasses global endpoint overrides and is not redirected by `run grok`; omit that field for a globally routed model. The E2E changes only its isolated test config between native and proxy phases. Real provider/subscription authentication, external Respan ingestion, subagent spawning and Windows Grok runtime behavior remain outside this mock qualification.

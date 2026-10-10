# Qwen Code

Qwen uses Claude-style command hooks, **not** Gemini's `AfterModel`/`BeforeModel` events. Setup appends owned groups beside herdr's SessionStart hook in `$QWEN_HOME/settings.json` (QWEN_HOME is the actual config directory), or `~/.qwen/settings.json`; `--project DIR` selects `DIR/.qwen/settings.json`. It does not change telemetry, auth, hook enablement, or herdr's entries. Remove deletes only our recorded entries.

## Setup entries

Each of SessionStart, UserPromptSubmit, PreToolUse, PostToolUse, PostToolUseFailure, SubagentStart, SubagentStop, Stop, StopFailure and SessionEnd receives:

```json
{"matcher":"*","hooks":[{"type":"command","command":"<shared quoted binary command> hook qwen --state-dir <STATE> --config-dir <CONFIG>","timeout":5000,"env":{"HERDR_RESPAN_QWEN_HOOK":"1"}}]}
```

The real command is built by the shared hook-command helper with absolute, shell-quoted paths. Windows entries additionally specify `shell: "powershell"` and use `& 'binary' ...`, avoiding Git Bash/cmd autodetection ambiguity. The timeout is **five seconds**: `5000` uses Qwen’s explicit >=1000 legacy-millisecond interpretation, remaining compatible with older millisecond-only releases; values below 1000 mean seconds on current Qwen. Hooks write nothing to stdout. Core gates on HERDR_ENV, opt-out and pause; the adapter additionally requires its per-command environment marker and Qwen-shaped payloads.

Conservative basic-hook floor: Qwen Code **0.14.0**. The complete schema below is sourced at `c3968ace2f54af27b626c3f75ae72cf889533ef9`, not proven on the earliest release. `disableAllHooks`, `--bare`, `--safe-mode`, and untrusted project hooks suppress capture.

## Spans

- Turns: UserPromptSubmit → Stop; StopFailure/session shutdown without Stop closes an error turn. `prompt_id` is used where available, otherwise persisted prompt timestamp identity.
- Chats: one accepted assistant JSONL record per UUID, including tool-call-only rounds. Usage comes from `usageMetadata.promptTokenCount`, `candidatesTokenCount`, `cachedContentTokenCount`, and `thoughtsTokenCount`; unknown fields remain null. Stop's `input_tokens` is **context occupancy**, never request usage. Provider and cost remain unknown unless the source establishes them. Child transcript records may omit model, so no parent-model guess is made.
- Tools: native `tool_use_id`; tool response/failure, arguments, result, duration_ms when exposed, enclosing child agent_id.
- Subagents: SubagentStart/Stop and child transcript. The adapter defers bridge start emission until Stop to read the sibling `.meta.json` sidecar. Matching `agentId` and `parentSessionId` permit causal parenting by `toolUseId`; absent/invalid sidecars leave the child directly under the turn. No timestamp-nearest-tool inference.

Core owns deterministic keys, span parenting, redaction, caps, proxy suppression and spool delivery. Adapter state contains only scrubbed input and correlation/cursor state in `$STATE/agents/qwen/`. Historical transcript lines are skipped on SessionStart/resume; partial final lines wait for a later observation. Stop/SubagentStop also register a six-second final-tail window, drained by Core’s bounded flush settlement callbacks, so transcript writes that lag the hook can still arrive. The shared JSONL cursor handles truncation/rotation, and native parser invocations are serialized by the shared state lock. Hooks first atomically persist a capped/redacted native queue, then try the lock once without waiting; a later hook or detached settlement drains queued events in arrival order under that lock. Queued events retain native timestamps, correlation IDs, pane metadata, run ID and proxy mode; only settlement may wait for the lock. SessionStart snapshots the observed resume tail before queuing, so deferred processing does not skip newly appended records. Metadata-only mode (`content_cap_bytes = 0`) removes prompt/tool/response content before queue or transcript-input state is written; unused native aliases are not persisted. Bridge deduplication prevents repeated hooks/replayed UUIDs from charging usage twice.

### Fidelity limits

Transcript spans describe accepted model rounds, **not** every HTTP attempt, rejected retry, provider error or live streaming chunk. Timestamps bound record observation; they are not exact request latency. Input is recorded user/tool-result content since the preceding assistant round, not a reconstruction of the entire provider request. Reasoning counts may be provider-reported **or estimated from reasoning text** by Qwen; `qwen.reasoning_usage_source=provider_or_text_estimate` preserves that uncertainty. Native child recorder currently omits model identity. A hard kill can omit Stop/SubagentStop; Core's turn timeout handles unfinished turns.

**Qwen OAuth's free tier was discontinued on 2026-04-15 and is no longer offered in the auth dialog** ([official auth documentation](https://qwenlm.github.io/qwen-code-docs/en/users/configuration/auth/)). Current documented routes use API keys/Coding Plan. Legacy OAuth overwrites request baseURL from resource_url on every operation; an OPENAI_BASE_URL proxy override is not reliable for that old mode. This adapter never modifies login/token stores.

## Fixture provenance and verification

The original fixtures below are synthetic; no real secrets, personal data or credentials were used. Source: [QwenLM/qwen-code@c3968ace](https://github.com/QwenLM/qwen-code/tree/c3968ace2f54af27b626c3f75ae72cf889533ef9).

- `fixtures/hooks.json`: `packages/core/src/hooks/types.ts:261-300,812-1335` (base/turn/tool/subagent payloads).
- `fixtures/transcript.jsonl`: `packages/core/src/services/chatRecordingService.ts:362-490,2756-2787` (ChatRecord and accepted assistant usage).
- `fixtures/agent-Explorer-test.jsonl`: `packages/core/src/agents/agent-transcript.ts:713-725,780-799` (child ChatRecord/round usage, intentionally no model field).
- `fixtures/agent-Explorer-test.meta.json`: `packages/core/src/agents/agent-transcript.ts:107-132`; `packages/core/src/tools/agent/agent.ts:3760-3765,4685-4690` (causal parent tool id).
- `fixtures/expected-batch.json`: hand-derived bridge-v1 golden translation of these synthetic fixtures.

Runnable checks: `cargo test agents::qwen`; fixture pipeline is compared with the golden bridge batch through the actual assembler/spool, including turn/chat/tool/child parenting and non-cumulative usage. Other checks cover held-lock nonblocking queues, ordered settlement with original context, metadata-only on-disk sentinels, queued resume tails, partial lines, replacement rotation, replay deduplication, late-final settlement, proxy suppression including unknown-model child usage, resume/history skipping, foreign payload rejection, null usage, errors, sidecar identity and setup/remove round-trip preserving herdr. Windows execution remains unqualified.
### Live mock-provider capture

`fixtures/live-hooks.json` and `live-transcript-{0,1}.jsonl` were **captured live 2026-10-10 against the loopback mock provider**, using Qwen Code 0.25.0 on Spark/Linux aarch64. Session IDs and workstation/transcript paths were consistently scrubbed; native schema, timestamps, request/tool IDs, tool output and usage are retained. `live_qwen_025_mock_text_and_tool_transcripts` replays them through the real assembler/spool, proving two turns, three chats with input=11/output=7/cache_read=3, and one successful `run_shell_command` echo with causal parenting. Qwen emits an empty `UserPromptSubmit` continuation sharing the original prompt_id after a tool result; that does not create another trace. Its auto-memory extractor appears only as system telemetry in this capture and is not an accepted assistant round.

`bash scripts/e2e/run_qwen.sh` exercises the real CLI, setup/hooks/transcripts, detached flush and mock OTLP inside its own `hra-e2e-qwen` pane. Build and copy the release binary to `bin/herdr-respan.exe`, or set `HRA_BIN` to the absolute worktree release binary. HOME, QWEN_HOME, XDG roots and the agent environment are isolated; only herdr metadata variables survive into the native process. The driver asserts exactly six spans across two traces, exact native transcript and exported chat usage, echo arguments/result, pane/workspace/machine metadata, no outside-herdr delivery, and semantic setup/remove restoration. Settings preseed `$version: 4` because the real CLI otherwise performs an unrelated first-run schema migration. No adapter defect was found. Actual provider retries/authentication, real Respan ingestion, subagents/errors, and Windows are not qualified by this mocked happy-path run.

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

Core owns deterministic keys, span parenting, redaction, caps, proxy suppression and spool delivery. Adapter state contains only scrubbed input and correlation/cursor state in `$STATE/agents/qwen/`. Historical transcript lines are skipped on SessionStart/resume; partial final lines wait for a later observation. Stop/SubagentStop also register a six-second final-tail window, drained by Core’s bounded flush settlement callbacks, so transcript writes that lag the hook can still arrive. The shared JSONL cursor handles truncation/rotation, and the shared state lock serializes concurrent native parser invocations. Bridge deduplication prevents repeated hooks/replayed UUIDs from charging usage twice.

### Fidelity limits

Transcript spans describe accepted model rounds, **not** every HTTP attempt, rejected retry, provider error or live streaming chunk. Timestamps bound record observation; they are not exact request latency. Input is recorded user/tool-result content since the preceding assistant round, not a reconstruction of the entire provider request. Reasoning counts may be provider-reported **or estimated from reasoning text** by Qwen; `qwen.reasoning_usage_source=provider_or_text_estimate` preserves that uncertainty. Native child recorder currently omits model identity. A hard kill can omit Stop/SubagentStop; Core's turn timeout handles unfinished turns.

**Qwen OAuth's free tier was discontinued on 2026-04-15 and is no longer offered in the auth dialog** ([official auth documentation](https://qwenlm.github.io/qwen-code-docs/en/users/configuration/auth/)). Current documented routes use API keys/Coding Plan. Legacy OAuth overwrites request baseURL from resource_url on every operation; an OPENAI_BASE_URL proxy override is not reliable for that old mode. This adapter never modifies login/token stores.

## Fixture provenance and verification

All fixtures are synthetic; no real sessions, secrets, personal data, credentials, provider calls or Respan calls were used. Source: [QwenLM/qwen-code@c3968ace](https://github.com/QwenLM/qwen-code/tree/c3968ace2f54af27b626c3f75ae72cf889533ef9).

- `fixtures/hooks.json`: `packages/core/src/hooks/types.ts:261-300,812-1335` (base/turn/tool/subagent payloads).
- `fixtures/transcript.jsonl`: `packages/core/src/services/chatRecordingService.ts:362-490,2756-2787` (ChatRecord and accepted assistant usage).
- `fixtures/agent-Explorer-test.jsonl`: `packages/core/src/agents/agent-transcript.ts:713-725,780-799` (child ChatRecord/round usage, intentionally no model field).
- `fixtures/agent-Explorer-test.meta.json`: `packages/core/src/agents/agent-transcript.ts:107-132`; `packages/core/src/tools/agent/agent.ts:3760-3765,4685-4690` (causal parent tool id).
- `fixtures/expected-batch.json`: hand-derived bridge-v1 golden translation of these synthetic fixtures.

Runnable checks: `cargo test agents::qwen`; fixture pipeline is compared with the golden bridge batch through the actual assembler/spool, including turn/chat/tool/child parenting and non-cumulative usage. Other checks cover partial lines, replacement rotation, replay deduplication, late-final settlement, proxy suppression including unknown-model child usage, resume/history skipping, foreign payload rejection, null usage, errors, sidecar identity and setup/remove round-trip preserving herdr. No live-agent or Windows execution claim is made; those require orchestrator-owned integration runs.
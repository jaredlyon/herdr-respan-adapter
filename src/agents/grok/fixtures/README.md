# Grok fixture provenance

Source-schema fixtures below are synthetic. The live fixtures were captured against loopback mocks, then scrubbed; no secrets or personal data are retained.

- `hooks.json`: synthetic from xai-org/grok-build@2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8 `crates/codegen/xai-grok-hooks/src/event.rs:365-580` (native camelCase envelope and aliases, prompt/tool/subagent/stop fields). Typed spawn `toolResult` is from `crates/codegen/xai-grok-tools/src/types/output.rs:155-173` (ToolRunResult envelope) and `crates/codegen/xai-grok-tools/src/implementations/grok_build/task/mod.rs:748-770` (SubagentCompletedOutput). Fixture ordering intentionally places gate/observe SubagentStop before PostToolUse to exercise the exact delayed identity join.
- `chat_history.jsonl`: synthetic from the same commit `crates/codegen/xai-grok-sampling-types/src/conversation.rs:71-331` (ConversationItem tagged variants, UserItem content/prompt_index, AssistantItem content/tool_calls/model_id, ToolResultItem tool_call_id/content). Raw persistence source: `crates/codegen/xai-chat-state/src/persistence.rs:15-22`.
- `spawn-batch.json`: hand-authored expected shared bridge batch for the sixth hooks.json event after the first five, with synthetic ns timestamps 1 through 6. Expected parenting uses child-1 → spawn-1, not timing/name inference. This is an expected adapter output, not a claimed upstream payload.

Tests use temporary directories only and the shared bridge/spool pipeline. Runtime/version/Windows launch verification remains separate from these source-schema fixtures.

## Captured live mock fixtures

- `live-hooks.json`: captured live 2026-10-10 UTC (2026-10-09 operator date), Grok Build 1.0.50 (`c58f321264ba`), Linux aarch64, `scripts/e2e/run_grok.sh` against the stdlib mock provider. Source evidence: `~/.hra-e2e/grok/run-IxX2GixE/hooks.jsonl` lines 6–12. Session/prompt UUIDs and absolute work/session/output paths are replaced with deterministic mock values; timestamps and native payload fields, typed Bash result bytes, clipping flags and duration are preserved. The trailing shutdown Stop intentionally has no lastAssistantMessage.
- `live-chat_history.jsonl`: same run's tool-session conversation lines 4–7 (user prompt, assistant tool request, tool result, assistant text). Unrelated built-in system/rule/workflow preamble rows were omitted. This is the real serialization, including native tool name `run_terminal_command` and its required description argument.
- `live_mock_hooks_replay_native_and_proxy_without_duplicate_shutdown_chats`: replays captured events through the shared bridge with staged transcript writes; asserts one turn/two no-usage chats/one successful echo tool, exact tool parenting/output, duplicate-ingest idempotency and no duplicate shutdown chat. With proxy active the same capture yields only the turn/tool, proving native chat suppression.

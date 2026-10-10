# Synthetic Grok fixtures

Every value is synthetic; no live capture, secrets, or personal data.

- `hooks.json`: synthetic from xai-org/grok-build@2bdd1d6a6369de0e8c68132ea4539e9abd9e14a8 `crates/codegen/xai-grok-hooks/src/event.rs:365-580` (native camelCase envelope and aliases, prompt/tool/subagent/stop fields). Typed spawn `toolResult` is from `crates/codegen/xai-grok-tools/src/types/output.rs:155-173` (ToolRunResult envelope) and `crates/codegen/xai-grok-tools/src/implementations/grok_build/task/mod.rs:748-770` (SubagentCompletedOutput). Fixture ordering intentionally places gate/observe SubagentStop before PostToolUse to exercise the exact delayed identity join.
- `chat_history.jsonl`: synthetic from the same commit `crates/codegen/xai-grok-sampling-types/src/conversation.rs:71-331` (ConversationItem tagged variants, UserItem content/prompt_index, AssistantItem content/tool_calls/model_id, ToolResultItem tool_call_id/content). Raw persistence source: `crates/codegen/xai-chat-state/src/persistence.rs:15-22`.
- `spawn-batch.json`: hand-authored expected shared bridge batch for the sixth hooks.json event after the first five, with synthetic ns timestamps 1 through 6. Expected parenting uses child-1 → spawn-1, not timing/name inference. This is an expected adapter output, not a claimed upstream payload.

Tests use temporary directories only and the shared bridge/spool pipeline. Runtime/version/Windows launch verification remains separate from these source-schema fixtures.

# Fixture provenance

`turn.json`: synthetic bridge batch derived from `can1357/oh-my-pi@0af0f96b62c218c3d058a203c394885e39be477f`:

- `packages/coding-agent/src/extensibility/shared-events.ts:184-229`: model context and agent continuation/turn boundaries.
- `packages/coding-agent/src/extensibility/extensions/types.ts:460-519,844-852`: root/child identity, native session manager, array-valued system prompt.
- `packages/ai/src/types.ts:1199-1246`: completed assistant response/model/provider/usage/error fields.
- `packages/catalog/src/types.ts:197-262`: token usage and local cost buckets.
- `packages/coding-agent/src/task/types.ts:34-40`: raw child-session event payload.
- `packages/tui/src/overlays/session-observer-registry.ts:23-39`: child lifecycle and spawning-tool correlation.

`../assets/check.ts` encodes the synthetic native input sequence and compares the real extension mapper's result against this golden. Rust tests deserialize the same golden through the real bridge and check the emitted span tree. All names, paths and content are fictional; no live capture, credentials, or personal data are present. Linked source references and fidelity limits are in the [adapter README](../README.md).

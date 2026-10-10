# omp

`herdr-respan-omp.ts` is a Bun-loaded extension for `@oh-my-pi/pi-coding-agent` (`omp`), independent of the pi adapter. Source compatibility is pinned to omp 18.8.6 at `0af0f96b62c218c3d058a203c394885e39be477f`; no older minimum is claimed or version gate imposed.

## Setup and remove

Setup writes only `<extensions>/herdr-respan-omp.ts`, alongside herdr's `herdr-agent-state.ts`. Resolution follows herdr: nonempty `PI_CODING_AGENT_DIR` (tilde expanded), otherwise `$HOME/$PI_CONFIG_DIR/agent/extensions` (`PI_CONFIG_DIR` defaults to `.omp`). `--project DIR` writes `DIR/.omp/extensions/herdr-respan-omp.ts`. Paths to the stable binary, state, and config are JSON-escaped into the asset; the extension invokes the shared argv form `hook omp --state-dir STATE --config-dir CONFIG` without a shell. No settings, credentials, or `models.yml` entries are added. Named profiles are outside herdr's installer resolution; install separately in the selected profile's extensions directory if using one.

As herdr does, setup refuses a shared pi/omp directory, including symlink or normalized aliases. Since both installers honor `PI_CODING_AGENT_DIR`, setting that variable for global omp setup is necessarily a conflict; separate agent directories are required. Remove deletes only the unchanged owned asset and preserves herdr/custom files.

## Capture

- One agent/turn span per user run, spanning all automatic continuations. `agent_end.willContinue` retains the buffer; continuation-capable `session_stop` is not treated as completion.
- One chat span per completed assistant message: context and array-valued system prompt, final content, actual upstream model/provider when exposed, input/output/cache read/cache write/`reasoningTokens`, and locally estimated `usage.cost.total`. Unknown usage remains null; reported zero remains zero.
- Tool spans from execution start/end, arguments/result/error and elapsed timestamps.
- Child agent spans from `task:subagent:lifecycle`, parented to the known spawning tool; child chats/tools from `task:subagent:event` parented to that child. Rebound child-extension callbacks are ignored, including depth-zero child sessions, to prevent duplicates.

The asset requires exactly herdr's root gates (`HERDR_ENV=1`, socket/pane present, `OMPCODE!=1`, `ctx.hasUI===true`), plus `HERDR_RESPAN!=off` and the workspace pause file. Rust checks gating again before ingest. Gateway/proxy model spans are suppressed. No API keys, OTLP, redaction, network delivery, or spool logic resides in the extension.

## Limits

At most one hook subprocess is spawned per completed user run; streaming chunks never spawn. Detached children delay batch delivery until their observed lifecycle closes. Session switch/shutdown closes outstanding child lifecycles as errors and flushes unfinished turns. An abrupt process kill loses the in-memory buffer. Children without a known spawning-tool correlation are not guessed or attached to an unrelated turn. Native callbacks do not expose all physical provider retry attempts or auxiliary/compaction generation; use the proxy for per-request fidelity. Child request input and lifecycle output are unknown unless provided by the consumed events and remain null. Native agent version is not exposed by these callbacks and remains null. Millisecond timestamps are converted to nanoseconds, not claimed to have nanosecond precision.

## Synthetic fixtures and checks

`fixtures/turn.json` is a synthetic bridge golden, not captured live. Its native input sequence is encoded in `assets/check.ts`, derived from [oh-my-pi@0af0f96b shared-events.ts:184-229](https://github.com/can1357/oh-my-pi/blob/0af0f96b62c218c3d058a203c394885e39be477f/packages/coding-agent/src/extensibility/shared-events.ts#L184-L229), [extension types.ts:460-519,844-852](https://github.com/can1357/oh-my-pi/blob/0af0f96b62c218c3d058a203c394885e39be477f/packages/coding-agent/src/extensibility/extensions/types.ts#L460-L519), [AssistantMessage:1199-1246](https://github.com/can1357/oh-my-pi/blob/0af0f96b62c218c3d058a203c394885e39be477f/packages/ai/src/types.ts#L1199-L1246), [task/types.ts:34-40](https://github.com/can1357/oh-my-pi/blob/0af0f96b62c218c3d058a203c394885e39be477f/packages/coding-agent/src/task/types.ts#L34-L40), and [SubagentLifecyclePayload:23-39](https://github.com/can1357/oh-my-pi/blob/0af0f96b62c218c3d058a203c394885e39be477f/packages/tui/src/overlays/session-observer-registry.ts#L23-L39). Gates and relocation match herdr `2563803d` `src/integration/assets/omp/herdr-agent-state.ts:11-24,330-336`, `env.rs:35-56`, `targets.rs:89-97`.

Run `cargo test agents::omp` for batch shape, span tree, foreign-payload rejection, setup idempotence/remove preservation, relocated/shared paths and escaping. Run `bun src/agents/omp/assets/check.ts` for real mapper → golden, gating, continuation, child de-duplication, detached completion, cancellation, gateway suppression and shutdown. A Bun build of `assets/herdr-respan.ts` checks syntax only. No real agent/provider/Respan runs have been performed by this slice.

# Gemini CLI

Setup appends named `herdr-respan` command-hook groups for `SessionStart`,
`BeforeAgent`, `BeforeModel`, `AfterModel`, `BeforeTool`, `AfterTool`, `AfterAgent`,
and `SessionEnd`. The only edited file is `~/.gemini/settings.json`, or
`$GEMINI_CLI_HOME/.gemini/settings.json` (the variable replaces the home root), or
`<project>/.gemini/settings.json` with project setup. Existing groups, `telemetry`,
and `hooksConfig` are never altered. Remove deletes only owned entries.
Commands use the shared quoted binary/state/config arguments; Windows uses a
PowerShell `&` invocation, Unix uses POSIX quoting. Each inserted command has
`name: "herdr-respan"`, `env: {"HERDR_RESPAN_ADAPTER":"gemini"}`, and
`timeout: 5000` **milliseconds**. All observer hooks stay silent; the binary's
shared gate and failure handling keep them exit-zero/no-op outside herdr.

## Spans

- One agent/turn span per `BeforeAgent` → `AfterAgent`.
- One chat span per `BeforeModel` request, accumulating every `AfterModel` chunk.
  A `finishReason` alone is not sufficient: usage-only chunks may follow. The
  next `BeforeModel`, `BeforeTool`, `AfterAgent`, or `SessionEnd` closes the
  response. Several model rounds stay under the same turn. Model request input
  is normalized from `model` role to `assistant`; output uses the first candidate
  and does not duplicate the translator's `text` and `parts` copies.
- Tools pair `BeforeTool` and `AfterTool` by name and hashed arguments, FIFO for
  identical invocations. A persisted sequence suffix makes tool ids unique even
  when different hooks share Gemini's millisecond timestamp. Gemini does not expose
  tool-use ids, so simultaneous
  identical tools cannot be unambiguously distinguished. Unfinished tools and
  an open turn at SessionEnd are emitted as errors.
- No native subagent spans: this pinned hook vocabulary has no subagent events,
  and child-transcript-to-spawning-tool correlation has not been proven.

Usage counters from chunk hooks replace previous counters; they are never
summed. At response boundaries, an incremental read of the hook's exact
`transcript_path` supplements cached and thinking tokens. Repeated full-message
records are deduplicated by id. Only a single newly tokenized Gemini record with
matching model and available input/output counters is accepted; ambiguous
records leave those extra fields unknown. The JSONL reader handles incomplete
lines, truncation, and file rotation; historical token records are consumed at
session/turn/request starts rather than attributed to the new response. Missing
session files leave hook usage intact. A legacy whole-conversation `.json`
recording is not a supported supplemental source; native hook spans still work.
The recorder is a model-round source, not an audit of every failed HTTP attempt.
Native hooks expose no explicit provider-attempt error event. A request with no
observed response is marked error; streaming failure after partial output cannot
be classified reliably. Core suppresses native model spans when the proxy is
active, applies content caps/redaction, and owns span keys and parenting.

Per-session persisted state survives one-shot hook processes, with bounded
payload and token-id deduplication. Content fields are scrubbed/capped before
adapter-state persistence, including metadata-only mode. Native state changes
are serialized separately from bridge ingestion. Source timestamps identify
turns/requests/tools; observation timestamps measure the hook-to-boundary window,
not exact provider transport timing. No final chunk is fabricated if shutdown
hooks do not arrive; the shared assembler handles an unclosed turn timeout.

## Support and evidence

Basic hooks require Gemini CLI **v0.26.0+** (the stable-hooks announcement).
Current hook shapes, PowerShell selection, and JSONL recording here are pinned to
`google-gemini/gemini-cli@9b6e0265d16bbd29ca51e33c9e0c01dc4cec5e83`; they are not
proven to exist at that oldest release. Setup reports this distinction, does not
override disabled hook controls, and notes project trust requirements.

All fixtures are **synthetic**, with no provider calls, accounts, or personal
content:

- `fixtures/stream.json`: native payloads from
  [`packages/core/src/hooks/types.ts:505-718`](https://github.com/google-gemini/gemini-cli/blob/9b6e0265d16bbd29ca51e33c9e0c01dc4cec5e83/packages/core/src/hooks/types.ts#L505-L718)
  and normalized request/response from
  [`hookTranslator.ts:22-66,158-184`](https://github.com/google-gemini/gemini-cli/blob/9b6e0265d16bbd29ca51e33c9e0c01dc4cec5e83/packages/core/src/hooks/hookTranslator.ts#L22-L66).
  Three chunks include repeated cumulative usage and a usage-only tail. The
  chunk loop and delayed recorder commit are from
  [`core/geminiChat.ts:1510-1670`](https://github.com/google-gemini/gemini-cli/blob/9b6e0265d16bbd29ca51e33c9e0c01dc4cec5e83/packages/core/src/core/geminiChat.ts#L1510-L1670).
- `fixtures/session.jsonl`: synthetic full-message/update records from
  [`services/chatRecordingTypes.ts:13-169`](https://github.com/google-gemini/gemini-cli/blob/9b6e0265d16bbd29ca51e33c9e0c01dc4cec5e83/packages/core/src/services/chatRecordingTypes.ts#L13-L169)
  and [`chatRecordingService.ts:1043-1164`](https://github.com/google-gemini/gemini-cli/blob/9b6e0265d16bbd29ca51e33c9e0c01dc4cec5e83/packages/core/src/services/chatRecordingService.ts#L1043-L1164).
- `fixtures/tools-same-timestamp.json`: synthetic concurrent tool payloads from
  `hooks/types.ts:505-718` above, sharing a timestamp because
  [`hookEventHandler.ts:389`](https://github.com/google-gemini/gemini-cli/blob/9b6e0265d16bbd29ca51e33c9e0c01dc4cec5e83/packages/core/src/hooks/hookEventHandler.ts#L389)
  constructs base inputs with `new Date().toISOString()` (millisecond precision).
- `fixtures/expected-batch.json`: adapter golden derived from these synthetic
  payloads and the shared bridge v1 contract, not a captured upstream payload.
  Inline tools, foreign Qwen/Claude inputs, and settings copies in tests are
  synthetic from those hook interfaces and configuration examples in
  [`docs/hooks/index.md`](https://github.com/google-gemini/gemini-cli/blob/9b6e0265d16bbd29ca51e33c9e0c01dc4cec5e83/docs/hooks/index.md).

Tests cover a golden Batch, exactly one chat span for a multi-chunk request,
actual bridge spool parenting and deduplication, cross-process state, multiple
model rounds/tools per turn, same-timestamp concurrent tool identities, foreign rejection, metadata-only persistence, and
setup→remove with existing herdr/telemetry controls. No real Gemini session,
Windows process launch, provider/proxy login, or Respan delivery has been
exercised by this slice.

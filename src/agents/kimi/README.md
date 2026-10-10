# MoonshotAI Kimi Code CLI

Targets MoonshotAI `kimi-cli`, not a similarly named CLI. Shell hooks require
**1.28.0 or newer**. Setup executes `kimi --version` when available on PATH,
rejects an older or unparseable version, and otherwise prints an install notice.
The current wire contract is source-validated against the revision below; all
its newer optional fields are not guaranteed to exist in 1.28.0.

## Setup

Adds ten `[[hooks]]` entries to `$KIMI_SHARE_DIR/config.toml`, falling back to
`~/.kimi/config.toml`: `SessionStart`, `SessionEnd`, `UserPromptSubmit`,
`PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `Stop`, `StopFailure`,
`SubagentStart`, `SubagentStop`. Each has `matcher = ""`, `timeout = 10`, and
an absolute, shared-builder command invoking `herdr-respan hook kimi` with
explicit state/config paths. Windows uses CMD quoting because Kimi invokes
Python's shell subprocess runner. No script assets or credential edits.

Existing entries (including herdr's) and TOML comments are preserved. Removal
only removes the owned entries. Invalid MoonshotAI events such as
`PermissionRequest`, `PermissionResult`, and `Interrupt` are never installed.
`KIMI_CODE_HOME` and `~/.kimi-code` are deliberately not used. Existing invalid
herdr entries are not repaired. `--project` still uses Kimi's global config;
a separately selected native `--config-file` must be configured explicitly.

## Native capture

- Turn boundaries and user prompt: hooks. Because another Stop hook can
  continue the turn, final closure waits for wire `TurnEnd` when wire exists.
- Tool timing, input, output and failure: hooks, joined by `tool_call_id`.
- Successful model-step spans: `StepBegin`, `ContentPart`, and `StatusUpdate`
  in `wire.jsonl`; `message_id` identifies the response. Total input tokens
  are **input_other + input_cache_read + input_cache_creation** when all
  components are known; missing counters stay null, never zero.
- Subagents: wire `SubagentEvent.agent_id`, nested events and
  `parent_tool_call_id` (legacy `task_tool_call_id` accepted). The subagent
  span is parented to its spawning tool, child chat/tools to that subagent.
  Hook agent names alone are not used to guess concurrent identity.

Model/provider, reasoning usage and USD cost remain null: the durable wire
source does not establish those values. Step spans are successful steps, not
all failed/retried provider attempts. Input is null on chat spans because
wire does not contain the full authoritative per-request input. Assistant
output contains observed content parts. Compaction context size is not billed
request usage. Proxy duplicate suppression, gating, caps, redaction, keys,
parenting and spool delivery remain exclusively in Core.

Session wire files are discovered at
`<share>/sessions/<work-directory-hash>/<session-id>/wire.jsonl`. A unique
native session ID is required; ambiguous directories fail closed for capture.
The path is cached, avoiding another hashing dependency. Hook payloads are
queued into private, Core-scrubbed adapter state before serialized translation,
with their original pane metadata, run ID, proxy mode and relocated share path.
The settle callback drains queued payloads and tails complete JSONL records;
Core's detached flush retries late writes. Historical wire content preceding the first queued hook is skipped; newer
records are buffered until their corresponding hook time so detached startup
cannot accidentally discard the first turn. Wire lag exceeding those retries
is picked up by a later hook/flush. With no wire file, hooks still capture
turns/tools, but model steps and subagent identity are unavailable. Interruption
without a terminal hook is closed by Core's turn timeout. Session translation
state is retained to allow resume; this is not an independent export pipeline.

## Fixtures and checks

All fixtures are **synthetic**, with no provider calls, real secrets or personal
data. Source revision: MoonshotAI/kimi-cli
`9ab1286b8fe4e6bcd116949a27ce5e0ac3389c82`:

- `fixtures/hooks.json`: [src/kimi_cli/hooks/events.py](https://github.com/MoonshotAI/kimi-cli/blob/9ab1286b8fe4e6bcd116949a27ce5e0ac3389c82/src/kimi_cli/hooks/events.py#L8-L151).
- `fixtures/wire.jsonl`: [wire/types.py](https://github.com/MoonshotAI/kimi-cli/blob/9ab1286b8fe4e6bcd116949a27ce5e0ac3389c82/src/kimi_cli/wire/types.py#L38-L268),
  [wire/file.py](https://github.com/MoonshotAI/kimi-cli/blob/9ab1286b8fe4e6bcd116949a27ce5e0ac3389c82/src/kimi_cli/wire/file.py#L26-L48),
  [kosong/message.py](https://github.com/MoonshotAI/kimi-cli/blob/9ab1286b8fe4e6bcd116949a27ce5e0ac3389c82/packages/kosong/src/kosong/message.py#L84-L232),
  [kosong/tooling/__init__.py](https://github.com/MoonshotAI/kimi-cli/blob/9ab1286b8fe4e6bcd116949a27ce5e0ac3389c82/packages/kosong/src/kosong/tooling/__init__.py#L314-L320).
- `fixtures/expected-batch.json`: golden shared bridge batch derived from
  those synthetic inputs (six emitted spans).

Tests cover the golden batch, idempotent shared-bridge spool tree, tool→subagent
parentage, null model/unknown usage, legacy parent alias, foreign payload
rejection, late partial records, final-close deferral, version gating, and
TOML coexistence/idempotence/removal. No live Kimi, Windows shell execution,
provider authentication, or Respan ingestion is claimed verified here.

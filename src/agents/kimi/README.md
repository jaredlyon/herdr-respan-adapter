# Kimi Code CLI 2.x

Targets the official **Kimi Code CLI**, tested with `2.1.1` installed at
`~/.local/share/kimi-code/bin/kimi`. Legacy Python MoonshotAI `kimi-cli` is
not supported. Setup runs `kimi --version` on PATH and requires a 2.x version;
legacy 1.x and its deprecation notice are refused with an explicit install/PATH
message. Put the successor's bin directory ahead of any legacy shim.

## Setup

Adds eight `[[hooks]]` entries in `$KIMI_CODE_HOME/config.toml`, defaulting to
`~/.kimi-code/config.toml`: `SessionStart`, `SessionEnd`, `UserPromptSubmit`,
`PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `Stop`, and `StopFailure`.
Entries contain only `event`, `command`, and `timeout = 10`; omitting `matcher`
matches all. Commands use the shared shell quoting builder (POSIX or CMD).
Existing user/herdr `[[hooks]]` entries, order, and comments are preserved;
remove deletes only our owned entries. No credentials or provider settings are
changed. `--project` still selects this user-global config. `KIMI_SHARE_DIR`
and legacy `~/.kimi` are never used.

The [official hooks reference](https://moonshotai.github.io/kimi-code/en/customization/hooks.md)
is the source for the allowed config fields and event names. Hooks require
`client_type = "kimi_code_cli"`, a native session ID and cwd. Foreign and legacy
payloads are ignored. Live `UserPromptSubmit.prompt` is an array of content
parts, not necessarily a string.

## Capture and limits

The [data-locations reference](https://moonshotai.github.io/kimi-code/en/configuration/data-locations.md)
establishes `$KIMI_CODE_HOME/sessions/<workDirKey>/<sessionId>/agents/main/wire.jsonl`.
The adapter scans one bucket level for the unique native session ID and caches
the path; ambiguous matches fail closed. Complete lines are incrementally tailed;
late/partial records are retried by Core settlement. Queued hooks retain their
original home, pane metadata, run ID and proxy mode. Historical records before
the first hook are not replayed.

Kimi Code's durable wire schema differs from legacy kimi-cli:

- `turn.prompt` / `turn.ended`: turn ID, user content, final response, status.
  Stop hooks do not prematurely close a durable turn.
- `llm.request`: actual model ID and provider (not a config alias).
- `context.append_loop_event` with `step.begin`, `content.part`, `step.end`:
  one chat per settled step, response ID, assistant content, per-step usage.
  Wire `time` is milliseconds, converted to nanoseconds.
- `step.end.usage`: input = `inputOther + inputCacheRead + inputCacheCreation`
  when all three are available; output = `output`. Unknown counters stay null.
  Cumulative `usage.record` and context token estimates are ignored.
- Native Pre/PostToolUse hooks plus wire `tool.call`: tool timing, input,
  output and failure joined by `tool_call_id` / `toolCallId`.

Without a wire file, hooks still capture turns/tools, but no model spans or
usage are invented. Full authoritative per-request chat input, reasoning token
usage, cost, provider retries/failures, and child-agent identity are not
established by these fixtures. **Successor subagent correlation is unimplemented**:
child-agent wires and non-main `agentId` records are ignored, not guessed from
legacy `SubagentEvent` or hook names. No subagent observer hooks are installed.
Proxy duplicate suppression, gating, redaction, caps, and delivery belong to Core.
Windows quoting compiles in CI but actual Windows hook execution is not qualified
by this Linux run.

## Live fixtures and regression checks

`fixtures/hooks.json` and `fixtures/wire.jsonl` were **captured live 2026-10-09
against the loopback mock provider**, using Kimi Code CLI 2.1.1 on Spark,
Linux aarch64. Evidence: `~/.hra-e2e/kimi/run-YaC0WhAW`. Paths/session IDs were
scrubbed; provider keys are dummy and no real account was used. The wire fixture
retains only records consumed by the parser, excluding system prompts,
request bodies, and unrelated context/index events. It covers a Bash echo and
two model steps. Old synthetic kimi-cli fixtures and their golden were removed.

Tests replay the captures through the shared bridge and assert an idempotent
turn → two chats + Bash tree, model/provider, exact input=11/output=7/cache=3,
legacy/foreign rejection, modern discovery, late/partial settlement, original
metadata/proxy mode, unknown usage, error turn status, and TOML coexistence with
herdr/idempotent setup/remove.

```sh
export PATH="$HOME/.local/share/kimi-code/bin:$HOME/.local/bin:$PATH"
cargo test agents::kimi
cargo build --release
HRA_BIN="$PWD/target/release/herdr-respan" bash scripts/e2e/run_kimi.sh
```

The real-agent driver uses isolated HOME/XDG/KIMI_CODE_HOME, disables telemetry
and update preflight, and defines an `openai` provider pointing only at 127.0.0.1.
`--prompt` is noninteractive; it is not combined with `--auto`. The two prompts
produce exactly six OTLP spans in two traces: text turn → chat, tool turn →
two chats + Bash echo. It checks parentage, exact usage, successful status,
native session IDs, herdr pane/workspace/machine metadata, no delivery for the
same tool prompt outside herdr, and semantic remove restoration. Raw hooks,
wires, provider requests, stdout/stderr and OTLP remain under each
`~/.hra-e2e/kimi/run-*`. No production configs, credentials, foreign herdr
sessions, or real Respan/provider endpoints are touched. Authentication,
real-network/Respan behavior, and successor subagents are not mock-qualified.

Metadata-only capture (`content_cap_bytes = 0`) strips native prompt/tool/output fields before queuing and removes content from future wire backlog rows and pending step buffers before state saves; event correlation and usage remain.

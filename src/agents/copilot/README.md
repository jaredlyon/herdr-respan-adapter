# Copilot CLI adapter

Setup appends direct command handlers in `$COPILOT_HOME/settings.json` (default `~/.copilot/settings.json`), next to herdr's entries. With `--project`, it uses `.github/copilot/settings.local.json`. Events: `sessionStart`, `userPromptSubmitted`, `postToolUse`, `postToolUseFailure`, `agentStop`, `sessionEnd`. Each entry contains `type: command`, shell-quoted `bash`/`powershell` commands from Core, `timeoutSec: 10`, and `env.HERDR_RESPAN_COPILOT_EVENT` identifying the event: native camelCase payloads have no event-name field. No permission or pre-tool hook is installed. Observational hooks print nothing; Core gates execution to herdr and makes failures exit zero.

## Spans

- Turn spans use native session identity and prompt boundaries. Hooks do not document a turn ID; their timestamp provides a deterministic turn ID when one is absent.
- Successful/failed post-tool events produce tool spans. No tool-call ID or start timestamp is documented, so missing IDs use tool name plus completion timestamp and duration is unknown (zero-length window). Concurrent tools completing with the same name and timestamp cannot be distinguished without native IDs.
- Without the native exporter, each closed turn produces one chat span with unknown model/usage. Prompt content is available; final response content is included only if present in the hook payload. `agentStop` normally has a transcript path, not response content. This adapter does not infer request usage from persisted session history.
- With the exporter, requests are correlated by `gen_ai.conversation.id` (session) and `github.copilot.turn_id`. When hooks lack native turn IDs, the request start timestamp must fall in that session's prompt-to-stop window. Each request emits a metadata-only chat with model/provider and optional input/output/cache counts. Missing usage stays null. The native turn ID is retained as `copilot.turn_id` metadata. Request chat spans replace, rather than accompany, the usage-less chat for that turn.
- No subagent spans: documented hook start payloads lack unique instance and spawn-tool IDs, and built-in general-purpose subagents do not fire these hooks. This adapter does not invent parentage.

On turn end, the adapter incrementally reads `$STATE/agents/copilot/otel.jsonl` using Core's `jsonl::Cursor` (complete lines, rotation/truncation handling). An exporter-enabled turn can wait for Core's three settlement rounds (two seconds apart) to accommodate late export. If no correlated native request exists by the third call, settlement emits exactly one fallback and closes the turn. Rows arriving after fallback are not emitted again. All request rows already available for the turn are included; later additional requests for an already-settled turn cannot safely change an immutable emitted trace. Per-session cursors avoid one conversation consuming another's rows. Session state and content pass through Core's cap/redaction helpers before persistence; mutation is guarded by Core's state lock. Hooks first persist a scrubbed queue entry, then try a nonblocking lock; contention leaves the queue for the lock holder or settlement to drain, rather than losing events or blocking the agent.

The parser supports OTLP JSON `resourceSpans/scopeSpans/spans` and serialized SDK spans (attributes map, span context, `[seconds,nanoseconds]` times), including array/`span`/`spans` wrappers. The exact current Copilot file-exporter envelope is **not live-verified**. Malformed/non-chat/unidentified records are ignored. Content-capture attributes are never consumed, and `github.copilot.cost` is not treated as USD: it is a model billing multiplier.

## Optional shell-profile offer

Setup notifies the user that tokens require the exporter environment variable and offers `copilot-otel-profile`. Decline leaves profiles unchanged; `--yes` alone does not accept the offer. Explicit `--accept copilot-otel-profile` or interactive acceptance adds a marked, gated line only to existing profiles:

- bash: `.bashrc`, `.bash_profile`, `.bash_login`, `.profile`;
- zsh: `.zshrc`, `.zprofile`, `.zshenv` under `ZDOTDIR` or home;
- fish: `config.fish` under `XDG_CONFIG_HOME/fish` or `~/.config/fish`;
- PowerShell: `profile.ps1` and `Microsoft.PowerShell_profile.ps1` under `XDG_CONFIG_HOME/powershell` (default `~/.config/powershell`), `~/Documents/PowerShell`, and `~/Documents/WindowsPowerShell`.

Exact shell lines (PATH is the shell-escaped absolute `$STATE/agents/copilot/otel.jsonl`):

```sh
if [ "${HERDR_ENV:-}" = 1 ]; then export COPILOT_OTEL_FILE_EXPORTER_PATH='PATH'; fi
```
```fish
if test "$HERDR_ENV" = 1; set -gx COPILOT_OTEL_FILE_EXPORTER_PATH 'PATH'; end
```
```powershell
if ($env:HERDR_ENV -eq '1') { $env:COPILOT_OTEL_FILE_EXPORTER_PATH = 'PATH' }
```

`Edit::TextArg` stores the exact path for removal; marker blocks preserve LF/CRLF and whether an added separator newline was necessary. Reapplying is idempotent. Remove deletes only the exact owned block and refuses externally modified blocks; unrelated later additions survive. No shell profile is created. Restart the shell inside herdr after accepting. `run copilot` enables the file exporter without a profile edit. The adapter never sets `OTEL_INSTRUMENTATION_GENAI_CAPTURE_MESSAGE_CONTENT`; if users enable raw content capture independently, their native exporter file may contain sensitive data outside our spool.

## Limits and verification

A complete-schema minimum Copilot version is not established by the official reference; use a runtime exposing the listed hooks and OTel file exporter. Sandbox grants must permit access to the stable binary/state directory; setup does not widen policy or authenticate. Foreign Claude/Cursor/Grok payloads are rejected by shape and Grok's hook environment. No provider/Respan calls or real Copilot sessions are required by tests.

Settings containing JSONC comments or trailing commas fail safely without edits and print the exact hook entries to merge manually into existing event arrays. Core does not automatically merge JSONC in this version; the adapter never strips comments. All tests use synthetic fixtures and temporary paths. Windows commands and real shell startup behavior require the integration matrix, not Rust-only fixture tests.

## Fixture provenance

- `fixtures/hooks.json`: synthetic from the current [GitHub Copilot hooks reference](https://docs.github.com/en/copilot/reference/hooks-reference), sections command hooks and event input payloads, read 2026-10-09. Contains native camelCase and PascalCase compatibility payloads; no actual agent capture.
- `fixtures/otel.jsonl`: synthetic OTLP JSON span envelope using the documented attribute names in [Copilot OpenTelemetry monitoring](https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-command-reference#opentelemetry-monitoring), read 2026-10-09. Envelope follows the [OTLP JSON mapping](https://opentelemetry.io/docs/specs/otlp/#json-protobuf-encoding); it is not claimed to be a captured Copilot exporter line.
- `fixtures/otel-sdk.jsonl`: synthetic flat SDK span records with the same documented attributes. Envelope corroborated by the public [aiusage Copilot parser at v0.6.0](https://github.com/RandomCodeSpace/aiusage/blob/v0.6.0/adapter/copilot/copilot.go), `otelRecord` and `timestampFromParts`; no third-party implementation is copied and no runtime-verification claim is inherited.
- `fixtures/chat-batch.json`: expected bridge batch for that synthetic exporter fixture, including null unknown usage and native turn-ID metadata.

Runnable checks: `cargo test agents::copilot`; `cargo clippy -- -D warnings`. Tests cover fixture-to-batch golden normalization, turn/tool/chat spool parentage, late export and bounded fallback, idempotence, foreign payload rejection, metadata-only persistence, optional offer accept/decline, and setup/remove round trips preserving herdr and profile contents.

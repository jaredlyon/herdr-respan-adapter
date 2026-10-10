# herdr-respan-adapter

A [herdr](https://herdr.dev/) plugin that traces CLI coding agents to [Respan](https://respan.ai/). Plugin ID: **`jaredlyon.respan-adapter`**. One Rust binary, one redaction/spool/OTLP pipeline; agent adapters translate native events into that shared pipeline. Native tracing is the default. The local provider pass-through proxy is explicit and opt-in, not a Respan gateway.

## Install

Requires herdr **0.9.3+**. Install on **each host running the agents**, not just the client used to connect to it:

```sh
herdr plugin install jaredlyon/herdr-respan-adapter
# To pin a release:
herdr plugin install --ref v0.1.0 jaredlyon/herdr-respan-adapter
```

The repository is private during development; GitHub installation requires access. For a local checkout (linking does not build):

```sh
sh scripts/install.sh
herdr plugin link "$PWD"
herdr plugin action list --plugin jaredlyon.respan-adapter
```

On Windows, use `powershell -NoProfile -ExecutionPolicy Bypass -File scripts/install.ps1`. Installers select a prebuilt release, verify it against `SHA256SUMS`, and write `bin/herdr-respan.exe` (the `.exe` filename is intentional on Unix too). Missing downloads fall back to `cargo build --release --locked`; checksum mismatches are fatal. Supported release targets: Linux x64/arm64 musl, macOS Apple Silicon/Intel, Windows x64 MSVC. Under Rosetta the installer selects the Apple Silicon binary. Binaries are unsigned. Source builds require Rust/Cargo and the normal platform compiler; the installer does not install toolchains.

Find the persistent plugin configuration directory:

```sh
herdr plugin config-dir jaredlyon.respan-adapter
```

Put `RESPAN_API_KEY=<your key>` in `.env` **inside that directory**; on Unix set mode 0600. Do not put a key in the manifest, agent configs, commands, screenshots or Git. Resolution order is plugin `.env`, then the API-key active profile in `~/.respan/credentials.json` (selected by `~/.respan/config.json`), then `RESPAN_API_KEY` from the environment. OAuth credentials are not repurposed.

## Setup and actions

```sh
herdr plugin action invoke jaredlyon.respan-adapter.setup
herdr plugin action invoke jaredlyon.respan-adapter.status
# Explicitly sends one synthetic trace; no model/provider call:
herdr plugin action invoke jaredlyon.respan-adapter.test
```

Setup opens a popup listing agents detected on PATH or by their config directories. Select names or `all`; read the notices, optional offers and unified diff, then explicitly confirm. Setup merges beside existing/herdr-owned entries, keeps an ownership record, and installs a stable binary copy under the plugin state directory. It never replaces agent login credentials. Optional offers default to **no**; unattended `--yes` accepts offers only when their IDs are passed using `--accept <id>`.

For direct binary use, provide `HERDR_PLUGIN_STATE_DIR` and `HERDR_PLUGIN_CONFIG_DIR`, or explicit `--state-dir DIR --config-dir DIR`; herdr injects them automatically into plugin actions/panes. Examples (with those directories supplied):

```sh
herdr-respan setup claude codex --dry-run
herdr-respan setup claude codex
herdr-respan setup claude --project /path/to/repo
herdr-respan setup --all --yes
herdr-respan remove codex
```

Relocated agent config directories are honored by each adapter; consult its linked README for exact variables and files. Global setup is the default; supported project-local setup uses `--project`. Startup refreshes already-owned binary/extension assets after version changes; new config entries always require another explicit setup.

Actions: `setup`, `remove` (all owned adapters), `status`, `test`, `run`, `off`, `on`, `pause`, `resume`, `config-export`, `config-import`, `config-push`. Terminal entrypoints include `status`, `setup`, `import`, secret `export`, `push`, agent chooser `run-choice`, and the `run` split. Ordinary action commands are asynchronous and their output appears in herdr logs; credential export instead opens a PTY popup so the blob is not captured in those action logs.

Optional herdr keybinding (edit your own herdr config; never inserted automatically):

```toml
[[keys.command]]
key = "prefix+r"
type = "plugin_action"
command = "jaredlyon.respan-adapter.status"
description = "Respan status"
```

## Agent coverage

This table describes native adapter mechanisms and span sources. Every agent except Cursor has been run end-to-end as its real CLI inside a herdr pane against local mock providers (`scripts/e2e/run_<agent>.sh`; Cursor's inference is server-side, so it's fixture-tested only). Real provider logins and live Respan ingestion are verified separately; see each adapter README for exact versions and limits. Unknown usage remains unknown, never zero.

| Agent | Mechanism | Native spans / limits |
|---|---|---|
| [Claude Code](src/agents/claude/README.md) | Claude settings hooks + incremental transcript JSONL | Turn, per-assistant-message chat with usage, tool, subagent; transcript settlement/deduplication avoids progressive usage double counting. |
| [Codex](src/agents/codex/README.md) | `hooks.json` + enabled hooks feature + rollout JSONL | Turn, per-response chat with usage, tool, child thread/subagent; `token_usage_record` is per response, `token_count` is a snapshot. One-time `/hooks` trust required. |
| [omp](src/agents/omp/README.md) | Own Bun TypeScript extension | Turn, completed-message chat with usage/cost, tool, subagent where exposed; skips `OMPCODE=1`. |
| [pi](src/agents/pi/README.md) | Own jiti-loaded TypeScript extension | Turn, completed-message chat with usage/cost, tool, subagent where exposed; no direct-export Respan extension or second pipeline. |
| [Gemini CLI](src/agents/gemini/README.md) | Settings hooks, accumulating streamed AfterModel events | Turn, chat with available usage, tool; response boundaries matter. Existing `telemetry` is untouched. |
| [OpenCode](src/agents/opencode/README.md) | JavaScript plugin with distinct V1/V2 entrypoints | Turn, settled-message chat with usage/cost, tool, subagent where exposed; batches at completion/idle, not per streamed chunk. |
| [Cursor Agent](src/agents/cursor/README.md) | Version-1 flat command hooks | Turn, chat, tool; optional stop token counters when exposed. Server-side inference cannot be captured with the local proxy. |
| [Hermes](src/agents/hermes/README.md) | Python plugin on observer hooks, per-turn batch | Turn, per-attempt chat with usage/errors, tool, subagent, auxiliary chat tagged `hermes.aux_task`; only interactive herdr sessions, not Discord/gateway. Older versions may omit auxiliary hooks. |
| [Copilot](src/agents/copilot/README.md) | Native hooks + optional OTel JSONL exporter tail | Turn/tool/chat from hooks, per-request model/usage from exporter. Setup offers an opt-in herdr-gated shell-profile exporter line; `run copilot` always enables the exporter. |
| [Kimi Code](src/agents/kimi/README.md) | Kimi Code CLI 2.x `[[hooks]]` + `wire.jsonl` | Turn, per-step chat with model and usage, tool. `KIMI_CODE_HOME` or `~/.kimi-code`; the deprecated legacy `kimi-cli` is refused. Subagent correlation not implemented yet. |
| [Qwen](src/agents/qwen/README.md) | Claude-style settings hooks + transcript JSONL | Turn, transcript chat with available usage, tool; no AfterModel hook. Qwen OAuth has been discontinued upstream. |
| [Grok](src/agents/grok/README.md) | Grok hook file + transcript | Turn, tool, subagent; no native per-request usage. Use `run grok` for request/model/usage spans through the proxy. |
| Every other/custom herdr agent | Pane detection/status/exit events | Lifecycle-only agent spans: working → idle/done/blocked/exited, duration and herdr metadata. No prompts, tools or inferred token counts. |

Turn traces use native session IDs as Respan thread identifiers, with chat/tool/subagent children. Subagents parent under their spawning tool where native data permits. Git context includes repository **name**, branch and HEAD SHA, never the remote URL. Lifecycle tracing is skipped when a tier-1 adapter is installed. Herdr event hooks are best-effort notifications, not a durable replay queue.

## Proxy / launch wrapper

```sh
herdr plugin action invoke jaredlyon.respan-adapter.run
# Or, with plugin config/state directories provided:
herdr-respan run claude -- -p 'hello'
herdr-respan run codex
herdr-respan run grok
herdr-respan run copilot
herdr-respan run --protocol openai -- any-compatible-command
```

The action launches the focused agent in a split, or opens a chooser first. Generic protocols are `anthropic`, `openai`, `gemini`: common base-URL environment variables are set for a command you explicitly launch. Generic mode is best-effort request tracing, not native turns/tools. An agent must actually honor the override; see the proxy and adapter limitations before relying on it.

The loopback proxy forwards the agent's own provider authentication and streamed responses; it does not install provider keys or change subscription logins. Claude uses `ANTHROPIC_BASE_URL` without setting `ANTHROPIC_API_KEY`; Codex uses `-c openai_base_url=...` for both API-key and ChatGPT modes, including WebSocket responses. Native model spans are suppressed when the proxy is active or the provider base URL is already `api.respan.ai`, avoiding duplicate model spans. Hermes already exposes native per-attempt data; Cursor's server-side calls cannot be proxied. Copilot's wrapper enables its local OTel exporter rather than pretending ordinary Copilot service inference honors an OpenAI override.

## Privacy, gating and volume

Full content is captured by default, capped **per field at 16 KiB**. Built-in scrubbing covers common provider/GitHub/AWS/Slack tokens, Bearer tokens, private keys and secret assignments; add regexes for your environment. Scrubbing occurs **before spooling**, but pattern matching is not a guarantee that all sensitive content is removed. Inspect your settings before tracing confidential work.

Create `config.toml` in the plugin config directory to override defaults:

```toml
customer_identifier = ""         # empty means hostname
content_cap_bytes = 16384          # 0 disables content, not spans
spool_max_bytes = 268435456        # 256 MiB; oldest ready files dropped at cap
base_url = "https://api.respan.ai"
tool_spans = true
subagent_spans = true
aux_spans = true                   # Hermes auxiliary calls
redact_patterns = []
[proxy]
listen = "127.0.0.1:0"
idle_exit_secs = 600
[proxy.upstream]
anthropic = "https://api.anthropic.com"
openai = "https://api.openai.com"
gemini = "https://generativelanguage.googleapis.com"
```

Native adapters no-op outside `HERDR_ENV=1`. To turn tracing off and back on at will:

| Scope | Off | On |
|---|---|---|
| This machine (every workspace, pane and `run`) | action **Turn Respan tracing off**, or `herdr-respan off` | **Turn Respan tracing on**, or `herdr-respan on` |
| One workspace | action **Pause this workspace** (workspace/pane menus), or `herdr-respan pause [--workspace <id>]` | **Resume this workspace**, or `herdr-respan resume [--workspace <id>]` |
| One pane | `HERDR_RESPAN=off` in that pane | unset it |

```sh
herdr plugin action invoke jaredlyon.respan-adapter.off
herdr plugin action invoke jaredlyon.respan-adapter.on
```

None of these edit agent configs (`remove` does). While off, nothing new is captured and `run` launches agents directly without the proxy; spans captured before you turned it off are still delivered. Workspace pauses are kept when you switch tracing back on. The status popup shows when tracing is off.

**A busy user can exceed Respan Free's 100,000 viewable spans/month.** Model, tool, subagent and auxiliary activity can create many spans per turn. Set `tool_spans=false`, `subagent_spans=false`, `aux_spans=false` to reduce volume; `herdr-respan off`, a workspace pause or `HERDR_RESPAN=off` stops capture entirely. `content_cap_bytes=0` reduces stored content but **does not reduce span count**. Status shows this host's month-to-date successfully sent spans (not an account-wide billing counter), spool depth/drops, last flush, coverage and key source. OTLP ingest does not consume model credits; provider calls made by your agents still incur their usual costs. Consult [Respan pricing](https://respan.ai/pricing) for current plan limits.

Spool and operational state stay on the host. Errors-only logging uses `$HERDR_PLUGIN_STATE_DIR/herdr-respan.log` (1 MiB × 2 rotation); `HERDR_RESPAN_LOG=debug` enables debug logging. Keys/content are not intentionally logged. Keep state/config directories private and remove any manually copied export files after use.

## Remote hosts

Install and explicitly set up the plugin on each remote host. Config transfer is **opt-in**, includes an API key and does not install remote agent hooks.

```sh
herdr plugin action invoke jaredlyon.respan-adapter.config-push
# Direct CLI: herdr-respan config push <saved-machine-label-or-id>
```

Push resolves an exact saved machine ID/unique label via `$HERDR_BIN_PATH machine list --json`, then uses ordinary OpenSSH with its saved target unchanged. It runs remote `herdr plugin config-dir jaredlyon.respan-adapter` via POSIX shell or encoded PowerShell and streams base64 file content on SSH stdin, never command arguments or captured logs. No remote `herdr-respan` binary is needed for transfer; remote **herdr must be on PATH**, and POSIX hosts need `base64`. POSIX files are mode 0600; Windows uses owner-only ACLs. SSH authentication/config are OpenSSH's; herdr-managed connection reuse is not guaranteed. Accept/verify the host key and configure key authentication separately; push uses BatchMode and strict host checking. `herdr --machine X plugin config-dir ...` is **not supported**, because `--machine` only forwards supported API operations.

Paste fallback:

```sh
# Local trusted terminal: displays a secret blob once in a popup.
herdr plugin action invoke jaredlyon.respan-adapter.config-export
# On the destination host:
herdr plugin action invoke jaredlyon.respan-adapter.config-import
# Or open destination import popup through a connected remote API:
herdr --machine <id> plugin pane open --plugin jaredlyon.respan-adapter --entrypoint import
```

The export is base64, **not encryption and not an expiring token**. Treat the entire blob as a key. Import confirms replacement first, hides pasted input and accepts bracketed paste or multiline text ended by a blank line/Ctrl-D, at most 1 MiB. Ctrl-C cancels. Never export through a regular captured herdr action log. Direct `config export` writes to stdout for trusted terminal/pipeline use; direct `config import` reads stdin. Transfers overwrite only the two plugin config files, not agent configs or login stores.

Import and push stage both files privately in the destination directory, then rename `.env` **first** and `config.toml` **last** (the commit point). A failed config rename restores the previous `.env`, or removes it if none existed; a failed key write never publishes the new endpoint. Windows rejects reparse-point destinations and uses directory-entry replacement, never an in-place write to the destination files. Reparse checks run before securing directory ACLs. Transfers serialize with `.transfer-lock`; an interrupted process may leave that lock and a private staging directory. After confirming no transfer is running, inspect the two files, remove the stale lock/staging directory and retry the transfer. If rollback itself fails, the private staging backup is retained for recovery.

This is ordered replacement with rollback, **not a reader snapshot**: during a manual transfer a concurrent reader can briefly see the old endpoint with the new key, and already-running senders can retain earlier configuration. Stop active senders before transferring a changed `base_url`; do not treat the two-file layout as a general atomic configuration store.

## Troubleshooting

- **Codex says awaiting trust / no Codex spans:** run setup, then open Codex **inside herdr**, use **`/hooks`**, and explicitly trust/enable the installed hooks once. Restart as Codex requests. The status popup says `awaiting Codex trust (run /hooks in Codex once)` until the first Codex span arrives; this is an observation, not an inspection of Codex's trust store. The plugin never edits that store. Existing `notify` settings are untouched.
- **No spans:** check `herdr-respan status` (tracing on/off), setup ownership, `HERDR_ENV=1`, pane opt-out/workspace pause, relocated agent config and the per-agent README. Missing hook payload/usage is not invented from terminal text.
- **Key not configured / flush failure:** status reports the key **source**, never the key. Check plugin `.env` and Respan API-key profile. Ready spool files survive transient flush errors; cap drops are counted. Check network/base URL without posting keys/content into issue reports.
- **Link succeeds but commands fail:** linking validates only the manifest and does not build. Run the appropriate installer; `bin/herdr-respan.exe` must exist, be executable on Unix, and match the host OS/architecture.
- **Popup does not open:** herdr needs an active workspace/tab/focused pane. Close any existing popup first. Large imports use noncanonical hidden input, avoiding the normal terminal's short line-buffer limit.
- **Remote push fails:** confirm saved machine enabled/unique, normal `ssh <target>` works, host key is trusted, remote `herdr` is on PATH and config directory writable. Use paste fallback when plain SSH authentication differs from herdr's managed connection.
- **Kimi:** supported is the current Kimi Code CLI 2.x (`KIMI_CODE_HOME` / `~/.kimi-code`). The old MoonshotAI `kimi-cli` is deprecated upstream (current releases only print a pointer to Kimi Code), so setup refuses it.

## Uninstall

Remove owned adapter entries **before** uninstalling the plugin:

```sh
herdr plugin action invoke jaredlyon.respan-adapter.remove
# Wait for successful completion in plugin logs, then:
herdr plugin uninstall jaredlyon.respan-adapter
# For a linked development checkout instead:
herdr plugin unlink jaredlyon.respan-adapter
```

Equivalent direct binary command is `herdr-respan remove --all` with the correct config/state directories. Removal preserves unrelated/herdr-owned entries. Herdr deliberately retains plugin config/state; purge those directories yourself only after deciding whether to retain queued traces and keys. Do not delete arbitrary agent configs.

## Development and release

`cargo test --locked` and `cargo clippy --locked --all-targets -- -D warnings` are the Rust checks; CI runs Linux/macOS/Windows. Tag `v<manifest-version>` to build the five release targets and publish raw binaries plus `SHA256SUMS`. Keep manifest and Cargo package versions aligned. Public marketplace listing requires a public repository and the `herdr-plugin` topic; publishing/access/topic changes are operator decisions, not automatic installer side effects.

Config round-trip, failed key writes, key rollback, publish ordering, linked-file target preservation, bounded paste parsing, machine selection and POSIX remote push have synthetic checks. Windows script-text tests cover reparse rejection, exclusive staging and rename/rollback ordering; they are not a real Windows execution. The push check substitutes temporary herdr/SSH executables, verifies exact file bytes/0600 permissions, and proves credentials are absent from SSH argv; no actual remote connection or remote adapter binary is used. The synthetic machine-list schema follows [herdr catalog.rs at 2563803d](https://github.com/herdrdev/herdr/blob/2563803dca97c040beaf3dc3acdcb5a3221b4238/src/client/endpoint/catalog.rs#L19-L82). No real credentials or agent configs are needed. Local PTY checks can use `script -qc` and temporary config/state/home directories. Live agent/provider/Respan checks are separate from fixture tests and require explicit operator authorization.
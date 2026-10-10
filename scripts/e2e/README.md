# Real-agent E2E against loopback mocks

From the repository on Spark:

```sh
export PATH="$HOME/.local/bin:$PATH"
~/.cargo/bin/cargo build --release
cp target/release/herdr-respan bin/herdr-respan.exe
bash scripts/e2e/run_claude.sh
```

Requires Python 3 (stdlib only), Bash, herdr and the real Claude Code binary. The Claude driver is qualified with Claude Code 2.1.296 and herdr 0.9.3 on Linux aarch64. No provider accounts, login or Respan credits are needed. Both HTTP servers bind **127.0.0.1 only**, atomically select free ports by binding port 0, and never forward requests anywhere.

Each run retains evidence under `~/.hra-e2e/claude/run-*`: provider request log, captured native hook stdin, actual agent transcripts, stdout/stderr, setup diff, original settings, received raw OTLP bodies and flattened `spans.json`. The provider logs every path and headers, redacting authentication/key/cookie headers and excluding URL queries. Request bodies contain only this isolated mock run; do not point this harness at real provider credentials.

The driver relocates Claude with `CLAUDE_CONFIG_DIR`, disables nonessential traffic and updates, clears inherited OAuth/auth-token overrides, and sets a dummy API key plus the loopback `ANTHROPIC_BASE_URL`. Only the pre-authorized `Bash(echo hra-e2e)` tool is exposed. It installs the real adapter into isolated state/config dirs, creates its own `hra-e2e-claude` headless herdr session with a separate XDG/config root, and runs two real `claude -p` calls. No user configs, linked production-plugin state, existing herdr sessions or Hermes gateway service are touched. EXIT cleanup stops only its own session and mock processes; evidence remains for inspection.

Acceptance checks: two traces; one turn and one chat for the text prompt; one turn, two chats and one Bash tool for `RUN_TOOL`; every chat has input=11, output=7, cache_read=3; all children have the correct turn parent; native thread IDs and herdr pane/workspace/machine metadata are present; status is OK; duplicate/orphan delivery is absent. Metadata follows canonical OTLP `respan.metadata` (JSON string), with cache usage under `llm.usage.cache_read_input_tokens`. The same tool prompt outside herdr must leave the receiver byte-for-byte unchanged. `remove` must restore the isolated settings JSON value exactly (including capture-only hooks and Claude's pre-seeded updater setting).

## Shared driver API

Source `lib.sh`, then:

- `hra_init <agent>`: set `HRA_ROOT`, `HRA_BIN`, `HRA_REPO`, `HRA_AGENT`, `HRA_SESSION`; register cleanup.
- `hra_start_mocks`: set `HRA_PROVIDER_URL` and `HRA_OTLP_URL`, write isolated plugin `config.toml` and dummy `.env`; raw bodies go to `$HRA_ROOT/otlp/*.json`.
- `hra_start_pane KEY=value ...`: create named server and workspace pane with caller environment, set `HRA_PANE`; no focus change to another session.
- `hra_prompt '<shell command>' <label>`: atomically submit command+Enter, wait up to 120 seconds for process exit (not echoed terminal text), retain `<label>.out/.err/.exit`, fail on nonzero agent exit.
- `hra_flush`: wait for the real sender and pending adapter settlement to empty isolated spool.
- `hra_dump`: emit flattened OTLP spans as JSON.
- `hra_assert`: dump to `spans.json`, assert delivery, unique IDs, valid parents and herdr metadata; agent drivers additionally assert their own exact counts, usage and tool result.
- `hra_herdr <args...>`: issue commands against only that driver's XDG/config/session context.

`HRA_BIN` may override the default release binary. Mocks accept `--port` (default 0) and required `--port-file`; the provider requires `--log`, receiver requires `--output`.

## Mock protocols and limits

`mock_provider.py` implements Anthropic Messages streaming/nonstreaming and count_tokens; OpenAI Chat Completions streaming/nonstreaming; OpenAI Responses SSE/nonstreaming; Gemini streamGenerateContent SSE/generateContent/countTokens; and model-list/health probes. `RUN_TOOL` in conversation content emits one harmless shell call, followed by final text after a tool result appears. Message/response/tool IDs are request-unique. Anthropic usage starts with output=0 and finishes output=7 to exercise real incremental transcripts. OpenAI usage includes cached prompt details; Gemini includes cachedContentTokenCount. Unknown routes return 404, rather than silently pretending an unimplemented protocol works.

This is a deterministic local HTTP harness, not provider emulation or real-network qualification. OpenAI WebSocket transport, subscription authentication and real Respan ingestion are not exercised by the Claude driver. Scrubbed real Claude captures and their replay regression live in `src/agents/claude/fixtures/live-*` and `live_tests.rs`; provenance is documented in the agent README.

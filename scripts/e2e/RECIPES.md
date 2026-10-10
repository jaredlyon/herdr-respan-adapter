# Real-agent mock-provider recipes

Verified on Spark (Linux aarch64), 2026-10-09. These are **endpoint reachability** checks, not completed turns or plugin/OTLP acceptance. A temporary stdlib HTTP listener bound only to `127.0.0.1`, recorded method/path/body, and returned HTTP 400 with a synthetic error. Each compatible binary reached its local inference endpoint; it then failed on that intentional response or was terminated. No provider login, real provider key, Respan call, gateway restart, or herdr session was used for these checks. Full streaming mocks and pane acceptance live in [README.md](README.md).

## Installed inventory

| Canonical agent | Binary / launch | Observed version | Local inference protocol / result |
| --- | --- | --- | --- |
| claude | `~/.local/bin/claude` | 2.1.296 | Anthropic Messages; POST `/v1/messages?beta=true` |
| codex | `~/.local/bin/codex` | codex-cli 0.162.1 | OpenAI Responses; POST `/v1/responses` |
| hermes | existing checkout's `venv/bin/python cli.py` (see below) | v0.21.5+4905.gf42f579 (2026.9.24) | OpenAI Chat; POST `/v1/chat/completions` |
| pi | `~/.local/bin/pi` | 1.1.0 | OpenAI Chat; POST `/v1/chat/completions` |
| omp | `bun ~/omp-repro/src/packages/coding-agent/src/cli.ts` | omp/18.5.0 | OpenAI Chat; POST `/v1/chat/completions` |
| gemini | `~/.local/bin/gemini` | 0.63.0 | Gemini; POST `/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse` |
| opencode | `~/.local/bin/opencode` | 1.18.35 | OpenAI Chat; POST `/v1/chat/completions` |
| qwen | `~/.local/bin/qwen` | 0.25.0 | OpenAI Chat; POST `/v1/chat/completions` |
| kimi (legacy Python) | `~/.local/bin/kimi` | 1.28.0 | OpenAI Chat; POST `/v1/chat/completions` |
| kimi (successor Kimi Code) | `~/.local/share/kimi-code/bin/kimi` | 2.1.1 | OpenAI Chat; POST `/v1/chat/completions` |
| grok | `~/.local/bin/grok` | 1.0.50 (c58f321264ba) | OpenAI Chat primary; POST `/v1/chat/completions`, plus auxiliary POST `/v1/responses` |
| copilot | `~/.local/bin/copilot` | GitHub Copilot CLI 1.0.95 | OpenAI Chat BYOK; POST `/v1/chat/completions` without GitHub auth |
| cursor | `~/.local/bin/cursor-agent` | 2026.10.01-e373342 | **Impossible with standard provider mocks**; proprietary POST `/auth/exchange_user_api_key` |

### User-level installation commands used

```sh
export PATH="$HOME/.local/bin:$PATH"
npm install --global --prefix "$HOME/.local" \
  @earendil-works/pi-coding-agent @google/gemini-cli opencode-ai \
  @qwen-code/qwen-code @github/copilot
uv tool install 'kimi-cli==1.28.0'
curl -fsSL https://x.ai/cli/install.sh | bash
curl -fsSL https://cursor.com/install | bash
```

The npm installation's `allowScripts` warning was observed; all five installed binaries nevertheless launched and reached the listener. Do not globally enable arbitrary install scripts to silence that warning. The official Grok installer placed binaries in `~/.grok/bin` and added that bin directory to `.bashrc`; its installer-created `~/.local/bin/agent` symlink was subsequently replaced by Cursor's official installer. Use explicit `grok` and `cursor-agent` names, never ambiguous `agent`. No sudo was used.

OMP uses the existing populated source checkout; its root `package.json` defines `dev` as `bun --cwd=packages/coding-agent src/cli.ts`. The equivalent absolute script invocation above avoids changing the E2E working directory and does not modify the checkout. The checkout README recommends Bun for npm-package installation; another OMP install was unnecessary.

See the [official Grok installation instructions](https://github.com/xai-org/grok-build#installing-the-released-binary), [Cursor installer](https://cursor.com/install), and [OMP source README](https://github.com/can1357/oh-my-pi/blob/main/README.md).

## Common isolation (required for every recipe)

Run in a fresh shell with no inherited provider credentials, profiles, dotenv files, or agent configuration. The probes used an explicitly constructed environment, not the caller's full environment. Preserve herdr's own `HERDR_*` pane variables when adapting these commands into the pane harness; do not preserve ambient API keys or OAuth variables.

First set `REAL_HOME=$HOME`, `AGENT` to the canonical name, and `BASE` to your mock provider's **host root**. For the successor Kimi probe use `AGENT=kimi-successor` but the plugin canonical kind is still `kimi`.

```sh
REAL_HOME=$HOME
AGENT=claude                         # change per recipe
BASE=http://127.0.0.1:18080           # actual mock listener port
ROOT="$REAL_HOME/.hra-e2e/$AGENT"
export REAL_HOME BASE ROOT
export PATH="$REAL_HOME/.local/bin:/usr/local/bin:/usr/bin:/bin"
mkdir -p "$ROOT"/{home,agent,work,state,config,xdg/{config,cache,data,state}}
export HOME="$ROOT/home"
export XDG_CONFIG_HOME="$ROOT/xdg/config"
export XDG_CACHE_HOME="$ROOT/xdg/cache"
export XDG_DATA_HOME="$ROOT/xdg/data"
export XDG_STATE_HOME="$ROOT/xdg/state"
export LANG=C.UTF-8 CI=1 NO_COLOR=1 TERM=dumb
export NO_PROXY=127.0.0.1,localhost DO_NOT_TRACK=1 OTEL_SDK_DISABLED=true
cd "$ROOT/work"
```

`HOME` and XDG relocation are defense in depth: some tools, caches, and generic shared resources do not follow the agent-specific variable. The native relocation variable in each section is also required so plugin setup writes next to the config the binary actually reads. Never run these commands from the user's source repo, where project config might add providers or extensions.

Initialize each provider config **before** plugin setup, so setup merges rather than being overwritten. For plugin E2E only (not performed by these reachability checks):

```sh
BIN="$REAL_HOME/src/herdr-respan-adapter/bin/herdr-respan.exe"
printf 'base_url = "http://127.0.0.1:18081"\n' > "$ROOT/config/config.toml"
printf 'RESPAN_API_KEY=dummy\n' > "$ROOT/config/.env"
"$BIN" setup "$AGENT" --yes --state-dir "$ROOT/state" --config-dir "$ROOT/config"
# Run both prompts in a session you create named hra-e2e-<canonical-agent>.
# Stop that session, then remove only this isolated setup:
"$BIN" remove "$AGENT" --state-dir "$ROOT/state" --config-dir "$ROOT/config"
```

Replace the OTLP port with the actual mock receiver's port. For successor Kimi, use canonical `kimi` rather than `kimi-successor` in setup/remove; **current legacy-targeting plugin setup needs an explicit successor compatibility change first**. Native print-mode and hook approval are separate: a provider request does not prove hooks are trusted or emitted. In particular, Codex's one-time `/hooks` trust step is not bypassed by the provider recipe; never write its trust store.

## Claude

```sh
export CLAUDE_CONFIG_DIR="$ROOT/agent"
export ANTHROPIC_API_KEY=dummy ANTHROPIC_BASE_URL="$BASE"
export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1
claude -p --model claude-haiku-4-5 --dangerously-skip-permissions 'Say hello'
```

Protocol: `anthropic`. No onboarding/config preseed was needed in this isolated print-mode launch. The dangerous-permission flag is solely for the harmless mocked tool turn in an empty E2E workspace; never use it with untrusted real model responses. Dummy API-key mode avoids OAuth without touching a subscription login. The listener received POST `/v1/messages?beta=true` on port 43469.

## Codex

```sh
export CODEX_HOME="$ROOT/agent" OPENAI_API_KEY=dummy
codex exec --skip-git-repo-check \
  -c 'model_provider="mock"' \
  -c 'model_providers.mock.name="Mock"' \
  -c "model_providers.mock.base_url=\"$BASE/v1\"" \
  -c 'model_providers.mock.env_key="OPENAI_API_KEY"' \
  -c 'model_providers.mock.wire_api="responses"' \
  -m gpt-4.1 'Say hello'
```

Protocol: `openai-responses` over HTTP SSE. This explicit custom provider does not require auth.json or a real account. `exec` bypassed interactive onboarding; `--skip-git-repo-check` permits the empty workspace. The default read-only sandbox is enough for a harmless echo. The listener received POST `/v1/responses` on port 33389. Model metadata and unavailable Linux user-namespace warnings appeared but did not prevent the request. This deliberately tests an API-key custom provider, not ChatGPT WebSocket authentication. Hook trust remains the separate `/hooks` step noted above.

## Hermes

The existing `~/.local/bin/hermes` forwards to a managed Python 3.14 shim whose normal chat launch failed with `ModuleNotFoundError: No module named 'ruamel'`. Do not repair that shared runtime or restart `hermes-gateway` for E2E. Use the existing checkout's working `venv` (observed OpenAI SDK 2.24.0):

```sh
export HERMES_HOME="$ROOT/agent"
export OPENAI_API_KEY=dummy OPENAI_BASE_URL="$BASE/v1"
export HERMES_SKIP_SETUP=1 HERMES_DISABLE_LAZY_INSTALLS=1
"$REAL_HOME/jare-ai/hermes-agent/venv/bin/python" \
  "$REAL_HOME/jare-ai/hermes-agent/cli.py" \
  --provider custom --base-url "$BASE/v1" --api-key dummy \
  --model mock-model --query 'Say hello' --oneshot
```

Protocol: `openai-chat`. No wizard, login, or config preseed was needed. `HERMES_DISABLE_LAZY_INSTALLS=1` is important: without it Hermes begins building a new isolated runtime/source-update dependency environment before inference. `--oneshot` forces answer-and-exit behavior even in a herdr PTY. Endpoint discovery first hit local `/api/v1/models`, `/api/tags`, `/v1/props`, `/props`, `/version`, `/v1/models`, `/models`, `/v1/models/mock-model`, and POST `/api/show`; rejected discovery was tolerated, then POST `/v1/chat/completions` reached port 42527. A listener must not stop on the first discovery GET if it intends to check inference.

## pi

```sh
export PI_CODING_AGENT_DIR="$ROOT/agent" PI_OFFLINE=1 PI_TELEMETRY=0
cat > "$PI_CODING_AGENT_DIR/models.json" <<EOF
{"providers":{"mock":{"baseUrl":"$BASE/v1","api":"openai-completions","apiKey":"dummy","models":[{"id":"mock-model"}]}}}
EOF
pi --offline --no-approve -p --model mock/mock-model 'Say hello'
```

Protocol: `openai-chat` (`openai-completions` is pi's API name). No onboarding needed. `--no-approve` ignores **project-local** resources; it does not disable the user-level adapter extension. Do not add `--no-extensions` in plugin E2E. Offline mode disables startup network operations, not the explicitly configured local model request. The listener received POST `/v1/chat/completions` on port 43497. This is the installed pi 1.1.0 contract, not an older pi version.

## omp

```sh
export PI_CODING_AGENT_DIR="$ROOT/agent"
export PI_CONFIG_DIR=omp-config
cat > "$PI_CODING_AGENT_DIR/models.yml" <<EOF
providers:
  mock:
    baseUrl: $BASE/v1
    apiKey: dummy
    api: openai-completions
    models:
      - id: mock-model
        name: Mock
        reasoning: false
        input: [text]
        contextWindow: 128000
        maxTokens: 4096
        cost: {input: 0, output: 0, cacheRead: 0, cacheWrite: 0}
EOF
bun "$REAL_HOME/omp-repro/src/packages/coding-agent/src/cli.ts" \
  -p --model mock/mock-model 'Say hello'
```

Protocol: `openai-chat`. No onboarding needed. The actual probe used an absolute `PI_CONFIG_DIR=$ROOT/omp-config`; OMP treats this as a **directory name under HOME**, resulting in a nested but still isolated path. The relative `omp-config` shown above avoids that unnecessary nesting; the verified `PI_CODING_AGENT_DIR` controls the model/extension location in either case. Do not inherit `OMP_PROFILE` or `PI_PROFILE`: named profiles ignore the default agent-directory override. Keep extensions enabled for plugin E2E. The listener received POST `/v1/chat/completions` on port 38283.

## Gemini CLI

```sh
export GEMINI_CLI_HOME="$ROOT/home"
export GEMINI_API_KEY=dummy GOOGLE_GEMINI_BASE_URL="$BASE"
export GEMINI_TELEMETRY_ENABLED=false
mkdir -p "$GEMINI_CLI_HOME/.gemini"
cat > "$GEMINI_CLI_HOME/.gemini/settings.json" <<'EOF'
{"security":{"auth":{"selectedType":"gemini-api-key"}},"telemetry":{"enabled":false}}
EOF
gemini --skip-trust -p 'Say hello' -m gemini-2.5-flash
```

Protocol: `gemini`. **GEMINI_CLI_HOME is a home root, not the `.gemini` directory**; both binary and plugin setup append `.gemini/settings.json`. Without selectedType preseed the tested CLI exited `Invalid auth method selected.` even with GEMINI_API_KEY. API-key auth plus `--skip-trust` bypasses OAuth and workspace prompts. The listener received POST `/v1beta/models/gemini-2.5-flash:streamGenerateContent?alt=sse` on port 41873. Give the host root, not `/v1beta`; the SDK adds version/model/action paths. This tests direct Gemini API-key mode, not Code Assist RPC/OAuth.

## OpenCode

```sh
export OPENCODE_CONFIG_DIR="$ROOT/agent"
export OPENCODE_DISABLE_SHARE=true OPENCODE_DISABLE_MODELS_FETCH=true
export OPENCODE_CONFIG_CONTENT="{\"provider\":{\"mock\":{\"npm\":\"@ai-sdk/openai-compatible\",\"name\":\"Mock\",\"options\":{\"baseURL\":\"$BASE/v1\",\"apiKey\":\"dummy\"},\"models\":{\"mock-model\":{\"name\":\"Mock\"}}}},\"model\":\"mock/mock-model\"}"
opencode run --model mock/mock-model 'Say hello'
```

Protocol: `openai-chat`. `run` needed no onboarding/trust preseed in the empty workspace. Both `OPENCODE_CONFIG_DIR` and XDG directories must be isolated because provider config, plugins, and state have distinct paths. The listener received POST `/v1/chat/completions` on port 45557. This recipe uses the observed singular `provider` V1 config shape. No login or built-in provider credentials are necessary.

## Qwen Code

```sh
export QWEN_HOME="$ROOT/agent"
export OPENAI_API_KEY=dummy OPENAI_BASE_URL="$BASE/v1"
qwen --auth-type openai --openai-api-key dummy --openai-base-url "$BASE/v1" \
  --telemetry=false --yolo -m mock-model -p 'Say hello'
```

Protocol: `openai-chat`. Explicit API-key auth bypasses the removed OAuth login flow; `--yolo` permits the mocked harmless tool turn. No first-run config preseed was needed. Do not add `--bare` or `--safe-mode`, which suppress hooks. The listener received POST `/v1/chat/completions` on port 39887. A no-sandbox YOLO warning was printed; the command is intended only for trusted canned E2E responses in this empty isolated workspace.

## Kimi: legacy Python CLI and official successor

These are **different products/contracts**, both installed and independently checked. Do not infer the successor's home, hooks, or transcripts from the legacy implementation.

### Legacy Python kimi-cli 1.28.0

```sh
export KIMI_SHARE_DIR="$ROOT/agent"
cat > "$KIMI_SHARE_DIR/config.toml" <<EOF
default_model = "mock"
[providers.mock]
type = "openai_legacy"
base_url = "$BASE/v1"
api_key = "dummy"
[models.mock]
provider = "mock"
model = "mock-model"
max_context_size = 128000
EOF
"$REAL_HOME/.local/bin/kimi" --print --yolo --prompt 'Say hello'
```

Protocol: `openai-chat`. Custom provider config bypasses login/setup; print mode plus YOLO bypasses interactive approvals. The listener received POST `/v1/chat/completions` on port 41975. The legacy default home is `~/.kimi`, relocation `KIMI_SHARE_DIR`, and hooks use `[[hooks]]` in config.toml.

Installing latest `kimi-cli` initially gave 1.52.0. Its `__main__.py` intentionally short-circuits every CLI action before the original CLI dispatch. Exact printed notice:

> kimi-cli is no longer maintained. Please use the new Kimi Code CLI. Install: curl -fsSL https://code.kimi.com/kimi-code/install.sh | bash

It is not a functioning latest agent for this harness; the installed legacy binary was replaced with the requested supported minimum, `uv tool install --force 'kimi-cli==1.28.0'`.

### Successor Kimi Code 2.1.1

Installed separately with the [official installer](https://code.kimi.com/kimi-code/install.sh). Minimal installer PATH avoids its automatic migration/rename of existing Python `kimi` shims; relocation also keeps its installer-created region marker out of real configs:

```sh
curl -fsSL https://code.kimi.com/kimi-code/install.sh | \
  env PATH=/usr/bin:/bin \
  KIMI_INSTALL_DIR=$HOME/.local/share/kimi-code \
  KIMI_CODE_HOME=$HOME/.hra-e2e/kimi-successor/agent \
  KIMI_NO_MODIFY_PATH=1 bash
```

The binary is `~/.local/share/kimi-code/bin/kimi` (actual binary name `kimi`, version 2.1.1). It is deliberately not placed ahead of legacy `~/.local/bin/kimi` on PATH. For this subsection initialize common isolation with `AGENT=kimi-successor`:

```sh
export KIMI_CODE_HOME="$ROOT/agent"
cat > "$KIMI_CODE_HOME/config.toml" <<EOF
default_model = "mock"
[providers.mock]
type = "openai"
base_url = "$BASE/v1"
api_key = "dummy"
[models.mock]
provider = "mock"
model = "mock-model"
max_context_size = 128000
EOF
printf '[upgrade]\nauto_install = false\n' > "$KIMI_CODE_HOME/tui.toml"
"$REAL_HOME/.local/share/kimi-code/bin/kimi" --prompt 'Say hello'
```

Protocol: `openai-chat`; the listener received POST `/v1/chat/completions` on port 33737 without login. `--prompt` is itself noninteractive. **Do not combine it with `--auto`**: the observed binary rejects that combination. Default data root is `~/.kimi-code`, relocation `KIMI_CODE_HOME`. All config/session/credential/log paths move with it, while generic `.agents` resources also require HOME isolation.

Official successor hook config is `[[hooks]]` in `$KIMI_CODE_HOME/config.toml`, allowing only `event`, optional `matcher`, `command`, optional `timeout` (seconds, 1–600, default 30). Example:

```toml
[[hooks]]
event = "PostToolUse"
command = "/absolute/path/to/observer"
timeout = 5
```

Supported events include SessionStart, SessionEnd, TurnStarted, UserPromptSubmit, PreToolUse, PostToolUse, PostToolUseFailure, Stop, StopFailure, SubagentStart, SubagentStop, and more. Hook stdin uses snake_case and `client_type = "kimi_code_cli"`; session wire data is now `$KIMI_CODE_HOME/sessions/<workDirKey>/<sessionId>/agents/main/wire.jsonl`. Those facts reverse the claim that `.kimi-code` is always an incorrect herdr target: it is correct for the official successor, not legacy Python kimi-cli. Successor hooks were documented, **not plugin-integrated or acceptance-tested by this prep task**.

Sources: [migration](https://moonshotai.github.io/kimi-code/en/guides/migration.md), [data locations](https://moonshotai.github.io/kimi-code/en/configuration/data-locations.md), [providers](https://moonshotai.github.io/kimi-code/en/configuration/providers.md), [hooks](https://moonshotai.github.io/kimi-code/en/customization/hooks.md).

## Grok Build (official xai-org)

```sh
export GROK_HOME="$ROOT/agent" XAI_API_KEY=dummy
export GROK_MODELS_BASE_URL="$BASE/v1" GROK_XAI_API_BASE_URL="$BASE/v1"
export GROK_CLI_CHAT_PROXY_BASE_URL="$BASE/v1"
cat > "$GROK_HOME/config.toml" <<EOF
[models]
default = "mock"
[model.mock]
model = "mock-model"
base_url = "$BASE/v1"
api_key = "dummy"
api_backend = "chat_completions"
context_window = 128000
EOF
grok --disable-web-search --always-approve -m mock -p 'Say hello'
```

Protocol: `openai-chat` for the configured primary model, with an auxiliary `openai-responses` request also observed. A bare dummy `XAI_API_KEY` plus built-in model first attempted GET `/v1/api-key` and then refused authentication on the listener's intentional 400. The explicit **per-model** API key/base URL above bypassed that built-in login requirement; it reached primary POST `/v1/chat/completions` on port 39545. Before primary inference it also issued local GET `/v1/models` (twice), GET `/`, and POST `/v1/responses`. Keep all three global endpoint overrides local, not just the custom primary model base URL. Single-prompt mode needed no onboarding/trust preseed. The harmless tool turn is permitted with `--always-approve`.

Sources: [official authentication](https://github.com/xai-org/grok-build/blob/main/crates/codegen/xai-grok-pager/docs/user-guide/02-authentication.md), [custom-model API backends](https://github.com/xai-org/grok-build/blob/main/crates/codegen/xai-grok-pager/docs/user-guide/11-custom-models.md).

## GitHub Copilot CLI

```sh
export COPILOT_HOME="$ROOT/agent"
export COPILOT_PROVIDER_BASE_URL="$BASE/v1" COPILOT_PROVIDER_API_KEY=dummy
export COPILOT_PROVIDER_TYPE=openai COPILOT_PROVIDER_WIRE_API=completions
export COPILOT_MODEL=gpt-4.1
copilot --no-remote --allow-all -p 'Say hello'
```

Protocol: `openai-chat` BYOK. `copilot help providers` explicitly states GitHub authentication is not required with a custom provider, and the real binary confirmed this by POST `/v1/chat/completions` on port 35779 with no login. No onboarding/config preseed was needed. `--allow-all` permits the mocked harmless tool turn in the isolated workspace; `--no-remote` avoids remote control. Do not inherit a `COPILOT_PROVIDERS_CONFIG` override or existing providers registry, which can take precedence over environment BYOK. `COPILOT_PROVIDER_WIRE_API=responses` is also documented, but this prep verified completions only.

## Cursor Agent — impossible with these mock-provider protocols

```sh
export CURSOR_CONFIG_DIR="$ROOT/agent"
export CURSOR_API_KEY=dummy CURSOR_API_ENDPOINT="$BASE"
cursor-agent --endpoint "$BASE" --api-key dummy --trust -p 'Say hello'
```

Observed local POST `/auth/exchange_user_api_key` on port 38261, body `{}`, followed by `Warning: The provided API key is invalid.` No standard inference request was sent. `--trust` bypasses the workspace prompt, and HOME/XDG plus CURSOR_CONFIG_DIR isolate config, but there is no OpenAI/Anthropic/Gemini provider base-URL override in this CLI. `--endpoint` / `CURSOR_API_ENDPOINT` targets **Cursor's proprietary application server**, whose auth exchange/session/model routing must also be implemented. A standard mock provider cannot replace server-side Cursor inference. Therefore no account-free text/tool/usage acceptance is possible with this project's standard provider mocks. This is not a claim that an entire proprietary Cursor server could never be emulated; that is a different harness. Do not attempt a real Cursor login or key to work around it. Native adapter fixture tests remain usable.

## Evidence and scope

Prep artifacts on Spark: `$HOME/.hra-e2e/<agent>/prep-result.json` (observed command, isolated environment, recorded request paths/body prefix, return code, output) and `prep-output.log`. The temporary listener script is not part of the committed harness. No real keys are in these artifacts. All listener ports were ephemeral and listeners/agent process groups were stopped after each check.

Eleven canonical agents plus both Kimi implementations reached local model requests. Cursor reached only its proprietary authentication endpoint and is explicitly impossible against the specified standard mocks. These checks deliberately do **not** claim successful completions, hooks, tool execution, exact usage, spool/OTLP delivery, herdr metadata, or remove round trips; those are the main E2E harness's acceptance work.

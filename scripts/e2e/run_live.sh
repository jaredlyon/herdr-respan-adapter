#!/usr/bin/env bash
# Live check: a real agent login and real Respan ingestion (no mocks). Spans pass through a loopback tap
# (mock_otlp.py --forward) so we can assert what was sent, then they're read back from Respan's API.
#   HRA_LIVE_DIR (default ~/.hra-e2e/live) must hold config/.env with RESPAN_API_KEY, plus
#   claude-token (CLAUDE_CODE_OAUTH_TOKEN from `claude setup-token`) or codex/auth.json (`codex login`).
# Usage: bash scripts/e2e/run_live.sh claude|codex
set -euo pipefail
agent=${1:?usage: run_live.sh claude|codex}
LIVE=${HRA_LIVE_DIR:-$HOME/.hra-e2e/live}
export HRA_BIN=${HRA_BIN:-$(cd "$(dirname "$0")/../.." && pwd)/target/release/herdr-respan}
source "$(dirname "$0")/lib.sh"
hra_init "live-$agent"
python3 "$HRA_REPO/scripts/e2e/mock_otlp.py" --port-file "$HRA_ROOT/tap.port" --output "$HRA_ROOT/otlp" --forward https://api.respan.ai >"$HRA_ROOT/tap.log" 2>&1 &
HRA_MOCK_PIDS+=("$!")
for ((i=0;i<50;i++)); do [[ -s "$HRA_ROOT/tap.port" ]] && break; sleep .1; done
printf 'base_url = "http://127.0.0.1:%s"\n' "$(cat "$HRA_ROOT/tap.port")" >"$HRA_ROOT/config/config.toml"
install -m 600 "$LIVE/config/.env" "$HRA_ROOT/config/.env"
text_prompt='Reply with exactly: hra-live-ok'
tool_prompt='Run the shell command `echo hra-e2e` and then reply with its output.'

case $agent in
claude)
    export CLAUDE_CONFIG_DIR="$HRA_ROOT/claude"
    mkdir -p "$CLAUDE_CONFIG_DIR"
    printf '%s' '{"permissions":{"allow":["Bash(echo hra-e2e)"]},"env":{"DISABLE_AUTOUPDATER":"1"}}' >"$CLAUDE_CONFIG_DIR/settings.json"
    printf '%s' '{"hasCompletedOnboarding":true,"theme":"dark","autoUpdates":false}' >"$CLAUDE_CONFIG_DIR/.claude.json"
    "$HRA_BIN" setup claude --yes --state-dir "$HRA_ROOT/state" --config-dir "$HRA_ROOT/config" >"$HRA_ROOT/setup.out"
    hra_start_pane "CLAUDE_CONFIG_DIR=$CLAUDE_CONFIG_DIR" "ANTHROPIC_API_KEY=" "ANTHROPIC_BASE_URL=" "DISABLE_AUTOUPDATER=1"
    # Token read inside the pane so it never appears in argv or the herdr API.
    run="CLAUDE_CODE_OAUTH_TOKEN=\$(cat '$LIVE/claude-token') claude -p --model claude-haiku-4-5-20251001 --tools Bash --allowedTools 'Bash(echo hra-e2e)' --setting-sources user --strict-mcp-config --mcp-config '{\"mcpServers\":{}}' --"
    ;;
codex)
    export CODEX_HOME="$HRA_ROOT/codex"
    mkdir -p "$CODEX_HOME" "$HRA_ROOT/home"
    install -m 600 "$LIVE/codex/auth.json" "$CODEX_HOME/auth.json"
    printf '%s\n' 'model = "gpt-6-luna"' 'approval_policy = "never"' 'sandbox_mode = "danger-full-access"' '[features]' 'hooks = true' >"$CODEX_HOME/config.toml"
    "$HRA_BIN" setup codex --yes --state-dir "$HRA_ROOT/state" --config-dir "$HRA_ROOT/config" >"$HRA_ROOT/setup.out"
    # Live-check-only pretrust inside this throwaway CODEX_HOME (same scheme as run_codex.sh); setup never does this.
    python3 - "$CODEX_HOME" <<'PY'
import hashlib, json, pathlib, re, sys
home = pathlib.Path(sys.argv[1]).resolve(); path = home/'hooks.json'; trust = ''
assert home != pathlib.Path.home()/'.codex'
for event, groups in json.loads(path.read_text())['hooks'].items():
    label = re.sub(r'(?<!^)(?=[A-Z])', '_', event).lower()
    for gi, group in enumerate(groups):
        for hi, hook in enumerate(group['hooks']):
            identity = {'event_name':label,'hooks':[{'type':'command','command':hook['command'],'timeout':hook.get('timeout',600),'async':False}]}
            if group.get('matcher'): identity['matcher'] = group['matcher']
            digest = 'sha256:' + hashlib.sha256(json.dumps(identity,sort_keys=True,separators=(',',':'),ensure_ascii=False).encode()).hexdigest()
            trust += f'\n[hooks.state.{json.dumps(f"{path}:{label}:{gi}:{hi}")}]\ntrusted_hash = "{digest}"\n'
(home/'config.toml').write_text((home/'config.toml').read_text()+trust)
PY
    hra_start_pane "CODEX_HOME=$CODEX_HOME"
    run="env -u OPENAI_API_KEY HOME='$HRA_ROOT/home' CODEX_HOME='$CODEX_HOME' codex exec --skip-git-repo-check --json"
    ;;
*) echo "unknown agent: $agent" >&2; exit 2 ;;
esac

hra_prompt "$run '$text_prompt' </dev/null" text
hra_prompt "$run '$tool_prompt' </dev/null" tool
hra_flush
hra_assert
[[ $agent == codex ]] && install -m 600 "$CODEX_HOME/auth.json" "$LIVE/codex/auth.json"  # keep refreshed tokens
"$HRA_BIN" remove "$agent" --state-dir "$HRA_ROOT/state" --config-dir "$HRA_ROOT/config"

python3 - "$HRA_ROOT" "$LIVE/config/.env" "$HRA_PANE" "$agent" <<'PY'
import collections, json, pathlib, sys, time, urllib.error, urllib.request
root, pane, agent = pathlib.Path(sys.argv[1]), sys.argv[3], sys.argv[4]
key = next(l.split('=',1)[1].strip() for l in pathlib.Path(sys.argv[2]).read_text().splitlines() if l.startswith('RESPAN_API_KEY='))
def attrs(s): return {a['key']: next(iter(a['value'].values())) for a in s['attributes']}
for r in root.glob('otlp/*.response'):
    res = json.loads(r.read_text())
    assert res['status'] == 200, res
    assert not json.loads(res['body'] or '{}').get('partialSuccess', {}).get('rejectedSpans'), res
spans = json.loads((root/'spans.json').read_text())
traces = collections.defaultdict(list)
for s in spans: traces[s['traceId']].append(s)
assert len(traces) == 2, list(traces)
shapes = []
for tid, tree in traces.items():
    kinds = collections.Counter(attrs(s)['respan.entity.log_type'] for s in tree)
    assert kinds['agent'] == 1 and kinds['chat'] >= 1, kinds
    for s in tree:
        a = attrs(s)
        if a['respan.entity.log_type'] == 'chat':
            assert int(a['gen_ai.usage.input_tokens']) > 0 and int(a['gen_ai.usage.output_tokens']) > 0, a
        if a['respan.entity.log_type'] == 'tool':
            assert 'echo hra-e2e' in a['traceloop.entity.input'], a
    shapes.append(kinds['tool'])
    # Read back from Respan; ingestion is asynchronous.
    for _ in range(60):
        try:
            req = urllib.request.Request(f'https://api.respan.ai/api/traces/{tid}/', headers={'Authorization': f'Bearer {key}'})
            with urllib.request.urlopen(req, timeout=30) as r: remote = json.load(r)
            if remote.get('span_count') == len(tree): break
        except urllib.error.HTTPError as e:
            if e.code != 404: raise
        time.sleep(2)
    else:
        raise SystemExit(f'trace {tid} not fully visible in Respan')
    def walk(nodes, depth=0):
        for n in nodes:
            md = n.get('metadata') or {}
            assert md.get('herdr.pane_id') == pane and md.get('agent') == agent, md  # flattened metadata promoted
            print('  '*depth + f"- {n['span_name']} [{n['log_type']}] model={n.get('model')} tokens={n.get('prompt_tokens')}/{n.get('completion_tokens')} cost={n.get('cost')} pane={md['herdr.pane_id']}")
            walk(n.get('children') or [], depth+1)
    print(f"Respan trace {tid}: {remote['span_count']} spans, {remote.get('llm_call_count')} LLM calls, "
          f"tokens {remote.get('total_prompt_tokens')}/{remote.get('total_completion_tokens')}, customer={remote.get('customer_identifier')}")
    walk(remote.get('span_tree') or [])
assert sorted(min(t,1) for t in shapes) == [0, 1], shapes
print(f'Live check passed: {len(spans)} spans sent, accepted and read back from Respan')
PY

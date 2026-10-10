#!/usr/bin/env bash
set -euo pipefail
export HRA_BIN=${HRA_BIN:-$(cd "$(dirname "$0")/../.." && pwd)/target/release/herdr-respan}
source "$(dirname "$0")/lib.sh"
hra_init codex
hra_start_mocks
export CODEX_HOME="$HRA_ROOT/codex" OPENAI_API_KEY=hra-e2e-dummy
mkdir -p "$CODEX_HOME" "$HRA_ROOT/home" "$HRA_ROOT/xdg-cache" "$HRA_ROOT/xdg-data" "$HRA_ROOT/xdg-state"
python3 - "$HRA_ROOT" "$HRA_PROVIDER_URL" <<'PY'
import json, pathlib, sys
root = pathlib.Path(sys.argv[1])
(root/'capture.py').write_text('import json,pathlib,sys\np=json.load(sys.stdin)\nwith pathlib.Path(sys.argv[1]).open("a") as f: f.write(json.dumps(p)+"\\n")\n')
command = f'python3 "{root}/capture.py" "{root}/hooks.jsonl"'
events = ['SessionStart','UserPromptSubmit','PostToolUse','Stop','SessionEnd']
hooks = {'hooks': {e:[{'hooks':[{'type':'command','command':command,'timeout':3 if e=='SessionEnd' else 10}]}] for e in events}}
(root/'codex/hooks.json').write_text(json.dumps(hooks))
(root/'hooks.before.json').write_text(json.dumps(hooks))
(root/'codex/config.toml').write_text(f'''model = "gpt-4.1"
model_provider = "mock"
approval_policy = "never"
sandbox_mode = "danger-full-access"
[features]
hooks = true
shell_snapshot = false
[analytics]
enabled = false
[feedback]
enabled = false
[model_providers.mock]
name = "Mock"
base_url = "{sys.argv[2]}/v1"
env_key = "OPENAI_API_KEY"
wire_api = "responses"
''')
PY
"$HRA_BIN" setup codex --yes --state-dir "$HRA_ROOT/state" --config-dir "$HRA_ROOT/config" >"$HRA_ROOT/setup.out"
# E2E-only pretrust: exact definitions in this fresh CODEX_HOME; product setup never does this.
python3 - "$HRA_ROOT" <<'PY'
import hashlib, json, pathlib, re, sys
root = pathlib.Path(sys.argv[1]).resolve()
home = (root/'codex').resolve()
assert home.is_relative_to(root) and home != pathlib.Path.home()/'.codex'
path = home/'hooks.json'
trust = ''
for event, groups in json.loads(path.read_text())['hooks'].items():
    label = re.sub(r'(?<!^)(?=[A-Z])', '_', event).lower()
    for gi, group in enumerate(groups):
        for hi, hook in enumerate(group['hooks']):
            assert hook['type'] == 'command'
            assert str(root) in hook['command'], hook
            # Codex normalizes timeout and hashes sorted compact JSON via TOML (None omitted).
            handler = {'type':'command','command':hook['command'],'timeout':hook.get('timeout',600),'async':False}
            identity = {'event_name':label,'hooks':[handler]}
            if group.get('matcher'): identity['matcher'] = group['matcher']
            digest = 'sha256:' + hashlib.sha256(json.dumps(identity,sort_keys=True,separators=(',',':'),ensure_ascii=False).encode()).hexdigest()
            key = f'{path}:{label}:{gi}:{hi}'
            trust += f'\n[hooks.state.{json.dumps(key)}]\ntrusted_hash = "{digest}"\n'
config = home/'config.toml'
config.write_text(config.read_text()+trust)
(root/'config.before.toml').write_text(config.read_text())
PY
# Isolate generic homes/caches as well as native config; preserve only Herdr pane metadata.
command="env -i PATH='$PATH' HOME='$HRA_ROOT/home' CODEX_HOME='$CODEX_HOME' OPENAI_API_KEY=hra-e2e-dummy XDG_CONFIG_HOME='$HRA_ROOT/xdg' XDG_CACHE_HOME='$HRA_ROOT/xdg-cache' XDG_DATA_HOME='$HRA_ROOT/xdg-data' XDG_STATE_HOME='$HRA_ROOT/xdg-state' LANG=C.UTF-8 CI=1 NO_COLOR=1 TERM=dumb NO_PROXY=127.0.0.1,localhost DO_NOT_TRACK=1 OTEL_SDK_DISABLED=true"
hra_start_pane "CODEX_HOME=$CODEX_HOME" "OPENAI_API_KEY=hra-e2e-dummy"
pane_command="$command HERDR_ENV=1 HERDR_RESPAN=on HERDR_PANE_ID=\$HERDR_PANE_ID HERDR_WORKSPACE_ID=\$HERDR_WORKSPACE_ID HERDR_TAB_ID=\$HERDR_TAB_ID codex exec --skip-git-repo-check --json"
hra_prompt "$pane_command 'Say the deterministic mock text.'" text
hra_prompt "$pane_command 'RUN_TOOL: execute echo hra-e2e then reply.'" tool
hra_flush
hra_assert
python3 - "$HRA_ROOT" <<'PY'
import collections, json, pathlib, sys
root = pathlib.Path(sys.argv[1])
spans = json.loads((root/'spans.json').read_text())
def attrs(s): return {a['key']:next(iter(a['value'].values())) for a in s['attributes']}
traces = collections.defaultdict(list)
for s in spans:
    assert s['status']['code'] == 1, s
    assert attrs(s)['respan.threads.thread_identifier']
    traces[s['traceId']].append(s)
assert len(traces) == 2, traces
rollouts = [list(map(json.loads,p.read_text().splitlines())) for p in (root/'codex/sessions').rglob('rollout-*.jsonl')]
assert len(rollouts) == 2
usage_records = [v['payload'] for rows in rollouts for v in rows if v['type']=='token_usage_record']
chats = [s for s in spans if attrs(s)['respan.entity.log_type']=='chat']
assert len(chats) == len(usage_records) == 3, (len(chats),usage_records)
assert len({r['response_id'] for r in usage_records}) == 3
counts = []
for tree in traces.values():
    turns = [s for s in tree if attrs(s)['respan.entity.log_type']=='agent']
    calls = [s for s in tree if attrs(s)['respan.entity.log_type']=='chat']
    tools = [s for s in tree if attrs(s)['respan.entity.log_type']=='tool']
    assert len(turns)==1 and not turns[0].get('parentSpanId'), tree
    print(f"trace {turns[0]['traceId']}: {turns[0]['name']}")
    for s in calls+tools:
        a = attrs(s)
        assert s['parentSpanId']==turns[0]['spanId'], s
        if s in calls:
            for k,v in [('gen_ai.usage.input_tokens',11),('gen_ai.usage.output_tokens',7),('gen_ai.usage.prompt_tokens',11),('gen_ai.usage.completion_tokens',7),('llm.usage.cache_read_input_tokens',3),('llm.usage.reasoning_tokens',0)]: assert int(a[k])==v, a
            assert json.loads(a['respan.metadata'])['codex.usage_source']=='token_usage_record'
            print(f"  chat {s['name']}: input=11 output=7 cache_read=3 reasoning=0")
        else:
            assert 'echo hra-e2e' in a['traceloop.entity.input'], a
            assert 'hra-e2e' in a['traceloop.entity.output'], a
            print(f"  tool {s['name']}: echo hra-e2e -> hra-e2e")
    counts.append((len(turns),len(calls),len(tools)))
assert sorted(counts)==[(1,1,0),(1,2,1)], counts
requests = [json.loads(l) for l in (root/'provider.jsonl').read_text().splitlines()]
assert len([r for r in requests if r['method']=='POST' and r['path']=='/v1/responses'])==3, requests
(root/'received.before.json').write_text(json.dumps({p.name:p.read_text() for p in (root/'otlp').glob('*.json')}))
print(f'Asserted {len(spans)} spans, one chat per {len(usage_records)} token_usage_records')
PY
# Close stdin: codex exec otherwise waits for appended piped input under SSH.
(cd "$HRA_ROOT/work" && eval "$command timeout 120 codex exec --skip-git-repo-check --json 'RUN_TOOL: execute echo hra-e2e then reply.'") </dev/null >"$HRA_ROOT/outside.out" 2>"$HRA_ROOT/outside.err"
hra_flush
python3 - "$HRA_ROOT" <<'PY'
import json, pathlib, sys
root=pathlib.Path(sys.argv[1])
assert json.loads((root/'received.before.json').read_text())=={p.name:p.read_text() for p in (root/'otlp').glob('*.json')}
requests = [json.loads(l) for l in (root/'provider.jsonl').read_text().splitlines()]
assert len([r for r in requests if r['method']=='POST' and r['path']=='/v1/responses']) == 5
events = [json.loads(l) for l in (root/'outside.out').read_text().splitlines()]
assert any(e['type']=='turn.completed' for e in events), events
assert any(e['type']=='item.completed' and e.get('item',{}).get('type')=='command_execution' and 'hra-e2e' in e['item']['aggregated_output'] and e['item']['exit_code']==0 for e in events), events
print('Outside-herdr gate: no new OTLP delivered')
PY
"$HRA_BIN" remove codex --state-dir "$HRA_ROOT/state" --config-dir "$HRA_ROOT/config"
python3 - "$HRA_ROOT" <<'PY'
import json, pathlib, sys, tomllib
root=pathlib.Path(sys.argv[1])
assert json.loads((root/'hooks.before.json').read_text())==json.loads((root/'codex/hooks.json').read_text())
assert tomllib.loads((root/'config.before.toml').read_text())==tomllib.loads((root/'codex/config.toml').read_text())
print('Remove: isolated hooks JSON and config TOML semantically restored; E2E trust preserved')
PY

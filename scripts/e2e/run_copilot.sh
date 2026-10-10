#!/usr/bin/env bash
set -euo pipefail
HRA_BIN=${HRA_BIN:-"$(cd "$(dirname "$0")/../.." && pwd)/target/release/herdr-respan"}
source "$(dirname "$0")/lib.sh"
hra_init copilot
hra_start_mocks
export HOME="$HRA_ROOT/home" COPILOT_HOME="$HRA_ROOT/copilot"
export XDG_CONFIG_HOME="$HRA_ROOT/xdg/config" XDG_CACHE_HOME="$HRA_ROOT/xdg/cache" XDG_DATA_HOME="$HRA_ROOT/xdg/data" XDG_STATE_HOME="$HRA_ROOT/xdg/state"
mkdir -p "$HOME" "$COPILOT_HOME"
python3 - "$HRA_ROOT" <<'PY'
import json, pathlib, sys
root = pathlib.Path(sys.argv[1])
(root/'capture.py').write_text('import json, pathlib, sys\npayload=json.load(sys.stdin)\nwith pathlib.Path(sys.argv[1]).open("a") as f: f.write(json.dumps({"event":sys.argv[2],"payload":payload})+"\\n")\n')
events = ['sessionStart','userPromptSubmitted','postToolUse','postToolUseFailure','agentStop','sessionEnd']
settings = {'hooks': {e: [{'type':'command','bash':f'python3 "{root}/capture.py" "{root}/hooks.jsonl" {e}','timeoutSec':10}] for e in events}}
(root/'copilot/settings.json').write_text(json.dumps(settings))
(root/'settings.before.json').write_text(json.dumps(settings))
(root/'home/.bashrc').write_text('# isolated E2E profile\n')
# Explicit environment construction prevents ambient account/provider/OTLP settings.
(root/'launch.py').write_text('''import os, subprocess, sys
env = {k:v for k,v in os.environ.items() if k.startswith('HERDR_') or k in ('PATH','HOME','COPILOT_HOME','XDG_CONFIG_HOME','XDG_CACHE_HOME','XDG_DATA_HOME','XDG_STATE_HOME')}
env.update(COPILOT_PROVIDER_BASE_URL=sys.argv[1]+'/v1', COPILOT_PROVIDER_API_KEY='hra-e2e-dummy', COPILOT_PROVIDER_TYPE='openai', COPILOT_PROVIDER_WIRE_API='completions', COPILOT_MODEL='gpt-4.1', NO_PROXY='127.0.0.1,localhost', DO_NOT_TRACK='1', CI='1', NO_COLOR='1', TERM='dumb')
if sys.argv[2] == 'exporter':
    env['COPILOT_OTEL_FILE_EXPORTER_PATH'] = sys.argv[3]+'/state/agents/copilot/otel.jsonl'
sys.exit(subprocess.call(['copilot','--no-remote','--no-remote-export','--allow-all','-p',sys.argv[4]],env=env))
''')
PY
"$HRA_BIN" setup copilot --yes --state-dir "$HRA_ROOT/state" --config-dir "$HRA_ROOT/config" >"$HRA_ROOT/setup.out"
hra_start_pane "HOME=$HOME" "COPILOT_HOME=$COPILOT_HOME" "XDG_CONFIG_HOME=$XDG_CONFIG_HOME" "XDG_CACHE_HOME=$XDG_CACHE_HOME" "XDG_DATA_HOME=$XDG_DATA_HOME" "XDG_STATE_HOME=$XDG_STATE_HOME"
command="python3 '$HRA_ROOT/launch.py' '$HRA_PROVIDER_URL'"
hra_prompt "$command hooks '$HRA_ROOT' 'Say the deterministic mock text.'" hooks-text
hra_prompt "$command hooks '$HRA_ROOT' 'RUN_TOOL: execute echo hra-e2e then reply.'" hooks-tool
hra_flush
hra_assert
python3 - "$HRA_ROOT" <<'PY'
import collections, json, pathlib, sys
root=pathlib.Path(sys.argv[1])
spans=json.loads((root/'spans.json').read_text())
def attrs(s): return {a['key']:next(iter(a['value'].values())) for a in s['attributes']}
traces=collections.defaultdict(list)
for s in spans: traces[s['traceId']].append(s)
counts=[]
for tree in traces.values():
    kinds=collections.Counter(attrs(s)['respan.entity.log_type'] for s in tree)
    counts.append((kinds['agent'],kinds['chat'],kinds['tool']))
    for s in tree:
        if attrs(s)['respan.entity.log_type']=='chat':
            assert not any(k.startswith('gen_ai.usage.') for k in attrs(s)), s
assert sorted(counts)==[(1,1,0),(1,1,1)], counts
assert not (root/'state/agents/copilot/otel.jsonl').exists()
(root/'hooks-spans.json').write_text(json.dumps(spans))
for state_file in (root/'state/agents/copilot').glob('session-*.json'):
    state=json.loads(state_file.read_text())
    assert len(state['turns'])==1 and state['turns'][0]['settled'] and state['turns'][0]['end'] is not None, state
print('Hooks-only: 2 turns, 2 usage-less chats, 1 echo tool; no dangling initial turns')
PY
"$HRA_BIN" setup copilot --yes --accept copilot-otel-profile --state-dir "$HRA_ROOT/state" --config-dir "$HRA_ROOT/config" >"$HRA_ROOT/setup-exporter.out"
exporter_command=". '$HOME/.bashrc' && test \"\$COPILOT_OTEL_FILE_EXPORTER_PATH\" = '$HRA_ROOT/state/agents/copilot/otel.jsonl' && $command exporter"
hra_prompt "$exporter_command '$HRA_ROOT' 'Say the deterministic mock text.'" exporter-text
hra_prompt "$exporter_command '$HRA_ROOT' 'RUN_TOOL: execute echo hra-e2e then reply.'" exporter-tool
hra_flush
hra_assert
python3 - "$HRA_ROOT" <<'PY'
import collections, json, pathlib, sys
root=pathlib.Path(sys.argv[1])
spans=json.loads((root/'spans.json').read_text())
previous={s['spanId'] for s in json.loads((root/'hooks-spans.json').read_text())}
spans=[s for s in spans if s['spanId'] not in previous]
def attrs(s): return {a['key']:next(iter(a['value'].values())) for a in s['attributes']}
traces=collections.defaultdict(list)
for s in spans:
    a=attrs(s)
    assert a['respan.threads.thread_identifier']
    assert s['status']['code']==1, s
    traces[s['traceId']].append(s)
counts=[]
for trace, tree in traces.items():
    turns=[s for s in tree if attrs(s)['respan.entity.log_type']=='agent']
    chats=[s for s in tree if attrs(s)['respan.entity.log_type']=='chat']
    tools=[s for s in tree if attrs(s)['respan.entity.log_type']=='tool']
    assert len(turns)==1
    assert not turns[0].get('parentSpanId')
    print(f"trace {trace}: {turns[0]['name']}")
    for child in chats+tools:
        a=attrs(child)
        assert child['parentSpanId']==turns[0]['spanId'], child
        if child in chats:
            assert a['gen_ai.request.model']=='gpt-4.1', a
            for key, expected in [('gen_ai.usage.input_tokens',11),('gen_ai.usage.output_tokens',7),('gen_ai.usage.prompt_tokens',11),('gen_ai.usage.completion_tokens',7),('llm.usage.cache_read_input_tokens',3)]:
                assert int(a[key])==expected, a
            assert 'traceloop.entity.input' not in a
            assert json.loads(a['respan.metadata'])['copilot.turn_id']
            print('  chat gpt-4.1: input=11 output=7 cache_read=3 (native exporter)')
        else:
            assert 'echo hra-e2e' in a['traceloop.entity.input'], a
            assert 'hra-e2e' in a['traceloop.entity.output'], a
            print('  tool: echo hra-e2e -> hra-e2e (native hook)')
    counts.append((len(turns),len(chats),len(tools)))
assert sorted(counts)==[(1,1,0),(1,2,1)], counts
exporter_rows=[json.loads(line) for line in (root/'state/agents/copilot/otel.jsonl').read_text().splitlines()]
requests=[r for r in exporter_rows if r.get('type')=='span' and r.get('attributes',{}).get('gen_ai.operation.name')=='chat']
chats=[s for s in spans if attrs(s)['respan.entity.log_type']=='chat']
assert len(requests)==len(chats)==3
for request in requests:
    start=request['startTime'][0]*1_000_000_000+request['startTime'][1]
    matches=[s for s in chats if int(s['startTimeUnixNano'])==start and attrs(s)['respan.threads.thread_identifier']==request['attributes']['gen_ai.conversation.id']]
    assert len(matches)==1, request
    assert json.loads(attrs(matches[0])['respan.metadata'])['copilot.turn_id']==request['attributes']['github.copilot.turn_id']
for state_file in (root/'state/agents/copilot').glob('session-*.json'):
    state=json.loads(state_file.read_text())
    assert len(state['turns'])==1 and state['turns'][0]['settled'] and state['turns'][0]['end'] is not None, state
(root/'received.before.json').write_text(json.dumps({p.name:p.read_text() for p in (root/'otlp').glob('*.json')}))
print('Exporter: 2 turns, 3 request chats with exact usage, 1 echo tool')
PY
(cd "$HRA_ROOT/work" && env -u HERDR_ENV -u HERDR_PANE_ID -u HERDR_WORKSPACE_ID -u HERDR_TAB_ID -u HERDR_RESPAN_PROXY python3 "$HRA_ROOT/launch.py" "$HRA_PROVIDER_URL" exporter "$HRA_ROOT" 'RUN_TOOL: execute echo hra-e2e then reply.') >"$HRA_ROOT/outside.out" 2>"$HRA_ROOT/outside.err"
hra_flush
python3 - "$HRA_ROOT" <<'PY'
import json, pathlib, sys
root=pathlib.Path(sys.argv[1])
assert json.loads((root/'received.before.json').read_text())=={p.name:p.read_text() for p in (root/'otlp').glob('*.json')}
print('Outside-herdr gate: no new OTLP delivered')
PY
"$HRA_BIN" remove copilot --state-dir "$HRA_ROOT/state" --config-dir "$HRA_ROOT/config"
python3 - "$HRA_ROOT" <<'PY'
import json, pathlib, sys
root=pathlib.Path(sys.argv[1])
assert json.loads((root/'settings.before.json').read_text())==json.loads((root/'copilot/settings.json').read_text())
assert (root/'home/.bashrc').read_text()=='# isolated E2E profile\n'
print('Remove: isolated settings/profile semantically restored')
PY

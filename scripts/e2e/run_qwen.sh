#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/lib.sh"
hra_init qwen
hra_start_mocks
export QWEN_HOME="$HRA_ROOT/agent"
mkdir -p "$QWEN_HOME" "$HRA_ROOT"/{home,agent-xdg/config,agent-xdg/cache,agent-xdg/data,agent-xdg/state}
python3 - "$HRA_ROOT" "$HRA_PROVIDER_URL" "$HOME/.local/bin/qwen" <<'PY'
import json, pathlib, shlex, sys
root = pathlib.Path(sys.argv[1])
(root/'capture.py').write_text('import json, pathlib, sys\npayload=json.load(sys.stdin)\nwith pathlib.Path(sys.argv[1]).open("a") as f: f.write(json.dumps(payload)+"\\n")\n')
command = f'python3 "{root}/capture.py" "{root}/hooks.jsonl"'
events = ['SessionStart','UserPromptSubmit','PreToolUse','PostToolUse','PostToolUseFailure','Stop','StopFailure','SessionEnd','SubagentStart','SubagentStop']
# Preseed the native schema version so Qwen's first-run migration does not change the remove baseline.
settings = {'$version': 4, 'telemetry': {'enabled': False}, 'hooks': {e: [{'matcher':'*','hooks':[{'type':'command','command':command}]}] for e in events}}
(root/'agent/settings.json').write_text(json.dumps(settings))
(root/'settings.before.json').write_text(json.dumps(settings))
env = {'HOME': str(root/'home'), 'QWEN_HOME': str(root/'agent'), 'PATH': '/home/jlyon/.local/bin:/usr/local/bin:/usr/bin:/bin', 'XDG_CONFIG_HOME': str(root/'agent-xdg/config'), 'XDG_CACHE_HOME': str(root/'agent-xdg/cache'), 'XDG_DATA_HOME': str(root/'agent-xdg/data'), 'XDG_STATE_HOME': str(root/'agent-xdg/state'), 'LANG': 'C.UTF-8', 'CI': '1', 'NO_COLOR': '1', 'TERM': 'dumb', 'NO_PROXY': '127.0.0.1,localhost', 'DO_NOT_TRACK': '1', 'OTEL_SDK_DISABLED': 'true', 'OPENAI_API_KEY': 'hra-e2e-dummy', 'OPENAI_BASE_URL': sys.argv[2]+'/v1'}
runner = '#!/usr/bin/env bash\nset -euo pipefail\nextra=()\nfor key in HERDR_ENV HERDR_PANE_ID HERDR_WORKSPACE_ID HERDR_TAB_ID HERDR_RESPAN HERDR_MACHINE; do\n    [[ -v $key ]] && extra+=("$key=${!key}")\ndone\n'
runner += 'exec env -i '+ ' '.join(shlex.quote(k+'='+v) for k,v in env.items()) + ' "${extra[@]}" '+shlex.quote(sys.argv[3])+' --auth-type openai --openai-api-key hra-e2e-dummy --openai-base-url '+shlex.quote(sys.argv[2]+'/v1')+' --telemetry=false --yolo -m mock-model -p "$1"\n'
(root/'run-agent.sh').write_text(runner)
PY
HOME="$HRA_ROOT/home" "$HRA_BIN" setup qwen --yes --state-dir "$HRA_ROOT/state" --config-dir "$HRA_ROOT/config" >"$HRA_ROOT/setup.out"
hra_start_pane
command="bash '$HRA_ROOT/run-agent.sh'"
hra_prompt "$command 'Say the deterministic mock text.'" text
hra_prompt "$command 'RUN_TOOL: execute echo hra-e2e then reply.'" tool
hra_flush
hra_assert
python3 - "$HRA_ROOT" <<'PY'
import collections, json, pathlib, sys
root = pathlib.Path(sys.argv[1])
spans = json.loads((root/'spans.json').read_text())
def attrs(span):
    return {a['key']:next(iter(a['value'].values())) for a in span['attributes']}
traces = collections.defaultdict(list)
for span in spans:
    assert span['status']['code'] == 1, span
    assert attrs(span)['respan.threads.thread_identifier'], span
    traces[span['traceId']].append(span)
assert len(traces) == 2, traces
counts = []
for trace, tree in traces.items():
    turns = [s for s in tree if attrs(s)['respan.entity.log_type']=='agent']
    chats = [s for s in tree if attrs(s)['respan.entity.log_type']=='chat']
    tools = [s for s in tree if attrs(s)['respan.entity.log_type']=='tool']
    assert len(turns)==1 and not turns[0].get('parentSpanId'), tree
    print(f"trace {trace}: {turns[0]['name']}")
    for child in chats+tools:
        assert child['parentSpanId']==turns[0]['spanId'], child
        a = attrs(child)
        if child in chats:
            for key, value in [('gen_ai.usage.input_tokens',11),('gen_ai.usage.output_tokens',7),('gen_ai.usage.prompt_tokens',11),('gen_ai.usage.completion_tokens',7),('llm.usage.cache_read_input_tokens',3)]:
                assert int(a[key])==value, a
            assert a['gen_ai.request.model']=='mock-model', a
            print(f"  chat {child['name']}: input=11 output=7 cache_read=3")
        else:
            assert 'echo hra-e2e' in a['traceloop.entity.input'], a
            assert 'hra-e2e' in a['traceloop.entity.output'], a
            print(f"  tool {child['name']}: echo hra-e2e -> hra-e2e")
    counts.append((len(turns),len(chats),len(tools)))
assert sorted(counts)==[(1,1,0),(1,2,1)], counts
# Usage must actually be exposed in native transcripts, not supplied by the driver.
hooks = [json.loads(line) for line in (root/'hooks.jsonl').read_text().splitlines()]
paths = {p['transcript_path'] for p in hooks}
records = [json.loads(line) for path in paths for line in pathlib.Path(path).read_text().splitlines()]
assistants = [r for r in records if r.get('type')=='assistant']
assert len(assistants)==3, assistants
for r in assistants:
    u=r['usageMetadata']
    assert (u['promptTokenCount'],u['candidatesTokenCount'],u['cachedContentTokenCount'])==(11,7,3), r
print(f'Asserted {len(spans)} spans, two traces, transcript-derived usage')
(root/'received.before.json').write_text(json.dumps({p.name:p.read_text() for p in (root/'otlp').glob('*.json')}))
PY
(cd "$HRA_ROOT/work" && env -u HERDR_ENV -u HERDR_PANE_ID -u HERDR_WORKSPACE_ID -u HERDR_TAB_ID -u HERDR_RESPAN_PROXY bash "$HRA_ROOT/run-agent.sh" 'RUN_TOOL: execute echo hra-e2e then reply.') >"$HRA_ROOT/outside.out" 2>"$HRA_ROOT/outside.err"
hra_flush
python3 - "$HRA_ROOT" <<'PY'
import json, pathlib, sys
root=pathlib.Path(sys.argv[1])
assert json.loads((root/'received.before.json').read_text())=={p.name:p.read_text() for p in (root/'otlp').glob('*.json')}
print('Outside-herdr gate: no new OTLP delivered')
PY
HOME="$HRA_ROOT/home" "$HRA_BIN" remove qwen --state-dir "$HRA_ROOT/state" --config-dir "$HRA_ROOT/config"
python3 - "$HRA_ROOT" <<'PY'
import json, pathlib, sys
root=pathlib.Path(sys.argv[1])
assert json.loads((root/'settings.before.json').read_text())==json.loads((root/'agent/settings.json').read_text())
print('Remove: isolated settings.json semantically restored')
PY

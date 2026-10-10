#!/usr/bin/env bash
set -euo pipefail
HRA_BIN=${HRA_BIN:-"$(cd "$(dirname "$0")/../.." && pwd)/target/release/herdr-respan"}
source "$(dirname "$0")/lib.sh"
hra_init grok
# The wrapper owns one additional daemon in this run's isolated state directory.
grok_cleanup() {
    local status=$?
    if [[ -f "$HRA_ROOT/state/proxy.json" ]]; then
        local pid
        pid=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["pid"])' "$HRA_ROOT/state/proxy.json")
        kill "$pid" 2>/dev/null || true
    fi
    hra_cleanup
    return "$status"
}
trap grok_cleanup EXIT
hra_start_mocks
REAL_HOME=$HOME
export GROK_HOME="$HRA_ROOT/grok"
mkdir -p "$GROK_HOME/hooks" "$HRA_ROOT/home" "$HRA_ROOT/xdg"
python3 - "$HRA_ROOT" "$HRA_PROVIDER_URL" "$REAL_HOME" "$HRA_BIN" <<'PY'
import json, pathlib, sys
root, base, home, binary = sys.argv[1:]
r = pathlib.Path(root)
(r/'grok/config.toml').write_text(f'[models]\ndefault = "mock"\n[model.mock]\nmodel = "mock-model"\nbase_url = "{base}/v1"\napi_key = "dummy"\napi_backend = "chat_completions"\ncontext_window = 128000\n')
(r/'config.before.toml').write_bytes((r/'grok/config.toml').read_bytes())
(r/'capture.py').write_text('import json,pathlib,sys\nv=json.load(sys.stdin)\nwith pathlib.Path(sys.argv[1]).open("a") as f: f.write(json.dumps(v)+"\\n")\n')
command = f'python3 "{r}/capture.py" "{r}/hooks.jsonl"'
events = ['SessionStart','UserPromptSubmit','PreToolUse','PostToolUse','PostToolUseFailure','Stop','StopFailure','StopCancelled','SessionEnd']
hooks = {'hooks': {e:[{'hooks':[{'type':'command','command':command}]}] for e in events}}
(r/'grok/hooks/herdr-respan.json').write_text(json.dumps(hooks))
(r/'hooks.before.json').write_text(json.dumps(hooks))
# Only explicit loopback endpoints/dummy credentials enter the agent's environment.
(r/'launch.py').write_text('import os,sys\n'+f'env={{"PATH":{home!r}+"/.local/bin:/usr/local/bin:/usr/bin:/bin", "HOME":{root!r}+"/home", "GROK_HOME":{root!r}+"/grok", "XDG_CONFIG_HOME":{root!r}+"/xdg/config", "XDG_CACHE_HOME":{root!r}+"/xdg/cache", "XDG_DATA_HOME":{root!r}+"/xdg/data", "XDG_STATE_HOME":{root!r}+"/xdg/state", "XAI_API_KEY":"dummy", "GROK_MODELS_BASE_URL":{base!r}+"/v1", "GROK_XAI_API_BASE_URL":{base!r}+"/v1", "GROK_CLI_CHAT_PROXY_BASE_URL":{base!r}+"/v1", "NO_PROXY":"127.0.0.1,localhost", "LANG":"C.UTF-8", "CI":"1", "DO_NOT_TRACK":"1", "OTEL_SDK_DISABLED":"true", "GROK_DISABLE_AUTOUPDATER":"1"}}\n'+f'env.update({{k:v for k,v in os.environ.items() if k.startswith("HERDR_")}})\ncmd=sys.argv[1:]\nif cmd[0]=={binary!r}:\n for k in ("GROK_MODELS_BASE_URL","GROK_XAI_API_BASE_URL","GROK_CLI_CHAT_PROXY_BASE_URL"): env.pop(k,None)\nos.execvpe(cmd[0],cmd,env)\n')
PY
"$HRA_BIN" setup grok --yes --state-dir "$HRA_ROOT/state" --config-dir "$HRA_ROOT/config" >"$HRA_ROOT/setup.out"
hra_start_pane "GROK_HOME=$GROK_HOME"
command="python3 '$HRA_ROOT/launch.py' grok --disable-web-search --always-approve --no-subagents --tools run_terminal_command -m mock -p"
hra_prompt "$command 'Say the deterministic mock text.'" text
hra_prompt "$command 'RUN_TOOL: execute echo hra-e2e then reply.'" tool
hra_flush
hra_assert
python3 - "$HRA_ROOT" <<'PY'
import collections,json,pathlib,sys
r=pathlib.Path(sys.argv[1]); spans=json.loads((r/'spans.json').read_text())
def attrs(s): return {a['key']:next(iter(a['value'].values())) for a in s['attributes']}
traces=collections.defaultdict(list)
for s in spans: traces[s['traceId']].append(s)
assert len(traces)==2,traces
counts=[]
for trace,tree in traces.items():
    turns=[s for s in tree if attrs(s)['respan.entity.log_type']=='agent']
    chats=[s for s in tree if attrs(s)['respan.entity.log_type']=='chat']
    tools=[s for s in tree if attrs(s)['respan.entity.log_type']=='tool']
    assert len(turns)==1,tree
    assert turns[0]['status']['code']==1,turns[0]
    assert len(chats)==(2 if tools else 1),tree
    assert not turns[0].get('parentSpanId')
    print('native trace',trace,turns[0]['name'])
    for s in chats+tools:
        a=attrs(s)
        assert s['parentSpanId']==turns[0]['spanId'],s
        assert a['respan.threads.thread_identifier'],s
        assert s['status']['code']==1,s
        if s in chats:
            assert not any('.usage.' in k for k in a),a
            print('  chat',s['name'],'usage unavailable natively')
        else:
            assert 'echo hra-e2e' in a['traceloop.entity.input'],a
            assert json.loads(a['traceloop.entity.output'])['value']['exit_code']==0,a
            assert json.loads(a['traceloop.entity.output'])['value']['output_for_prompt']=='exit: 0\nhra-e2e\n',a
            print('  tool',s['name'],'echo hra-e2e -> hra-e2e')
    counts.append((len(turns),len(chats),len(tools)))
assert sorted(counts)==[(1,1,0),(1,2,1)],counts
(r/'native.spans.json').write_text(json.dumps(spans))
(r/'received.before.json').write_text(json.dumps({p.name:p.read_text() for p in (r/'otlp').glob('*.json')}))
PY
(cd "$HRA_ROOT/work" && env -u HERDR_ENV -u HERDR_PANE_ID -u HERDR_WORKSPACE_ID -u HERDR_TAB_ID -u HERDR_RESPAN_PROXY python3 "$HRA_ROOT/launch.py" grok --disable-web-search --always-approve --no-subagents --tools run_terminal_command -m mock -p 'RUN_TOOL: execute echo hra-e2e then reply.') >"$HRA_ROOT/outside.out" 2>"$HRA_ROOT/outside.err"
hra_flush
python3 - "$HRA_ROOT" <<'PY'
import json,pathlib,sys
r=pathlib.Path(sys.argv[1])
assert json.loads((r/'received.before.json').read_text())=={p.name:p.read_text() for p in (r/'otlp').glob('*.json')}
print('Outside-herdr gate: no new OTLP delivered')
PY
# Qualify explicit wrapper routing separately from the native custom-model recipe.
# Grok has no invocation-only per-model base_url override. Omitting that field
# makes the custom primary model inherit the wrapper global endpoint; title
# generation still uses the built-in Responses model.
python3 - "$HRA_ROOT" "$HRA_PROVIDER_URL" <<'PY'
import pathlib,sys
r=pathlib.Path(sys.argv[1]); base=sys.argv[2]
(r/'grok/config.toml').write_text('[models]\ndefault = "mock"\n[model.mock]\nmodel = "mock-model"\napi_key = "dummy"\napi_backend = "chat_completions"\ncontext_window = 128000\n')
with (r/'config/config.toml').open('a') as f:
    f.write(f'\n[proxy]\nidle_exit_secs = 60\n[proxy.upstream]\nopenai = "{base}/v1"\nanthropic = "{base}"\ngemini = "{base}"\n')
(r/'proxy.request-offset').write_text(str(len((r/'provider.jsonl').read_text().splitlines())))
PY
proxy_command="python3 '$HRA_ROOT/launch.py' '$HRA_BIN' run grok --state-dir '$HRA_ROOT/state' --config-dir '$HRA_ROOT/config' -- --disable-web-search --always-approve --no-subagents --tools run_terminal_command -m mock -p"
hra_prompt "$proxy_command 'Say the deterministic mock text.'" proxy-text
hra_prompt "$proxy_command 'RUN_TOOL: execute echo hra-e2e then reply.'" proxy-tool
hra_flush
hra_assert
python3 - "$HRA_ROOT" <<'PY'
import collections,json,pathlib,sys
r=pathlib.Path(sys.argv[1])
all_spans=json.loads((r/'spans.json').read_text())
native={(s['traceId'],s['spanId']) for s in json.loads((r/'native.spans.json').read_text())}
spans=[s for s in all_spans if (s['traceId'],s['spanId']) not in native]
def attrs(s): return {a['key']:next(iter(a['value'].values())) for a in s['attributes']}
traces=collections.defaultdict(list)
for s in spans: traces[s['traceId']].append(s)
requests=[json.loads(l) for l in (r/'provider.jsonl').read_text().splitlines()][int((r/'proxy.request-offset').read_text()):]
inference=[q for q in requests if q['path'].endswith(('/responses','/chat/completions'))]
assert any(q['path'].endswith('/responses') for q in inference), 'Auxiliary Responses endpoint was not exercised'
assert any(q['path'].endswith('/chat/completions') for q in inference)
chats=[s for s in spans if attrs(s)['respan.entity.log_type']=='chat']
assert len(chats)==len(inference),(len(chats),len(inference),spans)
for s in chats:
    a=attrs(s)
    metadata=json.loads(a['respan.metadata'])
    assert metadata['source']=='proxy',metadata
    for key,value in [('input_tokens',11),('output_tokens',7),('prompt_tokens',11),('completion_tokens',7)]:
        assert int(a['gen_ai.usage.'+key])==value,a
    assert int(a['llm.usage.cache_read_input_tokens'])==3,a
assert len(traces)==2,traces
counts=[]
for trace,tree in traces.items():
    turns=[s for s in tree if attrs(s)['respan.entity.log_type']=='agent']
    children=[s for s in tree if attrs(s)['respan.entity.log_type']=='chat']
    tools=[s for s in tree if attrs(s)['respan.entity.log_type']=='tool']
    assert len(turns)==1,tree
    assert turns[0]['status']['code']==1,turns[0]
    assert len(children)==(4 if tools else 3),tree
    print('proxy trace',trace,turns[0]['name'])
    for s in children+tools:
        assert s['parentSpanId']==turns[0]['spanId'],s
        # Headless Grok can cancel its background post-Stop summary at exit;
        # preserve and print the real proxy status rather than requiring success.
        if s in children: print('  proxy chat',s['name'],'input=11 output=7 cache_read=3',s['status'])
        else:
            assert s['status']['code']==1,s
            a=attrs(s)
            assert 'echo hra-e2e' in a['traceloop.entity.input'],a
            assert json.loads(a['traceloop.entity.output'])['value']['exit_code']==0,a
            assert json.loads(a['traceloop.entity.output'])['value']['output_for_prompt']=='exit: 0\nhra-e2e\n',a
            print('  tool',s['name'],'echo hra-e2e -> hra-e2e')
    counts.append((len(turns),len(children),len(tools)))
assert sorted(counts)==[(1,3,0),(1,4,1)],counts
(r/'proxy.spans.json').write_text(json.dumps(spans))
print('Proxy: every primary/title/summary request traced once; no adapter chats')
PY
cp "$GROK_HOME/config.toml" "$HRA_ROOT/config.before-remove.toml"
"$HRA_BIN" remove grok --state-dir "$HRA_ROOT/state" --config-dir "$HRA_ROOT/config"
python3 - "$HRA_ROOT" <<'PY'
import json,pathlib,sys,tomllib
r=pathlib.Path(sys.argv[1])
assert json.loads((r/'hooks.before.json').read_text())==json.loads((r/'grok/hooks/herdr-respan.json').read_text())
assert tomllib.loads((r/'config.before-remove.toml').read_text())==tomllib.loads((r/'grok/config.toml').read_text())
print('Remove: isolated hooks and model config semantically restored')
PY

#!/usr/bin/env bash
set -euo pipefail
HRA_BIN=${HRA_BIN:-$(cd "$(dirname "$0")/../.." && pwd)/target/release/herdr-respan}
source "$(dirname "$0")/lib.sh"
hra_init opencode
hra_start_mocks
REAL_HOME=$HOME
export OPENCODE_CONFIG_DIR="$HRA_ROOT/agent"
mkdir -p "$OPENCODE_CONFIG_DIR/plugins" "$HRA_ROOT/home" "$HRA_ROOT/xdg"/{config,cache,data,state}
python3 - "$HRA_ROOT" "$HRA_PROVIDER_URL" "$REAL_HOME" <<'PY'
import json, pathlib, sys
root=pathlib.Path(sys.argv[1])
config={'$schema':'https://opencode.ai/config.json','provider':{'mock':{'npm':'@ai-sdk/openai-compatible','name':'Mock','options':{'baseURL':sys.argv[2]+'/v1','apiKey':'dummy'},'models':{'mock-model':{'name':'Mock'}}}},'model':'mock/mock-model','permission':{'bash':{'*':'deny','echo hra-e2e':'allow'}},'enabled_providers':['mock']}
(root/'agent/opencode.json').write_text(json.dumps(config))
(root/'agent/tui.json').write_text(json.dumps({'theme':'opencode'}))
(root/'config.before.json').write_text(json.dumps(config))
(root/'tui.before.json').write_text((root/'agent/tui.json').read_text())
# Independent native observer: captures live events, never synthesizes adapter input.
(root/'agent/plugins/capture.js').write_text('import { appendFileSync } from "node:fs";\nconst file='+json.dumps(str(root/'native.jsonl'))+';\nconst save=(x)=>{try{appendFileSync(file,JSON.stringify(x)+"\\n")}catch{}};\nexport default {id:"hra-e2e.capture",server(){return {"chat.message":async(input,output)=>save({type:"chat.message",input,output}),event:async({event})=>save(event)}}};\n')
# Clear ambient keys/profiles while preserving only pane identity for the real plugin gate.
(root/'launch.sh').write_text('''#!/usr/bin/env bash
set -euo pipefail
ROOT='''+repr(str(root))+'''
exec env -i PATH='''+repr(sys.argv[3]+'/.local/bin:/usr/local/bin:/usr/bin:/bin')+''' HOME="$ROOT/home" \
 XDG_CONFIG_HOME="$ROOT/xdg/config" XDG_CACHE_HOME="$ROOT/xdg/cache" \
 XDG_DATA_HOME="$ROOT/xdg/data" XDG_STATE_HOME="$ROOT/xdg/state" \
 OPENCODE_CONFIG_DIR="$ROOT/agent" OPENCODE_DISABLE_SHARE=true OPENCODE_DISABLE_MODELS_FETCH=true \
 OPENCODE_DISABLE_AUTOUPDATE=true DO_NOT_TRACK=1 NO_PROXY=127.0.0.1,localhost \
 HERDR_ENV="${HERDR_ENV-}" HERDR_PANE_ID="${HERDR_PANE_ID-}" HERDR_WORKSPACE_ID="${HERDR_WORKSPACE_ID-}" \
 HERDR_TAB_ID="${HERDR_TAB_ID-}" HERDR_SOCKET_PATH="${HERDR_SOCKET_PATH-}" HERDR_RESPAN=on \
 LANG=C.UTF-8 TERM=xterm-256color opencode "$@"
''')
PY
HOME="$HRA_ROOT/home" XDG_STATE_HOME="$HRA_ROOT/xdg/state" "$HRA_BIN" setup opencode --yes --state-dir "$HRA_ROOT/state" --config-dir "$HRA_ROOT/config" >"$HRA_ROOT/setup.out"
# OPENCODE_CONFIG_DIR must relocate the actual installed assets, not merely the provider config.
test -f "$OPENCODE_CONFIG_DIR/plugins/herdr-respan.js"
hra_start_pane
command="bash '$HRA_ROOT/launch.sh' run --model mock/mock-model --format json"
hra_prompt "$command 'Say the deterministic mock text.'" text
hra_prompt "$command 'RUN_TOOL: execute echo hra-e2e then reply.'" tool
hra_flush
hra_assert
# Full V1 TUI has a separate pane-local entrypoint. Drive two prompts in one client.
hra_herdr pane run "$HRA_PANE" "bash '$HRA_ROOT/launch.sh' --model mock/mock-model --prompt 'Say the deterministic mock text.'; printf '%s' \$? > '$HRA_ROOT/tui.exit'" >/dev/null
wait_traces() {
    local expected=$1
    for ((i=0;i<600;i++)); do
        hra_dump >"$HRA_ROOT/spans.json"
        if python3 - "$HRA_ROOT/spans.json" "$expected" 2>/dev/null <<'PY'
import json,sys
spans=json.load(open(sys.argv[1]))
assert len({s['traceId'] for s in spans}) >= int(sys.argv[2])
PY
        then return 0; fi
        sleep .2
    done
    hra_herdr pane read "$HRA_PANE" --source recent --lines 100
    echo "Timed out waiting for $expected traces" >&2
    return 1
}
wait_traces 3
hra_herdr pane send-text "$HRA_PANE" 'RUN_TOOL: execute echo hra-e2e then reply.' >/dev/null
hra_herdr pane send-keys "$HRA_PANE" Enter >/dev/null
wait_traces 4
hra_herdr pane read "$HRA_PANE" --source recent --lines 100 >"$HRA_ROOT/tui.out"
hra_herdr pane send-keys "$HRA_PANE" Ctrl+c >/dev/null
sleep .3
hra_herdr pane send-keys "$HRA_PANE" Ctrl+c >/dev/null
hra_flush
hra_assert
python3 - "$HRA_ROOT" <<'PY'
import collections,json,pathlib,sys
root=pathlib.Path(sys.argv[1]); spans=json.loads((root/'spans.json').read_text())
def attrs(s): return {a['key']:next(iter(a['value'].values())) for a in s['attributes']}
traces=collections.defaultdict(list)
for s in spans:
    assert s['status']['code']==1,s
    assert attrs(s)['respan.threads.thread_identifier'],s
    traces[s['traceId']].append(s)
assert len(traces)==4,traces
counts=[]
for trace,tree in traces.items():
    turns=[s for s in tree if attrs(s)['respan.entity.log_type']=='agent']
    chats=[s for s in tree if attrs(s)['respan.entity.log_type']=='chat']
    tools=[s for s in tree if attrs(s)['respan.entity.log_type']=='tool']
    assert len(turns)==1 and not turns[0].get('parentSpanId'),tree
    print(f"trace {trace}: {turns[0]['name']}")
    for s in chats+tools:
        a=attrs(s); assert s['parentSpanId']==turns[0]['spanId'],s
        if s in chats:
            # OpenCode reports noncached input separately from cache.read: 11 - 3.
            for key,value in [('gen_ai.usage.input_tokens',8),('gen_ai.usage.output_tokens',7),('llm.usage.cache_read_input_tokens',3)]:
                assert int(a[key])==value,a
            print('  chat: input=8 output=7 cache_read=3 (provider prompt total=11)')
        else:
            assert 'echo hra-e2e' in a['traceloop.entity.input'],a
            assert 'hra-e2e' in a['traceloop.entity.output'],a
            print('  tool: echo hra-e2e -> hra-e2e')
    counts.append((len(turns),len(chats),len(tools)))
assert sorted(counts)==[(1,1,0),(1,1,0),(1,2,1),(1,2,1)],counts
assert len({attrs(s)['respan.threads.thread_identifier'] for s in spans})==3
print(f'Asserted {len(spans)} spans: V1 run and full TUI, four prompts')
(root/'received.before.json').write_text(json.dumps({p.name:p.read_text() for p in (root/'otlp').glob('*.json')}))
PY
# Match the real terminal transport outside herdr; redirected stdin can keep run alive.
python3 - "$HRA_ROOT" <<'PY'
import errno,os,pathlib,pty,select,signal,sys,time
root=pathlib.Path(sys.argv[1]); pid,fd=pty.fork()
if pid==0:
    os.chdir(root/'work')
    for key in list(os.environ):
        if key.startswith('HERDR_'): del os.environ[key]
    os.execvp('bash',['bash',str(root/'launch.sh'),'run','--model','mock/mock-model','--format','json','RUN_TOOL: execute echo hra-e2e then reply.'])
try:
    deadline=time.monotonic()+60
    with (root/'outside.out').open('wb') as log:
        while time.monotonic()<deadline:
            if select.select([fd],[],[],.1)[0]:
                try: data=os.read(fd,65536)
                except OSError as error:
                    if error.errno==errno.EIO: break
                    raise
                if not data: break
                log.write(data)
            done,status=os.waitpid(pid,os.WNOHANG)
            if done:
                assert os.waitstatus_to_exitcode(status)==0,status
                pid=None; break
        else: raise AssertionError('outside OpenCode failed to exit in 60 seconds')
    if pid:
        _,status=os.waitpid(pid,0); pid=None
        assert os.waitstatus_to_exitcode(status)==0,status
finally:
    if pid:
        os.kill(pid,signal.SIGTERM); os.waitpid(pid,0)
    os.close(fd)
PY
hra_flush
python3 - "$HRA_ROOT" <<'PY'
import json,pathlib,sys
root=pathlib.Path(sys.argv[1])
assert json.loads((root/'received.before.json').read_text())=={p.name:p.read_text() for p in (root/'otlp').glob('*.json')}
print('Outside-herdr gate: no new OTLP delivered')
PY
HOME="$HRA_ROOT/home" XDG_STATE_HOME="$HRA_ROOT/xdg/state" "$HRA_BIN" remove opencode --state-dir "$HRA_ROOT/state" --config-dir "$HRA_ROOT/config"
python3 - "$HRA_ROOT" <<'PY'
import json,pathlib,sys
root=pathlib.Path(sys.argv[1])
assert json.loads((root/'config.before.json').read_text())==json.loads((root/'agent/opencode.json').read_text())
assert json.loads((root/'tui.before.json').read_text())==json.loads((root/'agent/tui.json').read_text())
for p in ['plugins/herdr-respan.js','herdr-respan/bridge.js','herdr-respan-tui.js','herdr-respan/tui.js','cli.json']:
    assert not (root/'agent'/p).exists(),p
assert (root/'agent/plugins/capture.js').exists()
print('Remove: isolated provider/TUI config restored and foreign plugin preserved')
PY

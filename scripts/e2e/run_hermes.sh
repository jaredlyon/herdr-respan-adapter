#!/usr/bin/env bash
set -euo pipefail
REAL_HOME=$HOME
HRA_BIN=${HRA_BIN:-"$(cd "$(dirname "$0")/../.." && pwd)/target/release/herdr-respan"}
source "$(dirname "$0")/lib.sh"
hra_init hermes
hra_start_mocks
export HERMES_HOME="$HRA_ROOT/agent"
mkdir -p "$HERMES_HOME" "$HRA_ROOT/home" "$HRA_ROOT/xdg"/{config,cache,data,state}
PYTHON="$REAL_HOME/jare-ai/hermes-agent/venv/bin/python"
CLI="$REAL_HOME/jare-ai/hermes-agent/cli.py"
python3 - "$HRA_ROOT" <<'PY'
import pathlib, sys
root = pathlib.Path(sys.argv[1])
plugin = root / 'agent/plugins/e2e-capture'
plugin.mkdir(parents=True)
hooks = ['pre_llm_call','post_llm_call','on_session_end','on_session_finalize','on_session_reset','pre_api_request','post_api_request','api_request_error','pre_tool_call','post_tool_call','subagent_start','subagent_stop','pre_auxiliary_call','post_auxiliary_call']
(plugin/'plugin.yaml').write_text('name: e2e-capture\nversion: "0.1.0"\nprovides_hooks:\n' + ''.join('  - '+n+'\n' for n in hooks))
(plugin/'__init__.py').write_text('import json, pathlib, threading\n_LOCK=threading.Lock()\n_PATH='+repr(str(root/'hooks.jsonl'))+'\n_HOOKS='+repr(hooks)+'''\ndef register(ctx):
    for name in _HOOKS:
        def callback(_name=name, **kwargs):
            with _LOCK, pathlib.Path(_PATH).open("a") as f:
                f.write(json.dumps([_name, kwargs], default=str)+"\\n")
            return None
        ctx.register_hook(name, callback)
''')
config = '# Isolated loopback E2E only\nplugins:\n  enabled: [e2e-capture] # preserve capture\nterminal:\n  backend: local\n  timeout: 10\n'
(root/'agent/config.yaml').write_text(config)
(root/'config.before.yaml').write_text(config)
PY
"$HRA_BIN" setup hermes --yes --state-dir "$HRA_ROOT/state" --config-dir "$HRA_ROOT/config" >"$HRA_ROOT/setup.out"
# Construct a clean agent environment; retain only herdr's pane correlation variables.
python3 - "$HRA_ROOT" "$PYTHON" "$CLI" "$PATH" "$HRA_PROVIDER_URL" <<'PY'
import pathlib, shlex, sys
root=pathlib.Path(sys.argv[1])
env={'HOME':str(root/'home'),'HERMES_HOME':str(root/'agent'),'PATH':sys.argv[4],
     'XDG_CONFIG_HOME':str(root/'xdg/config'),'XDG_CACHE_HOME':str(root/'xdg/cache'),
     'XDG_DATA_HOME':str(root/'xdg/data'),'XDG_STATE_HOME':str(root/'xdg/state'),
     'LANG':'C.UTF-8','CI':'1','NO_COLOR':'1','TERM':'dumb','NO_PROXY':'127.0.0.1,localhost',
     'DO_NOT_TRACK':'1','OTEL_SDK_DISABLED':'true','HERMES_SKIP_SETUP':'1','HERMES_DISABLE_LAZY_INSTALLS':'1',
     'OPENAI_API_KEY':'dummy','OPENAI_BASE_URL':sys.argv[5]+'/v1','TERMINAL_CWD':str(root/'work')}
script='#!/usr/bin/env bash\nset -euo pipefail\nenv_args=()\nfor key in ${!HERDR_@}; do env_args+=("$key=${!key}"); done\n'
script+='exec env -i "${env_args[@]}" '+ ' '.join(shlex.quote(k+'='+v) for k,v in env.items())+' '+shlex.quote(sys.argv[2])+' '+shlex.quote(sys.argv[3])+' --provider custom --base-url '+shlex.quote(sys.argv[5]+'/v1')+' --api-key dummy --model mock-model --toolsets terminal --max-turns 4 --oneshot --query "$1"\n'
(root/'agent-run.sh').write_text(script)
PY
hra_start_pane
hra_prompt "bash '$HRA_ROOT/agent-run.sh' 'Say the deterministic mock text.'" text
hra_prompt "bash '$HRA_ROOT/agent-run.sh' 'RUN_TOOL: execute echo hra-e2e then reply.'" tool
hra_flush
hra_assert
python3 - "$HRA_ROOT" <<'PY'
import collections, json, pathlib, sys
root=pathlib.Path(sys.argv[1])
spans=json.loads((root/'spans.json').read_text())
hooks=[json.loads(l) for l in (root/'hooks.jsonl').read_text().splitlines()]
posts=[k for n,k in hooks if n=='post_api_request']
assert len(posts)==3, posts
assert not [k for n,k in hooks if n=='api_request_error']
requests=[json.loads(l) for l in (root/'provider.jsonl').read_text().splitlines()]
aux=[k for n,k in hooks if n=='post_auxiliary_call']
assert len(aux)==2, aux
assert len([r for r in requests if r['path'].endswith('/chat/completions')])==len(posts)+len(aux)
assert all('127.0.0.1' in k['base_url'] for k in posts)
def attrs(s): return {a['key']:next(iter(a['value'].values())) for a in s['attributes']}
traces=collections.defaultdict(list)
for s in spans:
    assert s['status']['code']==1, s
    assert attrs(s)['respan.threads.thread_identifier']
    traces[s['traceId']].append(s)
assert len(traces)==2, traces
counts=[]
for trace, tree in traces.items():
    turns=[s for s in tree if attrs(s)['respan.entity.log_type']=='agent']
    chats=[s for s in tree if attrs(s)['respan.entity.log_type']=='chat']
    tools=[s for s in tree if attrs(s)['respan.entity.log_type']=='tool']
    assert len(turns)==1 and not turns[0].get('parentSpanId'), tree
    print(f"trace {trace}: {turns[0]['name']}")
    for child in chats+tools:
        a=attrs(child)
        assert child['parentSpanId']==turns[0]['spanId'], child
        if child in chats:
            for key,value in [('gen_ai.usage.input_tokens',11),('gen_ai.usage.output_tokens',7),('gen_ai.usage.prompt_tokens',11),('gen_ai.usage.completion_tokens',7),('llm.usage.cache_read_input_tokens',3)]:
                assert int(a[key])==value, a
            assert a['gen_ai.request.model']=='mock-model', a
            print('  chat: input=11 output=7 cache_read=3')
        else:
            assert child['name']=='terminal', child
            assert 'echo hra-e2e' in a['traceloop.entity.input'], a
            assert 'hra-e2e' in a['traceloop.entity.output'], a
            print('  tool terminal: echo hra-e2e -> hra-e2e')
    counts.append((len(turns),len(chats),len(tools)))
assert sorted(counts)==[(1,2,0),(1,3,1)], counts
# Native request IDs prove distinct post_api_request events, not cumulative post_llm_call usage.
assert len({p['api_request_id'] for p in posts})==3
chats=[s for s in spans if attrs(s)['respan.entity.log_type']=='chat']
assert len(chats)==len(posts)+len(aux)
assert len([s for s in chats if json.loads(attrs(s)['respan.metadata']).get('hermes.aux_task')=='title_generation'])==2
for s in chats:
    a=attrs(s)
    assert len([key for key,value in a.items() if key.startswith('gen_ai.prompt.') and key.endswith('.role') and value=='system'])==1, a
(root/'received.before.json').write_text(json.dumps({p.name:p.read_text() for p in (root/'otlp').glob('*.json')}))
print(f'Asserted {len(spans)} spans from 3 native provider attempts, 2 traces')
PY
(cd "$HRA_ROOT/work" && env -u HERDR_ENV -u HERDR_PANE_ID -u HERDR_WORKSPACE_ID -u HERDR_TAB_ID -u HERDR_RESPAN_PROXY bash "$HRA_ROOT/agent-run.sh" 'RUN_TOOL: execute echo hra-e2e then reply.') >"$HRA_ROOT/outside.out" 2>"$HRA_ROOT/outside.err"
hra_flush
python3 - "$HRA_ROOT" <<'PY'
import json, pathlib, sys
root=pathlib.Path(sys.argv[1])
assert json.loads((root/'received.before.json').read_text())=={p.name:p.read_text() for p in (root/'otlp').glob('*.json')}
assert 'hra-e2e mock response' in (root/'outside.out').read_text()
print('Outside-herdr gate: no new OTLP delivered')
PY
"$HRA_BIN" remove hermes --state-dir "$HRA_ROOT/state" --config-dir "$HRA_ROOT/config"
"$PYTHON" - "$HRA_ROOT" <<'PY'
import pathlib, sys, yaml
root=pathlib.Path(sys.argv[1])
assert yaml.safe_load((root/'config.before.yaml').read_text())==yaml.safe_load((root/'agent/config.yaml').read_text())
assert not (root/'agent/plugins/herdr-respan/__init__.py').exists()
assert not (root/'agent/plugins/herdr-respan/plugin.yaml').exists()
print('Remove: isolated config.yaml semantically restored')
PY

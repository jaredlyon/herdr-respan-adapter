#!/usr/bin/env bash
set -euo pipefail
HRA_PROXY_BIN=${HRA_BIN:-}
source "$(dirname "$0")/lib.sh"
HRA_BIN=${HRA_PROXY_BIN:-$HRA_REPO/target/release/herdr-respan}
hra_init proxy
export HRA_MOCK_CHUNK_DELAY_MS=80
hra_start_mocks
export REAL_HOME="$HOME"
export HOME="$HRA_ROOT/home" CLAUDE_CONFIG_DIR="$HRA_ROOT/claude" CODEX_HOME="$HRA_ROOT/codex"
export XDG_CONFIG_HOME="$HRA_ROOT/agent-xdg/config" XDG_CACHE_HOME="$HRA_ROOT/agent-xdg/cache" XDG_DATA_HOME="$HRA_ROOT/agent-xdg/data" XDG_STATE_HOME="$HRA_ROOT/agent-xdg/state"
export ANTHROPIC_API_KEY=hra-proxy-dummy OPENAI_API_KEY=hra-proxy-dummy
export ANTHROPIC_CUSTOM_HEADERS=$'x-hra-exact: MixedCase +/= bytes\nanthropic-beta: hra-proxy-beta'
export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 DISABLE_AUTOUPDATER=1 DO_NOT_TRACK=1 OTEL_SDK_DISABLED=true
unset ANTHROPIC_BASE_URL OPENAI_BASE_URL OPENAI_API_BASE ANTHROPIC_AUTH_TOKEN CLAUDE_CODE_OAUTH_TOKEN
mkdir -p "$HOME" "$CLAUDE_CONFIG_DIR" "$CODEX_HOME"
cat >>"$HRA_ROOT/config/config.toml" <<EOF
content_cap_bytes = 262144
[proxy]
idle_exit_secs = 2
[proxy.upstream]
anthropic = "$HRA_PROVIDER_URL"
openai = "$HRA_PROVIDER_URL/v1"
EOF
python3 - "$HRA_ROOT" <<'PY'
import json,pathlib,sys
root=pathlib.Path(sys.argv[1])
settings={'env':{'DISABLE_AUTOUPDATER':'1'},'permissions':{'allow':['Bash(echo hra-e2e)']}}
(root/'claude/settings.json').write_text(json.dumps(settings))
(root/'settings.before.json').write_text(json.dumps(settings))
(root/'claude/.claude.json').write_text(json.dumps({'hasCompletedOnboarding':True,'theme':'dark','autoUpdates':False}))
(root/'codex/config.toml').write_text(f'model = "gpt-4.1"\nmodel_provider = "mock"\n[features]\nenable_request_compression = false\n[model_providers.mock]\nname = "Mock"\nwire_api = "responses"\nbase_url = "http://127.0.0.1:{(root/"provider.port").read_text().strip()}/v1"\nenv_key = "OPENAI_API_KEY"\nhttp_headers = {{ "x-hra-exact" = "MixedCase +/= bytes", "ChatGPT-Account-ID" = "hra-proxy-account" }}\n')
(root/'config.before.toml').write_text((root/'codex/config.toml').read_text())
(root/'timestamp.py').write_text('import json,sys,time\nf=open(sys.argv[1],"a",buffering=1)\nfor line in sys.stdin:\n f.write(json.dumps({"received_ns":time.time_ns(),"line":line})+"\\n")\n print(line,end="",flush=True)\n')
PY
"$HRA_BIN" setup claude codex --yes --state-dir "$HRA_ROOT/state" --config-dir "$HRA_ROOT/config" >"$HRA_ROOT/setup.out"
hra_start_pane "HOME=$HOME" "CLAUDE_CONFIG_DIR=$CLAUDE_CONFIG_DIR" "CODEX_HOME=$CODEX_HOME" "XDG_CONFIG_HOME=$XDG_CONFIG_HOME" "XDG_CACHE_HOME=$XDG_CACHE_HOME" "XDG_DATA_HOME=$XDG_DATA_HOME" "XDG_STATE_HOME=$XDG_STATE_HOME" "ANTHROPIC_API_KEY=$ANTHROPIC_API_KEY" "OPENAI_API_KEY=$OPENAI_API_KEY" "ANTHROPIC_AUTH_TOKEN=" "CLAUDE_CODE_OAUTH_TOKEN=" "ANTHROPIC_BASE_URL=" "OPENAI_BASE_URL=" "ANTHROPIC_CUSTOM_HEADERS=$ANTHROPIC_CUSTOM_HEADERS" "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1" "DISABLE_AUTOUPDATER=1" "DO_NOT_TRACK=1" "OTEL_SDK_DISABLED=true"
wrapper="'$HRA_BIN' run --state-dir '$HRA_ROOT/state' --config-dir '$HRA_ROOT/config'"
claude="$wrapper claude -- -p --verbose --output-format stream-json --include-partial-messages --model claude-haiku-4-5-20251001 --tools Bash --allowedTools 'Bash(echo hra-e2e)' --setting-sources user --strict-mcp-config --mcp-config '{\"mcpServers\":{}}' --"
codex="$wrapper codex -- exec --json --skip-git-repo-check --dangerously-bypass-hook-trust --sandbox danger-full-access --disable unified_exec -c 'approval_policy=\"never\"' -c 'web_search=\"disabled\"' -m gpt-4.1"
# Timestamp native output, so streaming has to reach the real agent before provider EOF.
for agent in claude codex-sse codex-ws; do
    if [[ $agent == claude ]]; then command=$claude; else
        ws=false; [[ $agent == codex-ws ]] && ws=true
        command="$codex -c model_providers.mock.supports_websockets=$ws"
    fi
    for scenario in text tool; do
        prompt='Say the deterministic mock text.'
        [[ $scenario == tool ]] && prompt='RUN_TOOL: execute echo hra-e2e then reply.'
        label="$agent-$scenario"
        hra_prompt "(set -o pipefail; $command '$prompt' | python3 '$HRA_ROOT/timestamp.py' '$HRA_ROOT/$label.stream.jsonl')" "$label"
        python3 - "$HRA_ROOT" <<'PY'
import pathlib,sys
root=pathlib.Path(sys.argv[1])
(root/'proxy.last.json').write_bytes((root/'state/proxy.json').read_bytes())
PY
    done
done
hra_flush
hra_assert
python3 - "$HRA_ROOT" <<'PY'
import collections,hashlib,json,pathlib,sys
root=pathlib.Path(sys.argv[1]); spans=json.loads((root/'spans.json').read_text())
def attrs(s):return {a['key']:next(iter(a['value'].values())) for a in s['attributes']}
def meta(s):return json.loads(attrs(s)['respan.metadata'])
traces=collections.defaultdict(list)
for s in spans:traces[s['traceId']].append(s)
assert len(traces)==6,[(t,[s['name'] for s in tree]) for t,tree in traces.items()]
counts=[]
for trace,tree in traces.items():
    turns=[s for s in tree if attrs(s)['respan.entity.log_type']=='agent']
    chats=[s for s in tree if attrs(s)['respan.entity.log_type']=='chat']
    tools=[s for s in tree if attrs(s)['respan.entity.log_type']=='tool']
    assert len(turns)==1 and len(chats)==(2 if tools else 1) and len(tools)<=1,tree
    turn=turns[0]; counts.append((meta(turn)['agent'],len(chats),len(tools)))
    print(f"trace {trace}: {turn['name']}")
    for child in chats+tools:
        a=attrs(child); assert child['parentSpanId']==turn['spanId'],child
        if child in chats:
            assert meta(child)['source']=='proxy',meta(child)
            for key,n in [('gen_ai.usage.input_tokens',11),('gen_ai.usage.output_tokens',7),('gen_ai.usage.prompt_tokens',11),('gen_ai.usage.completion_tokens',7),('llm.usage.cache_read_input_tokens',3),('llm.usage.total_tokens',18)]:assert int(a[key])==n,a
            zero='llm.usage.cache_creation_input_tokens' if meta(child)['agent']=='claude' else 'llm.usage.reasoning_tokens'
            assert int(a[zero])==0,a
            assert meta(child)['http.status_code'] in (200,101),meta(child)
            print('  proxy chat: input=11 output=7 cache_read=3')
        else:
            assert 'echo hra-e2e' in a['traceloop.entity.input'] and 'hra-e2e' in a['traceloop.entity.output'],a
            print('  tool: echo hra-e2e -> hra-e2e')
assert sorted(counts)==sorted([('claude',1,0),('claude',2,1),('codex',1,0),('codex',2,1)] + [('codex',1,0),('codex',2,1)]),counts
requests=[json.loads(line) for line in (root/'provider.jsonl').read_text().splitlines()]
inference=[r for r in requests if r['body'] and r['path'].endswith(('/messages','/responses')) and r['body'].get('generate') is not False]
assert len(inference)==9,len(inference)
assert len([r for r in inference if r['path'].endswith('/messages')])==3
assert len([r for r in inference if r['body'].get('type')=='response.create'])==3
assert len([r for r in inference if r['path'].endswith('/responses') and r['body'].get('type')!='response.create'])==3
assert any(r['body'] and r['body'].get('generate') is False for r in requests),'WS prewarm missing'
sha=lambda v:hashlib.sha256(v.encode('latin1')).hexdigest()
for r in inference:
    h=r['header_value_sha256']
    assert h['x-hra-exact']==[sha('MixedCase +/= bytes')],h
    if r['path'].endswith('/messages'):
        assert h['x-api-key']==[sha('hra-proxy-dummy')],h
        assert h['anthropic-version']==[sha('2023-06-01')],h
        assert r['headers']['anthropic-beta'],r
    else:
        assert h['authorization']==[sha('Bearer hra-proxy-dummy')],h
        assert h['chatgpt-account-id']==[sha('hra-proxy-account')],h
        if r['transport']=='websocket':
            assert h['openai-beta']==[sha('responses_websockets=2026-02-06')],h
# Match each first full request and follow-up suffix to the actual proxy content.
chats=sorted((s for s in spans if attrs(s)['respan.entity.log_type']=='chat'),key=lambda s:int(s['startTimeUnixNano']))
def same_content(left,right):
    if isinstance(left,list) and isinstance(right,list):
        return len(left)==len(right) and all(same_content(a,b) for a,b in zip(left,right))
    if isinstance(left,dict) and isinstance(right,dict):
        a={k:v for k,v in left.items() if k!='cache_control'}
        b={k:v for k,v in right.items() if k!='cache_control'}
        return a.keys()==b.keys() and all(same_content(v,b[k]) if k=='content' else v==b[k] for k,v in a.items())
    return left==right
for i,(s,r) in enumerate(zip(chats,inference)):
    expected=json.loads(json.dumps(r['body']))
    if i%3==2:
        prior=inference[i-1]['body']
        for key in ('system','instructions'):
            if expected.get(key)==prior.get(key):expected.pop(key,None)
        for key in ('messages','input'):
            if isinstance(expected.get(key),list) and isinstance(prior.get(key),list):
                old=prior[key]; now=expected[key]
                if len(now)>=len(old) and all(same_content(a,b) for a,b in zip(now,old)):expected[key]=now[len(old):]
    actual=json.loads(attrs(s)['gen_ai.prompt.0.content'])
    assert actual==expected,(i,actual,expected)
events=[json.loads(line) for line in (root/'provider.events.jsonl').read_text().splitlines()]
for label,index in zip(('claude-text','claude-tool','codex-sse-text','codex-sse-tool','codex-ws-text','codex-ws-tool'),(0,2,3,5,6,8)):
    observed=[json.loads(line) for line in (root/(label+'.stream.jsonl')).read_text().splitlines()]
    text=[x for x in observed if 'hra-e2e mock response' in x['line']]
    assert text,label
    last=max(x['sent_ns'] for x in events if x['request_id']==inference[index]['request_id'])
    assert text[0]['received_ns']<last,('buffered stream',label)
print('Asserted real Claude SSE and Codex SSE/WS: headers/auth, usage, deltas, parenting, no duplicate adapter chats')
(root/'received.before.json').write_text(json.dumps({p.name:p.read_text() for p in (root/'otlp').glob('*.json')}))
PY
# Explicit run intentionally captures outside herdr; the ordinary installed adapters do not.
(cd "$HRA_ROOT/work" && env -u HERDR_ENV -u HERDR_PANE_ID -u HERDR_WORKSPACE_ID -u HERDR_TAB_ID -u HERDR_RESPAN_PROXY ANTHROPIC_BASE_URL="$HRA_PROVIDER_URL" claude -p --model claude-haiku-4-5-20251001 --tools Bash --setting-sources user --strict-mcp-config --mcp-config '{"mcpServers":{}}' -- 'Say the deterministic mock text.') </dev/null >"$HRA_ROOT/outside.out" 2>"$HRA_ROOT/outside.err"
(cd "$HRA_ROOT/work" && env -u HERDR_ENV -u HERDR_PANE_ID -u HERDR_WORKSPACE_ID -u HERDR_TAB_ID -u HERDR_RESPAN_PROXY codex exec --json --skip-git-repo-check --dangerously-bypass-hook-trust --sandbox danger-full-access --disable unified_exec -c 'approval_policy="never"' -c 'model_providers.mock.supports_websockets=false' -c 'web_search="disabled"' -- 'Say the deterministic mock text.') </dev/null >"$HRA_ROOT/outside-codex.out" 2>"$HRA_ROOT/outside-codex.err"
hra_flush
"$HRA_BIN" remove claude codex --state-dir "$HRA_ROOT/state" --config-dir "$HRA_ROOT/config"
python3 - "$HRA_ROOT" <<'PY'
import json,os,pathlib,socket,sys,time,tomllib
root=pathlib.Path(sys.argv[1])
assert json.loads((root/'received.before.json').read_text())=={p.name:p.read_text() for p in (root/'otlp').glob('*.json')}
assert json.loads((root/'settings.before.json').read_text())==json.loads((root/'claude/settings.json').read_text())
assert tomllib.loads((root/'config.before.toml').read_text())==tomllib.loads((root/'codex/config.toml').read_text())
for _ in range(100):
    if not (root/'state/proxy.json').exists():break
    time.sleep(.1)
assert not (root/'state/proxy.json').exists(),'Proxy failed to exit after idle'
assert not list((root/'state/proxy/runs').glob('*.json')),'Run registration leaked'
for name in ('proxy','proxy/runs'):
    assert (root/'state'/name).stat().st_mode & 0o777 == 0o700,'Capability directory is not owner-only'
info=json.loads((root/'proxy.last.json').read_text())
try:
    socket.create_connection(('127.0.0.1',info['port']),timeout=.2)
except OSError:
    pass
else:
    raise AssertionError('Proxy listener survived idle exit')
for _ in range(100):
    try:
        os.kill(info['pid'],0)
    except ProcessLookupError:
        break
    time.sleep(.1)
else:
    raise AssertionError('Proxy process survived idle exit')
print('Outside-herdr adapter gate, setup/remove restore and proxy idle exit passed')
PY

#!/usr/bin/env bash
set -euo pipefail
HRA_BIN=${HRA_BIN:-"$(cd "$(dirname "$0")/../.." && pwd)/target/release/herdr-respan"}
source "$(dirname "$0")/lib.sh"
hra_init kimi
hra_start_mocks
REAL_HOME=$HOME
KIMI_BIN=${KIMI_BIN:-$REAL_HOME/.local/share/kimi-code/bin/kimi}
export PATH="$(dirname "$KIMI_BIN"):$PATH" KIMI_CODE_HOME="$HRA_ROOT/agent"
mkdir -p "$KIMI_CODE_HOME" "$HRA_ROOT/home" "$HRA_ROOT/skills"
python3 - "$HRA_ROOT" "$HRA_PROVIDER_URL" "$KIMI_BIN" "$PATH" <<'PY'
import json, pathlib, shlex, sys
root=pathlib.Path(sys.argv[1]); base=sys.argv[2]
(root/'capture.py').write_text('import json, os, sys\npayload=json.load(sys.stdin)\nfd=os.open(sys.argv[1],os.O_WRONLY|os.O_APPEND|os.O_CREAT,0o600)\nos.write(fd,(json.dumps(payload)+"\\n").encode())\nos.close(fd)\n')
command=f'python3 "{root}/capture.py" "{root}/hooks.jsonl"'
config=f'''default_model = "mock"
[providers.mock]
type = "openai"
base_url = "{base}/v1"
api_key = "dummy"
[models.mock]
provider = "mock"
model = "mock-model"
max_context_size = 128000
'''
# Same [[hooks]] contract as herdr; these independent observers capture real payloads.
for event in ['SessionStart','SessionEnd','UserPromptSubmit','PreToolUse','PostToolUse','PostToolUseFailure','Stop','StopFailure','SubagentStart','SubagentStop']:
    config+=f'\n[[hooks]]\nevent = "{event}"\ncommand = {json.dumps(command)}\ntimeout = 10\n'
(root/'agent/config.toml').write_text(config); (root/'config.before.toml').write_text(config)
(root/'agent/tui.toml').write_text('[upgrade]\nauto_install = false\n')
q=shlex.quote
(root/'run-agent.sh').write_text(f'''#!/usr/bin/env bash
set -euo pipefail
cd {q(str(root/'work'))}
exec env -i PATH={q(sys.argv[4])} HOME={q(str(root/'home'))} KIMI_CODE_HOME={q(str(root/'agent'))} XDG_CONFIG_HOME={q(str(root/'home/.config'))} XDG_CACHE_HOME={q(str(root/'home/.cache'))} XDG_DATA_HOME={q(str(root/'home/.local/share'))} XDG_STATE_HOME={q(str(root/'home/.local/state'))} LANG=C.UTF-8 NO_COLOR=1 KIMI_DISABLE_TELEMETRY=1 KIMI_CODE_NO_AUTO_UPDATE=1 DO_NOT_TRACK=1 NO_PROXY=127.0.0.1,localhost HERDR_ENV="${{HERDR_ENV:-}}" HERDR_PANE_ID="${{HERDR_PANE_ID:-}}" HERDR_TAB_ID="${{HERDR_TAB_ID:-}}" HERDR_WORKSPACE_ID="${{HERDR_WORKSPACE_ID:-}}" HERDR_RESPAN=on {q(sys.argv[3])} --skills-dir {q(str(root/'skills'))} --prompt "$1"
''')
PY
"$HRA_BIN" setup kimi --yes --state-dir "$HRA_ROOT/state" --config-dir "$HRA_ROOT/config" >"$HRA_ROOT/setup.out"
hra_start_pane "KIMI_CODE_HOME=$KIMI_CODE_HOME"
hra_prompt "bash '$HRA_ROOT/run-agent.sh' 'Say the deterministic mock text.'" text
hra_prompt "bash '$HRA_ROOT/run-agent.sh' 'RUN_TOOL: execute echo hra-e2e then reply.'" tool
hra_flush
hra_assert
python3 - "$HRA_ROOT" <<'PY'
import collections, json, pathlib, sys
root=pathlib.Path(sys.argv[1]); spans=json.loads((root/'spans.json').read_text())
def attrs(s): return {a['key']:next(iter(a['value'].values())) for a in s['attributes']}
traces=collections.defaultdict(list)
for s in spans:
    assert s['status']['code']==1, s
    assert attrs(s)['respan.threads.thread_identifier']
    traces[s['traceId']].append(s)
assert len(traces)==2, traces
counts=[]
for trace,tree in traces.items():
    turns=[s for s in tree if attrs(s)['respan.entity.log_type']=='agent']
    chats=[s for s in tree if attrs(s)['respan.entity.log_type']=='chat']
    tools=[s for s in tree if attrs(s)['respan.entity.log_type']=='tool']
    assert len(turns)==1, tree
    print(f"trace {trace}: {turns[0]['name']}")
    for s in chats+tools:
        assert s['parentSpanId']==turns[0]['spanId'], s
        a=attrs(s)
        if s in chats:
            for key,value in [('gen_ai.usage.input_tokens',11),('gen_ai.usage.output_tokens',7),('llm.usage.cache_read_input_tokens',3)]:
                assert int(a[key])==value, a
            print('  chat: input=11 output=7 cache_read=3')
        else:
            assert 'echo hra-e2e' in a['traceloop.entity.input'], a
            assert 'hra-e2e' in a['traceloop.entity.output'], a
            print(f"  tool {s['name']}: echo hra-e2e -> hra-e2e")
    counts.append((len(turns),len(chats),len(tools)))
assert sorted(counts)==[(1,1,0),(1,2,1)], counts
(root/'received.before.json').write_text(json.dumps({p.name:p.read_text() for p in (root/'otlp').glob('*.json')}))
print(f'Asserted {len(spans)} spans, 2 traces')
PY
env -u HERDR_ENV -u HERDR_PANE_ID -u HERDR_WORKSPACE_ID -u HERDR_TAB_ID bash "$HRA_ROOT/run-agent.sh" 'RUN_TOOL: execute echo hra-e2e then reply.' >"$HRA_ROOT/outside.out" 2>"$HRA_ROOT/outside.err"
hra_flush
"$HRA_BIN" remove kimi --state-dir "$HRA_ROOT/state" --config-dir "$HRA_ROOT/config"
python3 - "$HRA_ROOT" <<'PY'
import json, pathlib, sys, tomllib
root=pathlib.Path(sys.argv[1])
assert json.loads((root/'received.before.json').read_text())=={p.name:p.read_text() for p in (root/'otlp').glob('*.json')}
assert tomllib.loads((root/'config.before.toml').read_text())==tomllib.loads((root/'agent/config.toml').read_text())
print('Outside-herdr gate: no new OTLP; remove: isolated TOML semantically restored')
PY

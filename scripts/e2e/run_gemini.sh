#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/lib.sh"
hra_init gemini
hra_start_mocks
export GEMINI_CLI_HOME="$HRA_ROOT/home"
mkdir -p "$GEMINI_CLI_HOME/.gemini" "$HRA_ROOT/xdg"/{config,cache,data,state}
python3 - "$HRA_ROOT" <<'PY'
import json, pathlib, sys
root = pathlib.Path(sys.argv[1])
(root / 'capture.py').write_text('import json, pathlib, sys\npayload=json.load(sys.stdin)\nwith pathlib.Path(sys.argv[1]).open("a") as f: f.write(json.dumps(payload)+"\\n")\nprint("{}")\n')
command = f'python3 "{root}/capture.py" "{root}/hooks.jsonl"'
events = ['SessionStart','BeforeAgent','BeforeModel','AfterModel','BeforeTool','AfterTool','AfterAgent','SessionEnd']
settings = {'security': {'auth': {'selectedType': 'gemini-api-key'}}, 'telemetry': {'enabled': False}, 'hooks': {event: [{'matcher': '*', 'hooks': [{'name': 'hra-capture', 'type': 'command', 'command': command}]}] for event in events}}
(root / 'home/.gemini/settings.json').write_text(json.dumps(settings))
(root / 'settings.before.json').write_text(json.dumps(settings))
PY
HOME="$GEMINI_CLI_HOME" "$HRA_BIN" setup gemini --yes --state-dir "$HRA_ROOT/state" --config-dir "$HRA_ROOT/config" >"$HRA_ROOT/setup.out"
hra_start_pane "HOME=$GEMINI_CLI_HOME" "GEMINI_CLI_HOME=$GEMINI_CLI_HOME"
# env -i excludes inherited keys/OAuth/project profiles; only pane identity survives.
common="env -i PATH='$PATH' HOME='$GEMINI_CLI_HOME' GEMINI_CLI_HOME='$GEMINI_CLI_HOME' XDG_CONFIG_HOME='$HRA_ROOT/xdg/config' XDG_CACHE_HOME='$HRA_ROOT/xdg/cache' XDG_DATA_HOME='$HRA_ROOT/xdg/data' XDG_STATE_HOME='$HRA_ROOT/xdg/state' LANG=C.UTF-8 CI=1 NO_COLOR=1 TERM=dumb NO_PROXY=127.0.0.1,localhost DO_NOT_TRACK=1 GEMINI_TELEMETRY_ENABLED=false GEMINI_API_KEY=hra-e2e-dummy GOOGLE_GEMINI_BASE_URL='$HRA_PROVIDER_URL'"
command="$common HERDR_ENV=1 HERDR_RESPAN=on HERDR_PANE_ID=\"\$HERDR_PANE_ID\" HERDR_WORKSPACE_ID=\"\$HERDR_WORKSPACE_ID\" HERDR_TAB_ID=\"\$HERDR_TAB_ID\" gemini --skip-trust --approval-mode auto_edit --allowed-tools run_shell_command -m gemini-2.5-flash -p"
hra_prompt "$command 'Say the deterministic mock text.'" text
hra_prompt "$command 'RUN_TOOL: execute echo hra-e2e then reply.'" tool
hra_flush
hra_assert
python3 - "$HRA_ROOT" <<'PY'
import collections, json, pathlib, sys
root = pathlib.Path(sys.argv[1])
spans = json.loads((root / 'spans.json').read_text())
hooks = [json.loads(line) for line in (root / 'hooks.jsonl').read_text().splitlines()]
def attrs(span):
    return {a['key']: next(iter(a['value'].values())) for a in span['attributes']}
traces = collections.defaultdict(list)
for span in spans:
    a = attrs(span)
    assert a['respan.threads.thread_identifier'], a
    assert span['status']['code'] == 1, span
    traces[span['traceId']].append(span)
assert len(traces) == 2, traces
counts = []
for trace, tree in traces.items():
    turns = [s for s in tree if attrs(s)['respan.entity.log_type'] == 'agent']
    chats = [s for s in tree if attrs(s)['respan.entity.log_type'] == 'chat']
    tools = [s for s in tree if attrs(s)['respan.entity.log_type'] == 'tool']
    assert len(turns) == 1
    assert len(chats) == (2 if tools else 1), tree
    assert len(tools) <= 1
    parent = turns[0]
    assert not parent.get('parentSpanId')
    session = attrs(parent)['respan.threads.thread_identifier']
    observed = [h for h in hooks if h['session_id'] == session]
    assert sum(h['hook_event_name'] == 'BeforeModel' for h in observed) == len(chats)
    assert sum(h['hook_event_name'] == 'AfterModel' for h in observed) > len(chats), observed
    print(f"trace {trace}: {parent['name']} [{parent['spanId']}]")
    for child in chats + tools:
        a = attrs(child)
        assert child['parentSpanId'] == parent['spanId'], child
        if child in chats:
            for key, value in [('input_tokens',11), ('output_tokens',7), ('cache_read_input_tokens',3), ('prompt_tokens',11), ('completion_tokens',7)]:
                prefix = 'llm.usage.' if key == 'cache_read_input_tokens' else 'gen_ai.usage.'
                assert int(a[prefix + key]) == value, a
            print(f"  chat {child['name']}: input=11 output=7 cache_read=3")
        else:
            assert child['name'] == 'run_shell_command', child
            assert 'echo hra-e2e' in a['traceloop.entity.input'], a
            assert 'hra-e2e' in a['traceloop.entity.output'], a
            print(f"  tool {child['name']}: echo hra-e2e -> hra-e2e")
    counts.append((len(turns),len(chats),len(tools)))
assert sorted(counts) == [(1,1,0),(1,2,1)], counts
assert 'hra-e2e mock response' in (root/'text.out').read_text()
assert 'hra-e2e mock response' in (root/'tool.out').read_text()
assert sum('hra-e2e mock response' in attrs(s).get('gen_ai.completion.0.content', '') for s in spans if attrs(s)['respan.entity.log_type'] == 'chat') == 2
requests = [json.loads(line) for line in (root / 'provider.jsonl').read_text().splitlines()]
streams = [r for r in requests if 'streamGenerateContent' in r['path']]
assert len(streams) == 3, streams
assert sum(attrs(s)['respan.entity.log_type'] == 'chat' for s in spans) == len(streams)
print(f'Asserted {len(spans)} spans, 2 traces, exactly one chat per streamed response')
(root / 'received.before.json').write_text(json.dumps({p.name:p.read_text() for p in (root/'otlp').glob('*.json')}))
PY
(cd "$HRA_ROOT/work" && bash -c "$common gemini --skip-trust --approval-mode auto_edit --allowed-tools run_shell_command -m gemini-2.5-flash -p 'RUN_TOOL: execute echo hra-e2e then reply.'") >"$HRA_ROOT/outside.out" 2>"$HRA_ROOT/outside.err"
hra_flush
python3 - "$HRA_ROOT" <<'PY'
import json, pathlib, sys
root = pathlib.Path(sys.argv[1])
assert json.loads((root/'received.before.json').read_text()) == {p.name:p.read_text() for p in (root/'otlp').glob('*.json')}
requests = [json.loads(line) for line in (root/'provider.jsonl').read_text().splitlines()]
assert sum('streamGenerateContent' in r['path'] for r in requests) == 5
assert 'hra-e2e mock response' in (root/'outside.out').read_text()
print('Outside-herdr gate: real tool turn completed; no new OTLP delivered')
PY
HOME="$GEMINI_CLI_HOME" "$HRA_BIN" remove gemini --state-dir "$HRA_ROOT/state" --config-dir "$HRA_ROOT/config"
python3 - "$HRA_ROOT" <<'PY'
import json, pathlib, sys
root = pathlib.Path(sys.argv[1])
assert json.loads((root/'settings.before.json').read_text()) == json.loads((root/'home/.gemini/settings.json').read_text())
print('Remove: isolated settings.json semantically restored, including telemetry')
PY

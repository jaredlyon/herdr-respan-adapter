#!/usr/bin/env bash
set -euo pipefail
HRA_BIN=${HRA_BIN:-"$(cd "$(dirname "$0")/../.." && pwd)/target/release/herdr-respan"}
source "$(dirname "$0")/lib.sh"
hra_init pi
hra_start_mocks
export PI_CODING_AGENT_DIR="$HRA_ROOT/agent"
mkdir -p "$PI_CODING_AGENT_DIR/extensions" "$HRA_ROOT/home" "$HRA_ROOT/xdg"/{config,cache,data,state}
python3 - "$HRA_ROOT" "$HRA_PROVIDER_URL" "$PATH" <<'PY'
import json, pathlib, shlex, sys
root = pathlib.Path(sys.argv[1])
models = {'providers': {'mock': {'baseUrl': sys.argv[2] + '/v1', 'api': 'openai-completions', 'apiKey': 'hra-e2e-dummy', 'models': [{'id': 'mock-model'}]}}}
settings = {'compaction': {'enabled': False}, 'retry': {'enabled': False}}
(root/'agent/models.json').write_text(json.dumps(models))
(root/'agent/settings.json').write_text(json.dumps(settings))
(root/'config.before.json').write_text(json.dumps({'models': models, 'settings': settings}))
(root/'agent/extensions/capture.ts').write_text('''import { appendFileSync } from "node:fs";
export default function(pi: any) {
  for (const name of ["before_agent_start", "context", "message_start", "message_end", "tool_execution_start", "tool_execution_end", "agent_settled", "session_shutdown"]) {
    pi.on(name, (event: any, ctx: any) => {
      appendFileSync(process.env.HRA_CAPTURE!, JSON.stringify({ ...event, at_ms: Date.now(), mode: ctx.mode, cwd: ctx.cwd, session_id: ctx.sessionManager.getSessionId(), session_path: ctx.sessionManager.getSessionFile() }) + "\\n");
    });
  }
}
''')
# Construct a clean environment, carrying only the pane's inherited Herdr identity.
env = {'PATH':sys.argv[3], 'HOME':str(root/'home'), 'PI_CODING_AGENT_DIR':str(root/'agent'), 'PI_OFFLINE':'1', 'PI_TELEMETRY':'0', 'LANG':'C.UTF-8', 'CI':'1', 'NO_COLOR':'1', 'TERM':'dumb', 'NO_PROXY':'127.0.0.1,localhost', 'DO_NOT_TRACK':'1', 'OTEL_SDK_DISABLED':'true', 'HRA_CAPTURE':str(root/'native.jsonl')}
for kind in ('CONFIG','CACHE','DATA','STATE'):
    env['XDG_'+kind+'_HOME'] = str(root/'xdg'/kind.lower())
launch = '#!/usr/bin/env bash\nset -euo pipefail\nidentity=()\nwhile IFS= read -r key; do identity+=("$key=${!key}"); done < <(compgen -v HERDR_)\nexec env -i '
launch += ' '.join(shlex.quote(k+'='+v) for k,v in env.items())
launch += ' "${identity[@]}" pi --offline --no-approve -p --tools bash --model mock/mock-model "$@"\n'
(root/'launch.sh').write_text(launch)
PY
"$HRA_BIN" setup pi --yes --state-dir "$HRA_ROOT/state" --config-dir "$HRA_ROOT/config" >"$HRA_ROOT/setup.out"
hra_start_pane
hra_prompt "bash '$HRA_ROOT/launch.sh' 'Say the deterministic mock text.'" text
hra_prompt "bash '$HRA_ROOT/launch.sh' 'RUN_TOOL: execute echo hra-e2e then reply.'" tool
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
    a = attrs(span)
    assert a['respan.threads.thread_identifier']
    assert span['status']['code'] == 1, span
    traces[span['traceId']].append(span)
assert len(traces) == 2, traces
counts = []
for tree in traces.values():
    turns = [s for s in tree if attrs(s)['respan.entity.log_type'] == 'agent']
    chats = [s for s in tree if attrs(s)['respan.entity.log_type'] == 'chat']
    tools = [s for s in tree if attrs(s)['respan.entity.log_type'] == 'tool']
    assert len(turns) == 1
    parent = turns[0]
    assert not parent.get('parentSpanId')
    print(f"trace {parent['traceId']}: {parent['name']}")
    for child in chats + tools:
        a = attrs(child)
        assert child['parentSpanId'] == parent['spanId'], child
        if child in chats:
            for key, value in [('gen_ai.usage.input_tokens',11), ('gen_ai.usage.output_tokens',7), ('llm.usage.cache_read_input_tokens',3), ('llm.usage.cache_creation_input_tokens',0), ('llm.usage.reasoning_tokens',0), ('gen_ai.usage.prompt_tokens',11), ('gen_ai.usage.completion_tokens',7)]:
                assert int(a[key]) == value, a
            print('  chat: input=11 (total prompt) output=7 cache_read=3 cache_write=0')
        else:
            assert child['name'] == 'bash', child
            assert 'echo hra-e2e' in a['traceloop.entity.input'], a
            assert 'hra-e2e' in a['traceloop.entity.output'], a
            print('  tool: bash echo hra-e2e -> hra-e2e')
    counts.append((len(turns),len(chats),len(tools)))
assert sorted(counts) == [(1,1,0),(1,2,1)], counts
native = [json.loads(line) for line in (root/'native.jsonl').read_text().splitlines()]
completed = [e['message'] for e in native if e['type'] == 'message_end' and e.get('message',{}).get('role') == 'assistant']
assert len(completed) == 3, completed
for message in completed:
    assert message['usage']['input'] == 8 and message['usage']['output'] == 7
    assert message['usage']['cacheRead'] == 3 and message['usage']['cacheWrite'] == 0
assert len([e for e in native if e['type'] == 'agent_settled']) == 2
requests = [json.loads(line) for line in (root/'provider.jsonl').read_text().splitlines()]
assert len(requests) == 3 and all(r['path'] == '/v1/chat/completions' for r in requests), requests
(root/'received.before.json').write_text(json.dumps({p.name:p.read_text() for p in (root/'otlp').glob('*.json')}))
print(f'Asserted {len(spans)} spans, 2 traces, 3 completed-assistant messages')
PY
(cd "$HRA_ROOT/work" && env -u HERDR_ENV -u HERDR_PANE_ID -u HERDR_WORKSPACE_ID -u HERDR_TAB_ID -u HERDR_SOCKET_PATH -u HERDR_RESPAN_PROXY bash "$HRA_ROOT/launch.sh" 'RUN_TOOL: execute echo hra-e2e then reply.') </dev/null >"$HRA_ROOT/outside.out" 2>"$HRA_ROOT/outside.err"
hra_flush
python3 - "$HRA_ROOT" <<'PY'
import json, pathlib, sys
root = pathlib.Path(sys.argv[1])
assert json.loads((root/'received.before.json').read_text()) == {p.name:p.read_text() for p in (root/'otlp').glob('*.json')}
requests = [json.loads(line) for line in (root/'provider.jsonl').read_text().splitlines()]
assert len(requests) == 5 and all(r['path'] == '/v1/chat/completions' for r in requests)
print('Outside-herdr gate: two real local requests, no new OTLP delivered')
PY
"$HRA_BIN" remove pi --state-dir "$HRA_ROOT/state" --config-dir "$HRA_ROOT/config"
python3 - "$HRA_ROOT" <<'PY'
import json, pathlib, sys
root = pathlib.Path(sys.argv[1])
before = json.loads((root/'config.before.json').read_text())
for name in ('models','settings'):
    assert json.loads((root/f'agent/{name}.json').read_text()) == before[name]
assert not (root/'agent/extensions/herdr-respan.ts').exists()
assert (root/'agent/extensions/capture.ts').exists()
print('Remove: models/settings semantically restored; foreign extension retained')
PY

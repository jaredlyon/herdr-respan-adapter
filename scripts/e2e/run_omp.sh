#!/usr/bin/env bash
set -euo pipefail
HRA_BIN=${HRA_BIN:-$(cd "$(dirname "$0")/../.." && pwd)/target/release/herdr-respan}
source "$(dirname "$0")/lib.sh"
hra_init omp
hra_start_mocks
REAL_HOME=$HOME
OMP_CLI="$REAL_HOME/omp-repro/src/packages/coding-agent/src/cli.ts"
BUN=$(command -v bun)
export HOME="$HRA_ROOT/home" PI_CONFIG_DIR=omp-config
unset PI_CODING_AGENT_DIR OMP_PROFILE PI_PROFILE
AGENT_DIR="$HOME/$PI_CONFIG_DIR/agent"
mkdir -p "$AGENT_DIR/extensions"
cat >"$AGENT_DIR/models.yml" <<EOF
providers:
  mock:
    baseUrl: $HRA_PROVIDER_URL/v1
    apiKey: dummy
    api: openai-completions
    models:
      - id: mock-model
        name: Mock
        reasoning: false
        input: [text]
        contextWindow: 128000
        maxTokens: 4096
        cost: {input: 0, output: 0, cacheRead: 0, cacheWrite: 0}
EOF
cat >"$AGENT_DIR/config.yml" <<'EOF'
startup:
  setupWizard: false
  checkUpdate: false
  quiet: true
  showSplash: false
  changelogMode: hidden
marketplace:
  autoUpdate: off
telemetry:
  otlpExportEnabled: false
EOF
printf '// Isolated pre-existing extension; setup/remove must preserve it.\nexport default function () {}\n' >"$AGENT_DIR/extensions/existing.ts"
python3 - "$AGENT_DIR" "$HRA_ROOT/before.json" <<'PY'
import json, pathlib, sys
p=pathlib.Path(sys.argv[1])
pathlib.Path(sys.argv[2]).write_text(json.dumps({str(f.relative_to(p)):f.read_text() for f in p.rglob('*') if f.is_file()}))
PY
# Install via PI_CONFIG_DIR: global setup intentionally rejects the shared pi/omp override.
"$HRA_BIN" setup omp --yes --state-dir "$HRA_ROOT/state" --config-dir "$HRA_ROOT/config" >"$HRA_ROOT/setup.out"
export PI_CODING_AGENT_DIR="$AGENT_DIR"
cat >"$AGENT_DIR/extensions/capture.ts" <<'EOF'
import { appendFileSync, writeFileSync } from 'node:fs';
import { collect } from './herdr-respan-omp';
export default function(api: any) {
  api.on('session_start', (_e: any, ctx: any) => writeFileSync(process.env.HRA_ROOT + '/ready', String(ctx.hasUI)));
  collect(api, batch => appendFileSync(process.env.HRA_ROOT + '/batches.jsonl', JSON.stringify(batch) + '\n'));
}
EOF
# Clean environment excludes inherited credentials, profiles, telemetry exporters and dotenv roots.
cat >"$HRA_ROOT/launch.sh" <<EOF
#!/usr/bin/env bash
set -euo pipefail
exec env -i HOME='$HOME' PATH='$PATH' LANG=C.UTF-8 TERM=xterm-256color \\
 PI_CONFIG_DIR=omp-config PI_CODING_AGENT_DIR='$AGENT_DIR' HRA_ROOT='$HRA_ROOT' \\
 XDG_CONFIG_HOME='$HRA_ROOT/xdg/config' XDG_CACHE_HOME='$HRA_ROOT/xdg/cache' \\
 XDG_DATA_HOME='$HRA_ROOT/xdg/data' XDG_STATE_HOME='$HRA_ROOT/xdg/state' \\
 NO_PROXY=127.0.0.1,localhost DO_NOT_TRACK=1 OTEL_SDK_DISABLED=true \\
 HTTP_PROXY='$HRA_PROVIDER_URL' HTTPS_PROXY='$HRA_PROVIDER_URL' \\
 HERDR_ENV="\${HERDR_ENV:-}" HERDR_SOCKET_PATH="\${HERDR_SOCKET_PATH:-}" \\
 HERDR_PANE_ID="\${HERDR_PANE_ID:-}" HERDR_WORKSPACE_ID="\${HERDR_WORKSPACE_ID:-}" \\
 HERDR_TAB_ID="\${HERDR_TAB_ID:-}" HERDR_RESPAN="\${HERDR_RESPAN:-on}" \\
 '$BUN' '$OMP_CLI' --model mock/mock-model --tools bash --no-lsp --no-skills --no-rules --no-title --no-prewalk --no-pty --yolo
EOF
hra_start_pane "HOME=$HOME"
hra_herdr pane run "$HRA_PANE" "bash '$HRA_ROOT/launch.sh'; printf '%s' \$? > '$HRA_ROOT/tui.exit'" >/dev/null
for ((i=0;i<1200;i++)); do
    [[ -s "$HRA_ROOT/ready" ]] && break
    sleep .1
done
[[ $(cat "$HRA_ROOT/ready") == true ]]
for prompt in 'Say the deterministic mock text.' 'RUN_TOOL: execute echo hra-e2e then reply.'; do
    expected=$(( ${expected:-0} + 1 ))
    hra_herdr pane run "$HRA_PANE" "$prompt" >/dev/null
    for ((i=0;i<1200;i++)); do
        if [[ -f "$HRA_ROOT/batches.jsonl" ]] && [[ $(wc -l <"$HRA_ROOT/batches.jsonl") -ge $expected ]]; then break; fi
        sleep .1
    done
    [[ -f "$HRA_ROOT/batches.jsonl" ]] && [[ $(wc -l <"$HRA_ROOT/batches.jsonl") == $expected ]]
done
hra_herdr pane send-keys "$HRA_PANE" ctrl+d >/dev/null
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
for trace, tree in traces.items():
    turns=[s for s in tree if attrs(s)['respan.entity.log_type']=='agent']
    chats=[s for s in tree if attrs(s)['respan.entity.log_type']=='chat']
    tools=[s for s in tree if attrs(s)['respan.entity.log_type']=='tool']
    assert len(turns)==1
    turn=turns[0]; assert not turn.get('parentSpanId')
    print(f"trace {trace}: {turn['name']}")
    for s in chats+tools:
        a=attrs(s); assert s['parentSpanId']==turn['spanId'], s
        if s in chats:
            for key,value in [('gen_ai.usage.input_tokens',11),('gen_ai.usage.output_tokens',7),('gen_ai.usage.prompt_tokens',11),('gen_ai.usage.completion_tokens',7),('llm.usage.cache_read_input_tokens',3),('llm.usage.cache_creation_input_tokens',0)]:
                assert int(a[key])==value, a
            print('  chat mock-model: input=11 (total prompt) output=7 cache_read=3')
        else:
            assert 'echo hra-e2e' in a['traceloop.entity.input'], a
            assert 'hra-e2e' in a['traceloop.entity.output'], a
            print('  tool bash: echo hra-e2e -> hra-e2e')
    counts.append((len(turns),len(chats),len(tools)))
assert sorted(counts)==[(1,1,0),(1,2,1)], counts
(root/'received.before.json').write_text(json.dumps({p.name:p.read_text() for p in (root/'otlp').glob('*.json')}))
print(f'Asserted {len(spans)} spans, two interactive prompts')
PY
# Run the same real TUI under an ordinary PTY, not print mode: ctx.hasUI remains true.
python3 - "$HRA_ROOT" "$AGENT_DIR" <<'PY'
import json, os, pathlib, pty, select, signal, sys, time
root=pathlib.Path(sys.argv[1]); agent=pathlib.Path(sys.argv[2]); old=set(agent.glob('sessions/**/*.jsonl'))
(root/'ready').unlink()
pid,fd=pty.fork()
if pid==0:
    os.chdir(root/'work')
    for key in list(os.environ):
        if key.startswith('HERDR_'): del os.environ[key]
    os.execvp('bash',['bash',str(root/'launch.sh')])
try:
    deadline=time.monotonic()+120; submitted=False; done=False
    with (root/'outside.pty').open('wb') as log:
        while time.monotonic()<deadline:
            readable,_,_=select.select([fd],[],[],.1)
            if readable:
                try: data=os.read(fd,65536)
                except OSError: break
                log.write(data)
            if not submitted and (root/'ready').exists():
                os.write(fd,b'RUN_TOOL: execute echo hra-e2e then reply.\r'); submitted=True
            new=set(agent.glob('sessions/**/*.jsonl'))-old
            for file in new:
                messages=[json.loads(line).get('message',{}) for line in file.read_text().splitlines()]
                if any(m.get('role')=='toolResult' for m in messages) and any(m.get('role')=='assistant' and m.get('stopReason')=='stop' for m in messages): done=True
            if done: break
    assert done, 'Outside-herdr interactive tool turn failed'
    os.write(fd,b'\x04')
finally:
    os.kill(pid,signal.SIGTERM); os.waitpid(pid,0); os.close(fd)
print('Outside-herdr real interactive TUI completed tool turn')
PY
hra_flush
python3 - "$HRA_ROOT" <<'PY'
import json, pathlib, sys
r=pathlib.Path(sys.argv[1]); assert json.loads((r/'received.before.json').read_text())=={p.name:p.read_text() for p in (r/'otlp').glob('*.json')}
assert len((r/'batches.jsonl').read_text().splitlines())==2
print('Outside-herdr gate: no new batches or OTLP')
PY
"$HRA_BIN" remove omp --state-dir "$HRA_ROOT/state" --config-dir "$HRA_ROOT/config"
python3 - "$AGENT_DIR" "$HRA_ROOT/before.json" <<'PY'
import json, pathlib, sys
p=pathlib.Path(sys.argv[1]); before=json.loads(pathlib.Path(sys.argv[2]).read_text())
assert not (p/'extensions/herdr-respan-omp.ts').exists()
for file,text in before.items(): assert (p/file).read_text()==text, file
print('Remove: original models/config/custom extension restored unchanged')
PY

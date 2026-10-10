#!/usr/bin/env bash
set -euo pipefail
source "$(dirname "$0")/lib.sh"
hra_init claude
hra_start_mocks
export CLAUDE_CONFIG_DIR="$HRA_ROOT/claude"
export ANTHROPIC_BASE_URL="$HRA_PROVIDER_URL" ANTHROPIC_API_KEY=hra-e2e-dummy
export CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 DISABLE_AUTOUPDATER=1
unset ANTHROPIC_AUTH_TOKEN CLAUDE_CODE_OAUTH_TOKEN
mkdir -p "$CLAUDE_CONFIG_DIR"
python3 - "$HRA_ROOT" <<'PY'
import json, pathlib, sys
root = pathlib.Path(sys.argv[1])
(root / 'capture.py').write_text('import json, pathlib, sys\npayload=json.load(sys.stdin)\nwith pathlib.Path(sys.argv[1]).open("a") as f: f.write(json.dumps(payload)+"\\n")\n')
command = f'python3 "{root}/capture.py" "{root}/hooks.jsonl"'
events = ['SessionStart','UserPromptSubmit','PreToolUse','PostToolUse','PostToolUseFailure','Stop','StopFailure','SessionEnd','SubagentStart','SubagentStop']
settings = {'env': {'DISABLE_AUTOUPDATER': '1'}, 'permissions': {'allow': ['Bash(echo hra-e2e)']}, 'hooks': {e: [{'hooks': [{'type': 'command', 'command': command}]}] for e in events}}
(root / 'claude/settings.json').write_text(json.dumps(settings))
(root / 'settings.before.json').write_text(json.dumps(settings))
(root / 'claude/.claude.json').write_text(json.dumps({'hasCompletedOnboarding': True, 'theme': 'dark', 'autoUpdates': False}))
PY
"$HRA_BIN" setup claude --yes --state-dir "$HRA_ROOT/state" --config-dir "$HRA_ROOT/config" >"$HRA_ROOT/setup.out"
hra_start_pane "CLAUDE_CONFIG_DIR=$CLAUDE_CONFIG_DIR" "ANTHROPIC_BASE_URL=$HRA_PROVIDER_URL" "ANTHROPIC_API_KEY=hra-e2e-dummy" "ANTHROPIC_AUTH_TOKEN=" "CLAUDE_CODE_OAUTH_TOKEN=" "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1" "DISABLE_AUTOUPDATER=1"
# Only Bash is exposed; it is pre-authorized for this harmless command, not a blanket bypass.
command="claude -p --model claude-haiku-4-5-20251001 --tools Bash --allowedTools 'Bash(echo hra-e2e)' --setting-sources user --strict-mcp-config --mcp-config '{\"mcpServers\":{}}' --"
hra_prompt "$command 'Say the deterministic mock text.'" text
hra_prompt "$command 'RUN_TOOL: execute echo hra-e2e then reply.'" tool
hra_flush
hra_assert
python3 - "$HRA_ROOT" "$HRA_PANE" <<'PY'
import collections, json, pathlib, sys
root = pathlib.Path(sys.argv[1])
spans = json.loads((root / 'spans.json').read_text())
def attrs(span):
    return {a['key']: next(iter(a['value'].values())) for a in span['attributes']}
traces = collections.defaultdict(list)
for span in spans:
    a = attrs(span)
    metadata = json.loads(a['respan.metadata'])
    assert metadata['herdr.pane_id'] == sys.argv[2], a
    for key in ('herdr.workspace_id', 'herdr.machine'):
        assert metadata[key], a
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
            assert 'echo hra-e2e' in a['traceloop.entity.input'], a
            assert 'hra-e2e' in a['traceloop.entity.output'], a
            print(f"  tool {child['name']}: echo hra-e2e -> hra-e2e")
    counts.append((len(turns),len(chats),len(tools)))
assert sorted(counts) == [(1,1,0),(1,2,1)], counts
print(f"Asserted {len(spans)} spans, 2 traces; pane={sys.argv[2]}")
(root / 'received.before.json').write_text(json.dumps({p.name:p.read_text() for p in (root/'otlp').glob('*.json')}))
PY
# Same installed hooks, but no Herdr gate: provider requests still occur, OTLP must not.
(cd "$HRA_ROOT/work" && env -u HERDR_ENV -u HERDR_PANE_ID -u HERDR_WORKSPACE_ID -u HERDR_TAB_ID -u HERDR_RESPAN_PROXY claude -p --model claude-haiku-4-5-20251001 --tools Bash --allowedTools 'Bash(echo hra-e2e)' --setting-sources user --strict-mcp-config --mcp-config '{"mcpServers":{}}' -- 'RUN_TOOL: execute echo hra-e2e then reply.') >"$HRA_ROOT/outside.out" 2>"$HRA_ROOT/outside.err"
hra_flush
python3 - "$HRA_ROOT" <<'PY'
import json, pathlib, sys
root=pathlib.Path(sys.argv[1])
assert json.loads((root/'received.before.json').read_text()) == {p.name:p.read_text() for p in (root/'otlp').glob('*.json')}
print('Outside-herdr gate: no new OTLP delivered')
PY
"$HRA_BIN" remove claude --state-dir "$HRA_ROOT/state" --config-dir "$HRA_ROOT/config"
python3 - "$HRA_ROOT" <<'PY'
import json, pathlib, sys
root=pathlib.Path(sys.argv[1])
assert json.loads((root/'settings.before.json').read_text()) == json.loads((root/'claude/settings.json').read_text())
print('Remove: isolated settings.json semantically restored')
PY

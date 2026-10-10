#!/usr/bin/env bash
# Source this file from a driver. Every run owns only its mocks and named session.
set -euo pipefail
export PATH="$HOME/.local/bin:$PATH"
HRA_REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
HRA_BIN=${HRA_BIN:-$HRA_REPO/bin/herdr-respan.exe}

hra_init() {
    HRA_AGENT=$1
    HRA_SESSION="hra-e2e-$1"
    HRA_ROOT=$(mktemp -d "$HOME/.hra-e2e/$1/run-XXXXXXXX" 2>/dev/null) || {
        mkdir -p "$HOME/.hra-e2e/$1"
        HRA_ROOT=$(mktemp -d "$HOME/.hra-e2e/$1/run-XXXXXXXX")
    }
    export HRA_ROOT HRA_AGENT HRA_BIN
    mkdir -p "$HRA_ROOT"/{state,config,otlp,work}
    HRA_MOCK_PIDS=()
    HRA_SESSION_OWNED=0
    trap hra_cleanup EXIT
    printf 'E2E artifacts: %s\n' "$HRA_ROOT"
}

hra_cleanup() {
    local status=$?
    if [[ ${HRA_SESSION_OWNED:-0} == 1 ]]; then
        XDG_CONFIG_HOME="$HRA_ROOT/xdg" HERDR_CONFIG_PATH="$HRA_ROOT/herdr.toml" herdr session stop "$HRA_SESSION" >/dev/null 2>&1 || true
    fi
    for pid in "${HRA_MOCK_PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
    for pid in "${HRA_MOCK_PIDS[@]}"; do wait "$pid" 2>/dev/null || true; done
    return "$status"
}

hra_start_mocks() {
    python3 "$HRA_REPO/scripts/e2e/mock_provider.py" --port-file "$HRA_ROOT/provider.port" --log "$HRA_ROOT/provider.jsonl" --chunk-delay-ms "${HRA_MOCK_CHUNK_DELAY_MS:-0}" >"$HRA_ROOT/provider.log" 2>&1 &
    HRA_MOCK_PIDS+=("$!")
    python3 "$HRA_REPO/scripts/e2e/mock_otlp.py" --port-file "$HRA_ROOT/otlp.port" --output "$HRA_ROOT/otlp" >"$HRA_ROOT/otlp.log" 2>&1 &
    HRA_MOCK_PIDS+=("$!")
    for ((i=0;i<100;i++)); do
        [[ -s "$HRA_ROOT/provider.port" && -s "$HRA_ROOT/otlp.port" ]] && break
        sleep .1
    done
    HRA_PROVIDER_URL="http://127.0.0.1:$(cat "$HRA_ROOT/provider.port")"
    HRA_OTLP_URL="http://127.0.0.1:$(cat "$HRA_ROOT/otlp.port")"
    export HRA_PROVIDER_URL HRA_OTLP_URL
    printf 'base_url = "%s"\ncustomer_identifier = "hra-e2e"\n' "$HRA_OTLP_URL" >"$HRA_ROOT/config/config.toml"
    printf 'RESPAN_API_KEY=hra-e2e-dummy\n' >"$HRA_ROOT/config/.env"
    chmod 600 "$HRA_ROOT/config/.env"
}

hra_herdr() { XDG_CONFIG_HOME="$HRA_ROOT/xdg" HERDR_CONFIG_PATH="$HRA_ROOT/herdr.toml" herdr --session "$HRA_SESSION" "$@"; }

hra_start_pane() {
    # A separate Herdr config root also excludes the user's linked plugin/event hooks.
    printf '' >"$HRA_ROOT/herdr.toml"
    XDG_CONFIG_HOME="$HRA_ROOT/xdg" HERDR_CONFIG_PATH="$HRA_ROOT/herdr.toml" HERDR_RESPAN=off herdr --session "$HRA_SESSION" server >"$HRA_ROOT/herdr.log" 2>&1 &
    HRA_SERVER_PID=$!
    HRA_SESSION_OWNED=1
    for ((i=0;i<100;i++)); do
        if hra_herdr api snapshot >"$HRA_ROOT/snapshot.json" 2>/dev/null; then break; fi
        kill -0 "$HRA_SERVER_PID" 2>/dev/null || { cat "$HRA_ROOT/herdr.log"; return 1; }
        sleep .1
    done
    local env_args=(--env HERDR_RESPAN=on --env "PATH=$PATH")
    for item in "$@"; do env_args+=(--env "$item"); done
    hra_herdr workspace create --cwd "$HRA_ROOT/work" --label "$HRA_SESSION" --no-focus "${env_args[@]}" >"$HRA_ROOT/pane.json"
    HRA_PANE=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["result"]["root_pane"]["pane_id"])' "$HRA_ROOT/pane.json")
    export HRA_PANE
}

hra_prompt() {
    local command=$1 label=$2
    # File completion cannot match the command echoed by the terminal before execution.
    local marker="$HRA_ROOT/$label.exit"
    hra_herdr pane run "$HRA_PANE" "$command > '$HRA_ROOT/$label.out' 2> '$HRA_ROOT/$label.err'; printf '%s' \$? > '$marker'" >/dev/null
    for ((i=0;i<1200;i++)); do
        if [[ -s "$marker" ]]; then
            [[ $(cat "$marker") == 0 ]] || { cat "$HRA_ROOT/$label.err" "$HRA_ROOT/$label.out"; return 1; }
            return 0
        fi
        sleep .1
    done
    cat "$HRA_ROOT/$label.err" "$HRA_ROOT/$label.out" 2>/dev/null || true
    echo "Timed out: $label" >&2
    return 1
}

hra_flush() {
    for ((i=0;i<200;i++)); do
        "$HRA_BIN" flush --state-dir "$HRA_ROOT/state" --config-dir "$HRA_ROOT/config"
        if python3 - "$HRA_ROOT/state" 2>/dev/null <<'PY'
import pathlib, sys
root = pathlib.Path(sys.argv[1])
assert not list(root.glob('spool/*.ready'))
assert not list(root.glob('spool/*.jsonl'))
assert not (root / 'spool/flush.lock').exists()
assert not list(root.glob('agents/*/settle-pending'))
PY
        then return 0; fi
        sleep .1
    done
    echo 'Spool failed to settle' >&2
    return 1
}

hra_dump() {
    python3 - "$HRA_ROOT/otlp" <<'PY'
import json, pathlib, sys
spans = []
for file in sorted(pathlib.Path(sys.argv[1]).glob('*.json')):
    for resource in json.loads(file.read_text())['resourceSpans']:
        for scope in resource['scopeSpans']:
            spans.extend(scope['spans'])
print(json.dumps(spans, indent=2))
PY
}

# Basic cross-agent invariants; drivers assert their own exact cardinality/usage.
hra_assert() {
    hra_dump >"$HRA_ROOT/spans.json"
    python3 - "$HRA_ROOT/spans.json" "$HRA_PANE" <<'PY'
import json, sys
spans = json.load(open(sys.argv[1]))
assert spans, 'No OTLP spans received'
ids = {(s['traceId'],s['spanId']) for s in spans}
assert len(ids) == len(spans), 'Duplicate span delivery'
for span in spans:
    a = {v['key']:next(iter(v['value'].values())) for v in span['attributes']}
    m = json.loads(a['respan.metadata'])
    assert m['herdr.pane_id'] == sys.argv[2]
    assert m['herdr.workspace_id'] and m['herdr.machine']
    if span.get('parentSpanId'):
        assert (span['traceId'],span['parentSpanId']) in ids, 'Orphan span'
PY
}

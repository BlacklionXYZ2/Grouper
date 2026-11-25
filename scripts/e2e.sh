#!/usr/bin/env bash
# End-to-end smoke test for Grouper (Linux / macOS / Git Bash).
# Exercises the real `grouper-hub` and `grouper` binaries together with no API keys.
#
# Usage:
#   scripts/e2e.sh [--no-build] [--keep] [--timeout <sec>]

set -uo pipefail

NO_BUILD=0
KEEP=0
TIMEOUT_SEC=60

while [ $# -gt 0 ]; do
    case "$1" in
        --no-build) NO_BUILD=1 ;;
        --keep)     KEEP=1 ;;
        --timeout)
            shift
            if [ $# -eq 0 ]; then
                echo "Error: --timeout requires a number of seconds" >&2
                exit 2
            fi
            TIMEOUT_SEC="$1"
            ;;
        -h|--help)
            echo "Usage: scripts/e2e.sh [--no-build] [--keep] [--timeout <sec>]"
            exit 0
            ;;
        *)
            echo "Unknown argument: $1" >&2
            exit 2
            ;;
    esac
    shift
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

IS_WINDOWS_BASH=0
if command -v cygpath >/dev/null 2>&1; then
    IS_WINDOWS_BASH=1
fi

to_native_path() {
    if [ "$IS_WINDOWS_BASH" -eq 1 ]; then
        cygpath -w "$1"
    else
        printf '%s' "$1"
    fi
}

# Isolate from any ambient GROUPER_* settings on the host.
unset GROUPER_HUB GROUPER_TOKEN GROUPER_DATA GROUPER_BIND \
      GROUPER_MASTER_KEY GROUPER_MASTER_KEY_FILE GROUPER_PROVIDER_KEY \
      GROUPER_WORKSPACE GROUPER_MCP_CONFIG GROUPER_FOLDERS_CONFIG \
      GROUPER_CACHE GROUPER_USAGE_PROBES

MANAGED_PIDS=()
TEMP_ROOT=""
STEP_NUMS=()
STEP_NAMES=()
STEP_STATUSES=()
STEP_TIMES=()
STEP_DETAILS=()
SETUP_FAILED=0

stop_pid() {
    local pid="$1"
    if [ -z "$pid" ]; then
        return 0
    fi
    if kill -0 "$pid" 2>/dev/null; then
        kill "$pid" 2>/dev/null || true
        local i=0
        while kill -0 "$pid" 2>/dev/null && [ "$i" -lt 25 ]; do
            sleep 0.2
            i=$((i + 1))
        done
        if kill -0 "$pid" 2>/dev/null; then
            kill -9 "$pid" 2>/dev/null || true
        fi
    fi
    wait "$pid" 2>/dev/null || true
}

cleanup() {
    for pid in "${MANAGED_PIDS[@]:-}"; do
        stop_pid "$pid"
    done
    if [ -n "$TEMP_ROOT" ] && [ -d "$TEMP_ROOT" ]; then
        if [ "$KEEP" -eq 1 ]; then
            echo ""
            echo "Kept temp dir: $TEMP_ROOT"
        else
            local attempt=0
            while [ -d "$TEMP_ROOT" ] && [ "$attempt" -lt 10 ]; do
                rm -rf "$TEMP_ROOT" 2>/dev/null && break
                sleep 0.2
                attempt=$((attempt + 1))
            done
        fi
    fi
}

trap cleanup EXIT INT TERM

pick_free_port() {
    if command -v python3 >/dev/null 2>&1; then
        python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()'
        return 0
    fi
    if command -v python >/dev/null 2>&1; then
        python -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()'
        return 0
    fi
    if command -v perl >/dev/null 2>&1; then
        perl -MIO::Socket::INET -e 'my $s=IO::Socket::INET->new(LocalAddr=>"127.0.0.1",LocalPort=>0,Proto=>"tcp",Listen=>1) or die $!; print $s->sockport,"\n";'
        return 0
    fi
    if command -v powershell.exe >/dev/null 2>&1; then
        powershell.exe -NoProfile -Command '$l=[System.Net.Sockets.TcpListener]::new([System.Net.IPAddress]::Loopback,0);$l.Start();Write-Output $l.LocalEndpoint.Port;$l.Stop()' | tr -d '\r'
        return 0
    fi
    echo $((20000 + (RANDOM % 30000)))
}

# Portable timeout wrapper (uses GNU timeout if present, otherwise pure-bash watchdog).
run_with_timeout() {
    local secs="$1"
    shift
    if command -v timeout >/dev/null 2>&1; then
        timeout "$secs" "$@"
        return $?
    fi
    "$@" &
    local cmd_pid=$!
    (
        sleep "$secs"
        kill "$cmd_pid" 2>/dev/null || true
    ) &
    local watcher_pid=$!
    wait "$cmd_pid"
    local rc=$?
    kill "$watcher_pid" 2>/dev/null || true
    wait "$watcher_pid" 2>/dev/null || true
    return "$rc"
}

wait_until() {
    local desc="$1"
    local secs="$2"
    shift 2
    local deadline=$(( $(date +%s) + secs ))
    while [ "$(date +%s)" -le "$deadline" ]; do
        if "$@"; then
            return 0
        fi
        sleep 0.25
    done
    echo "Timed out after ${secs}s waiting for: $desc" >&2
    return 1
}

grouper_cli() {
    run_with_timeout "$TIMEOUT_SEC" "$CLI_BIN" --hub "$HUB_URL" --token "$ADMIN_TOKEN" --cache "$CACHE_FILE_NATIVE" "$@"
}

run_step() {
    local num="$1"
    local name="$2"
    local fn="$3"
    echo "==> [$num/10] $name"
    local start_s end_s elapsed detail_file detail rc
    start_s="$(date +%s)"
    detail_file="$LOG_DIR/step_${num}_detail.txt"
    : > "$detail_file"
    "$fn" >"$detail_file" 2>&1
    rc=$?
    end_s="$(date +%s)"
    elapsed=$((end_s - start_s))
    detail="$(tr -d '\r' < "$detail_file" | tail -n 3 | tr '\n' ' ' | sed 's/[[:space:]]*$//')"
    [ -z "$detail" ] && detail="ok"
    STEP_NUMS+=("$num")
    STEP_NAMES+=("$name")
    STEP_TIMES+=("$elapsed")
    STEP_DETAILS+=("$detail")
    if [ "$rc" -eq 0 ]; then
        STEP_STATUSES+=("PASS")
        echo "    PASS (${elapsed}s): $detail"
    else
        STEP_STATUSES+=("FAIL")
        echo "    FAIL (${elapsed}s): $detail"
    fi
}

# Build workspace unless --no-build.
if [ "$NO_BUILD" -eq 0 ]; then
    echo "==> Building workspace (cargo build --workspace)..."
    if ! (cd "$REPO_ROOT" && cargo build --workspace); then
        echo "FATAL: cargo build --workspace failed" >&2
        exit 1
    fi
fi

if [ -x "$REPO_ROOT/target/debug/grouper-hub.exe" ]; then
    HUB_BIN="$REPO_ROOT/target/debug/grouper-hub.exe"
    CLI_BIN="$REPO_ROOT/target/debug/grouper.exe"
else
    HUB_BIN="$REPO_ROOT/target/debug/grouper-hub"
    CLI_BIN="$REPO_ROOT/target/debug/grouper"
fi

if [ ! -x "$HUB_BIN" ] || [ ! -x "$CLI_BIN" ]; then
    echo "FATAL: missing binaries ($HUB_BIN / $CLI_BIN)" >&2
    exit 1
fi

TEMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/grouper-e2e.XXXXXX")"
DATA_DIR="$TEMP_ROOT/data"
WORKSPACE_DIR="$TEMP_ROOT/workspace"
RESTORED_DIR="$TEMP_ROOT/restored"
LOG_DIR="$TEMP_ROOT/logs"
MASTER_KEY_FILE="$TEMP_ROOT/master.key"
MCP_CONFIG_FILE="$TEMP_ROOT/mcp.json"
CACHE_FILE="$TEMP_ROOT/cache.db"
BACKUP_ARCHIVE="$TEMP_ROOT/backup.tar.gz"

mkdir -p "$DATA_DIR" "$WORKSPACE_DIR" "$RESTORED_DIR" "$LOG_DIR"
printf '{"includeSubagentChat": false}' > "$MCP_CONFIG_FILE"

DATA_DIR_NATIVE="$(to_native_path "$DATA_DIR")"
WORKSPACE_DIR_NATIVE="$(to_native_path "$WORKSPACE_DIR")"
RESTORED_DIR_NATIVE="$(to_native_path "$RESTORED_DIR")"
MASTER_KEY_NATIVE="$(to_native_path "$MASTER_KEY_FILE")"
MCP_CONFIG_NATIVE="$(to_native_path "$MCP_CONFIG_FILE")"
CACHE_FILE_NATIVE="$(to_native_path "$CACHE_FILE")"
BACKUP_ARCHIVE_NATIVE="$(to_native_path "$BACKUP_ARCHIVE")"

export GROUPER_MASTER_KEY_FILE="$MASTER_KEY_NATIVE"

PORT="$(pick_free_port)"
BIND="127.0.0.1:$PORT"
HUB_URL="http://$BIND"

echo "==> Temp root: $TEMP_ROOT"
echo "==> Hub URL:   $HUB_URL"

PLATFORM_NAME="$(uname -s | tr '[:upper:]' '[:lower:]')"
case "$PLATFORM_NAME" in
    darwin*) PLATFORM_NAME="macos" ;;
    linux*)  PLATFORM_NAME="linux" ;;
    mingw*|msys*|cygwin*) PLATFORM_NAME="windows" ;;
    *)       PLATFORM_NAME="unix" ;;
esac

# Inline scripted provider options JSON. Distinct uppercase trigger tokens prevent
# finished thread summaries from re-triggering earlier orchestrator rules.
SCRIPTED_OPTIONS='{"inline":{"delay_ms":40,"rules":[{"system":"helpful personal assistant","last":"E2E_STD_PING","reply":[{"type":"text","text":"E2E_STD_PONG: standard chat answered."}]},{"system":"orchestrator of Grouper","last":"\"Online Smoke Task\" done]","reply":[{"type":"text","text":"ORCH_ONLINE_FINAL: Online Smoke Task finished cleanly."}]},{"system":"orchestrator of Grouper","last":"\"Scope Check Task\" done]","reply":[{"type":"text","text":"ORCH_SCOPE_FINAL: Scope Check Task finished cleanly."}]},{"system":"orchestrator of Grouper","last":"\"Offline Handover Task\" done]","reply":[{"type":"text","text":"ORCH_OFFLINE_FINAL: Offline Handover Task handed over and finished."}]},{"system":"orchestrator of Grouper","last":"reached its hub checkpoint","reply":[{"type":"text","text":"ORCH_CHECKPOINT_ACK: hub checkpoint ready, waiting for device."}]},{"system":"orchestrator of Grouper","last":"continues on Smoke Device","reply":[{"type":"text","text":"ORCH_HANDOVER_ACK: thread handed over to device."}]},{"system":"orchestrator of Grouper","last":"\"waiting_device\"","reply":[{"type":"text","text":"ORCH_WAITING_ACK: device offline, hub checkpoint started."}]},{"system":"orchestrator of Grouper","last":"\"thread_id\"","reply":[{"type":"text","text":"ORCH_STARTED_ACK: thread started."}]},{"system":"orchestrator of Grouper","last":"TRIGGER_ONLINE_TASK","reply":[{"type":"text","text":"Starting the device task now."},{"type":"tool_use","name":"start_thread","input":{"title":"Online Smoke Task","device":"Smoke Device","goal":"Write artifact.txt and run a shell verification command."}}]},{"system":"orchestrator of Grouper","last":"TRIGGER_SCOPE_TASK","reply":[{"type":"text","text":"Starting scope check thread on hub."},{"type":"tool_use","name":"start_thread","input":{"title":"Scope Check Task","device":"hub","goal":"Confirm thread scoping on the hub."}}]},{"system":"orchestrator of Grouper","last":"TRIGGER_OFFLINE_TASK","reply":[{"type":"text","text":"Starting offline handover task for Smoke Device."},{"type":"tool_use","name":"start_thread","input":{"title":"Offline Handover Task","device":"Smoke Device","goal":"Prepare checkpoint on hub then continue and finish on Smoke Device."}}]},{"system":"on the device \"Smoke Device\"","last":"E2E_SHELL_VERIFIED","reply":[{"type":"text","text":"THREAD_ONLINE_SUMMARY: wrote artifact.txt and verified shell."}]},{"system":"on the device \"Smoke Device\"","last":"to artifact.txt","reply":[{"type":"tool_use","name":"run_shell","input":{"command":"echo E2E_SHELL_VERIFIED"}}]},{"system":"on the device \"Smoke Device\"","last":"# Brief: Online Smoke Task","reply":[{"type":"tool_use","name":"write_file","input":{"path":"artifact.txt","content":"grouper-e2e-artifact-content\n"}}]},{"system":"on the Grouper hub","last":"# Brief: Scope Check Task","reply":[{"type":"text","text":"THREAD_SCOPE_SUMMARY: scope check done."}]},{"system":"on the Grouper hub","last":"to checkpoint.txt","reply":[{"type":"text","text":"HUB_CHECKPOINT_REPORT: wrote checkpoint.txt on hub, waiting for device."}]},{"system":"on the Grouper hub","last":"# Brief: Offline Handover Task","reply":[{"type":"tool_use","name":"write_file","input":{"path":"checkpoint.txt","content":"hub-checkpoint-data\n"}}]},{"system":"on the device \"Smoke Device\"","last":"hub-checkpoint-data","reply":[{"type":"text","text":"THREAD_OFFLINE_FINISHED: continued on device and read hub-checkpoint-data."}]},{"system":"on the device \"Smoke Device\"","last":"is available again and this thread continues here","reply":[{"type":"tool_use","name":"read_file","input":{"path":"checkpoint.txt"}}]}],"default":[{"type":"text","text":"UNEXPECTED_DEFAULT_REPLY"}]}}'

setup_environment() {
    local out
    out="$(run_with_timeout "$TIMEOUT_SEC" "$HUB_BIN" --data "$DATA_DIR_NATIVE" device add "Smoke Admin" --platform "$PLATFORM_NAME" --admin)" || return 1
    ADMIN_TOKEN="$(printf '%s\n' "$out" | tr -d '\r' | awk 'NF{line=$0} END{print line}')"
    ADMIN_ID="${ADMIN_TOKEN%%.*}"
    [ -n "$ADMIN_ID" ] && [ "$ADMIN_ID" != "$ADMIN_TOKEN" ] || { echo "Failed to parse admin token" >&2; return 1; }

    out="$(run_with_timeout "$TIMEOUT_SEC" "$HUB_BIN" --data "$DATA_DIR_NATIVE" device add "Smoke Device" --platform "$PLATFORM_NAME")" || return 1
    DEVICE_TOKEN="$(printf '%s\n' "$out" | tr -d '\r' | awk 'NF{line=$0} END{print line}')"
    DEVICE_ID="${DEVICE_TOKEN%%.*}"
    [ -n "$DEVICE_ID" ] && [ "$DEVICE_ID" != "$DEVICE_TOKEN" ] || { echo "Failed to parse device token" >&2; return 1; }

    run_with_timeout "$TIMEOUT_SEC" "$HUB_BIN" --data "$DATA_DIR_NATIVE" provider scripted \
        --kind scripted --key "e2e-dummy-provider-key" --options "$SCRIPTED_OPTIONS" >/dev/null || return 1

    run_with_timeout "$TIMEOUT_SEC" "$HUB_BIN" --data "$DATA_DIR_NATIVE" model e2e-model \
        --provider scripted --model e2e-fake-1 --roles orchestrator,standard,thread >/dev/null || return 1

    "$HUB_BIN" --data "$DATA_DIR_NATIVE" serve --bind "$BIND" >"$LOG_DIR/hub_out.log" 2>"$LOG_DIR/hub_err.log" &
    HUB_PID=$!
    MANAGED_PIDS+=("$HUB_PID")

    hub_healthy() {
        kill -0 "$HUB_PID" 2>/dev/null || return 1
        local body
        body="$(curl -fsS --max-time 5 "$HUB_URL/healthz" 2>/dev/null | tr -d '\r\n')"
        [ "$body" = "ok" ]
    }
    wait_until "hub /healthz" "$TIMEOUT_SEC" hub_healthy || return 1

    "$CLI_BIN" --hub "$HUB_URL" --token "$DEVICE_TOKEN" --cache "$CACHE_FILE_NATIVE" \
        connect --workspace "$WORKSPACE_DIR_NATIVE" --mcp-config "$MCP_CONFIG_NATIVE" \
        >"$LOG_DIR/device_1_out.log" 2>"$LOG_DIR/device_1_err.log" &
    DEVICE_PID=$!
    MANAGED_PIDS+=("$DEVICE_PID")

    device_ready() {
        kill -0 "$DEVICE_PID" 2>/dev/null || return 1
        local devs
        devs="$(grouper_cli devices 2>/dev/null | tr -d '\r')"
        printf '%s\n' "$devs" | grep -qE "${DEVICE_ID}.*online.*run_shell"
    }
    wait_until "Smoke Device online with run_shell" "$TIMEOUT_SEC" device_ready || return 1
}

STD_CONV_ID=""
PROJ_CONV_ID=""
ONLINE_THREAD_ID=""

step_1() {
    local h_code h_body i_code i_body
    h_code="$(curl -sS --max-time "$TIMEOUT_SEC" -o "$LOG_DIR/s1_healthz.txt" -w '%{http_code}' "$HUB_URL/healthz")" || return 1
    h_body="$(tr -d '\r\n' < "$LOG_DIR/s1_healthz.txt")"
    if [ "$h_code" != "200" ] || [ "$h_body" != "ok" ]; then
        echo "/healthz returned $h_code ($h_body)" >&2
        return 1
    fi
    i_code="$(curl -sS --max-time "$TIMEOUT_SEC" -o "$LOG_DIR/s1_hub.json" -w '%{http_code}' "$HUB_URL/v1/hub")" || return 1
    i_body="$(cat "$LOG_DIR/s1_hub.json")"
    if [ "$i_code" != "200" ]; then
        echo "/v1/hub returned $i_code ($i_body)" >&2
        return 1
    fi
    printf '%s' "$i_body" | grep -q '"hub_id"' || { echo "missing hub_id: $i_body" >&2; return 1; }
    printf '%s' "$i_body" | grep -q '"hub_version"' || { echo "missing hub_version: $i_body" >&2; return 1; }
    printf '%s' "$i_body" | grep -q '"protocol_version"' || { echo "missing protocol_version: $i_body" >&2; return 1; }
    echo "healthz=ok, /v1/hub=$i_body"
}

step_2() {
    local out
    out="$(grouper_cli chat --standard "E2E_STD_PING" | tr -d '\r')" || { echo "grouper chat --standard failed" >&2; return 1; }
    printf '%s\n' "$out" | grep -q 'E2E_STD_PONG' || { echo "missing E2E_STD_PONG in output" >&2; return 1; }
    STD_CONV_ID="$(printf '%s\n' "$out" | sed -nE 's/^conversation[[:space:]]+([^[:space:]]+).*/\1/p' | head -n 1)"
    [ -n "$STD_CONV_ID" ] || { echo "could not parse conversation id" >&2; return 1; }
    echo "conv=$STD_CONV_ID"
}

step_3() {
    local chat_out="$LOG_DIR/step3_chat_out.log"
    local chat_err="$LOG_DIR/step3_chat_err.log"
    "$CLI_BIN" --hub "$HUB_URL" --token "$ADMIN_TOKEN" --cache "$CACHE_FILE_NATIVE" \
        chat "TRIGGER_ONLINE_TASK" >"$chat_out" 2>"$chat_err" &
    local chat_pid=$!
    MANAGED_PIDS+=("$chat_pid")

    local approval_id=""
    local deadline=$(( $(date +%s) + TIMEOUT_SEC ))
    while [ "$(date +%s)" -le "$deadline" ]; do
        approval_id="$(tr -d '\r' < "$chat_out" | sed -nE 's/.*run `grouper approve ([A-Za-z0-9_-]+)`.*/\1/p' | head -n 1)"
        if [ -n "$approval_id" ]; then
            break
        fi
        if ! kill -0 "$chat_pid" 2>/dev/null; then
            echo "grouper chat exited before approval: $(cat "$chat_out" "$chat_err")" >&2
            return 1
        fi
        sleep 0.25
    done
    if [ -z "$approval_id" ]; then
        stop_pid "$chat_pid"
        echo "timed out waiting for approval prompt" >&2
        return 1
    fi

    grep -q 'write_file' "$chat_out" || { stop_pid "$chat_pid"; echo "write_file did not run before approval" >&2; return 1; }
    grep -qE 'approval [A-Za-z0-9_-]+ on .*: run_shell' "$chat_out" || { stop_pid "$chat_pid"; echo "approval was not for run_shell" >&2; return 1; }

    grouper_cli approve "$approval_id" >/dev/null || { stop_pid "$chat_pid"; echo "grouper approve $approval_id failed" >&2; return 1; }

    deadline=$(( $(date +%s) + TIMEOUT_SEC ))
    while kill -0 "$chat_pid" 2>/dev/null; do
        if [ "$(date +%s)" -gt "$deadline" ]; then
            stop_pid "$chat_pid"
            echo "timed out waiting for grouper chat to finish after approval" >&2
            return 1
        fi
        sleep 0.25
    done
    wait "$chat_pid"
    local rc=$?
    if [ "$rc" -ne 0 ]; then
        echo "grouper chat exited $rc: $(cat "$chat_out" "$chat_err")" >&2
        return 1
    fi

    local final_out
    final_out="$(tr -d '\r' < "$chat_out")"
    PROJ_CONV_ID="$(printf '%s\n' "$final_out" | sed -nE 's/^conversation[[:space:]]+([^[:space:]]+).*/\1/p' | head -n 1)"
    ONLINE_THREAD_ID="$(printf '%s\n' "$final_out" | sed -nE 's/.*thread[[:space:]]+(t[A-Za-z0-9_-]+)[[:space:]]+"Online Smoke Task".*/\1/p' | head -n 1)"
    if [ -z "$PROJ_CONV_ID" ] || [ -z "$ONLINE_THREAD_ID" ]; then
        echo "failed to parse conv/thread ids" >&2
        return 1
    fi
    printf '%s\n' "$final_out" | grep -q "thread ${ONLINE_THREAD_ID} done:" || { echo "thread did not finish done" >&2; return 1; }
    printf '%s\n' "$final_out" | grep -q 'ORCH_ONLINE_FINAL' || { echo "missing orchestrator final reply" >&2; return 1; }
    echo "conv=$PROJ_CONV_ID thread=$ONLINE_THREAD_ID approved=$approval_id"
}

step_4() {
    if [ -z "$ONLINE_THREAD_ID" ]; then
        echo "prerequisite online thread id missing" >&2
        return 1
    fi
    local list_out
    list_out="$(grouper_cli files "$ONLINE_THREAD_ID" | tr -d '\r')" || return 1
    printf '%s\n' "$list_out" | grep -q 'artifact\.txt' || { echo "artifact.txt not listed: $list_out" >&2; return 1; }

    local dl_path="$TEMP_ROOT/downloaded_artifact.txt"
    local dl_native
    dl_native="$(to_native_path "$dl_path")"
    grouper_cli files "$ONLINE_THREAD_ID" artifact.txt -o "$dl_native" >/dev/null || return 1

    local exp_path="$TEMP_ROOT/expected_artifact.txt"
    printf 'grouper-e2e-artifact-content\n' > "$exp_path"
    if ! cmp -s "$exp_path" "$dl_path"; then
        echo "downloaded artifact.txt content mismatch" >&2
        return 1
    fi
    echo "listed and downloaded artifact.txt ($(wc -c < "$dl_path" | tr -d ' ') bytes)"
}

step_5() {
    if [ -z "$PROJ_CONV_ID" ] || [ -z "$ONLINE_THREAD_ID" ] || [ -z "$STD_CONV_ID" ]; then
        echo "prerequisite conversation/thread ids missing" >&2
        return 1
    fi
    local scope_out scope_conv scope_thread
    scope_out="$(grouper_cli chat "TRIGGER_SCOPE_TASK" | tr -d '\r')" || return 1
    scope_conv="$(printf '%s\n' "$scope_out" | sed -nE 's/^conversation[[:space:]]+([^[:space:]]+).*/\1/p' | head -n 1)"
    scope_thread="$(printf '%s\n' "$scope_out" | sed -nE 's/.*thread[[:space:]]+(t[A-Za-z0-9_-]+)[[:space:]]+"Scope Check Task".*/\1/p' | head -n 1)"
    [ -n "$scope_conv" ] && [ -n "$scope_thread" ] || { echo "missing scope conv/thread" >&2; return 1; }

    local proj_t scope_t std_t proj_n scope_n
    proj_t="$(grouper_cli threads -c "$PROJ_CONV_ID" | tr -d '\r')" || return 1
    proj_n="$(printf '%s\n' "$proj_t" | awk 'NF{c++} END{print c+0}')"
    if [ "$proj_n" -ne 1 ] || ! printf '%s\n' "$proj_t" | grep -q "$ONLINE_THREAD_ID" || printf '%s\n' "$proj_t" | grep -q "$scope_thread"; then
        echo "proj scoping failed: $proj_t" >&2
        return 1
    fi

    scope_t="$(grouper_cli threads -c "$scope_conv" | tr -d '\r')" || return 1
    scope_n="$(printf '%s\n' "$scope_t" | awk 'NF{c++} END{print c+0}')"
    if [ "$scope_n" -ne 1 ] || ! printf '%s\n' "$scope_t" | grep -q "$scope_thread" || printf '%s\n' "$scope_t" | grep -q "$ONLINE_THREAD_ID"; then
        echo "scope conv scoping failed: $scope_t" >&2
        return 1
    fi

    std_t="$(grouper_cli threads -c "$STD_CONV_ID" | tr -d '\r' | awk 'NF{c++} END{print c+0}')" || return 1
    if [ "$std_t" -ne 0 ]; then
        echo "expected 0 threads in standard conv, got $std_t" >&2
        return 1
    fi
    echo "proj=1 ($ONLINE_THREAD_ID), scope=1 ($scope_thread), std=0"
}

step_6() {
    local out calls
    out="$(grouper_cli usage --by model | tr -d '\r')" || return 1
    calls="$(printf '%s\n' "$out" | awk '$1=="e2e-fake-1" {print $2; exit}')"
    if [ -z "$calls" ] || ! [ "$calls" -gt 0 ] 2>/dev/null; then
        echo "expected e2e-fake-1 calls > 0, got: $out" >&2
        return 1
    fi
    echo "e2e-fake-1 calls=$calls"
}

step_7() {
    local needle="cobalt-orchid-42"
    local add_out list_out
    add_out="$(grouper_cli memory add "E2E smoke fact: $needle" --tags e2e,smoke | tr -d '\r')" || return 1
    printf '%s\n' "$add_out" | grep -qE 'saved #[0-9]+' || { echo "unexpected memory add output: $add_out" >&2; return 1; }
    list_out="$(grouper_cli memory list --search "$needle" | tr -d '\r')" || return 1
    printf '%s\n' "$list_out" | grep -q "$needle" || { echo "memory search missed $needle: $list_out" >&2; return 1; }
    echo "added and found '$needle'"
}

step_8() {
    local hdr="$LOG_DIR/s8_root_hdr.txt"
    local body="$LOG_DIR/s8_root_body.html"
    local code
    code="$(curl -sS --max-time "$TIMEOUT_SEC" -D "$hdr" -o "$body" -w '%{http_code}' "$HUB_URL/")" || return 1
    [ "$code" = "200" ] || { echo "GET / returned $code" >&2; return 1; }
    grep -iqE '<!doctype html|<html' "$body" || { echo "GET / did not return HTML" >&2; return 1; }
    grep -iq '^content-security-policy:.*default-src' "$hdr" || { echo "GET / missing CSP header" >&2; return 1; }

    local login_hdr="$LOG_DIR/s8_login_hdr.txt"
    local login_body="$LOG_DIR/s8_login_body.json"
    code="$(curl -sS --max-time "$TIMEOUT_SEC" -X POST \
        -H "Content-Type: application/json" \
        -H "X-Grouper: 1" \
        -H "Origin: $HUB_URL" \
        -d "{\"token\":\"$ADMIN_TOKEN\"}" \
        -D "$login_hdr" -o "$login_body" -w '%{http_code}' \
        "$HUB_URL/v1/session")" || return 1
    [ "$code" = "200" ] || { echo "POST /v1/session returned $code: $(cat "$login_body")" >&2; return 1; }

    local cookie_pair
    cookie_pair="$(tr -d '\r' < "$login_hdr" | grep -i '^set-cookie:' | sed -nE 's/^[^:]*:[[:space:]]*(grouper_session=[^;]+).*/\1/p' | head -n 1)"
    [ -n "$cookie_pair" ] || { echo "Set-Cookie missing grouper_session" >&2; return 1; }

    local conv_body="$LOG_DIR/s8_convs.json"
    code="$(curl -sS --max-time "$TIMEOUT_SEC" \
        -H "Cookie: $cookie_pair" \
        -o "$conv_body" -w '%{http_code}' \
        "$HUB_URL/v1/conversations")" || return 1
    [ "$code" = "200" ] || { echo "Cookie GET /v1/conversations returned $code" >&2; return 1; }
    if [ -n "$PROJ_CONV_ID" ]; then
        grep -q "$PROJ_CONV_ID" "$conv_body" || { echo "conversations missing $PROJ_CONV_ID" >&2; return 1; }
    fi

    local csrf_body="$LOG_DIR/s8_csrf.json"
    code="$(curl -sS --max-time "$TIMEOUT_SEC" -X POST \
        -H "Content-Type: application/json" \
        -H "Cookie: $cookie_pair" \
        -H "X-Grouper: 1" \
        -H "Origin: https://evil.example" \
        -d '{"content":"csrf-probe"}' \
        -o "$csrf_body" -w '%{http_code}' \
        "$HUB_URL/v1/memories")" || return 1
    [ "$code" = "403" ] || { echo "expected 403 for foreign Origin POST, got $code: $(cat "$csrf_body")" >&2; return 1; }

    echo "GET / HTML+CSP ok, session cookie works, foreign Origin POST=403"
}

step_9() {
    stop_pid "$DEVICE_PID"

    device_offline() {
        local devs
        devs="$(grouper_cli devices 2>/dev/null | tr -d '\r')"
        printf '%s\n' "$devs" | grep -qE "${DEVICE_ID}.*offline"
    }
    wait_until "Smoke Device offline" "$TIMEOUT_SEC" device_offline || return 1

    local chat_out offline_conv offline_thread=""
    chat_out="$(grouper_cli chat "TRIGGER_OFFLINE_TASK" | tr -d '\r')" || return 1
    offline_conv="$(printf '%s\n' "$chat_out" | sed -nE 's/^conversation[[:space:]]+([^[:space:]]+).*/\1/p' | head -n 1)"
    [ -n "$offline_conv" ] || { echo "missing offline conv id" >&2; return 1; }

    local deadline=$(( $(date +%s) + TIMEOUT_SEC ))
    while [ "$(date +%s)" -le "$deadline" ]; do
        offline_thread="$(grouper_cli threads -c "$offline_conv" 2>/dev/null | tr -d '\r' | awk '$2=="waiting_device"{print $1; exit}')"
        [ -n "$offline_thread" ] && break
        sleep 0.25
    done
    [ -n "$offline_thread" ] || { echo "thread never entered waiting_device" >&2; return 1; }

    checkpoint_ready() {
        grouper_cli log "$offline_conv" 2>/dev/null | tr -d '\r' | grep -q 'hub checkpoint'
    }
    wait_until "hub checkpoint event" "$TIMEOUT_SEC" checkpoint_ready || return 1

    "$CLI_BIN" --hub "$HUB_URL" --token "$DEVICE_TOKEN" --cache "$CACHE_FILE_NATIVE" \
        connect --workspace "$WORKSPACE_DIR_NATIVE" --mcp-config "$MCP_CONFIG_NATIVE" \
        >"$LOG_DIR/device_2_out.log" 2>"$LOG_DIR/device_2_err.log" &
    DEVICE_PID=$!
    MANAGED_PIDS+=("$DEVICE_PID")

    offline_finished() {
        local t l
        t="$(grouper_cli threads -c "$offline_conv" 2>/dev/null | tr -d '\r')"
        printf '%s\n' "$t" | grep -qE "^${offline_thread}[[:space:]]+done\b" || return 1
        l="$(grouper_cli log "$offline_conv" 2>/dev/null | tr -d '\r')"
        printf '%s\n' "$l" | grep -q 'handed over to' && printf '%s\n' "$l" | grep -q 'ORCH_OFFLINE_FINAL'
    }
    wait_until "offline thread handover and completion" "$TIMEOUT_SEC" offline_finished || return 1
    echo "conv=$offline_conv thread=$offline_thread waiting_device->handed_over->done"
}

step_10() {
    local orig_list restored_list
    orig_list="$(run_with_timeout "$TIMEOUT_SEC" "$HUB_BIN" --data "$DATA_DIR_NATIVE" device list | tr -d '\r' | awk 'NF' | sort)" || return 1

    run_with_timeout "$TIMEOUT_SEC" "$HUB_BIN" --data "$DATA_DIR_NATIVE" backup "$BACKUP_ARCHIVE_NATIVE" >/dev/null || return 1
    [ -s "$BACKUP_ARCHIVE" ] || { echo "backup archive missing or empty" >&2; return 1; }

    run_with_timeout "$TIMEOUT_SEC" "$HUB_BIN" --data "$RESTORED_DIR_NATIVE" restore "$BACKUP_ARCHIVE_NATIVE" >/dev/null || return 1

    restored_list="$(run_with_timeout "$TIMEOUT_SEC" "$HUB_BIN" --data "$RESTORED_DIR_NATIVE" device list | tr -d '\r' | awk 'NF' | sort)" || return 1
    if [ "$orig_list" != "$restored_list" ]; then
        echo "restored device list differs from original" >&2
        return 1
    fi
    printf '%s\n' "$restored_list" | grep -q "$ADMIN_ID" || { echo "missing admin id $ADMIN_ID" >&2; return 1; }
    printf '%s\n' "$restored_list" | grep -q "$DEVICE_ID" || { echo "missing device id $DEVICE_ID" >&2; return 1; }
    echo "backup+restore verified ($ADMIN_ID, $DEVICE_ID)"
}

if setup_environment; then
    run_step 1  "/healthz and /v1/hub" step_1
    run_step 2  "Standard chat answers" step_2
    run_step 3  "Project chat, device thread, approval, and final reply" step_3
    run_step 4  "grouper files lists and downloads thread artifact" step_4
    run_step 5  "Threads are scoped per conversation" step_5
    run_step 6  "grouper usage --by model shows calls" step_6
    run_step 7  "Memory add and search" step_7
    run_step 8  "Web app HTML/CSP, session cookie, and CSRF guard" step_8
    run_step 9  "Offline device checkpoint, handover, and completion" step_9
    run_step 10 "Hub backup, restore, and device list parity" step_10
else
    SETUP_FAILED=1
    echo "FATAL SETUP ERROR" >&2
fi

echo ""
echo "================ E2E Smoke Test Summary ================"
PASSED=0
FAILED=0
for i in "${!STEP_NUMS[@]}"; do
    if [ "${STEP_STATUSES[$i]}" = "PASS" ]; then
        PASSED=$((PASSED + 1))
    else
        FAILED=$((FAILED + 1))
    fi
    printf "%2s. [%s] (%2ss) %s -- %s\n" \
        "${STEP_NUMS[$i]}" "${STEP_STATUSES[$i]}" "${STEP_TIMES[$i]}" "${STEP_NAMES[$i]}" "${STEP_DETAILS[$i]}"
done
echo "========================================================"
echo "Result: $PASSED passed, $FAILED failed (setup_error=$SETUP_FAILED)"

if [ "$SETUP_FAILED" -ne 0 ] || [ "$FAILED" -gt 0 ] || [ "$PASSED" -lt 10 ]; then
    exit 1
fi
exit 0

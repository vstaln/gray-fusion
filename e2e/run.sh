#!/usr/bin/env bash
set -uo pipefail

E2E="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
STUB_PY="$E2E/stub_provider.py"
GRAY_BIN="${GRAY_BIN:-$HOME/.local/bin/gray}"
PLUGIN_BIN="${PLUGIN_BIN:-}"
CASES="${CASES:-all}"
FUSION_TMO="${GRAY_FUSION_HANDOFF_TIMEOUT:-}"
ROOT="${E2E_ROOT:-$E2E/tmp/run-$(date +%Y%m%d-%H%M%S)}"
mkdir -p "$ROOT"
RESULTS="$ROOT/results.txt"
: >"$RESULTS"
T0=$(date +%s)
NEW_MODE=0
[ -n "$PLUGIN_BIN" ] && NEW_MODE=1
if [ "$NEW_MODE" = 1 ] && [ -z "$FUSION_TMO" ]; then
    FUSION_TMO=20
fi

echo "e2e root: $ROOT"
echo "gray bin: $GRAY_BIN"
[ "$NEW_MODE" = 1 ] && echo "plugin bin: $PLUGIN_BIN (handoff timeout ${FUSION_TMO}s)"

report() {
    printf '%-4s  %-26s %s\n' "$1" "$2" "$3" | tee -a "$RESULTS"
}

want() {
    [ "$CASES" = all ] && return 0
    case ",$CASES," in *",$1,"*) return 0 ;; esac
    return 1
}

slug_token() {
    echo "custom:127.0.0.1:$1" | sed 's/[^A-Za-z0-9-]/-/g'
}

new_case() {
    CASE_NAME="$1"
    CASE_DIR="$ROOT/$1"
    HOME_DIR="$CASE_DIR/home"
    CWD="$CASE_DIR/cwd"
    mkdir -p "$HOME_DIR" "$CWD"
    TAG="STUBTAG-$1-$$"
    STUB_PID=""
    PORT=""
}

start_stub() {
    STUB_SCRIPT="$CASE_DIR/script.json" \
    STUB_LOG="$CASE_DIR/requests.jsonl" \
    STUB_PORT_FILE="$CASE_DIR/port" \
        python3 "$STUB_PY" --port 0 >"$CASE_DIR/stub.out" 2>"$CASE_DIR/stub.err" &
    STUB_PID=$!
    local i
    for i in $(seq 1 100); do
        [ -s "$CASE_DIR/port" ] && break
        sleep 0.1
    done
    PORT=$(cat "$CASE_DIR/port")
    echo "[$CASE_NAME] stub on 127.0.0.1:$PORT (pid $STUB_PID)"
}

stop_stub() {
    [ -n "$STUB_PID" ] && kill "$STUB_PID" 2>/dev/null
    [ -n "$STUB_PID" ] && wait "$STUB_PID" 2>/dev/null
    STUB_PID=""
}

write_home() {
    local port="$1" mode="$2"
    local base="http://127.0.0.1:$port/v1"
    local tok sidekick_part rows
    tok=$(slug_token "$port")
    if [ "$mode" = composite ]; then
        sidekick_part="stub-side"
        rows='[["stub-lead","Stub Lead"],["stub-side","Stub Side"],{"id":"fusion","name":"Fusion","declared":true,"variants":[{"id":"stub-lead","parts":{"lead":"stub-lead","sidekick":"stub-side"}}],"slots":[{"key":"lead","label":"Lead","options":[{"id":"stub-lead","name":"Stub Lead"}]},{"key":"sidekick","label":"Sidekick","options":[{"id":"stub-side","name":"Stub Side"}]}]}]'
    else
        sidekick_part="$tok:stub-side"
        rows='[["stub-lead","Stub Lead"],["stub-side","Stub Side"]]'
    fi
    local model="fusion"
    if [ "$NEW_MODE" = 1 ] && [ "$mode" = plain ]; then
        model="gray-fusion"
    fi
    cat >"$HOME_DIR/config.json" <<EOF
{"base_url":"$base","api_key":"e2e-stub-key","model":"$model","model_parts":{"lead":"stub-lead","sidekick":"$sidekick_part"},"parts_memory":{"url:$base|gray-fusion":{"lead":"stub-lead","sidekick":"$sidekick_part"}},"auto_update":false,"gw_auto":false,"cron_auto":false,"memory_auto":false,"skills_auto":false,"intro":false,"lean":true}
EOF
    cat >"$HOME_DIR/auth.json" <<EOF
{"custom:127.0.0.1:$port":"e2e-stub-key"}
EOF
    printf '{"%s":{"fetched_at":%s,"rows":%s}}\n' "$base" "$(date +%s)" "$rows" >"$HOME_DIR/provider_models.json"
    if [ "$NEW_MODE" = 1 ]; then
        env -i PATH="$PATH" HOME="$HOME" \
            GRAY_HOME="$HOME_DIR" GRAY_USER_HOME="$HOME_DIR/user" \
            "$GRAY_BIN" plugin install "$PLUGIN_BIN" --yes --force \
            >>"$CASE_DIR/install.log" 2>&1 || echo "[$CASE_NAME] plugin install failed"
    fi
}

run_lead() {
    local name="$1" prompt="$2" tmo="$3"; shift 3
    (
        cd "$CWD"
        env -i \
            PATH="$PATH" HOME="$HOME" TERM=dumb \
            GRAY_HOME="$HOME_DIR" GRAY_USER_HOME="$HOME_DIR/user" \
            GRAY_BIN="$GRAY_BIN" GRAY_NO_CACHE_WARM=1 \
            ${FUSION_TMO:+GRAY_FUSION_HANDOFF_TIMEOUT=$FUSION_TMO} \
            ${SIDE_SLEEP:+SIDE_SLEEP=$SIDE_SLEEP} \
            STUB_TAG="$TAG" \
            timeout --signal=KILL "$tmo" \
            "$GRAY_BIN" -p "$prompt" --json --max-requests 50 "$@" \
            >"$CASE_DIR/$name.jsonl" 2>"$CASE_DIR/$name.stderr"
    )
    echo "$?" >"$CASE_DIR/$name.exit"
}

lead_sid() {
    python3 - "$1" <<'PY'
import json, sys
sid = None
for line in open(sys.argv[1]):
    line = line.strip()
    if not line:
        continue
    try:
        sid = json.loads(line).get("session_id") or sid
    except Exception:
        pass
print(sid or "")
PY
}

tagged_procs() {
    local p pid cmd
    {
        for p in /proc/[0-9]*/environ; do
            if tr '\0' '\n' <"$p" 2>/dev/null | grep -qx "STUB_TAG=$1"; then
                pid="${p#/proc/}"
                pid="${pid%/environ}"
                cmd=$(tr '\0\n' '  ' <"/proc/$pid/cmdline" 2>/dev/null)
                echo "$pid $cmd"
            fi
        done
        for p in /proc/[0-9]*/cmdline; do
            cmd=$(tr '\0\n' '  ' <"$p" 2>/dev/null)
            case "$cmd" in
            *"sleep 987654"*)
                pid="${p#/proc/}"
                pid="${pid%/cmdline}"
                echo "$pid $cmd"
                ;;
            esac
        done
    } | awk '!seen[$1]++'
}

req_field() {
    python3 - "$CASE_DIR/requests.jsonl" "$1" "$2" <<'PY'
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1])]
role, idx = sys.argv[2].rsplit("[", 1)
idx = int(idx.rstrip("]"))
sel = [r for r in rows if r["role"] == role][idx]
val = sel
for part in sys.argv[3].split("."):
    if isinstance(val, list):
        val = val[int(part)]
    else:
        val = val.get(part)
print(json.dumps(val))
PY
}

case_timeoutflow() {
    new_case timeoutflow
    cat >"$CASE_DIR/script.json" <<'EOF'
{"lead":[
 {"tool_calls":[{"name":"handoff","args":{"task":"run the marker command","label":"t1"}}]},
 {"tool_calls":[{"name":"handoff","args":{"task":"follow-up brief","label":"t2"}}]},
 {"text":"LEAD-TIMEOUT-DONE"}],
 "side":[
 {"tool_calls":[{"name":"bash","args":{"command":"echo STEP-1-MARKER; sleep 987654","timeout":3600}}]},
 {"tool_calls":[{"name":"bash","args":{"command":"echo STEP-2-MARKER"}}]},
 {"text":"SIDE-2-REPORT"}]}
EOF
    start_stub
    write_home "$PORT" plain
    local lead_tmo=720
    [ "$NEW_MODE" = 1 ] && lead_tmo=$((FUSION_TMO + 90))
    if [ "$NEW_MODE" = 1 ]; then
        echo "[$CASE_NAME] lead run starts; handoff timeout is ${FUSION_TMO}s"
    else
        echo "[$CASE_NAME] lead run starts; handoff timeout is 600s, this case takes ~10min"
    fi
    run_lead lead1 "begin work" "$lead_tmo"
    local rc; rc=$(cat "$CASE_DIR/lead1.exit")
    echo "[$CASE_NAME] lead exited rc=$rc"

    sleep 2
    local procs n
    procs=$(tagged_procs "$TAG" | sort -u)
    n=$( [ -z "$procs" ] && echo 0 || echo "$procs" | wc -l )
    if [ "$n" -eq 0 ]; then
        report PASS "1-orphans" "no sidekick descendants survived the timeout"
    else
        report FAIL "1-orphans" "$n descendant(s) survived kill_tree: $(echo "$procs" | tr '\n' ';')"
        tagged_procs "$TAG" | while read -r p _; do kill -9 "$p" 2>/dev/null; done
    fi

    local side2_n side2_has_prior
    side2_n=$(python3 - "$CASE_DIR/requests.jsonl" <<'PY'
import json, sys
sides = [json.loads(l) for l in open(sys.argv[1]) if json.loads(l)["role"] == "side"]
if len(sides) < 2:
    print("MISSING")
else:
    r = sides[1]
    hist = " ".join(m["head"] for m in r["msgs"])
    prior = "yes" if (r["n_messages"] > 2 or "marker command" in hist or "STEP-1" in hist) else "no"
    print("%s prior-history:%s" % (r["n_messages"], prior))
PY
)
    case "$side2_n" in
    MISSING)
        report FAIL "2-resume-after-timeout" "no second sidekick request observed" ;;
    *"prior-history:yes"*)
        report PASS "2-resume-after-timeout" "second sidekick run resumed history ($side2_n)" ;;
    *)
        report FAIL "2-resume-after-timeout" "second sidekick run started fresh: $side2_n" ;;
    esac
    if [ "$NEW_MODE" = 1 ]; then
        find "$HOME_DIR/fusion" -name 'sidekick-*.session' 2>/dev/null | sed 's/^/  session file: /'
    fi
    grep -h "agent run start" "$HOME_DIR/logs/gray.log" 2>/dev/null | tail -5 | sed 's/^/  log: /'

    local tr
    tr=$(python3 - "$CASE_DIR/requests.jsonl" <<'PY'
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1])]
leads = [r for r in rows if r["role"] == "lead"]
if len(leads) < 2:
    print("MISSING")
else:
    res = [m["head"] for m in leads[1]["msgs"] if m["role"] == "tool"]
    print(" || ".join(res) if res else "NO-TOOL-RESULT")
PY
)
    case "$tr" in
    *STEP-1*)
        report PASS "3-partial-report" "handoff result carried sidekick progress: $tr" ;;
    *)
        report FAIL "3-partial-report" "handoff result after timeout was: ${tr:0:160}" ;;
    esac
    stop_stub
}

case_usage() {
    new_case usage
    cat >"$CASE_DIR/script.json" <<'EOF'
{"lead":[
 {"tool_calls":[{"name":"handoff","args":{"task":"say hi","label":"u1"}}]},
 {"text":"LEAD-USAGE-DONE"}],
 "side":[{"text":"SIDE-USAGE-REPORT"}]}
EOF
    start_stub
    write_home "$PORT" plain
    run_lead lead1 "begin work" 240
    local rc; rc=$(cat "$CASE_DIR/lead1.exit")
    local sid; sid=$(lead_sid "$CASE_DIR/lead1.jsonl")
    echo "[$CASE_NAME] lead rc=$rc sid=$sid"

    local sums; sums=$(python3 - "$CASE_DIR" "$sid" <<'PY'
import json, re, sys, os
case_dir, sid = sys.argv[1], sys.argv[2]
log = open(os.path.join(case_dir, "home/logs/gray.log")).read()
pat = re.compile(r"request usage: kind=(\S+) session=(\S+) in=(\d+) cache_read=(\d+) cache_write=(\d+) out=(\d+) reasoning=(\d+)")
all_sum = [0, 0]; lead_sum = [0, 0]; dash = 0; nlead = 0
tool_sum = [0, 0]; turn_sum = [0, 0]
for m in pat.finditer(log):
    i, o = int(m.group(3)), int(m.group(6))
    kind = m.group(1)
    if kind.startswith("tool:"):
        tool_sum[0] += i; tool_sum[1] += o
    else:
        turn_sum[0] += i; turn_sum[1] += o
    all_sum[0] += i; all_sum[1] += o
    if m.group(2) == "-": dash += 1
    if m.group(2) == sid and kind == "turn":
        lead_sum[0] += i; lead_sum[1] += o; nlead += 1
sdir = os.path.join(case_dir, "home/sessions")
rec = {}
p = os.path.join(sdir, sid + ".jsonl")
if os.path.exists(p):
    for line in open(p):
        try: e = json.loads(line)
        except Exception: continue
        if e.get("usage"): rec = e["usage"]
print(json.dumps({"ri": rec.get("input_tokens"), "ro": rec.get("output_tokens"),
                  "li": lead_sum[0], "lo": lead_sum[1],
                  "ai": all_sum[0], "ao": all_sum[1],
                  "ti": tool_sum[0], "to": tool_sum[1],
                  "ui": turn_sum[0], "uo": turn_sum[1],
                  "dash": dash, "nlead": nlead}))
PY
)
    echo "[$CASE_NAME] sums: $sums"
    local ri ro li lo ai ao dash nlead
    ri=$(echo "$sums" | python3 -c 'import json,sys;print(json.load(sys.stdin)["ri"])')
    ro=$(echo "$sums" | python3 -c 'import json,sys;print(json.load(sys.stdin)["ro"])')
    li=$(echo "$sums" | python3 -c 'import json,sys;print(json.load(sys.stdin)["li"])')
    lo=$(echo "$sums" | python3 -c 'import json,sys;print(json.load(sys.stdin)["lo"])')
    ai=$(echo "$sums" | python3 -c 'import json,sys;print(json.load(sys.stdin)["ai"])')
    ao=$(echo "$sums" | python3 -c 'import json,sys;print(json.load(sys.stdin)["ao"])')
    dash=$(echo "$sums" | python3 -c 'import json,sys;print(json.load(sys.stdin)["dash"])')
    nlead=$(echo "$sums" | python3 -c 'import json,sys;print(json.load(sys.stdin)["nlead"])')
    local ti to ui uo
    ti=$(echo "$sums" | python3 -c 'import json,sys;print(json.load(sys.stdin)["ti"])')
    to=$(echo "$sums" | python3 -c 'import json,sys;print(json.load(sys.stdin)["to"])')
    ui=$(echo "$sums" | python3 -c 'import json,sys;print(json.load(sys.stdin)["ui"])')
    uo=$(echo "$sums" | python3 -c 'import json,sys;print(json.load(sys.stdin)["uo"])')
    if [ "$ri" = "$ui" ] && [ "$ro" = "$uo" ]; then
        report PASS "5-usage" "session record in=$ri out=$ro == lead+sidekick turn sum (in=$ui out=$uo)"
    else
        report FAIL "5-usage" "session record in=$ri out=$ro; turn sum in=$ui out=$uo; all-lines in=$ai out=$ao"
    fi
    if [ "$NEW_MODE" = 1 ]; then
        if [ "$ti" -gt 0 ] && [ "$ti" = "$((ui - li))" ]; then
            report PASS "5b-usage-tool-line" "kind=tool:handoff in=$ti out=$to == sidekick sum"
        else
            report FAIL "5b-usage-tool-line" "kind=tool:* sum in=$ti out=$to; expected sidekick in=$((ui-li)) out=$((uo-lo))"
        fi
    fi
    if [ "$dash" -gt 0 ]; then
        report FAIL "6a-session-id-print" "$dash request-usage line(s) with session=- (lead sid $sid seen on $nlead lines)"
    elif [ "$nlead" -gt 0 ]; then
        report PASS "6a-session-id-print" "-p path logs session=$sid on $nlead lines (drop is cron-fired side, see 6b)"
    else
        report FAIL "6a-session-id-print" "no lead request-usage lines found at all (sid $sid)"
    fi
    stop_stub
}

case_perlead() {
    new_case perlead
    cat >"$CASE_DIR/script.json" <<'EOF'
{"lead":[
 {"tool_calls":[{"name":"handoff","args":{"task":"brief ALPHA","label":"A"}}]},
 {"text":"LEAD-A-DONE"},
 {"tool_calls":[{"name":"handoff","args":{"task":"brief BRAVO","label":"B"}}]},
 {"text":"LEAD-B-DONE"}],
 "side":[
 {"tool_calls":[{"name":"bash","args":{"command":"echo SIDE-A-STEP"}}]},
 {"text":"SIDE-A-REPORT"},
 {"tool_calls":[{"name":"bash","args":{"command":"echo SIDE-B-STEP"}}]},
 {"text":"SIDE-B-REPORT"}]}
EOF
    start_stub
    write_home "$PORT" plain
    run_lead leadA "begin work A" 240
    run_lead leadB "begin work B" 240
    local sidA sidB
    sidA=$(lead_sid "$CASE_DIR/leadA.jsonl")
    sidB=$(lead_sid "$CASE_DIR/leadB.jsonl")
    echo "[$CASE_NAME] leadA sid=$sidA leadB sid=$sidB"
    ls "$HOME_DIR/fusion/" 2>/dev/null | sed 's/^/  fusion file: /'
    local info
    info=$(python3 - "$CASE_DIR/requests.jsonl" <<'PY'
import json, sys
sides = [json.loads(l) for l in open(sys.argv[1]) if json.loads(l)["role"] == "side"]
if len(sides) < 3:
    print("MISSING (%d side requests)" % len(sides))
else:
    r = sides[2]
    hist = " ".join(m["head"] for m in r["msgs"])
    prior = "yes" if ("ALPHA" in hist or "SIDE-A" in hist) else "no"
    print("n_messages=%s prior-history:%s" % (r["n_messages"], prior))
PY
)
    case "$info" in
    *"prior-history:yes"*)
        report FAIL "4-per-lead-session" "lead B's handoff resumed lead A's sidekick ($info)" ;;
    MISSING*)
        report FAIL "4-per-lead-session" "second sidekick run never happened: $info" ;;
    *)
        report PASS "4-per-lead-session" "lead B got a fresh sidekick ($info)" ;;
    esac
    if [ "$NEW_MODE" = 1 ]; then
        local dirs
        dirs=$(find "$HOME_DIR/fusion" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l)
        find "$HOME_DIR/fusion" -name 'sidekick-*.session' 2>/dev/null | sed 's/^/  session file: /'
        if [ "$dirs" -ge 2 ]; then
            report PASS "4b-per-lead-dirs" "$dirs per-lead sidekick session dirs under fusion/"
        else
            report FAIL "4b-per-lead-dirs" "expected 2 per-lead dirs under fusion/, found $dirs"
        fi
    fi
    stop_stub
}

case_doublefusion() {
    new_case doublefusion
    cat >"$CASE_DIR/script.json" <<'EOF'
{"lead":[{"text":"LEAD-NO-TOOLS-DONE"}],"side":[]}
EOF
    start_stub
    write_home "$PORT" composite
    run_lead lead1 "begin work" 240
    local tools
    tools=$(req_field 'lead[0]' tools)
    echo "[$CASE_NAME] lead request tools: $tools"
    case "$tools" in
    *handoff*)
        report FAIL "7-no-double-fusion" "handoff tool added although provider list already has composite row: $tools" ;;
    *)
        report PASS "7-no-double-fusion" "no handoff tool on provider-owned composite row" ;;
    esac
    if [ "$NEW_MODE" = 1 ]; then
        local rowcheck
        rowcheck=$(python3 - "$HOME_DIR/provider_models.json" <<'PY'
import json, sys
d = json.load(open(sys.argv[1]))
has_full = False
for k, v in d.items():
    rows = v.get("rows") if isinstance(v, dict) else v
    for r in rows or []:
        if isinstance(r, dict) and r.get("slots"):
            has_full = True
print("yes" if has_full else "no")
PY
)
        if [ "$rowcheck" = yes ]; then
            report PASS "7b-no-synth" "list already has a composite row; host synthesis must skip it"
        else
            report FAIL "7b-no-synth" "provider_models.json lost its composite row"
        fi
    fi
    stop_stub
}

case_cronsession() {
    new_case cronsession
    cat >"$CASE_DIR/script.json" <<'EOF'
{"lead":[{"text":"CRON-TURN-OK"},{"text":"REPL-TURN-OK"},{"text":"REPL-TURN-OK-2"},{"text":"REPL-TURN-OK-3"}],"side":[]}
EOF
    start_stub
    write_home "$PORT" plain
    python3 - "$HOME_DIR/config.json" <<'PY'
import json, sys
p = sys.argv[1]
c = json.load(open(p))
c["model"] = "stub-lead"
c.pop("model_parts", None)
c.pop("cron_auto", None)
json.dump(c, open(p, "w"))
PY
    mkdir -p "$HOME_DIR/cron"
    python3 - "$HOME_DIR/cron/jobs.json" "$CWD" <<'PY'
import json, sys, time
now = int(time.time())
job = {"id": "aaaa1111bbbb", "name": "e2e-fire", "prompt": "say CRON-FIRED",
       "schedule": {"Once": {"at": now - 10}}, "enabled": True, "state": "active",
       "created_at": now - 60, "next_run_at": now - 5, "deliver": "local",
       "workdir": sys.argv[2], "skills": [], "repeat": {}}
json.dump([job], open(sys.argv[1], "w"))
PY
    (
        cd "$CWD"
        ( sleep 12; printf 'manual prompt\r'; sleep 8 ) | \
        env -i PATH="$PATH" HOME="$HOME" TERM=xterm \
            GRAY_HOME="$HOME_DIR" GRAY_USER_HOME="$HOME_DIR/user" \
            GRAY_NO_CACHE_WARM=1 \
            timeout --signal=KILL 30 script -qec "$GRAY_BIN" /dev/null \
            >"$CASE_DIR/repl.out" 2>"$CASE_DIR/repl.stderr"
    )
    local dash named
    dash=$(grep -c "request usage: kind=turn session=- " "$HOME_DIR/logs/gray.log" 2>/dev/null)
    named=$(grep -o "request usage: kind=turn session=[a-z][a-z-]* " "$HOME_DIR/logs/gray.log" 2>/dev/null | sort -u | tr '\n' ' ')
    if [ "${dash:-0}" -gt 0 ]; then
        report FAIL "6b-session-id-cronfire" "cron-fired turn logged session=- x$dash (interactive turns logged: $named)"
    else
        report PASS "6b-session-id-cronfire" "all request-usage lines carry a session id ($named)"
    fi
    stop_stub
}

case_modelswitch() {
    new_case modelswitch
    cat >"$CASE_DIR/script.json" <<'EOF'
{"lead":[{"text":"PRE-SWITCH-OK"},{"tool_calls":[{"name":"handoff","args":{"task":"say hi","label":"s1"}}]},{"text":"POST-SWITCH-DONE"},{"text":"POST-SWITCH-DONE2"}],"side":[{"text":"SIDE-SWITCH-REPORT"}]}
EOF
    start_stub
    write_home "$PORT" plain
    python3 - "$HOME_DIR/config.json" <<'PY'
import json, sys
p = sys.argv[1]
c = json.load(open(p))
c["model"] = "stub-lead"
c.pop("model_parts", None)
json.dump(c, open(p, "w"))
PY
    (
        cd "$CWD"
        ( sleep 12; printf 'first prompt\r'; sleep 6; printf '/models gray-fusion\r'; sleep 8; printf 'second prompt\r'; sleep 15 ) | \
        env -i PATH="$PATH" HOME="$HOME" TERM=xterm \
            GRAY_HOME="$HOME_DIR" GRAY_USER_HOME="$HOME_DIR/user" \
            GRAY_BIN="$GRAY_BIN" GRAY_NO_CACHE_WARM=1 \
            ${FUSION_TMO:+GRAY_FUSION_HANDOFF_TIMEOUT=$FUSION_TMO} \
            timeout --signal=KILL 60 script -qec "stty rows 40 cols 120; exec $GRAY_BIN" /dev/null \
            >"$CASE_DIR/repl.out" 2>"$CASE_DIR/repl.stderr"
    )
    local t1 t2
    t1=$(req_field 'lead[0]' tools 2>/dev/null)
    t2=$(req_field 'lead[-1]' tools 2>/dev/null)
    echo "[$CASE_NAME] pre-switch tools: $t1"
    echo "[$CASE_NAME] post-switch tools: $t2"
    case "$t1" in
    *handoff*) report FAIL "8-model-switch" "handoff present BEFORE the switch: $t1" ;;
    esac
    case "$t2" in
    *handoff*)
        report PASS "8-model-switch" "after /model gray-fusion the request carries handoff: $t2" ;;
    *)
        report FAIL "8-model-switch" "post-switch request lacks handoff: ${t2:-missing}" ;;
    esac
    stop_stub
}

case_background() {
    new_case background
    cat >"$CASE_DIR/script.json" <<'EOF'
{"lead":[
 {"tool_calls":[{"name":"handoff","args":{"task":"background job","label":"bg1","block":false}}]},
 {"tool_calls":[{"name":"handoff_wait","args":{"seconds":60}}]},
 {"text":"LEAD-BG-DONE"}],
 "side":[{"text":"SIDE-BG-REPORT"}]}
EOF
    start_stub
    write_home "$PORT" plain
    run_lead lead1 "begin work" 240
    local tr
    tr=$(python3 - "$CASE_DIR/requests.jsonl" <<'PY'
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1])]
leads = [r for r in rows if r["role"] == "lead"]
outs = []
for r in leads[1:]:
    for m in r["msgs"]:
        if m["role"] == "tool" and "running in background" not in m["head"]:
            outs.append(m["head"])
print(" || ".join(outs) if outs else "NO-TOOL-RESULTS")
PY
)
    echo "[$CASE_NAME] tool results seen by lead: ${tr:0:200}"
    case "$tr" in
    *SIDE-BG-REPORT*)
        report PASS "9-background" "handoff_wait returned the background report: ${tr:0:160}" ;;
    *)
        if [ "$NEW_MODE" = 1 ]; then
            report FAIL "9-background" "expected background start + collected report, got: ${tr:0:160}"
        else
            report FAIL "9-background" "old binary has no block/handoff_wait: ${tr:0:160}"
        fi ;;
    esac
    stop_stub
}

case_firstturn() {
    new_case firstturn
    cat >"$CASE_DIR/script.json" <<'EOF'
{"lead":[{"text":"FIRST-TURN-OK"},{"text":"FIRST-TURN-OK-2"}],"side":[]}
EOF
    start_stub
    write_home "$PORT" plain
    python3 - "$HOME_DIR/config.json" <<'PY'
import json, sys
p = sys.argv[1]
c = json.load(open(p))
c["model"] = "stub-lead"
c.pop("model_parts", None)
json.dump(c, open(p, "w"))
PY
    (
        cd "$CWD"
        ( sleep 12; printf 'hello first turn\r'; sleep 10 ) | \
        env -i PATH="$PATH" HOME="$HOME" TERM=xterm \
            GRAY_HOME="$HOME_DIR" GRAY_USER_HOME="$HOME_DIR/user" \
            GRAY_BIN="$GRAY_BIN" GRAY_NO_CACHE_WARM=1 \
            timeout --signal=KILL 40 script -qec "stty rows 40 cols 120; exec $GRAY_BIN" /dev/null \
            >"$CASE_DIR/repl.out" 2>"$CASE_DIR/repl.stderr"
    )
    local dash named
    dash=$(grep -c "request usage: kind=turn session=- " "$HOME_DIR/logs/gray.log" 2>/dev/null)
    named=$(grep -o "request usage: kind=turn session=[a-z][a-z-]*" "$HOME_DIR/logs/gray.log" 2>/dev/null | sort -u | tr '\n' ' ')
    if [ -n "$named" ] && [ "${dash:-0}" -eq 0 ]; then
        report PASS "6c-first-turn-sid" "first interactive turn logged real session id: $named"
    else
        report FAIL "6c-first-turn-sid" "named=$named dash=${dash:-0}"
    fi
    stop_stub
}

case_longhandoff() {
    new_case longhandoff
    cat >"$CASE_DIR/script.json" <<'EOF'
{"lead":[
 {"tool_calls":[{"name":"handoff","args":{"task":"slow job","label":"L"}}]},
 {"text":"LEAD-LONG-DONE"}],
 "side":[
 {"tool_calls":[{"name":"bash","args":{"command":"sleep ${SIDE_SLEEP:-45}","timeout":3600}}]},
 {"text":"SIDE-LONG-FINAL-REPORT"}]}
EOF
    start_stub
    write_home "$PORT" plain
    local saved_tmo="$FUSION_TMO"
    FUSION_TMO=${LONGHANDOFF_TMO:-120}
    run_lead lead1 "begin work" $(( ${LONGHANDOFF_TMO:-120} + 120 ))
    FUSION_TMO="$saved_tmo"
    local tr
    tr=$(python3 - "$CASE_DIR/requests.jsonl" <<'PY'
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1])]
leads = [r for r in rows if r["role"] == "lead"]
outs = [m["head"] for r in leads[1:] for m in r["msgs"] if m["role"] == "tool"]
print(" || ".join(outs) if outs else "NO-TOOL-RESULTS")
PY
)
    if [ "$NEW_MODE" = 1 ]; then
        case "$tr" in
        *SIDE-LONG-FINAL-REPORT*)
            report PASS "10-long-handoff" "blocking handoff >30s returned the sidekick report: ${tr:0:120}" ;;
        *)
            report FAIL "10-long-handoff" "expected SIDE-LONG-FINAL-REPORT, got: ${tr:0:160}" ;;
        esac
    else
        case "$tr" in
        *SIDE-LONG-FINAL-REPORT*)
            report FAIL "10-long-handoff" "old binary returned report over 30s cap (unexpected): ${tr:0:120}" ;;
        *timeout*|*timed\ out*)
            report PASS "10-long-handoff" "old binary capped the tool call at ~30s: ${tr:0:120}" ;;
        *)
            report FAIL "10-long-handoff" "old binary: ${tr:0:160}" ;;
        esac
    fi
    stop_stub
}

case_cancel() {
    new_case cancel
    cat >"$CASE_DIR/script.json" <<'EOF'
{"lead":[{"tool_calls":[{"name":"handoff","args":{"task":"long job","label":"c1"}}]}],
 "side":[{"tool_calls":[{"name":"bash","args":{"command":"echo CANCEL-MARK; sleep 987654","timeout":3600}}]}]}
EOF
    start_stub
    write_home "$PORT" plain
    (
        cd "$CWD"
        env -i PATH="$PATH" HOME="$HOME" TERM=dumb \
            GRAY_HOME="$HOME_DIR" GRAY_USER_HOME="$HOME_DIR/user" \
            GRAY_BIN="$GRAY_BIN" GRAY_NO_CACHE_WARM=1 \
            ${FUSION_TMO:+GRAY_FUSION_HANDOFF_TIMEOUT=$FUSION_TMO} \
            ${SIDE_SLEEP:+SIDE_SLEEP=$SIDE_SLEEP} \
            STUB_TAG="$TAG" \
            "$GRAY_BIN" -p "begin work" --json --max-requests 50 \
            >"$CASE_DIR/lead1.jsonl" 2>"$CASE_DIR/lead1.stderr" &
        echo $! >"$CASE_DIR/lead.pid"
    )
    sleep 12
    kill -INT "$(cat "$CASE_DIR/lead.pid")" 2>/dev/null
    local i alive=""
    for i in $(seq 1 40); do
        alive=$(tagged_procs "$TAG" | sort -u)
        [ -z "$alive" ] && break
        sleep 0.5
    done
    kill -9 "$(cat "$CASE_DIR/lead.pid")" 2>/dev/null
    if [ "$NEW_MODE" = 1 ]; then
        if [ -z "$alive" ]; then
            report PASS "11-cancel" "SIGINT on the lead killed the sidekick tree within 20s (tool/cancel)"
        else
            report FAIL "11-cancel" "survivors after SIGINT: $(echo "$alive" | tr '\n' ';')"
            tagged_procs "$TAG" | while read -r p _; do kill -9 "$p" 2>/dev/null; done
        fi
    else
        if [ -n "$alive" ]; then
            report FAIL "11-cancel" "old binary left survivors (expected): $(echo "$alive" | tr '\n' ';')"
            tagged_procs "$TAG" | while read -r p _; do kill -9 "$p" 2>/dev/null; done
        else
            report FAIL "11-cancel" "old binary cleaned up after SIGINT (unexpected)"
        fi
    fi
    stop_stub
}

case_leaddeath() {
    new_case leaddeath
    cat >"$CASE_DIR/script.json" <<'EOF'
{"lead":[{"tool_calls":[{"name":"handoff","args":{"task":"long job","label":"d1"}}]}],
 "side":[{"tool_calls":[{"name":"bash","args":{"command":"echo DEATH-MARK; sleep 987654","timeout":3600}}]}]}
EOF
    start_stub
    write_home "$PORT" plain
    (
        cd "$CWD"
        env -i PATH="$PATH" HOME="$HOME" TERM=dumb \
            GRAY_HOME="$HOME_DIR" GRAY_USER_HOME="$HOME_DIR/user" \
            GRAY_BIN="$GRAY_BIN" GRAY_NO_CACHE_WARM=1 \
            ${FUSION_TMO:+GRAY_FUSION_HANDOFF_TIMEOUT=$FUSION_TMO} \
            ${SIDE_SLEEP:+SIDE_SLEEP=$SIDE_SLEEP} \
            STUB_TAG="$TAG" \
            "$GRAY_BIN" -p "begin work" --json --max-requests 50 \
            >"$CASE_DIR/lead1.jsonl" 2>"$CASE_DIR/lead1.stderr" &
        echo $! >"$CASE_DIR/lead.pid"
    )
    sleep 12
    kill -9 "$(cat "$CASE_DIR/lead.pid")" 2>/dev/null
    local i alive=""
    for i in $(seq 1 40); do
        alive=$(tagged_procs "$TAG" | sort -u)
        [ -z "$alive" ] && break
        sleep 0.5
    done
    if [ "$NEW_MODE" = 1 ]; then
        if [ -z "$alive" ]; then
            report PASS "12-lead-death" "SIGKILL on the lead left zero sidekick processes within 20s (stdin-EOF drain)"
        else
            report FAIL "12-lead-death" "survivors after lead SIGKILL: $(echo "$alive" | tr '\n' ';')"
            tagged_procs "$TAG" | while read -r p _; do kill -9 "$p" 2>/dev/null; done
        fi
    else
        if [ -n "$alive" ]; then
            report FAIL "12-lead-death" "old binary left survivors (expected): $(echo "$alive" | tr '\n' ';')"
            tagged_procs "$TAG" | while read -r p _; do kill -9 "$p" 2>/dev/null; done
        else
            report FAIL "12-lead-death" "old binary cleaned up after lead death (unexpected)"
        fi
    fi
    stop_stub
}

case_parallel() {
    new_case parallel
    cat >"$CASE_DIR/script.json" <<'EOF'
{"lead":[
 {"tool_calls":[{"name":"handoff","args":{"task":"parallel A","label":"pa"}}]},
 {"text":"LEAD-F-DONE"},
 {"text":"LEAD-P-DONE"},
 {"text":"LEAD-P-DONE2"}],
 "side":[{"text":"SIDE-PAR-REPORT"}]}
EOF
    start_stub
    write_home "$PORT" plain
    python3 - "$HOME_DIR/config.json" <<'PY'
import json, sys
p = sys.argv[1]
c = json.load(open(p))
c["model"] = "stub-lead"
c.pop("model_parts", None)
json.dump(c, open(p, "w"))
PY
    local tok; tok=$(slug_token "$PORT")
    (
        run_lead leadF "begin fusion work" 90 --model gray-fusion --model-parts "lead=stub-lead,sidekick=${tok}:stub-side" &
        LP=$!
        run_lead leadP "begin plain work" 90 --model stub-lead &
        LP2=$!
        wait $LP $LP2
    )
    local tf tp
    tf=$(python3 - "$CASE_DIR/requests.jsonl" <<'PY'
import json, sys
for r in (json.loads(l) for l in open(sys.argv[1])):
    if r["role"] == "lead" and len(r["msgs"]) > 1 and "fusion work" in r["msgs"][1]["head"]:
        print(",".join(r["tools"]))
        break
else:
    print("MISSING")
PY
)
    tp=$(python3 - "$CASE_DIR/requests.jsonl" <<'PY'
import json, sys
for r in (json.loads(l) for l in open(sys.argv[1])):
    if r["role"] == "lead" and len(r["msgs"]) > 1 and "plain work" in r["msgs"][1]["head"]:
        print(",".join(r["tools"]))
        break
else:
    print("MISSING")
PY
)
    echo "[$CASE_NAME] fusion lead tools: $tf"
    echo "[$CASE_NAME] plain lead tools: $tp"
    if [ "$NEW_MODE" = 1 ]; then
        case "$tf:$tp" in
        *handoff*:*handoff*)
            report FAIL "13-parallel-models" "plain lead also got handoff: $tp" ;;
        *handoff*:*)
            if [ "$tp" != "MISSING" ]; then
                report PASS "13-parallel-models" "fusion lead has handoff, plain lead does not ($tp)"
            else
                report FAIL "13-parallel-models" "plain lead request never seen"
            fi ;;
        *)
            report FAIL "13-parallel-models" "fusion lead lacks handoff: $tf" ;;
        esac
    else
        report FAIL "13-parallel-models" "old binary: no plugin tools at all ($tf / $tp)"
    fi
    stop_stub
}

want usage && case_usage
want cronsession && case_cronsession
want firstturn && case_firstturn
want perlead && case_perlead
want doublefusion && case_doublefusion
want modelswitch && case_modelswitch
want background && case_background
want longhandoff && case_longhandoff
want cancel && case_cancel
want leaddeath && case_leaddeath
want parallel && case_parallel
want timeoutflow && case_timeoutflow

echo
echo "== results ($ROOT/results.txt) =="
cat "$RESULTS"
echo "wall: $(( $(date +%s) - T0 ))s"

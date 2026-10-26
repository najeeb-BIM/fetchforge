#!/usr/bin/env bash
# Wrapper: starts test server + bridge (fresh /tmp), runs audit_concurrency_bridge.py.
set -u
ROOT=/tmp/ff-audit-conc
rm -rf "$ROOT"; mkdir -p "$ROOT/data" "$ROOT/downloads" "$ROOT/server"
python3 - <<'PY'
root="/tmp/ff-audit-conc/server"
open(root+"/slow.bin","wb").write(bytes(((i*13)%256) for i in range(2*1024*1024)))
PY
cd "$(dirname "$0")/../.."
python3 tests/server.py "$ROOT/server" 18831 > "$ROOT/server.log" 2>&1 &
SRV=$!
cat > "$ROOT/data/settings.json" <<EOF
{"download_dir": "$ROOT/downloads", "max_simultaneous": 3, "max_segments": 1, "max_retries": 0, "bandwidth_kbps": null, "organise_by_category": true}
EOF
./target/debug/fetchforge-bridge serve --data-dir "$ROOT/data" --port 18832 > "$ROOT/bridge.log" 2>&1 &
BPID=$!
trap 'kill $BPID $SRV 2>/dev/null' EXIT
sleep 1
TOKEN=$(cat "$ROOT/data/bridge-token.txt")
echo "BRIDGE_PID=$BPID THREADS_BEFORE=$(ls /proc/$BPID/task 2>/dev/null | wc -l) FD_BEFORE=$(ls /proc/$BPID/fd 2>/dev/null | wc -l) RSS_BEFORE=$(grep VmRSS /proc/$BPID/status 2>/dev/null)"
python3 tests/audit/audit_concurrency_bridge.py "http://127.0.0.1:18832" "$TOKEN" "$ROOT"
echo "BRIDGE_AFTER THREADS=$(ls /proc/$BPID/task 2>/dev/null | wc -l) FD=$(ls /proc/$BPID/fd 2>/dev/null | wc -l) RSS=$(grep VmRSS /proc/$BPID/status 2>/dev/null)"
sleep 2
echo "BRIDGE_IDLE_CPU check: ps -o pid,%cpu,etime,cmd -p $BPID"
ps -o pid,%cpu,etime,cmd -p $BPID 2>/dev/null || true
echo "AUDIT-CONCURRENCY-BRIDGE-SH DONE"

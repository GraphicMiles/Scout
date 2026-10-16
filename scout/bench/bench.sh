#!/usr/bin/env bash
# Reproducible benchmark for the Scout runtime.
#
#   ./bench/bench.sh [tasks_per_scenario]
#
# Measures what the brief asks for: idle and peak RSS, CPU time, cold start,
# time to first useful result, extraction accuracy and task success rate.
# Results are written to bench/results/latest.json.

set -uo pipefail
cd "$(dirname "$0")/.."

TASKS="${1:-100}"
FIXTURE_PORT=8799
SCOUT_PORT=8798
KEY=bench_key_0123456789abcdefgh
RESULTS=bench/results/latest.json
mkdir -p bench/results

cleanup() {
  [ -n "${SCOUT_PID:-}" ] && kill "$SCOUT_PID" 2>/dev/null
  [ -n "${FIXTURE_PID:-}" ] && kill "$FIXTURE_PID" 2>/dev/null
  wait 2>/dev/null
}
trap cleanup EXIT

python3 bench/fixture_server.py "$FIXTURE_PORT" &
FIXTURE_PID=$!
sleep 1

# ---------------------------------------------------------------- cold start
START_MS=$(date +%s%3N)
SCOUT_API_KEYS="$KEY" \
SCOUT_DEV_MODE=0 \
SCOUT_PRIVATE_ALLOWLIST=127.0.0.1 \
PORT="$SCOUT_PORT" \
SCOUT_LOG_LEVEL=warn \
./target/release/scout &
SCOUT_PID=$!

HEALTHY_MS=""
for _ in $(seq 1 400); do
  if curl -sf "http://127.0.0.1:$SCOUT_PORT/health" >/dev/null 2>&1; then
    HEALTHY_MS=$(( $(date +%s%3N) - START_MS ))
    break
  fi
  sleep 0.01
done

if [ -z "$HEALTHY_MS" ]; then echo "server did not become healthy" >&2; exit 1; fi

rss_kb() { grep VmRSS /proc/"$1"/status 2>/dev/null | awk '{print $2}'; }
cpu_ticks() { awk '{print $14+$15}' /proc/"$1"/stat 2>/dev/null; }

# Poll RSS in the background and record the high-water mark.
PEAK_FILE=$(mktemp)
( while kill -0 "$SCOUT_PID" 2>/dev/null; do rss_kb "$SCOUT_PID" >> "$PEAK_FILE"; sleep 0.02; done ) &
SAMPLER=$!

sleep 1
IDLE_RSS_KB=$(rss_kb "$SCOUT_PID")
CPU_BEFORE=$(cpu_ticks "$SCOUT_PID")

# ------------------------------------------------------------------ workload
run_scenario() {
  local label="$1" path="$2" n="$3"

  local session
  session=$(curl -s -X POST "http://127.0.0.1:$SCOUT_PORT/v1/sessions" \
    -H "Authorization: Bearer $KEY" -H 'content-type: application/json' -d '{}' \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')

  local ok=0 fail=0 accurate=0
  local first_ms="" t0 t1 total_ms=0
  local latencies=()

  for i in $(seq 1 "$n"); do
    t0=$(date +%s%3N)
    out=$(curl -s -X POST "http://127.0.0.1:$SCOUT_PORT/v1/extract" \
      -H "Authorization: Bearer $KEY" -H 'content-type: application/json' \
      -d "{\"sessionId\":\"$session\",\"url\":\"http://127.0.0.1:$FIXTURE_PORT$path\",\"strategy\":\"selectors\",\"schema\":{\"item\":\"li.item\",\"fields\":{\"name\":{\"selector\":\".name\"},\"price\":{\"selector\":\".price\",\"type\":\"price\"}}}}" \
      -m 60)
    t1=$(date +%s%3N)
    local ms=$(( t1 - t0 ))
    total_ms=$(( total_ms + ms ))
    latencies+=("$ms")
    [ -z "$first_ms" ] && first_ms=$ms

    # Accuracy: the first record must match the fixture exactly.
    verdict=$(printf '%s' "$out" | python3 -c '
import sys, json
try:
    d = json.load(sys.stdin)
except Exception:
    print("fail"); raise SystemExit
if "error" in d:
    print("fail"); raise SystemExit
rows = d.get("data")
if not isinstance(rows, list) or not rows:
    print("fail"); raise SystemExit
r = rows[0]
name = r.get("name"); price = r.get("price") or {}
ok_name = name == "Item 0"
ok_price = isinstance(price, dict) and price.get("amount") == 0.5 and price.get("currency") == "USD"
print("ok" if (ok_name and ok_price) else "inaccurate")
' 2>/dev/null)
    case "$verdict" in
      ok) ok=$((ok+1)); accurate=$((accurate+1));;
      inaccurate) ok=$((ok+1));;
      *) fail=$((fail+1));;
    esac
  done

  curl -s -X DELETE "http://127.0.0.1:$SCOUT_PORT/v1/sessions/$session" \
    -H "Authorization: Bearer $KEY" > /dev/null

  # Percentiles via python for clarity.
  printf '%s\n' "${latencies[@]}" | python3 -c "
import sys
xs = sorted(int(x) for x in sys.stdin if x.strip())
def p(q): return xs[min(len(xs)-1, int(len(xs)*q))]
print(f\"{'$label'} success={$ok}/$n accuracy={$accurate}/$ok first={$first_ms}ms p50={p(0.5)}ms p95={p(0.95)}ms total={$total_ms}ms\")
print(f\"$label,$ok,$fail,$accurate,$first_ms,{p(0.5)},{p(0.95)},$total_ms\")
" > /tmp/scout_bench_$label.txt
  tail -1 /tmp/scout_bench_$label.txt >> "$RESULTS.rows"
  head -1 /tmp/scout_bench_$label.txt
}

rm -f "$RESULTS.rows"
run_scenario "small" "/small" "$TASKS"
run_scenario "large" "/large" "$TASKS"

CPU_AFTER=$(cpu_ticks "$SCOUT_PID")

sleep 0.5
kill "$SAMPLER" 2>/dev/null
PEAK_RSS_KB=$(sort -n "$PEAK_FILE" 2>/dev/null | tail -1)
FINAL_RSS_KB=$(rss_kb "$SCOUT_PID")
TOTAL_CPU_S=$(python3 -c "print(round(($CPU_AFTER - $CPU_BEFORE)/100.0, 2))")
CPUS=$(nproc 2>/dev/null || echo 1)
NOW=$(date -u +%Y-%m-%dT%H:%M:%SZ)
NODE_VER=$(./target/release/scout --version 2>/dev/null || echo "0.1.0")

ROWS=$(cat "$RESULTS.rows")

python3 - "$RESULTS" "$NOW" "$HEALTHY_MS" "$IDLE_RSS_KB" "$PEAK_RSS_KB" "$FINAL_RSS_KB" "$TOTAL_CPU_S" "$CPUS" "$TASKS" "$ROWS" <<'PY'
import sys, json, os
out, when, cold, idle, peak, final, cpu, cpus, tasks, rows = sys.argv[1:]
results = []
for line in rows.strip().splitlines():
    label, ok, fail, acc, first, p50, p95, total = line.split(",")
    n = int(ok) + int(fail)
    results.append({
        "scenario": label,
        "tasks": n,
        "successRate": round(int(ok)/n, 4) if n else 0,
        "extractionAccuracy": round(int(acc)/n, 4) if n else 0,
        "firstResultMs": int(first),
        "p50Ms": int(p50),
        "p95Ms": int(p95),
        "cpuMsPerTask": round(float(cpu)*1000/ (n*len(rows.strip().splitlines())), 2),
    })
doc = {
    "when": when,
    "runtime": "scout (rust)",
    "cpus": int(cpus),
    "note": "Scout process only, read from /proc. Local fixtures with no network latency, no JavaScript and no hostile markup: these numbers measure the runtime, not real-site compatibility.",
    "coldStartMs": int(cold),
    "idleRssMb": round(int(idle)/1024, 1),
    "peakRssMb": round(int(peak)/1024, 1),
    "finalRssMb": round(int(final)/1024, 1),
    "totalCpuSec": float(cpu),
    "tasksPerScenario": int(tasks),
    "results": results,
}
with open(out, "w") as f:
    json.dump(doc, f, indent=2)
print("\n== summary ==")
print(json.dumps({k: v for k, v in doc.items() if k != "results"}, indent=2))
for r in results:
    print(json.dumps(r))
PY
rm -f "$RESULTS.rows" "$PEAK_FILE"

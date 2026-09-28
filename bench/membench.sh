#!/usr/bin/env bash
# Memory benchmark: every case runs in a fresh isolate (wasm memory only grows, so the
# post-request size is that isolate's peak). Target: local `wrangler dev` (default) or a
# deployed URL via TARGET=https://... (then isolates are reused; use for pass/fail only).
#
#   bench/membench.sh                    # full matrix
#   FILES="fhv" QUERIES="scan" bench/membench.sh
set -uo pipefail
cd "$(dirname "$0")/.."
PORT=8799
declare -A URL=(
  [green_1.4MB]=https://d37ci6vzurychx.cloudfront.net/trip-data/green_tripdata_2024-01.parquet
  [fhv_15MB]=https://d37ci6vzurychx.cloudfront.net/trip-data/fhv_tripdata_2024-01.parquet
  [yellow_48MB]=https://d37ci6vzurychx.cloudfront.net/trip-data/yellow_tripdata_2024-01.parquet
)
declare -A SQL=(
  [count]="SELECT count(*) FROM '%s'"
  [scan]="SELECT max(COLUMNS(*)) FROM '%s'"
)
FILES="${FILES:-green_1.4MB fhv_15MB yellow_48MB}"
QUERIES="${QUERIES:-count scan}"
MODES="${MODES:-buffer stream}"
EFCS="${EFCS:-1 0}"
mb() { [[ "$1" =~ ^[0-9]+$ ]] && awk -v b="$1" 'BEGIN{printf "%.1f", b/1048576}' || echo "$1"; }
hdr() { grep -i "^$1:" /tmp/membench.h | tr -d '\r' | cut -d' ' -f2; }

start_worker() {
  [ -n "${TARGET:-}" ] && return
  npx -y wrangler@latest dev -c wrangler.bench.toml --port $PORT > /tmp/membench.dev.log 2>&1 &
  DEV_PID=$!
  for _ in $(seq 1 60); do curl -s -o /dev/null "http://localhost:$PORT/?q=SELECT%201" && return; sleep 1; done
  echo "dev server did not start"; tail -5 /tmp/membench.dev.log; exit 1
}
stop_worker() {
  [ -n "${TARGET:-}" ] && return
  pkill -P $DEV_PID 2>/dev/null; kill $DEV_PID 2>/dev/null; wait $DEV_PID 2>/dev/null
  pkill -f "workerd.*$PORT" 2>/dev/null; sleep 1
}
BASE="${TARGET:-http://localhost:$PORT}"

printf "%-12s %-6s %-7s %-4s %-6s %8s %10s %10s %10s %10s %10s\n" file query fetch efc status time_s dl_MB wasm_open wasm_dl wasm_peak duckdb_MB
for f in $FILES; do for q in $QUERIES; do for m in $MODES; do for e in $EFCS; do
  start_worker
  sql=$(printf "${SQL[$q]}" "${URL[$f]}")
  out=$(curl -s -D /tmp/membench.h -o /tmp/membench.body -w "%{http_code} %{time_total}" -G \
        --data-urlencode "q=$sql" --data-urlencode "fetch=$m" --data-urlencode "efc=$e" "$BASE/")
  read -r code t <<<"$out"
  printf "%-12s %-6s %-7s %-4s %-6s %8.2f %10s %10s %10s %10s %10s\n" "$f" "$q" "$m" "$e" "$code" "$t" \
    "$(mb "$(hdr x-remote-bytes)")" "$(mb "$(hdr x-wasm-mem-before)")" "$(mb "$(hdr x-wasm-mem-fetched)")" \
    "$(mb "$(hdr x-wasm-mem-after)")" "$(mb "$(hdr x-duckdb-mem)")"
  [ "$code" != 200 ] && head -c 200 /tmp/membench.body | tr '\n' ' ' && echo
  stop_worker
done; done; done; done

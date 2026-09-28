#!/usr/bin/env bash
# Sweeps parquet_prefetch_column_gap for one query against the JSPI build (range reads) and
# reports subrequests, bytes fetched, wall time and wasm memory.
#   BASE=http://localhost:8787 bench/gapsweep.sh "<sql over 'https://...parquet'>" [gap ...]
set -uo pipefail
BASE="${BASE:-http://localhost:8787}"; BUDGET="${BUDGET:-1000}"
Q="$1"; shift
GAPS="${*:-default 0 1048576 4194304 16777216 67108864}"
h() { grep -i "^$1:" /tmp/gapsweep.h | tr -d '\r' | awk '{print $2}'; }
mb() { awk -v b="${1:-0}" 'BEGIN{printf "%.1f", b/1048576}'; }
printf "%-10s %-6s %6s %11s %8s %8s\n" gap status reqs fetched_MB time_s wasm_MB
for g in $GAPS; do
  if [ "$g" = default ]; then pre="RESET parquet_prefetch_column_gap"; else pre="SET parquet_prefetch_column_gap = $g"; fi
  out=$(curl -s -m 300 -D /tmp/gapsweep.h -o /tmp/gapsweep.body -G --data-urlencode "q=$pre; $Q" \
        --data-urlencode "io=range" --data-urlencode "budget=$BUDGET" -w "%{http_code} %{time_total}" "$BASE/")
  printf "%-10s %-6s %6s %11s %8s %8s\n" "$g" "${out% *}" "$(h x-range-requests)" "$(mb "$(h x-range-bytes)")" \
    "${out#* }" "$(mb "$(h x-wasm-mem-after)")"
  [ "${out% *}" != 200 ] && head -c 200 /tmp/gapsweep.body && echo
done

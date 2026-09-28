#!/usr/bin/env bash
# Usage: bench.sh <variant>...   times representative queries (best of 3, includes ~startup)
cd "$(dirname "$0")"
declare -a Q=(
  "startup|SELECT 1"
  "filter|SELECT count(*) FROM range(20000000) r(i) WHERE i % 7 BETWEEN 2 AND 4 AND i > 1000"
  "groupby|SELECT count(*) FROM (SELECT i % 1000 k, sum(i), avg(i), min(i) FROM range(10000000) r(i) GROUP BY k)"
  "hashjoin|SELECT count(*) FROM range(2000000) a(i) JOIN range(2000000) b(j) ON i = j"
  "sort|SELECT sum(x) FROM (SELECT hash(i) x FROM range(3000000) r(i) ORDER BY x LIMIT 10)"
  "sort_str|SELECT count(*) FROM (SELECT i::VARCHAR s FROM range(2000000) r(i) ORDER BY s)"
  "quantile|SELECT quantile_cont(i, 0.5), mode(i % 100) FROM range(3000000) r(i)"
  "win_quant|SELECT sum(q) FROM (SELECT median(i) OVER (ORDER BY i ROWS BETWEEN 500 PRECEDING AND 500 FOLLOWING) q FROM range(200000) r(i))"
)
printf "%-10s" query; for v in "$@"; do printf "%12s" "$v"; done; echo
for q in "${Q[@]}"; do
  name="${q%%|*}"; sql="${q#*|}"; printf "%-10s" "$name"
  for v in "$@"; do
    best=999999
    for _ in 1 2 3; do
      s=$(python3 -c 'import time;print(time.time())'); node "build/$v/driver.js" "$sql" >/dev/null 2>&1 || { best=ERR; break; }
      e=$(python3 -c 'import time;print(time.time())'); ms=$(python3 -c "print(int(($e-$s)*1000))"); [ "$ms" -lt "$best" ] && best=$ms
    done
    printf "%10sms" "$best"
  done; echo
done

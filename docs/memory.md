# Memory

A Worker isolate has **128 MB** for JS heap and wasm linear memory combined. Wasm memory only grows,
so the `x-wasm-mem-after` header on each response is the isolate's peak so far.

## Range reads (current design)

| state | peak wasm memory (MB) |
|---|---:|
| DuckDB opened (`memory_limit=64MB`), no queries | 18.4 |
| parquet GROUP BY on 48 MB or 473 MB files (projected columns only) | 18.4–26.6 |
| parquet scan of every column, 48 MB file | 49.9 |
| after one CSV / JSON scan (fixed reader buffers) | 43–70 |

Fetched bytes pass through the JS heap as short-lived `ArrayBuffer`s and are copied into DuckDB's buffers,
so a file is never held whole unless the server can't serve ranges (see
[range-reads.md](range-reads.md#compressed-responses)). Details and limits:
[range-reads.md](range-reads.md#known-limits).

## History: download-first (removed)

The first version downloaded each remote file into Emscripten's in-memory filesystem (JS heap) before the
query. That is removed. The numbers are kept in
[download-first-memory-local.txt](download-first-memory-local.txt) and
[download-first-memory-deployed.txt](download-first-memory-deployed.txt):

| 48 MB parquet | buffered download | streamed download | range reads (now) |
|---|---:|---:|---:|
| wasm memory after download (MB) | 122–133 | 18.5 (+48 in the JS heap) | — |
| GROUP BY, deployed on the Free plan | ❌ 1102 | ✅ first query per isolate, ❌ repeats | ✅ repeatable, 4.1 MB fetched |

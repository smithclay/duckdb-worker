# Range reads over fetch() via JSPI

DuckDB's file I/O is synchronous, and a Worker can only `fetch()` asynchronously. JS Promise
Integration (JSPI) bridges the two: a synchronous DuckDB read suspends the wasm stack on a `fetch()`
promise with a `Range` header, then resumes with the bytes. DuckDB reads only the footer and the column
chunks a query needs, and memory is bounded by DuckDB's buffers rather than the file size.

## How it fits together

| piece | role |
|---|---|
| `src/entry.js` | HTTP handler; calls the `query_jspi` export (a Promise) one query at a time per isolate |
| `src/jspi.rs` | `#[wasm_bindgen(jspi)] query_jspi`, `#[wasm_bindgen(suspending)]` fetch imports, per-query subrequest budget |
| `src/jspi_fs.cpp` | `JspiHttpFileSystem`: DuckDB `FileSystem` for `http(s)://`; large reads fetched exactly, small reads via a 1 MB block cache; sizes/blocks cached per query |
| `src/main.rs` | opens DuckDB (`memory_limit=64MB`, `parquet_prefetch_column_gap=4MB`), runs queries over the C API |

JSPI (`WebAssembly.Suspending` / `WebAssembly.promising`) is available on deployed Workers (checked on the
Free plan) and in local workerd (verified 2026-09-28).

## Toolchain

wasm-bindgen 0.2.129 does JSPI on the Emscripten target through lifecycle hooks that Emscripten doesn't
ship yet:

- emscripten PR [#27699](https://github.com/emscripten-core/emscripten/pull/27699) (`REENTRANT_JSPI`: a
  shadow stack per activation, on top of [#27698](https://github.com/emscripten-core/emscripten/pull/27698)
  JSPI hooks)
- binaryen PR [#9102](https://github.com/WebAssembly/binaryen/pull/9102) (`--jspi-hooks` pass)

`scripts/jspi-toolchain.sh` builds both on top of worker-build's pinned emsdk 6.0.10 (LLVM 24). Link flags:
`-sJSPI -sREENTRANT_JSPI`; rustc flag: `--cfg=wasm_bindgen_unstable_jspi`.

## Keeping subrequests down

Every read is a `fetch()` subrequest. The Free plan allows 50 external subrequests per invocation
(1,000 to Cloudflare services such as R2); paid plans allow 10,000 by default, and up to 10M
([limits](https://developers.cloudflare.com/workers/platform/limits/)). Round trips are also latency,
because reads are sequential.

- **Parquet coalesces.** DuckDB v2 merges column-chunk reads of a row group whose gap is below
  `parquet_prefetch_column_gap`. The worker sets it to 4 MB (`bench/gapsweep.sh`, 473 MB file, 19 row
  groups, 3 columns):

  | gap (MB) | HTTP range requests | bytes downloaded (MB) | notes |
  |---|---:|---:|---|
  | default (cost model) / 0 / 1 | 59 | 90 | over the Free cap |
  | **4** | **40** | **121** | 2.7 s deployed (Free plan) |
  | 16+ | — | — | not measured (origin started returning 403) |

- **Budget guard.** `?budget=` (default 50). When it's spent, the query fails with a clear
  "Subrequest budget exhausted" error instead of the runtime's generic one.
- **Data layout matters most.** Each row group costs at least one request per disjoint column region.
  `taxi_2019_04.parquet` (127 MB, 114 row groups of 1.1 MB) can't fit in 50; files with 100K–1M-row
  row groups ([DuckDB guidance](https://duckdb.org/docs/lts/guides/performance/file_formats)) do.
- Rejected: a block cache sized to the budget (`block = size / budget`). It guarantees the request count,
  but fetched 468 MB for the 473 MB query instead of 121 MB.

## Compressed responses

Workers' `fetch()` always negotiates compression and decodes transparently
([forum](https://community.cloudflare.com/t/workers-dont-support-range-requests-on-gzip-files/614199)).
On a compressed response, `Range`/`Content-Range` describe the encoded bytes, and the decoded body of a
partial range is empty or garbled, so asking for `Accept-Encoding: identity` doesn't help. Each URL is
therefore probed with a 1 KiB range: if exactly the requested bytes come back, ranges are used; otherwise
the whole decoded body is downloaded once and sliced for the rest of the query. This fallback is needed by
jsDelivr (compresses on the fly) and by shell.duckdb.org (stores files gzip-encoded).

## Results (deployed on the Workers Free plan via a `--temporary` account, 2026-09-28)

| query | HTTP range requests | bytes downloaded (MB) | query time (s) | peak wasm memory (MB) |
|---|---:|---:|---:|---:|
| 473 MB fhvhv, GROUP BY + 2 aggregates | 40 | 121 | 2.7 | 26.6 |
| 473 MB fhvhv, `count(*)` | 2 | 0.9 | 0.1–1.3 | 18.4 |
| 48 MB yellow taxi, GROUP BY | 5 | 4.1 | 0.4–1.2 | 18.4 |
| 48 MB yellow taxi, all columns | 5 | 48 | not recorded | 49.9 |
| 1.7 MB lineitem (gzip-stored, whole-body fallback) | 6 | 3.4 | 1.2 | 18.4 |
| 48 KB CSV / 100 KB JSON (whole-body fallback) | 2 | <0.1 | 0.2–0.5 | 43–45 |
| 127 MB taxi_2019_04 (114 row groups) | 50 | — | — | ❌ subrequest budget exhausted |

Column definitions are in the [README](../README.md#remote-files).

## Known limits

- **Repeat heavy queries.** Running the 473 MB aggregation twice in a row in the same isolate hits
  1102 on the second run, although wasm memory is only 26.6 MB. The first run's ~120 MB of response
  buffers are likely still awaiting JS garbage collection. The next request gets a fresh isolate.
- **CSV/JSON buffers.** DuckDB's JSON reader allocates `2 × maximum_object_size` (16 MB, which can only be
  raised), and CSV uses 16 × the maximum line size, whatever the file size. Wasm memory never shrinks, so
  one JSON query leaves an isolate at ~45–70 MB of wasm. Parquet is the format this design is built for.
- **Free-plan CPU** is 10 ms per request. The temporary account allowed ~1.7 s of CPU; don't rely on that.
- **One query at a time** per isolate (a suspended query holds the connection).
- The wasm is 20.2 MB (vs 17.5 MB before JSPI; `REENTRANT_JSPI` adds stack guard checks).

## Why not DuckDB v2's async I/O?

v2 adds async scan machinery: `AsyncResult`/`BLOCKED`, `ScanReadAhead` on an ASYNC task pool, and parquet
`ScheduleIO` planning each row group's reads. But `AsyncTask::Execute` still performs synchronous reads,
and with no threads it runs inline, so it doesn't let the wasm stack yield to JS. A JSPI-free alternative
would be a DuckDB patch adding externally-completed I/O tasks that the pending-query API
(`duckdb_pending_execute_task` → `NO_TASKS_AVAILABLE`) hands back to the host.

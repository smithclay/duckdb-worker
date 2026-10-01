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
| `src/jspi_fs.cpp` | `JspiHttpFileSystem`: DuckDB `FileSystem` for `http(s)://`; reads inside the probe's tail are free, large reads fetched exactly, small reads via a 1 MB block cache; sizes/blocks cached per query |
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

## The size probe

Each URL is probed once per query with a suffix range, `bytes=-1048576`. It returns the size (from
`Content-Range`), the ETag and Last-Modified, and the file's last 1 MiB, which holds a Parquet footer
(38 KB for the 473 MB file) or all of a small CSV/JSON file. Reads that fall inside that tail cost no
request, so `count(*)` on a Parquet file is one request.

The validators are reported to DuckDB (`GetVersionTag`, `GetLastModifiedTime`), and
`parquet_metadata_cache` reuses a file's parsed footer across queries in the same isolate while they
match. DuckDB's external file cache stays off: it splits reads into 2 MiB blocks and pins them, which
undoes parquet's coalesced reads (the 473 MB GROUP BY went from 40 requests to over 50) and filled the
isolate to 1102.

## Compressed responses

Workers' `fetch()` always negotiates compression and decodes transparently
([forum](https://community.cloudflare.com/t/workers-dont-support-range-requests-on-gzip-files/614199)),
and drops `Content-Encoding` from the response it hands back. On a compressed response, `Range` and
`Content-Range` describe the encoded bytes: a prefix range decodes to the wrong length, and a suffix range
of a file stored gzip-encoded comes back as raw gzip bytes of exactly the requested length. So:

- a probe is used only if exactly the requested bytes come back;
- if the response varies by encoding (`Vary: Accept-Encoding`) and has no strong ETag, a 16-byte prefix
  range must also decode to exactly 16 bytes (shell.duckdb.org and jsDelivr send weak ETags; CloudFront,
  S3 and GitHub raw send strong ones, so they skip this);
- otherwise the whole decoded body is downloaded once and sliced for the rest of the query, up to 32 MB,
  since it sits in the JS heap next to DuckDB.

Ranged URLs are also checked for changes: the first response's total size, Last-Modified and ETag are
remembered, and a later read that disagrees fails the query. Last-Modified is preferred over ETag when both
are present, because load-balanced origins can disagree on the ETag of an unchanged file.

## Results (deployed on the Workers Free plan via a `--temporary` account, 2026-10-01)

| query | HTTP range requests | bytes downloaded (MB) | query time (s) | peak wasm memory (MB) |
|---|---:|---:|---:|---:|
| 473 MB fhvhv, GROUP BY + 2 aggregates | 39 | 61 | 2.5 | 24.4 |
| 473 MB fhvhv, `count(*)` | 1 | 1.0 | 0.8–1.3 | 20.3 |
| 48 MB yellow taxi, GROUP BY | 4 | 4.2 | 0.5–1.0 | 20.3 |
| 48 MB yellow taxi, all columns (2026-09-28) | 5 | 48 | not recorded | 49.9 |
| 1.7 MB lineitem (gzip-stored: probe, prefix check, whole body) | 3 | 3.3 | 0.3 | 20.3 |
| 48 KB CSV / 100 KB JSON from jsDelivr (probe, whole body) | 2 | <0.1 | 0.1–0.5 | 46–48 |
| GitHub raw Parquet, `count(*)` | 2 | 0.9 | 0.3 | 20.3 |
| 127 MB taxi_2019_04 (114 row groups, 2026-09-28) | 50 | — | — | ❌ subrequest budget exhausted |

Requests are every `fetch()` the query made, including probes and fallbacks (before 2026-10-01 a probe
that fell back to a whole-body download counted as one).

Column definitions are in the [README](../README.md#remote-files).

## Known limits

- **Repeat heavy queries.** Running the 473 MB aggregation twice in a row in the same isolate hits
  1102 on the second run, although wasm memory is only 26.6 MB. The first run's ~120 MB of response
  buffers are likely still awaiting JS garbage collection. The next request gets a fresh isolate.
- **CSV/JSON buffers.** DuckDB's JSON reader allocates `2 × maximum_object_size` (16 MB, which can only be
  raised), and CSV uses 16 × the maximum line size, whatever the file size. Wasm memory never shrinks, so
  one JSON query leaves an isolate at ~45–70 MB of wasm. Parquet is the format this design is built for.
- **Free-plan CPU** is 10 ms per request. The temporary account allowed ~1.7 s of CPU; don't rely on that.
- **Very wide rows.** Results stop at 8 MB, but DuckDB builds a whole chunk (up to 2,048 rows) before
  any of it is written out, and string data isn't counted against `memory_limit`. Rows of ~100 KB each
  can exhaust the isolate (1102) before the cap applies.
- **One query at a time** per isolate (a suspended query holds the connection). Up to three more wait;
  further requests get HTTP 429.
- **Killed requests.** When the runtime kills a request mid-query (1102), its query stays suspended on a
  `fetch()` that never settles. After 30 s the next request abandons it (`abandon_stuck_query`): later
  queries get a fresh database, and the old one is leaked. Requests already queued behind it get a 503
  asking for a retry. A client that disconnects doesn't cause this: `waitUntil` lets its query finish.
- The wasm is 20.2 MB (vs 17.5 MB before JSPI; `REENTRANT_JSPI` adds stack guard checks).

## Why not DuckDB v2's async I/O?

v2 adds async scan machinery: `AsyncResult`/`BLOCKED`, `ScanReadAhead` on an ASYNC task pool, and parquet
`ScheduleIO` planning each row group's reads. But `AsyncTask::Execute` still performs synchronous reads,
and with no threads it runs inline, so it doesn't let the wasm stack yield to JS. A JSPI-free alternative
would be a DuckDB patch adding externally-completed I/O tasks that the pending-query API
(`duckdb_pending_execute_task` → `NO_TASKS_AVAILABLE`) hands back to the host.

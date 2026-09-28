# Spike: DuckDB range reads over fetch() via JSPI

Branch `jspi-range-reads`, 2026-09-28. Instead of downloading a remote file before the query, DuckDB reads
`http(s)://` files itself with `Range` requests: synchronous `FileSystem::Read` calls suspend the wasm stack
on a `fetch()` promise through JS Promise Integration (JSPI).

## How
- `src/jspi_fs.cpp`: `JspiHttpFileSystem`, a DuckDB `FileSystem` for `http(s)://` registered on
  open. Reads go through a per-handle read-ahead window: 4 MB for small reads, and at least 16 MB for large
  ones.
- `src/jspi.rs`: `dw_http_read` / `dw_http_size` (called from C++) use `#[wasm_bindgen(suspending)]`
  imports around `fetch()`, and `query_jspi` is a `#[wasm_bindgen(jspi)]` export (JS gets a Promise).
- `src/entry.js`: `?io=range` requests go to `this.query_jspi(sql)`, serialized per isolate; everything
  else goes to the Rust `#[event(fetch)]` handler.
- JSPI (`WebAssembly.Suspending` / `promising`) is available in production Workers and local workerd.

## Toolchain (unreleased pieces)
wasm-bindgen 0.2.129 supports JSPI on Emscripten through lifecycle hooks, which Emscripten doesn't ship yet:
- emscripten PR [#27699](https://github.com/emscripten-core/emscripten/pull/27699) (`REENTRANT_JSPI`, on
  top of [#27698](https://github.com/emscripten-core/emscripten/pull/27698) JSPI hooks)
- binaryen PR [#9102](https://github.com/WebAssembly/binaryen/pull/9102) (`--jspi-hooks`)

```sh
tiny/build.sh full-sb-keep
scripts/jspi-toolchain.sh && eval "$(scripts/jspi-toolchain.sh env)"
npx wrangler dev -c wrangler.jspi.toml        # or: npx wrangler deploy --temporary -c wrangler.jspi.toml
curl -G localhost:8787/ --data-urlencode io=range \
  --data-urlencode "q=SELECT count(*) FROM 'https://d37ci6vzurychx.cloudfront.net/trip-data/fhvhv_tripdata_2024-01.parquet'"
```
Response headers: `x-range-requests`, `x-range-bytes`, `x-wasm-mem-after`, `x-elapsed-ms`.

## Results on production (temporary account = Free plan)

| query | download-first (main) | JSPI range reads |
|---|---|---|
| 48 MB taxi, GROUP BY 2 cols, 3 repeats | 1102 on repeats | ✅ 3/3, 6 requests, 1.2 s, wasm 18.5–22 MB |
| 48 MB taxi, all columns | ✅ first per isolate only | ✅ 6 requests, 49 MB wasm |
| 127 MB taxi_2019_04, 2 cols | ❌ 1101 | ✅ 32 requests, 10.5 s, wasm 18.5 MB |
| 473 MB fhvhv, `count(*)` | ❌ | ✅ 2 requests, ~1 s |
| 473 MB fhvhv, GROUP BY 1 col | ❌ | ✅ 7.9 s wall, 1.7 s CPU |
| 473 MB fhvhv, GROUP BY + 2 aggregates | ❌ | ❌ exceeds 50 subrequests (Free plan) |

Before read-ahead, with 256 KB windows only, the 48 MB GROUP BY fetched 3.9 MB in 6 requests. Locally the
473 MB three-column aggregation succeeded (76 MB fetched, 60 requests).

## Limits / follow-ups
- **Subrequests**: every range read is a subrequest. The Free plan allows 50 per invocation and can't raise it
  (`[limits]` is rejected on Free); paid plans allow far more. Row groups × projected columns needs to fit.
  Next: coalesce all of a row group's column chunks into one request with gap tolerance, driven by
  parquet's `ScheduleIO` plan instead of guessing with windows.
- **Servers that store files `Content-Encoding: gzip`** (e.g. shell.duckdb.org) give ranges over the
  encoded bytes while `fetch()` decodes, so reads come back wrong. Needs a whole-file fallback when
  `Content-Encoding` is set.
- Wasm grows 17.5 → 20.7 MB with `REENTRANT_JSPI` (stack guard checks); not yet investigated.
- Queries run one at a time per isolate; the file system isn't used by the non-JSPI (`io` unset) path.
- DuckDB v2's async scan machinery (`AsyncResult`/`BLOCKED`, `ScanReadAhead`, parquet `ScheduleIO`)
  doesn't remove the need for JSPI: `AsyncTask::Execute` still does synchronous reads, and with no threads
  it runs inline. A JSPI-free path would be a DuckDB patch adding externally-completed I/O tasks, so the
  pending-query API (`duckdb_pending_execute_task` → `NO_TASKS_AVAILABLE`) can hand control back to Rust.

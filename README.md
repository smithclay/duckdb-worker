# duckdb-worker

**DuckDB v2 running natively inside a Cloudflare Worker, reading remote Parquet/CSV/JSON with HTTP
range requests.**

It is a Rust Worker on worker-build's [Emscripten target](https://blog.cloudflare.com/rust-workers-emscripten-target/)
that statically links a size-trimmed DuckDB `v2.0-cyanoptera` (`core_functions`, `parquet`, `json`).
DuckDB's synchronous file reads suspend on `fetch()` through **JSPI** (JS Promise Integration), so it
fetches only the byte ranges a query needs.

> An experiment. It relies on an unreleased DuckDB (v2.0 branch at `d591bb1d`), an experimental
> Workers target, and unmerged Emscripten/Binaryen PRs for JSPI (see [toolchain](docs/range-reads.md#toolchain)).

## Try it: no Cloudflare account needed

```sh
git clone https://github.com/smithclay/duckdb-worker && cd duckdb-worker
scripts/deploy-temporary.sh      # prebuilt dist/, needs only Node.js
```

This deploys with `wrangler deploy --temporary` to a throwaway account that lasts 60 minutes (a claim link
lets you keep it). It uses an empty wrangler config dir, so your own login is left untouched.

```sh
W=https://duckdb-tiny.<temp-subdomain>.workers.dev
curl $W/                                   # version, extensions, settings
curl -G $W/ --data-urlencode "q=SELECT hvfhs_license_num, count(*), avg(trip_miles)
  FROM 'https://d37ci6vzurychx.cloudfront.net/trip-data/fhvhv_tripdata_2024-01.parquet' GROUP BY 1"
curl -X POST $W/ --data "SELECT 42"        # SQL in the body also works
```

Results come back as TSV. Errors return HTTP 400 with DuckDB's message. The response headers
`x-range-requests`, `x-range-bytes`, `x-wasm-mem-after` and `x-elapsed-ms` show the cost of each query.
`?budget=` sets the maximum number of subrequests (default 50).

Each request runs one read-only statement (`SELECT` or `EXPLAIN`) against a database the isolate keeps
between requests, with its configuration locked. Results stream out and stop at 8 MB. An isolate runs one
query at a time and queues up to three more; past that it returns HTTP 429.

## Results

### Remote files

Measured on a Worker deployed to the Cloudflare Workers **Free plan** (a `--temporary` account),
2026-10-01 (the every-column row: 2026-09-28).

| query | HTTP range requests | bytes downloaded (MB) | query time (s) | peak DuckDB memory in the Worker (MB) |
|---|---:|---:|---:|---:|
| 473 MB Parquet, GROUP BY + 2 aggregates | 39 | 61 | 2.5 | 24 |
| 473 MB Parquet, `count(*)` | 1 | 1.0 | 0.8–1.3 | 20 |
| 48 MB Parquet, GROUP BY | 4 | 4.2 | 0.5–1.0 | 20 |
| 48 MB Parquet, every column | 5 | 48 | not recorded | 50 |
| 48 KB CSV / 100 KB JSON | 2 | <0.1 | 0.1–0.5 | 46–48 |

- **HTTP range requests**: `fetch()` calls DuckDB made to read the file, one subrequest each (the Free
  plan allows 50 per request). It includes the one that probes the file's size, which also brings back
  the file's last 1 MB (a Parquet footer, or all of a small file).
- **Bytes downloaded**: how much of the file was actually transferred. For Parquet, that's only the footer
  and the columns the query touches. Files from servers that compress their responses (these CSV/JSON
  files) are downloaded whole, once per query.
- **Query time**: end-to-end wall time measured by `curl`, including the downloads.
- **Peak DuckDB memory in the Worker**: the size of the WebAssembly linear memory that holds DuckDB after
  the query (`x-wasm-mem-after`). It only grows, so it's the peak, and it counts against the Worker's
  128 MB limit (along with the JS heap, which isn't measured here). About 20 MB is DuckDB's baseline.

Details, request tuning and known limits are in [docs/range-reads.md](docs/range-reads.md). The memory
model and the removed download-first design are in [docs/memory.md](docs/memory.md).

### Size

| build | `.wasm` file size, uncompressed (MB) | compressed (MB) |
|---|---:|---:|
| DuckDB + core_functions + parquet + json, `-O3` | 36.8 | 5.3 (brotli) |
| same, `-Oz` | 20.3 | 3.7 (brotli) |
| **same, `-Oz` + `SMALLER_BINARY` (except window/sort)**, used here | **16.4** | **3.5 (brotli)** |
| engine only, no extensions | 13.1 | 2.8 (brotli) |
| Worker bundle (the build above + Rust + JSPI) | 20.2 | 5.4 (gzip, as uploaded) |

`-Oz` is also *faster* than `-O3` here. Dropping `core_functions` saves only ~0.2 MB compressed and loses
`sum`/`avg`. LTO breaks C++ exception handling. The full matrix and benchmarks are in
[tiny/RESULTS.md](tiny/RESULTS.md).

## Limits worth knowing

- **Subrequests:** each range read counts. The Free plan allows 50 external subrequests per request
  (1,000 to R2 and other Cloudflare services); paid plans allow 10,000 or more. Parquet files with many
  small row groups need more requests; aim for 100K–1M rows per row group.
- **Memory:** 128 MB per isolate, and wasm memory never shrinks. JSON/CSV readers allocate fixed
  ~25–50 MB buffers. Back-to-back heavy queries in one isolate can hit 1102.
- **Files that can't be range-read** (the server compresses them or ignores `Range`) are downloaded
  whole, up to 32 MB. A file that changes during a query (size, Last-Modified or ETag) fails it.
- **CPU:** 10 ms per request on the Free plan (the temporary account was more lenient); paid is 30 s by
  default.

## Build from source

Requirements: Rust **1.98.0** with `wasm32-unknown-emscripten`, worker-build 0.8.7, cmake, ninja,
python3, and Node.js.

```sh
tiny/build.sh full-sb-keep                 # DuckDB static libs (fetches DuckDB @ d591bb1d + emsdk into vendor/)
scripts/jspi-toolchain.sh                  # emscripten/binaryen JSPI PR builds into vendor/jspi-toolchain
eval "$(scripts/jspi-toolchain.sh env)"
npx wrangler dev                           # local, http://localhost:8787
scripts/deploy-temporary.sh --from-source  # or: scripts/update-dist.sh to refresh dist/
```

| path | what |
|---|---|
| `src/entry.js` | HTTP handler → `query_jspi` |
| `src/jspi.rs`, `src/jspi_fs.cpp` | JSPI fetch bridge and DuckDB `FileSystem` for `http(s)://` |
| `src/main.rs`, `build.rs` | DuckDB C API and static linking |
| `tiny/` | DuckDB size experiment (`build.sh <variant>`, `bench.sh`) |
| `bench/gapsweep.sh` | sweep `parquet_prefetch_column_gap` for a query |
| `dist/` | prebuilt bundle used by `deploy-temporary.sh` |

## Open questions

- Read from R2 through a binding (a higher subrequest budget, no egress) instead of over public HTTP.
- The LTO exception-handling bug (`tiny/lto_eh_repro.cpp`).
- For heavy scans, [DuckDB on Cloudflare Containers](https://github.com/tobilg/cloudflare-duckdb) or
  [R2 SQL](https://blog.cloudflare.com/r2-sql-deep-dive/) are probably the better tools.

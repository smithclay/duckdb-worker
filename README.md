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

## Results

### Remote files (production, Free plan)

| query | requests | fetched | time | wasm |
|---|---:|---:|---:|---:|
| 473 MB Parquet, GROUP BY + 2 aggregates | 40 | 121 MB | 2.7 s | 27 MB |
| 473 MB Parquet, `count(*)` | 2 | 0.9 MB | ~0.1–1 s | 18 MB |
| 48 MB Parquet, GROUP BY | 5 | 4.1 MB | ~0.5 s | 18 MB |
| 48 MB Parquet, every column | 5 | 48 MB | — | 50 MB |
| CSV / JSON (small) | 2 | <0.1 MB | ~0.2–0.5 s | 43–45 MB |

Details, request tuning and known limits are in [docs/range-reads.md](docs/range-reads.md). The memory
model and the removed download-first design are in [docs/memory.md](docs/memory.md).

### Size

| build | wasm raw | brotli |
|---|---:|---:|
| DuckDB + core_functions + parquet + json, `-O3` | 36.8 MB | 5.3 MB |
| same, `-Oz` | 20.3 MB | 3.7 MB |
| **same, `-Oz` + `SMALLER_BINARY` (except window/sort)**, used here | **16.4 MB** | **3.5 MB** |
| engine only, no extensions | 13.1 MB | 2.8 MB |
| Worker bundle (the build above + Rust + JSPI) | 20.2 MB | 5.4 MB gzip |

`-Oz` is also *faster* than `-O3` here. Dropping `core_functions` saves only ~0.2 MB compressed and loses
`sum`/`avg`. LTO breaks C++ exception handling. The full matrix and benchmarks are in
[tiny/RESULTS.md](tiny/RESULTS.md).

## Limits worth knowing

- **Subrequests:** each range read counts. The Free plan allows 50 external subrequests per request
  (1,000 to R2 and other Cloudflare services); paid plans allow 10,000 or more. Parquet files with many
  small row groups need more requests; aim for 100K–1M rows per row group.
- **Memory:** 128 MB per isolate, and wasm memory never shrinks. JSON/CSV readers allocate fixed
  ~25–50 MB buffers. Back-to-back heavy queries in one isolate can hit 1102.
- **CPU:** 10 ms per request on the Free plan (the temporary account was more lenient); paid is 30 s by
  default.

## Build from source

Requirements: Rust **beta** with `wasm32-unknown-emscripten`, worker-build 0.8.7, cmake, ninja,
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

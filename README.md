# duckdb-worker

**DuckDB v2 running natively inside a Cloudflare Worker**: a Rust Worker compiled to
`wasm32-unknown-emscripten` with worker-build's new
[Emscripten target](https://blog.cloudflare.com/rust-workers-emscripten-target/), statically linking a
size-trimmed build of DuckDB `v2.0-cyanoptera` with `core_functions`, `parquet` and `json`.

> Experimental: the Emscripten target is an experimental preview, and DuckDB v2.0 is unreleased
> (built from the `v2.0-cyanoptera` branch at `d591bb1d`, 2026-09-28).

## Deploy in one command (no Cloudflare account)

```sh
git clone https://github.com/smithclay/duckdb-worker && cd duckdb-worker
scripts/deploy-temporary.sh
```

This deploys the prebuilt bundle in `dist/` with `wrangler deploy --temporary`. That creates a throwaway
Cloudflare account, prints a `*.workers.dev` URL, and gives you a claim link. The Worker is deleted after 60
minutes unless you claim it. The script needs only Node.js. It uses an empty wrangler config dir, so an
existing wrangler login on your machine is left untouched (`--temporary` refuses to run while logged in).

Then query it:

```sh
W=https://duckdb-tiny.<your-temp-subdomain>.workers.dev
curl $W/                                              # version, extensions, memory_limit
curl -G $W/ --data-urlencode "q=SELECT sum(i), avg(i) FROM range(1000000) r(i)"
curl -G $W/ --data-urlencode "q=SELECT l_returnflag, count(*) FROM 'https://shell.duckdb.org/data/tpch/0_01/parquet/lineitem.parquet' GROUP BY 1"
curl -G $W/ --data-urlencode "q=SELECT count(*) FROM read_csv('https://cdn.jsdelivr.net/npm/vega-datasets@2/data/seattle-weather.csv')"
curl -G $W/ --data-urlencode "q=SELECT Origin, avg(Miles_per_Gallon) FROM read_json('https://cdn.jsdelivr.net/npm/vega-datasets@2/data/cars.json') GROUP BY 1"
curl -X POST $W/ --data "SELECT 42"                   # SQL in the body also works
```

Results come back as TSV. SQL errors return HTTP 400 with DuckDB's message.

## How it works

- `tiny/build.sh full-sb-keep` builds DuckDB with emscripten 6.0.10, the version worker-build pins. It uses
  the exception encoding worker-build requires (`-fwasm-exceptions -sWASM_LEGACY_EXCEPTIONS=0`,
  because V8 rejects mixing EH encodings). The build is single-threaded with no extension loading.
- `build.rs` links the static archives into the Rust bin. `src/main.rs` calls the DuckDB C API through
  hand-written FFI and registers the statically linked extensions with the generated v2 static loader.
- There is one in-memory database per isolate, opened on first request with `memory_limit=64MB`
  (Workers cap an isolate at 128 MB).
- **Remote files**: DuckDB's file I/O is synchronous, and a Worker can only `fetch()` asynchronously.
  So each quoted `'http(s)://…'` literal in the SQL is downloaded first into Emscripten's in-memory
  filesystem, and the SQL is rewritten to point at the local copy. That means whole-file downloads with no
  range requests, and no `httpfs`. The body is streamed into a MEMFS file presized from `Content-Length`
  (`fetch=stream`, the default); `fetch=buffer` buffers it in wasm memory first, for comparison.
- Benchmark knobs: `fetch=stream|buffer`, `efc=0|1` (external file cache) and `ml=<memory_limit>`.
  Each response carries `x-remote-bytes`, `x-wasm-mem-before`, `x-wasm-mem-fetched`, `x-wasm-mem-after`
  (wasm memory only grows, so this is the isolate's peak) and `x-duckdb-mem` headers.

## Results

### Deployed Worker (2026-09-28)

| | |
|---|---|
| Upload | 17.2 MB raw / 5.3 MB gzip (fits the 64 MB Workers limit on any plan) |
| Startup time reported by wrangler | 112–180 ms |
| First request (isolate + DB open) | ~0.6–0.7 s end to end |
| Remote parquet: TPC-H lineitem, 3.3 MB | ✅ ~0.2–1.2 s |
| Remote parquet: 2-file join, 4.5 MB | ✅ ~0.9 s |
| Remote CSV / JSON (48 KB / 100 KB) | ✅ |
| Remote parquet, 50 MB (NYC taxi): `count(*)` (footer only) | ✅ 1.0 s |
| Remote parquet, 48 MB: scan/aggregation, buffered download | ❌ error 1102 (wasm memory reaches 2.5–2.8x the file size) |
| Remote parquet, 48 MB: scan/aggregation, streamed download | ✅ first query in a fresh isolate (scan 2.1 s, group by 1.1 s); ❌ 1102 on repeat queries in the same isolate |
| Remote parquet, 127 MB | ❌ error 1101 |

Right after a deploy, expect a few seconds of transient 1104/1042 errors while it propagates.

### Memory (`bench/membench.sh`, [local](bench/local-results.txt) / [deployed](bench/deployed-results.txt))

- A freshly opened DuckDB uses 18.5 MB of wasm linear memory.
- **Buffered download** (`resp.bytes()`): the growing `Vec` pushes wasm memory to about 2.5–2.8x the file
  size (48 MB file → 122–133 MB), and MEMFS holds another copy in the JS heap.
- **Streamed download**: wasm memory doesn't move during the download, and MEMFS holds 1x in the JS heap.
  The query then adds DuckDB working memory: +0 for `count(*)`, +14 MB to scan every column of 15 MB,
  +30 MB for 48 MB.
- Wasm memory never shrinks, and a dropped MEMFS file waits for JS GC. So a *second* large query in the
  same isolate can exceed 128 MB even when the first one fit.
- `enable_external_file_cache` made no measurable difference for local (MEMFS) files.

### How small can DuckDB v2 get? (`tiny/`, full table in [tiny/RESULTS.md](tiny/RESULTS.md))

Each variant is linked with a C API driver and run under node:

| variant | raw | brotli | notes |
|---|---:|---:|---|
| core_functions+parquet+json, `-O3` | 36.8 MB | 5.29 MB | |
| same, `-Oz` | 20.3 MB | 3.67 MB | `-Oz` is also *faster* than `-O3` here (smaller module = less V8 compile) |
| same, `-Oz` + `SMALLER_BINARY` | 16.2 MB | 3.50 MB | windowed median OOMs (see below) |
| **same, `-Oz` + `SMALLER_BINARY` except window/sort** (`full-sb-keep`, used here) | **16.4 MB** | **3.51 MB** | recommended |
| no extensions, `-Oz` + `SMALLER_BINARY` | 13.1 MB | 2.83 MB | engine floor without LTO |
| no extensions + LTO | 10.4 MB | 2.52 MB | ❌ broken exception handling |

Findings:
- **Engine floor**: the core engine alone (parser, binder, optimizer, operators, CSV, Arrow, geometry,
  variant) is ~13 MB raw / ~2.8 MB brotli.
- **`core_functions`** costs only ~1.4 MB raw / ~0.2 MB brotli. Without it you lose `sum`, `avg`,
  `string_agg` and most date/math/list functions; only 10 aggregates remain. Keep it.
- **`SMALLER_BINARY`** removes no features; it replaces specialized code paths with generic ones. The
  measured costs:
  - Filter-heavy queries run ~2.2x slower (`select_paths`).
  - Windowed `median`/`quantile`/`mad` re-aggregate every frame, and a 1001-row frame over 200k rows OOMed
    at 1.5 GiB (`window_specialization`).
  - Everything else was within noise.
  - Use `SMALLER_BINARY=ON SMALLER_BINARY_EXCEPT=window_specialization,sort_specialization`.
- **LTO bug**: under `-flto` (full or thin), a thrown `duckdb::Exception` is no longer caught by DuckDB's
  own `catch (std::exception&)`, so any SQL error aborts the isolate. It happens with both EH encodings and
  at every link opt level. Repro: `tiny/lto_eh_repro.cpp`. Not yet reported upstream.

## Build from source

Requirements: Rust **beta** with `wasm32-unknown-emscripten`, worker-build 0.8.7
(`cargo install worker-build`), cmake, ninja, python3, and node.

```sh
tiny/build.sh full-sb-keep            # fetches DuckDB @ d591bb1d + emsdk 6.0.10 into vendor/, ~1-2 min build
npx wrangler dev                      # local: http://localhost:8787 (runs worker-build)
scripts/deploy-temporary.sh --from-source
```

Other size variants: `tiny/build.sh <variant>` (see the `case` in the script). To benchmark variants:
`tiny/bench.sh full full-sb-keep`.

## Next steps

- **Range reads via JSPI**: JS Promise Integration (`WebAssembly.Suspending`/`promising`) is available
  in production Workers (verified 2026-09-28), so synchronous DuckDB reads can suspend on an async
  `fetch()` with a `Range` header, and memory becomes bounded by DuckDB's buffer pool rather than the file
  size. Either write a small custom `FileSystem`, or plug a JSPI-backed `HTTPClient` into DuckDB's
  `HTTPUtil` (`DBConfig::SetHTTPUtil`) and build `httpfs`.
- Reuse a downloaded file across requests in the same isolate (cache keyed by URL) instead of
  re-downloading.
- Older idea: true range-request reads (like `httpfs`) need a sync-to-async bridge (JSPI or Asyncify) behind a
  DuckDB `FileSystem`.
  [ducklings](https://tobilg.com/posts/custom-duckdb-wasm-builds-for-cloudflare-workers/) does this with
  Asyncify, but on JS-based EH, which is incompatible with this target.
- Track down the LTO exception bug.
- Deploy via the new [`cf` CLI](https://blog.cloudflare.com/cloudflare-cf-cli-launch/) once
  `cf deploy --temporary` is exposed; as of cf 1.0.0-beta.5 it is not.

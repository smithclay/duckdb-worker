# DuckDB v2 "tiny" wasm size experiment

DuckDB `v2.0-cyanoptera` @ d591bb1d, emsdk 6.0.10 (worker-build's pin), exnref wasm EH
(`-fwasm-exceptions -sWASM_LEGACY_EXCEPTIONS=0`), `DISABLE_THREADS`, `DISABLE_EXTENSION_LOAD`,
no autoload/autoinstall, no httplib. `driver.c` = C API open/query/print. Reproduce: `tiny/build.sh <variant>`.

| variant | extensions | flags | raw | gzip -9 | brotli | errors caught? |
|---|---|---|---:|---:|---:|---|
| full-o3 | core_functions, parquet, json | -O3 | 36.8 MB | 8.84 MB | 5.29 MB | yes |
| full | core_functions, parquet, json | -Oz | 20.3 MB | 5.47 MB | 3.67 MB | yes |
| full-sb | core_functions, parquet, json | -Oz, SMALLER_BINARY | 16.2 MB | 5.02 MB | 3.50 MB | yes |
| core | core_functions | -Oz | 18.7 MB | 4.84 MB | 3.22 MB | yes |
| core-sb | core_functions | -Oz, SB | 14.5 MB | 4.41 MB | 3.05 MB | yes |
| parquet-sb | parquet | -Oz, SB | 14.4 MB | 4.54 MB | 3.22 MB | yes |
| json-sb | json | -Oz, SB | 13.4 MB | 4.14 MB | 2.90 MB | yes |
| min | none | -Oz | 15.7 MB | 4.28 MB | 2.93 MB | yes |
| min-sb | none | -Oz, SB | 13.1 MB | 4.05 MB | 2.83 MB | yes |
| full-sb-lto | core_functions, parquet, json | -Oz, SB, -flto | 13.1 MB | 4.44 MB | 3.15 MB | **NO** |
| min-sb-lto | none | -Oz, SB, -flto | 10.4 MB | 3.54 MB | 2.52 MB | **NO** |
| min-sb-thinlto | none | -Oz, SB, -flto=thin | 10.8 MB | 3.63 MB | 2.59 MB | **NO** |
| min-sb-lto-nortti | none | + DISABLE_RTTI | 10.1 MB | 3.47 MB | 2.46 MB | **NO** |

## Findings
- `-Oz` vs `-O3` is the biggest lever: -45% raw, -30% brotli.
- The engine core is the floor: ~13 MB raw / ~2.8 MB brotli with nothing else linked
  (PEG parser, binder, planner/optimizer, operators, CSV reader, Arrow, geometry, variant are all core).
- core_functions costs ~1.4 MB raw / ~0.2 MB brotli (with SB). Without it only 10 aggregates remain
  (count, min, max, first, last, any_value, arbitrary, ...): **no sum, avg, string_agg**; 221 vs 483 scalars.
- SMALLER_BINARY saves ~2.5–4 MB raw but only ~0.1–0.2 MB compressed.
- parquet ≈ +1.3 MB raw / +0.4 MB brotli (includes brotli codec dictionary); json ≈ +0.3 MB raw.
- **LTO bug**: with full or thin LTO, a thrown `duckdb::Exception` is not caught by DuckDB's own
  `catch (std::exception&)`, so any SQL error escapes to JS as an uncaught `WebAssembly.Exception`.
  Happens with legacy EH too, and at -O0/-O2 link, so it's LTO + C++ type matching, not exnref.
  Unresolved.
- All sizes are far under Workers' 64 MiB limit; startup (<1 s to instantiate) and the 128 MB
  memory cap are the more likely constraints.

## What SMALLER_BINARY costs (full vs full-sb vs full-sb-keep, node, best of 3, incl. ~170 ms startup)
No SQL features are removed; specialized code paths fall back to generic ones. `tiny/bench.sh full full-sb full-sb-keep`:

| query | full | full-sb | full-sb-keep (EXCEPT window,sort) |
|---|---:|---:|---:|
| startup (SELECT 1) | 176 | 171 | 172 |
| filter (BETWEEN/compare, 20M rows) | 285 | 417 | 415 |
| group by (10M rows) | 286 | 268 | 270 |
| hash join (2M x 2M) | 356 | 351 | 352 |
| sort / sort strings | 209 / 343 | 205 / 336 | 204 / 335 |
| quantile/mode | 274 | 266 | 266 |
| windowed median, 1001-row frame, 200k rows | 334 | **OOM at 1.5 GiB** | 318 |

- `select_paths` trimming: filter-heavy query ~2.2x slower once ~170 ms startup is subtracted (~110 -> ~245 ms).
- `window_specialization` trimming: windowed quantile/median/MAD re-aggregate each frame -> OOM on big frames.
- `full-sb-keep` (`SMALLER_BINARY=ON SMALLER_BINARY_EXCEPT=window_specialization,sort_specialization`)
  is 16.4 MB raw / 3.51 MB br, i.e. +0.24 MB raw over full-sb for keeping those. Recommended default.

## -O3 vs -Oz runtime (node, same bench)
-O3 is slower than -Oz on every query (startup 225 vs 181 ms, filter 564 vs 293, hash join 464 vs 364):
the 2x larger module costs more in V8 compile/tier-up than it gains. -Oz wins on size and speed.

## Recommended Workers build
`full-sb-keep` config: emsdk 6.0.10, `-fwasm-exceptions -sWASM_LEGACY_EXCEPTIONS=0 -Oz`, no LTO, RTTI on,
DISABLE_THREADS, DISABLE_EXTENSION_LOAD, autoload/autoinstall off, DISABLE_BUILTIN_HTTPLIB,
SMALLER_BINARY=ON + SMALLER_BINARY_EXCEPT=window_specialization,sort_specialization,
static extensions core_functions (+ parquet / json only if needed) via generated static loader.

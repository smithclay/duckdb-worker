#!/usr/bin/env bash
# Usage: tiny/build.sh <variant>   (SKIP_BUILD=1 to only relink)
#
# Builds a DuckDB v2 static lib + selected in-tree extensions with emscripten, using the
# exception encoding worker-build forces (exnref wasm EH), links driver.c into a .wasm and
# records its size in tiny/results.txt. The Worker links tiny/build/full-sb-keep.
#
# First run fetches DuckDB (pinned commit) into vendor/duckdb and emsdk 6.0.10 (the version
# worker-build pins) into vendor/emsdk. Override with DUCKDB_SRC / EMSDK_DIR.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(dirname "$HERE")"
DUCKDB_REF="d591bb1da2de3cb12329c75aff2c31bebe922b59" # v2.0-cyanoptera, 2026-09-28
EMSDK_VERSION="6.0.10"
DUCKDB="${DUCKDB_SRC:-$ROOT/vendor/duckdb}"
EMSDK_DIR="${EMSDK_DIR:-$ROOT/vendor/emsdk}"

if [ ! -d "$DUCKDB/src" ]; then
  mkdir -p "$DUCKDB"
  git -C "$DUCKDB" init -q
  git -C "$DUCKDB" fetch -q --depth 1 https://github.com/duckdb/duckdb "$DUCKDB_REF"
  git -C "$DUCKDB" checkout -q FETCH_HEAD
fi
if [ ! -x "$EMSDK_DIR/emsdk" ]; then
  git clone -q --depth 1 https://github.com/emscripten-core/emsdk.git "$EMSDK_DIR"
  "$EMSDK_DIR/emsdk" install "$EMSDK_VERSION" >/dev/null
  "$EMSDK_DIR/emsdk" activate "$EMSDK_VERSION" >/dev/null
fi
source "$EMSDK_DIR/emsdk_env.sh" >/dev/null 2>&1

V="${1:?usage: build.sh <variant>}"
OPT="-Oz"; LINK=""; SB=""; SBX=""; LTO=""; RTTI=""
case "$V" in
  full-o3)           OPT="-O3"; LINK="core_functions parquet json" ;;
  full)              LINK="core_functions parquet json" ;;
  full-sb)           LINK="core_functions parquet json"; SB="ON" ;;
  full-sb-keep)      LINK="core_functions parquet json"; SB="ON"; SBX="window_specialization,sort_specialization" ;;
  full-sb-lto)       LINK="core_functions parquet json"; SB="ON"; LTO="-flto" ;;
  core)              LINK="core_functions" ;;
  core-sb)           LINK="core_functions"; SB="ON" ;;
  parquet-sb)        LINK="parquet"; SB="ON" ;;
  parquet-sb-lto)    LINK="parquet"; SB="ON"; LTO="-flto" ;;
  json-sb)           LINK="json"; SB="ON" ;;
  min)               ;;
  min-sb)            SB="ON" ;;
  min-sb-lto)        SB="ON"; LTO="-flto" ;;
  min-sb-thinlto)    SB="ON"; LTO="-flto=thin" ;;
  min-sb-lto-nortti) SB="ON"; LTO="-flto"; RTTI=1 ;;
  *) echo "unknown variant $V"; exit 1 ;;
esac
ALL="core_functions parquet json"
SKIP=""; for e in $ALL; do [[ " $LINK " == *" $e "* ]] || SKIP="$SKIP;$e"; done
# -ffile-prefix-map keeps local paths (from __FILE__) out of the published binary.
export EMCC_CFLAGS="-fwasm-exceptions -sWASM_LEGACY_EXCEPTIONS=0 $OPT $LTO -ffile-prefix-map=$HOME=~"
B="$HERE/build/$V"
mkdir -p "$B"
TARGETS="duckdb_static"; for e in $LINK; do TARGETS="$TARGETS ${e}_extension"; done
if [ -z "${SKIP_BUILD:-}" ]; then
  emcmake cmake -G Ninja -S "$DUCKDB" -B "$B" \
    -DCMAKE_BUILD_TYPE=Release \
    -DBUILD_SHELL=0 -DBUILD_UNITTESTS=0 -DENABLE_UNITTEST_CPP_TESTS=0 \
    -DDISABLE_THREADS=1 -DDISABLE_EXTENSION_LOAD=1 -DDISABLE_BUILTIN_HTTPLIB=1 \
    -DENABLE_EXTENSION_AUTOLOADING=0 -DENABLE_EXTENSION_AUTOINSTALL=0 \
    -DENABLE_SANITIZER=0 -DENABLE_UBSAN=0 \
    -DDUCKDB_EXPLICIT_PLATFORM=wasm_eh \
    -DBUILD_EXTENSIONS="${LINK// /;}" -DSKIP_EXTENSIONS="${SKIP#;}" \
    ${SB:+-DSMALLER_BINARY=$SB} ${SBX:+-DSMALLER_BINARY_EXCEPT=$SBX} ${RTTI:+-DDISABLE_RTTI=1} > "$B/configure.log" 2>&1
  ( time cmake --build "$B" --target $TARGETS -j "$(getconf _NPROCESSORS_ONLN)" ) > "$B/build.log" 2>&1
fi
# Static extension loader (v2): generated duckdb_register_static_extensions(), archived for the Worker
# (which calls it explicitly); the driver additionally uses the autoregister-before-main stub.
python3 "$DUCKDB/scripts/generate_static_extension_loader.py" -o "$B/static_extension_loader.c" $LINK
emcc -I"$DUCKDB/src/include" -c "$B/static_extension_loader.c" -o "$B/static_extension_loader.o"
rm -f "$B/libduckdb_ext_loader.a"; emar rcs "$B/libduckdb_ext_loader.a" "$B/static_extension_loader.o"
EXT_LIBS=""; for e in $LINK; do EXT_LIBS="$EXT_LIBS $B/extension/$e/lib${e}_extension.a"; done
LIBS="$EXT_LIBS $B/src/libduckdb_static.a $(find "$B/third_party" -name '*.a' | sort)"
em++ -x c "$HERE/driver.c" "$B/static_extension_loader.c" -x none "$DUCKDB/extension/loader/static_extension_autoregister.cpp" \
  -I"$DUCKDB/src/include" -o "$B/driver.js" \
  -Wl,--start-group $LIBS -Wl,--end-group \
  -sALLOW_MEMORY_GROWTH=1 -sENVIRONMENT=node -sASSERTIONS=0 -sEXIT_RUNTIME=1 > "$B/link.log" 2>&1 \
  || { tail -20 "$B/link.log"; exit 1; }
W="$B/driver.wasm"
RAW=$(wc -c < "$W" | tr -d ' '); GZ=$(gzip -9c "$W" | wc -c | tr -d ' '); BR=$(brotli -Zc "$W" 2>/dev/null | wc -c | tr -d ' ')
OUT=$(node "$B/driver.js" "SELECT 42 AS answer UNION ALL SELECT extension_name FROM duckdb_extensions() WHERE loaded" 2>&1 | tr '\n' ' ')
printf "%-18s raw=%9s gz=%9s br=%9s run=[%s]\n" "$V" "$RAW" "$GZ" "$BR" "$OUT" | tee -a "$HERE/results.txt"

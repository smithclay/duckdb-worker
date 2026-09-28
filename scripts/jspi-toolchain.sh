#!/usr/bin/env bash
# Builds the (unreleased) toolchain for the JSPI range-read build into vendor/jspi-toolchain:
#   - emscripten frontend from PR #27699 (REENTRANT_JSPI, includes the JSPI hooks of #27698)
#   - binaryen from PR WebAssembly/binaryen#9102 (--jspi-hooks pass)
#   - LLVM backend from worker-build's pinned emsdk 6.0.10 (run `worker-build --emscripten` once first)
# Then:  eval "$(scripts/jspi-toolchain.sh env)"; npx wrangler dev -c wrangler.jspi.toml
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
T="$ROOT/vendor/jspi-toolchain"
if [ "${1:-}" = "env" ]; then
  echo "export EMSCRIPTEN=$T/emscripten EMSDK=$T/emsdk DUCKDB_SRC=${DUCKDB_SRC:-$ROOT/vendor/duckdb}"
  exit 0
fi
case "$(uname -s)" in Darwin) CACHE="$HOME/Library/Caches/worker-build" ;; *) CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/worker-build" ;; esac
BASE="$CACHE/emsdk-6.0.10"
[ -d "$BASE/upstream/bin" ] || { echo "missing $BASE: run worker-build --emscripten once"; exit 1; }
mkdir -p "$T"
[ -d "$T/binaryen" ] || git clone -q --depth 50 -b jspi-hooks https://github.com/guybedford/binaryen.git "$T/binaryen"
git -C "$T/binaryen" submodule update -q --init --depth 1
cmake -S "$T/binaryen" -B "$T/binaryen/build" -G Ninja -DCMAKE_BUILD_TYPE=Release -DBUILD_TESTS=OFF >/dev/null
ninja -C "$T/binaryen/build" wasm-opt wasm-metadce wasm-emscripten-finalize wasm-split wasm-ctor-eval wasm-merge wasm-as wasm-dis
[ -d "$T/emsdk" ] || cp -R "$BASE" "$T/emsdk"
for tool in wasm-opt wasm-metadce wasm-emscripten-finalize wasm-split wasm-ctor-eval wasm-merge wasm-as wasm-dis; do
  cp "$T/binaryen/build/bin/$tool" "$T/emsdk/upstream/bin/"
done
mkdir -p "$T/emsdk/upstream/lib" && cp "$T/binaryen/build/lib/"libbinaryen.* "$T/emsdk/upstream/lib/"
[ -d "$T/emscripten" ] || git clone -q --depth 50 -b reentrant-jspi https://github.com/guybedford/emscripten.git "$T/emscripten"
(cd "$T/emscripten" && python3 bootstrap.py >/dev/null)
echo "done. eval \"\$(scripts/jspi-toolchain.sh env)\""

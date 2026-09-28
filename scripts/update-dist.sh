#!/usr/bin/env bash
# Rebuilds the Worker from source and refreshes the prebuilt bundle in dist/.
set -euo pipefail
cd "$(dirname "$0")/.."
eval "$(scripts/jspi-toolchain.sh env)"
RUSTFLAGS="--cfg=wasm_bindgen_unstable_jspi --remap-path-prefix=$HOME=~" worker-build --emscripten --release
rm -rf dist && mkdir dist
cp build/index.js build/index_bg.wasm dist/
sed 's|"../build/index.js"|"./index.js"|' src/entry.js > dist/entry.js
if strings dist/index_bg.wasm | grep -q "$HOME"; then echo "warning: local paths in dist/index_bg.wasm"; fi
ls -l dist

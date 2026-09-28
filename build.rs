//! Links a prebuilt DuckDB v2 static build (tiny/build.sh full-sb-keep) into the Worker.
use std::path::PathBuf;

fn main() {
    let dir = PathBuf::from(
        std::env::var("DUCKDB_BUILD_DIR")
            .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/tiny/build/full-sb-keep").into()),
    );
    println!("cargo:rerun-if-env-changed=DUCKDB_BUILD_DIR");
    if std::env::var("TARGET").unwrap() != "wasm32-unknown-emscripten" {
        return;
    }
    let mut libs = vec![dir.join("libduckdb_ext_loader.a")];
    for ext in ["core_functions", "parquet", "json"] {
        let lib = dir.join(format!("extension/{ext}/lib{ext}_extension.a"));
        if lib.exists() {
            libs.push(lib);
        }
    }
    libs.push(dir.join("src/libduckdb_static.a"));
    for entry in std::fs::read_dir(dir.join("third_party")).expect("third_party dir") {
        let sub = entry.unwrap().path();
        for f in std::fs::read_dir(&sub).unwrap() {
            let f = f.unwrap().path();
            if f.extension().is_some_and(|e| e == "a") {
                libs.push(f);
            }
        }
    }
    println!("cargo:rustc-link-arg=-Wl,--start-group");
    for lib in &libs {
        assert!(lib.exists(), "missing {}", lib.display());
        println!("cargo:rerun-if-changed={}", lib.display());
        println!("cargo:rustc-link-arg={}", lib.display());
    }
    println!("cargo:rustc-link-arg=-Wl,--end-group");
    // DuckDB is C++: have emcc pull in libc++/libc++abi like em++ would.
    println!("cargo:rustc-link-arg=-sDEFAULT_TO_CXX");
}

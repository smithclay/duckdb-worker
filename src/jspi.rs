//! JSPI bridge: lets DuckDB's synchronous file reads wait on fetch() range requests.
//!
//! `query_jspi` is a `#[wasm_bindgen(jspi)]` export, so JS gets a Promise and the whole
//! call tree below it may suspend. DuckDB reaches `dw_http_read` from inside
//! `duckdb_query` (via src/jspi_fs.cpp); that calls a `suspending` import, which parks the
//! wasm stack until the fetch() settles and then copies the bytes into DuckDB's buffer.
//!
//! Needs an Emscripten with JSPI lifecycle hooks (`-sREENTRANT_JSPI`) and
//! `--cfg=wasm_bindgen_unstable_jspi`; see wrangler.jspi.toml.
#![allow(deprecated)] // wasm-bindgen marks jspi/suspending as experimental via deprecation warnings

use js_sys::Uint8Array;
use std::cell::Cell;
use wasm_bindgen::prelude::*;

#[wasm_bindgen(inline_js = r#"
export async function dw_fetch_size(url) {
  let r = await fetch(url, { method: "HEAD" });
  const len = r.headers.get("content-length");
  if (r.ok && len !== null) return Number(len);
  // Some servers omit Content-Length on HEAD: ask for one byte and read Content-Range.
  r = await fetch(url, { headers: { Range: "bytes=0-0" } });
  const cr = r.headers.get("content-range");
  await r.body?.cancel();
  if (r.status === 206 && cr) return Number(cr.split("/")[1]);
  return -1;
}
export async function dw_fetch_range(url, start, len) {
  const r = await fetch(url, { headers: { Range: `bytes=${start}-${start + len - 1}` } });
  if (r.status === 206) return new Uint8Array(await r.arrayBuffer());
  if (r.status === 200) {
    // Server ignored Range: fall back to slicing the full body.
    return new Uint8Array(await r.arrayBuffer()).slice(start, start + len);
  }
  throw new Error(`HTTP ${r.status} for ${url} bytes=${start}+${len}`);
}
"#)]
extern "C" {
    #[wasm_bindgen(catch, suspending)]
    fn dw_fetch_size(url: &str) -> Result<f64, JsValue>;
    #[wasm_bindgen(catch, suspending)]
    fn dw_fetch_range(url: &str, start: f64, len: f64) -> Result<Uint8Array, JsValue>;
}

thread_local! {
    static REQUESTS: Cell<u64> = const { Cell::new(0) };
    static BYTES: Cell<u64> = const { Cell::new(0) };
}

unsafe fn url_str<'a>(ptr: *const u8, len: usize) -> &'a str {
    std::str::from_utf8(std::slice::from_raw_parts(ptr, len)).unwrap_or("")
}

#[no_mangle]
pub extern "C" fn dw_http_size(url: *const u8, url_len: usize) -> i64 {
    let url = unsafe { url_str(url, url_len) };
    REQUESTS.set(REQUESTS.get() + 1);
    match dw_fetch_size(url) {
        Ok(n) if n >= 0.0 => n as i64,
        _ => -1,
    }
}

#[no_mangle]
pub extern "C" fn dw_http_read(url: *const u8, url_len: usize, offset: u64, buf: *mut u8, len: u64) -> i64 {
    let url = unsafe { url_str(url, url_len) };
    REQUESTS.set(REQUESTS.get() + 1);
    match dw_fetch_range(url, offset as f64, len as f64) {
        Ok(bytes) if bytes.length() as u64 == len => {
            // Take the destination slice only after resuming: memory may have grown meanwhile.
            let dst = unsafe { std::slice::from_raw_parts_mut(buf, len as usize) };
            bytes.copy_to(dst);
            BYTES.set(BYTES.get() + len);
            len as i64
        }
        Ok(bytes) => bytes.length() as i64,
        Err(e) => {
            web_sys_log(&format!("range read failed: {e:?}"));
            -1
        }
    }
}

fn web_sys_log(msg: &str) {
    worker::console_error!("{msg}");
}

/// Runs `sql` with range-read I/O. Resolves to TSV; rejects with DuckDB's error.
#[wasm_bindgen(jspi)]
pub fn query_jspi(sql: String) -> Result<String, JsValue> {
    REQUESTS.set(0);
    BYTES.set(0);
    let (ok, body) = crate::query(&sql);
    if ok {
        Ok(body)
    } else {
        Err(JsValue::from_str(&body))
    }
}

/// Stats for the last `query_jspi` call, as JSON.
#[wasm_bindgen]
pub fn jspi_stats() -> String {
    format!(
        r#"{{"range_requests":{},"range_bytes":{},"wasm_memory":{},"duckdb_memory":"{}"}}"#,
        REQUESTS.get(),
        BYTES.get(),
        crate::wasm_memory_bytes(),
        crate::duckdb_memory_bytes()
    )
}

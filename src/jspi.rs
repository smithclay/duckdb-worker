//! JSPI bridge: lets DuckDB's synchronous file reads wait on fetch() range requests.
//!
//! `query_jspi` is a `#[wasm_bindgen(jspi)]` export, so JS gets a Promise and the whole
//! call tree below it may suspend. DuckDB reaches `dw_http_read` from inside
//! `duckdb_query` (via src/jspi_fs.cpp); that calls a `suspending` import, which parks the
//! wasm stack until the fetch() settles and then copies the bytes into DuckDB's buffer.
//!
//! Needs an Emscripten with JSPI lifecycle hooks (`-sREENTRANT_JSPI`) and
//! `--cfg=wasm_bindgen_unstable_jspi`; see scripts/jspi-toolchain.sh and wrangler.toml.
#![allow(deprecated)] // wasm-bindgen marks jspi/suspending as experimental via deprecation warnings

use js_sys::Uint8Array;
use std::cell::Cell;
use wasm_bindgen::prelude::*;

#[wasm_bindgen(inline_js = r#"
// Range reads are only trustworthy when the server sends stored bytes. Workers' fetch() always
// negotiates compression and transparently decodes, and on a compressed response Range and
// Content-Range describe the encoded bytes (the decoded body of a partial range comes back
// empty or garbled, and Content-Range reports the encoded size). So probe with a 1 KiB range:
// exactly the requested bytes back means ranges work; anything else means download the whole
// (decoded) body once and serve slices of it for this query.
const PROBE = 1024;
const wholeBodies = new Map();

export function dw_reset_bodies() {
  wholeBodies.clear();
}

async function wholeBody(url) {
  const r = await fetch(url);
  if (!r.ok) throw new Error(`HTTP ${r.status} for ${url}`);
  const body = new Uint8Array(await r.arrayBuffer());
  wholeBodies.set(url, body);
  return body;
}

export async function dw_fetch_size(url) {
  if (wholeBodies.has(url)) return wholeBodies.get(url).length;
  const r = await fetch(url, { headers: { range: `bytes=0-${PROBE - 1}` } });
  if (r.status === 206) {
    const probe = new Uint8Array(await r.arrayBuffer());
    const total = Number(r.headers.get("content-range")?.split("/")[1]);
    if (total > 0 && probe.length === Math.min(PROBE, total)) return total;
  } else {
    await r.body?.cancel();
  }
  return (await wholeBody(url)).length;
}

export async function dw_fetch_range(url, start, len) {
  let body = wholeBodies.get(url);
  if (!body) {
    const r = await fetch(url, { headers: { range: `bytes=${start}-${start + len - 1}` } });
    if (r.status === 206) {
      const part = new Uint8Array(await r.arrayBuffer());
      if (part.length === len) return part;
    } else {
      await r.body?.cancel();
    }
    body = await wholeBody(url);
  }
  return body.slice(start, start + len);
}
"#)]
extern "C" {
    fn dw_reset_bodies();
    #[wasm_bindgen(catch, suspending)]
    fn dw_fetch_size(url: &str) -> Result<f64, JsValue>;
    #[wasm_bindgen(catch, suspending)]
    fn dw_fetch_range(url: &str, start: f64, len: f64) -> Result<Uint8Array, JsValue>;
}

thread_local! {
    static REQUESTS: Cell<u64> = const { Cell::new(0) };
    static BYTES: Cell<u64> = const { Cell::new(0) };
    /// fetch() calls allowed per query (Workers cap subrequests per invocation: 50 on Free).
    static BUDGET: Cell<u64> = const { Cell::new(50) };
}

extern "C" {
    fn dw_reset_http_cache();
}

/// Counts a subrequest, or returns false when the budget is spent (so we fail with a clear
/// error instead of the runtime's "Too many subrequests").
fn take_request() -> bool {
    if REQUESTS.get() >= BUDGET.get() {
        return false;
    }
    REQUESTS.set(REQUESTS.get() + 1);
    true
}

#[no_mangle]
pub extern "C" fn dw_http_budget_remaining() -> i64 {
    BUDGET.get() as i64 - REQUESTS.get() as i64
}

unsafe fn url_str<'a>(ptr: *const u8, len: usize) -> &'a str {
    std::str::from_utf8(std::slice::from_raw_parts(ptr, len)).unwrap_or("")
}

#[no_mangle]
pub extern "C" fn dw_http_size(url: *const u8, url_len: usize) -> i64 {
    let url = unsafe { url_str(url, url_len) };
    if !take_request() {
        return -1;
    }
    match dw_fetch_size(url) {
        Ok(n) if n >= 0.0 => n as i64,
        _ => -1,
    }
}

#[no_mangle]
pub extern "C" fn dw_http_read(url: *const u8, url_len: usize, offset: u64, buf: *mut u8, len: u64) -> i64 {
    let url = unsafe { url_str(url, url_len) };
    if !take_request() {
        return -1;
    }
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

/// Runs `sql` with range-read I/O, making at most `budget` fetch() calls. Resolves to TSV;
/// rejects with DuckDB's error.
#[wasm_bindgen(jspi)]
pub fn query_jspi(sql: String, budget: u32) -> Result<String, JsValue> {
    REQUESTS.set(0);
    BYTES.set(0);
    BUDGET.set(budget as u64);
    // Cached sizes/blocks/bodies are per query: no stale data, no memory held between requests.
    unsafe { dw_reset_http_cache() };
    dw_reset_bodies();
    let (ok, body) = crate::query(&sql);
    unsafe { dw_reset_http_cache() };
    dw_reset_bodies();
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

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
use std::cell::{Cell, RefCell};
use wasm_bindgen::prelude::*;

#[wasm_bindgen(inline_js = r#"
// Range reads are only trustworthy when the server sends stored bytes. Workers' fetch() always
// negotiates compression and transparently decodes, and on a compressed response Range and
// Content-Range describe the encoded bytes (the decoded body of a partial range comes back
// empty or garbled, and Content-Range reports the encoded size). So probe with a 1 KiB range:
// exactly the requested bytes back means ranges work; anything else means download the whole
// (decoded) body once and serve slices of it for this query.
const PROBE = 1024;
// A whole body lives in the JS heap for the rest of the query, next to DuckDB's wasm memory,
// and the isolate has 128 MB for both.
const MAX_WHOLE_BODY = 32 * 1024 * 1024;
const wholeBodies = new Map();
// What each ranged URL looked like at its first response, to catch a file changing mid-query.
const versions = new Map();

export function dw_reset_bodies() {
  wholeBodies.clear();
  versions.clear();
}

// The full size from a 206's Content-Range ("bytes a-b/total"), or null.
function totalSize(r) {
  return r.headers.get("content-range")?.split("/")[1] ?? null;
}

function tooLarge(url) {
  return new Error(`${url} can't be read in ranges (the server compresses it or ignores Range) and is ` +
    `over the ${MAX_WHOLE_BODY >> 20} MB limit for downloading it whole`);
}

async function wholeBody(url) {
  const r = await fetch(url);
  if (!r.ok) {
    await r.body?.cancel();
    throw new Error(`HTTP ${r.status} for ${url}`);
  }
  // Content-Length is the encoded size when compressed, so the limit is also checked while reading.
  if (Number(r.headers.get("content-length")) > MAX_WHOLE_BODY) {
    await r.body?.cancel();
    throw tooLarge(url);
  }
  const chunks = [];
  let size = 0;
  if (r.body) {
    const reader = r.body.getReader();
    for (;;) {
      const { value, done } = await reader.read();
      if (done) break;
      size += value.length;
      if (size > MAX_WHOLE_BODY) {
        await reader.cancel();
        throw tooLarge(url);
      }
      chunks.push(value);
    }
  }
  const body = new Uint8Array(await new Blob(chunks).arrayBuffer());
  wholeBodies.set(url, body);
  return body;
}

// Same file if the total size matches and so does Last-Modified, or the ETag when there is no
// Last-Modified. Last-Modified wins over a differing ETag because load-balanced origins (e.g.
// Apache's inode-based ETags) can disagree on the ETag of one unchanged file.
function checkVersion(url, r) {
  const seen = {
    size: totalSize(r),
    modified: r.headers.get("last-modified"),
    etag: r.headers.get("etag"),
  };
  const first = versions.get(url);
  if (!first) {
    versions.set(url, seen);
    return;
  }
  const differs = (k) => first[k] !== null && seen[k] !== null && first[k] !== seen[k];
  if (differs("size") || (first.modified && seen.modified ? differs("modified") : differs("etag"))) {
    throw new Error(`${url} changed during the query`);
  }
}

export async function dw_fetch_size(url) {
  if (wholeBodies.has(url)) return wholeBodies.get(url).length;
  const r = await fetch(url, { headers: { range: `bytes=0-${PROBE - 1}` } });
  if (r.status === 206) {
    const probe = new Uint8Array(await r.arrayBuffer());
    const total = Number(totalSize(r));
    if (total > 0 && probe.length === Math.min(PROBE, total)) {
      checkVersion(url, r);
      return total;
    }
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
      checkVersion(url, r);
      const part = new Uint8Array(await r.arrayBuffer());
      if (part.length === len) return part;
    } else {
      await r.body?.cancel();
    }
    body = await wholeBody(url);
  }
  return body.subarray(start, start + len);
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
    /// Why the last dw_http_size / dw_http_read failed, for DuckDB's error message.
    static LAST_ERROR: RefCell<String> = const { RefCell::new(String::new()) };
}

fn fail(msg: String) -> i64 {
    worker::console_error!("{msg}");
    LAST_ERROR.set(msg);
    -1
}

fn js_error_message(e: &JsValue) -> String {
    e.dyn_ref::<js_sys::Error>()
        .map(|e| String::from(e.message()))
        .or_else(|| e.as_string())
        .unwrap_or_else(|| format!("{e:?}"))
}

/// Copies the last fetch error into `buf` (truncated to `cap`) and returns its length.
#[no_mangle]
pub extern "C" fn dw_http_last_error(buf: *mut u8, cap: usize) -> usize {
    LAST_ERROR.with_borrow(|msg| {
        let n = msg.len().min(cap);
        unsafe { std::ptr::copy_nonoverlapping(msg.as_ptr(), buf, n) };
        n
    })
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
        return fail(format!("subrequest budget of {} exhausted", BUDGET.get()));
    }
    match dw_fetch_size(url) {
        Ok(n) if n >= 0.0 => n as i64,
        Ok(n) => fail(format!("bad size {n} for {url}")),
        Err(e) => fail(js_error_message(&e)),
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
        Ok(bytes) => fail(format!("{url}: got {} bytes at offset {offset}, wanted {len}", bytes.length())),
        Err(e) => fail(js_error_message(&e)),
    }
}

/// Runs `sql` with range-read I/O, making at most `budget` fetch() calls. Resolves to TSV bytes;
/// rejects with DuckDB's error.
#[wasm_bindgen(jspi)]
pub fn query_jspi(sql: String, budget: u32) -> Result<Uint8Array, JsValue> {
    REQUESTS.set(0);
    BYTES.set(0);
    BUDGET.set(budget as u64);
    LAST_ERROR.set(String::new());
    // Cached sizes/blocks/bodies are per query: no stale data, no memory held between requests.
    unsafe { dw_reset_http_cache() };
    dw_reset_bodies();
    let result = match crate::query(&sql) {
        // The only copy of the result into JS.
        Ok(out) if out.ok => Ok(Uint8Array::from(out.bytes())),
        Ok(out) => Err(JsValue::from_str(&out.text())),
        Err(e) => Err(JsValue::from_str(&e)),
    };
    unsafe { dw_reset_http_cache() };
    dw_reset_bodies();
    result
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

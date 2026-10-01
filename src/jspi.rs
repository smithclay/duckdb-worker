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
// empty or garbled, and Content-Range reports the encoded size). So probe with a suffix range:
// exactly the requested bytes back means ranges work; anything else means download the whole
// (decoded) body once and serve slices of it for this query.
//
// The probe also returns the file's last 1 MiB, which holds a parquet footer (38 KB for the 473 MB
// fhvhv file) or all of a small CSV/JSON file, so the reads that follow often cost no request.
const TAIL = 1024 * 1024;
// fetch() calls since dw_take_fetches last ran: each is a subrequest.
let fetches = 0;

function get(url, init) {
  fetches++;
  return fetch(url, init);
}

export function dw_take_fetches() {
  const n = fetches;
  fetches = 0;
  return n;
}
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

// A partial response whose bytes are the stored file's. With a Content-Encoding (on the fly, or
// files stored gzip-encoded) Range addresses the encoded bytes, which may come back raw.
function rangeOk(r) {
  return r.status === 206 && (r.headers.get("content-encoding") ?? "identity") === "identity";
}

// fetch() drops Content-Encoding after decoding, and a suffix range of a file stored compressed
// comes back as raw encoded bytes of exactly the requested length. When the response varies by
// encoding and has no strong ETag (a strong one names exact bytes; GitHub Pages and jsDelivr send
// weak ones, CloudFront and S3 strong ones), check that a prefix range decodes to exactly the
// bytes asked for.
async function storedBytes(url, r, total) {
  const varies = /accept-encoding/i.test(r.headers.get("vary") ?? "");
  const strong = /^"/.test(r.headers.get("etag") ?? "");
  if (!varies || strong) return true;
  const want = Math.min(16, total);
  const prefix = await get(url, { headers: { range: `bytes=0-${want - 1}` } });
  return rangeOk(prefix) && (await prefix.arrayBuffer()).byteLength === want;
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
  const r = await get(url);
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

// [size, tail bytes or null, ETag, Last-Modified in ms since the epoch or 0]. The validators let
// DuckDB reuse a file's parsed parquet footer across queries; whole bodies get none.
export async function dw_fetch_size(url) {
  const whole = wholeBodies.get(url);
  if (whole) return [whole.length, null, "", 0];
  const r = await get(url, { headers: { range: `bytes=-${TAIL}` } });
  if (rangeOk(r)) {
    const tail = new Uint8Array(await r.arrayBuffer());
    const total = Number(totalSize(r));
    const [, first, last] = r.headers.get("content-range")?.match(/^bytes (\d+)-(\d+)\//) ?? [];
    if (total > 0 && Number(last) === total - 1 && tail.length === total - Number(first) &&
        tail.length === Math.min(TAIL, total) && (await storedBytes(url, r, total))) {
      checkVersion(url, r);
      const modified = Date.parse(r.headers.get("last-modified") ?? "");
      return [total, tail, r.headers.get("etag") ?? "", Number.isNaN(modified) ? 0 : modified];
    }
  } else {
    await r.body?.cancel();
  }
  return [(await wholeBody(url)).length, null, "", 0];
}

// Bytes of a whole body downloaded earlier in this query, without a request; null otherwise.
export function dw_whole_range(url, start, len) {
  return wholeBodies.get(url)?.subarray(start, start + len) ?? null;
}

export async function dw_fetch_range(url, start, len) {
  const r = await get(url, { headers: { range: `bytes=${start}-${start + len - 1}` } });
  if (rangeOk(r)) {
    checkVersion(url, r);
    const part = new Uint8Array(await r.arrayBuffer());
    if (part.length === len) return part;
  } else {
    await r.body?.cancel();
  }
  return (await wholeBody(url)).subarray(start, start + len);
}
"#)]
extern "C" {
    fn dw_reset_bodies();
    #[wasm_bindgen(catch, suspending)]
    fn dw_fetch_size(url: &str) -> Result<js_sys::Array, JsValue>;
    fn dw_whole_range(url: &str, start: f64, len: f64) -> Option<Uint8Array>;
    fn dw_take_fetches() -> u32;
    #[wasm_bindgen(catch, suspending)]
    fn dw_fetch_range(url: &str, start: f64, len: f64) -> Result<Uint8Array, JsValue>;
}

thread_local! {
    static REQUESTS: Cell<u64> = const { Cell::new(0) };
    static BYTES: Cell<u64> = const { Cell::new(0) };
    /// fetch() calls allowed per query (Workers cap subrequests per invocation: 50 on Free).
    static BUDGET: Cell<u64> = const { Cell::new(50) };
    /// Bumped by `abandon_stuck_query`; a fetch that resumes under a newer generation belongs to
    /// an abandoned query, whose file state is gone.
    static GENERATION: Cell<u32> = const { Cell::new(0) };
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

/// Whether the budget allows another fetch() (so we fail with a clear error instead of the
/// runtime's "Too many subrequests"). Fetches are counted after the fact by `count_fetches`.
fn budget_left() -> bool {
    REQUESTS.get() < BUDGET.get()
}

/// Adds the fetch() calls the JS side made since the last count (a probe can make several).
fn count_fetches() {
    REQUESTS.set(REQUESTS.get() + dw_take_fetches() as u64);
}

#[no_mangle]
pub extern "C" fn dw_http_budget_remaining() -> i64 {
    BUDGET.get() as i64 - REQUESTS.get() as i64
}

unsafe fn url_str<'a>(ptr: *const u8, len: usize) -> &'a str {
    std::str::from_utf8(std::slice::from_raw_parts(ptr, len)).unwrap_or("")
}

/// What a size probe learned besides the size (see dw_fetch_size). Mirrors `DwProbe` in
/// src/jspi_fs.cpp.
#[repr(C)]
pub struct DwProbe {
    /// The file's last `tail_len` bytes; C++ copies them out with dw_http_take_tail.
    tail_len: usize,
    etag: [u8; 256],
    etag_len: usize,
    /// Last-Modified in ms since the epoch, or 0.
    last_modified_ms: i64,
}

thread_local! {
    /// The tail from the last successful probe, until C++ takes it.
    static TAIL: RefCell<Option<Uint8Array>> = const { RefCell::new(None) };
}

#[no_mangle]
pub extern "C" fn dw_http_size(url: *const u8, url_len: usize, probe: *mut DwProbe) -> i64 {
    let url = unsafe { url_str(url, url_len) };
    if !budget_left() {
        return fail(format!("subrequest budget of {} exhausted", BUDGET.get()));
    }
    let generation = GENERATION.get();
    let info = dw_fetch_size(url);
    if GENERATION.get() != generation {
        return -1;
    }
    count_fetches();
    let info = match info {
        Ok(info) => info,
        Err(e) => return fail(js_error_message(&e)),
    };
    let size = info.get(0).as_f64().unwrap_or(-1.0);
    if size < 0.0 {
        return fail(format!("bad size {size} for {url}"));
    }
    let tail = info.get(1).dyn_into::<Uint8Array>().ok();
    let etag = info.get(2).as_string().unwrap_or_default();
    let probe = unsafe { &mut *probe };
    probe.tail_len = tail.as_ref().map_or(0, |t| t.length() as usize);
    // Without a tail the probe fell back to downloading the whole body.
    BYTES.set(BYTES.get() + if tail.is_some() { probe.tail_len as u64 } else { size as u64 });
    TAIL.set(tail);
    // An ETag too long for the buffer is dropped rather than truncated, so it never matches wrongly.
    probe.etag_len = if etag.len() <= probe.etag.len() { etag.len() } else { 0 };
    probe.etag[..probe.etag_len].copy_from_slice(&etag.as_bytes()[..probe.etag_len]);
    probe.last_modified_ms = info.get(3).as_f64().unwrap_or(0.0) as i64;
    size as i64
}

/// Copies the last probe's tail (`DwProbe::tail_len` bytes) into `buf`.
#[no_mangle]
pub extern "C" fn dw_http_take_tail(buf: *mut u8, len: usize) {
    if let Some(tail) = TAIL.take() {
        tail.copy_to(unsafe { std::slice::from_raw_parts_mut(buf, len) });
    }
}

#[no_mangle]
pub extern "C" fn dw_http_read(url: *const u8, url_len: usize, offset: u64, buf: *mut u8, len: u64) -> i64 {
    let url = unsafe { url_str(url, url_len) };
    // Slices of a whole body downloaded earlier in the query cost no request.
    let whole = dw_whole_range(url, offset as f64, len as f64);
    let fetched = whole.is_none();
    let bytes = match whole {
        Some(bytes) => Ok(bytes),
        None if !budget_left() => return -1,
        None => {
            let generation = GENERATION.get();
            let bytes = dw_fetch_range(url, offset as f64, len as f64);
            if GENERATION.get() != generation {
                return -1;
            }
            count_fetches();
            bytes
        }
    };
    match bytes {
        Ok(bytes) if bytes.length() as u64 == len => {
            // Take the destination slice only after resuming: memory may have grown meanwhile.
            let dst = unsafe { std::slice::from_raw_parts_mut(buf, len as usize) };
            bytes.copy_to(dst);
            if fetched {
                BYTES.set(BYTES.get() + len);
            }
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
    dw_take_fetches();
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

/// Gives up on a query whose request was killed while it waited on a fetch() that will never
/// settle: later queries get a fresh database, and the old query fails if it ever resumes.
#[wasm_bindgen]
pub fn abandon_stuck_query() {
    GENERATION.set(GENERATION.get() + 1);
    crate::abandon_database();
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

//! DuckDB v2 (tiny static build) inside a Rust Worker on the emscripten target.
//!
//! One in-memory database per isolate, opened on first query. HTTP handling lives in
//! src/entry.js, which calls the `query_jspi` export (src/jspi.rs); DuckDB reads http(s)
//! files itself with fetch() range requests through src/jspi_fs.cpp.

use std::cell::RefCell;
use std::ffi::{c_char, c_void, CStr, CString};

fn main() {}

mod jspi;

#[repr(C)]
struct DuckResult {
    deprecated_column_count: u64,
    deprecated_row_count: u64,
    deprecated_rows_changed: u64,
    deprecated_columns: *mut c_void,
    deprecated_error_message: *mut c_char,
    internal_data: *mut c_void,
}

type Handle = *mut c_void;

extern "C" {
    fn duckdb_register_static_extensions() -> i32;
    fn duckdb_create_config(out: *mut Handle) -> i32;
    fn duckdb_set_config(config: Handle, name: *const c_char, option: *const c_char) -> i32;
    fn duckdb_destroy_config(config: *mut Handle);
    fn duckdb_open_ext(path: *const c_char, out: *mut Handle, config: Handle, err: *mut *mut c_char) -> i32;
    fn duckdb_connect(db: Handle, out: *mut Handle) -> i32;
    fn duckdb_query(con: Handle, sql: *const c_char, out: *mut DuckResult) -> i32;
    fn duckdb_destroy_result(result: *mut DuckResult);
    fn duckdb_result_error(result: *mut DuckResult) -> *const c_char;
    fn duckdb_column_count(result: *mut DuckResult) -> u64;
    fn duckdb_row_count(result: *mut DuckResult) -> u64;
    fn duckdb_column_name(result: *mut DuckResult, col: u64) -> *const c_char;
    fn duckdb_value_varchar(result: *mut DuckResult, col: u64, row: u64) -> *mut c_char;
    fn duckdb_free(ptr: *mut c_void);
    fn dw_register_http_fs(db: Handle);
}

struct Db {
    _db: Handle,
    con: Handle,
}

thread_local! {
    static DB: RefCell<Option<Db>> = const { RefCell::new(None) };
}

unsafe fn cstr(p: *const c_char) -> String {
    if p.is_null() {
        String::new()
    } else {
        CStr::from_ptr(p).to_string_lossy().into_owned()
    }
}

fn open() -> std::result::Result<Db, String> {
    unsafe {
        duckdb_register_static_extensions();
        let mut config: Handle = std::ptr::null_mut();
        duckdb_create_config(&mut config);
        // Workers cap an isolate at 128 MB (JS + wasm); leave headroom for code and JS.
        for (k, v) in [("memory_limit", "64MB"), ("max_temp_directory_size", "0B")] {
            let (k, v) = (CString::new(k).unwrap(), CString::new(v).unwrap());
            if duckdb_set_config(config, k.as_ptr(), v.as_ptr()) != 0 {
                return Err(format!("set_config {k:?} failed"));
            }
        }
        let mut db: Handle = std::ptr::null_mut();
        let mut err: *mut c_char = std::ptr::null_mut();
        let rc = duckdb_open_ext(std::ptr::null(), &mut db, config, &mut err);
        duckdb_destroy_config(&mut config);
        if rc != 0 {
            let msg = cstr(err);
            duckdb_free(err as *mut c_void);
            return Err(format!("open failed: {msg}"));
        }
        // Range-read http(s) FileSystem; only usable from the JSPI export (src/jspi.rs).
        dw_register_http_fs(db);
        let mut con: Handle = std::ptr::null_mut();
        if duckdb_connect(db, &mut con) != 0 {
            return Err("connect failed".into());
        }
        // Range reads: every parquet read is a fetch() subrequest (50 per request on the Free
        // plan). Coalescing column chunks less than 4 MB apart into one read keeps a row group at
        // ~1-2 requests (473 MB / 19 row groups / 3 columns: 59 requests by default, 40 with this).
        let sql = CString::new("SET parquet_prefetch_column_gap = 4194304").unwrap();
        let mut res: DuckResult = std::mem::zeroed();
        let rc = duckdb_query(con, sql.as_ptr(), &mut res);
        duckdb_destroy_result(&mut res);
        if rc != 0 {
            return Err("SET parquet_prefetch_column_gap failed".into());
        }
        Ok(Db { _db: db, con })
    }
}

fn query(sql: &str) -> (bool, String) {
    DB.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            match open() {
                Ok(db) => *slot = Some(db),
                Err(e) => return (false, e),
            }
        }
        let con = slot.as_ref().unwrap().con;
        let Ok(sql) = CString::new(sql) else {
            return (false, "query contains a NUL byte".into());
        };
        unsafe {
            let mut res: DuckResult = std::mem::zeroed();
            if duckdb_query(con, sql.as_ptr(), &mut res) != 0 {
                let msg = cstr(duckdb_result_error(&mut res));
                duckdb_destroy_result(&mut res);
                return (false, msg);
            }
            let (cols, rows) = (duckdb_column_count(&mut res), duckdb_row_count(&mut res));
            let mut out = (0..cols)
                .map(|c| cstr(duckdb_column_name(&mut res, c)))
                .collect::<Vec<_>>()
                .join("\t");
            out.push('\n');
            for r in 0..rows {
                for c in 0..cols {
                    if c > 0 {
                        out.push('\t');
                    }
                    let v = duckdb_value_varchar(&mut res, c, r);
                    out.push_str(if v.is_null() { "NULL" } else { CStr::from_ptr(v).to_str().unwrap_or("?") });
                    duckdb_free(v as *mut c_void);
                }
                out.push('\n');
            }
            duckdb_destroy_result(&mut res);
            (true, out)
        }
    })
}

/// Current wasm linear memory. It only grows, so after a request it is the isolate's peak.
fn wasm_memory_bytes() -> usize {
    core::arch::wasm32::memory_size(0) * 65536
}

/// Bytes DuckDB's buffer manager currently holds, across all tags.
fn duckdb_memory_bytes() -> String {
    let (_, out) = query("SELECT coalesce(sum(memory_usage_bytes), 0) FROM duckdb_memory()");
    out.lines().nth(1).unwrap_or("?").to_string()
}

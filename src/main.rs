//! DuckDB v2 (tiny static build) inside a Rust Worker on the emscripten target.
//!
//! GET  /?q=<sql>   run a query, TSV back
//! POST /           run the body as SQL
//! GET  /           version + loaded extensions
//!
//! One in-memory database per isolate, opened on first request.
//!
//! Remote files: DuckDB's file I/O is synchronous and Workers can only fetch
//! asynchronously, so http(s) URLs in the SQL are downloaded up front with
//! `fetch()` into Emscripten's in-memory filesystem and the SQL is rewritten to
//! read the local copies. Whole files, no range requests.

use std::cell::RefCell;
use std::ffi::{c_char, c_void, CStr, CString};
use worker::*;

fn main() {}

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
        let mut con: Handle = std::ptr::null_mut();
        if duckdb_connect(db, &mut con) != 0 {
            return Err("connect failed".into());
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

/// Downloads every quoted http(s) URL in `sql` into /tmp/remote and returns the
/// rewritten SQL plus the total bytes fetched.
async fn prefetch_remote(sql: &str) -> Result<(String, usize)> {
    let mut out = String::with_capacity(sql.len());
    let mut total = 0;
    let mut n = 0;
    let mut rest = sql;
    let dir = std::path::Path::new("/tmp/remote");
    while let Some(start) = rest.find("'http") {
        let (head, tail) = rest.split_at(start + 1);
        out.push_str(head);
        let end = tail.find('\'').ok_or_else(|| Error::RustError("unterminated URL literal".into()))?;
        let url = &tail[..end];
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            out.push_str(url);
            rest = &tail[end..];
            continue;
        }
        let mut resp = Fetch::Url(Url::parse(url)?).send().await?;
        if resp.status_code() != 200 {
            return Err(Error::RustError(format!("GET {url}: HTTP {}", resp.status_code())));
        }
        let bytes = resp.bytes().await?;
        total += bytes.len();
        // Keep the extension so DuckDB's replacement scans pick the right reader.
        let name = url.split(['?', '#']).next().unwrap().rsplit('/').next().unwrap_or("file");
        std::fs::create_dir_all(dir).map_err(|e| Error::RustError(e.to_string()))?;
        n += 1;
        let local = dir.join(format!("{n}_{name}"));
        std::fs::write(&local, &bytes).map_err(|e| Error::RustError(e.to_string()))?;
        out.push_str(local.to_str().unwrap());
        rest = &tail[end..];
    }
    out.push_str(rest);
    Ok((out, total))
}

#[event(fetch)]
async fn fetch(mut req: Request, _env: Env, _ctx: Context) -> Result<Response> {
    let sql = match req.method() {
        Method::Post => req.text().await?,
        _ => req
            .url()?
            .query_pairs()
            .find(|(k, _)| k == "q")
            .map(|(_, v)| v.into_owned())
            .unwrap_or_else(|| {
                "SELECT version() AS version, (SELECT string_agg(extension_name, ',') FROM duckdb_extensions() WHERE loaded) AS extensions, current_setting('memory_limit') AS memory_limit".into()
            }),
    };
    let (sql, fetched) = match prefetch_remote(&sql).await {
        Ok(v) => v,
        Err(e) => return Ok(Response::ok(e.to_string())?.with_status(502)),
    };
    let (ok, body) = query(&sql);
    let _ = std::fs::remove_dir_all("/tmp/remote");
    let mut resp = Response::ok(body)?.with_status(if ok { 200 } else { 400 });
    resp.headers_mut().set("x-remote-bytes", &fetched.to_string())?;
    Ok(resp)
}

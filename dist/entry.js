// Worker entry. Every query goes through the `query_jspi` export: DuckDB reads http(s) files
// itself with fetch() range requests, suspending on each read via JSPI (src/jspi.rs).
//
//   GET  /?q=<sql>   run a query, TSV back (errors: HTTP 400 with DuckDB's message)
//   POST /           run the body as SQL
//   GET  /           version, loaded extensions, settings
//   &budget=<n>      max fetch() subrequests for the query (default 50, the Free plan's external cap)

// worker-build's shim attaches every #[wasm_bindgen] export to the entrypoint class as a method.
import Entrypoint from "./index.js";

const INFO_SQL = `SELECT version() AS version,
  (SELECT string_agg(extension_name, ',') FROM duckdb_extensions() WHERE loaded) AS extensions,
  current_setting('memory_limit') AS memory_limit,
  current_setting('parquet_prefetch_column_gap') AS parquet_prefetch_column_gap`;

// One DuckDB connection per isolate: run queries one at a time, since a query can be
// suspended mid-read while another request arrives.
let queue = Promise.resolve();

export default class extends Entrypoint {
  async fetch(request) {
    const url = new URL(request.url);
    const sql = request.method === "POST" ? await request.text() : (url.searchParams.get("q") ?? INFO_SQL);
    const budget = Number(url.searchParams.get("budget") ?? 50);
    const run = queue.then(async () => {
      const started = Date.now();
      let body, status = 200;
      try {
        body = await this.query_jspi(sql, budget);
      } catch (e) {
        body = String(e?.message ?? e);
        status = 400;
      }
      const stats = JSON.parse(this.jspi_stats());
      return new Response(body, {
        status,
        headers: {
          "x-range-requests": String(stats.range_requests),
          "x-range-bytes": String(stats.range_bytes),
          "x-range-budget": String(budget),
          "x-wasm-mem-after": String(stats.wasm_memory),
          "x-duckdb-mem": String(stats.duckdb_memory),
          "x-elapsed-ms": String(Date.now() - started),
        },
      });
    });
    queue = run.catch(() => {});
    return run;
  }
}

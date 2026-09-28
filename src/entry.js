// Worker entry for the JSPI build (wrangler.jspi.toml).
//
// ?io=range routes the query through the `query_jspi` export: DuckDB reads http(s) files
// itself with fetch() range requests instead of downloading them first. Everything else
// goes to the Rust #[event(fetch)] handler.
// worker-build's shim attaches every #[wasm_bindgen] export to the entrypoint class as a method.
import Entrypoint from "../build-jspi/index.js";

// One DuckDB connection per isolate: run range queries one at a time, since a query can be
// suspended mid-read while another request arrives.
let queue = Promise.resolve();

export default class extends Entrypoint {
  async fetch(request) {
    const url = new URL(request.url);
    if (url.searchParams.get("io") !== "range") return super.fetch(request);
    const sql = request.method === "POST" ? await request.text() : url.searchParams.get("q");
    const run = queue.then(async () => {
      const started = Date.now();
      let body, status = 200;
      try {
        body = await this.query_jspi(sql ?? "SELECT 42");
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

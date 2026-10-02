// Worker entry. Every query goes through the `query_jspi` export: DuckDB reads http(s) files
// itself with fetch() range requests, suspending on each read via JSPI (src/jspi.rs).
//
//   GET  /?q=<sql>   run one read-only statement, TSV back (errors: HTTP 400 with DuckDB's message)
//   POST /           run the body as SQL
//   GET  /           version, loaded extensions, settings
//   &budget=<n>      max fetch() subrequests for the query (default 50, the Free plan's external cap)

// worker-build's shim attaches every #[wasm_bindgen] export to the entrypoint class as a method.
import Entrypoint from "../build/index.js";

const INFO_SQL = `SELECT version() AS version,
  (SELECT string_agg(extension_name, ',') FROM duckdb_extensions() WHERE loaded) AS extensions,
  current_setting('memory_limit') AS memory_limit,
  current_setting('parquet_prefetch_column_gap') AS parquet_prefetch_column_gap`;

// One DuckDB connection per isolate: run queries one at a time, since a query can be
// suspended mid-read while another request arrives. Past MAX_PENDING, turn requests away rather
// than let them wait behind several multi-second scans (HTTP 429).
const MAX_PENDING = 4;
// A query whose request the runtime killed (1102) stays suspended on a fetch() that never settles.
// After STUCK_MS it is abandoned (abandon_stuck_query) and later queries use a fresh database.
const STUCK_MS = 30_000;
let queue = Promise.resolve();
let pending = 0;
let runningSince = 0;
// Bumped when a stuck query is abandoned, so that query can't touch the new queue's state.
let epoch = 0;

export default class extends Entrypoint {
  async fetch(request) {
    const url = new URL(request.url);
    const sql = request.method === "POST" ? await request.text() : (url.searchParams.get("q") ?? INFO_SQL);
    const budget = Number(url.searchParams.get("budget") ?? 50);
    this.recoverIfStuck();
    if (pending >= MAX_PENDING) {
      return new Response(`Busy: ${pending} queries already running or queued in this isolate\n`, {
        status: 429,
        headers: { "retry-after": "1" },
      });
    }
    pending++;
    const prev = queue;
    const mine = epoch;
    let gaveUp = false;
    const run = prev.then(async () => {
      // Skip queries whose request gave up waiting: they would run without a live request.
      if (gaveUp) return null;
      const started = (runningSince = Date.now());
      let body, status = 200;
      try {
        body = await this.query_jspi(sql, budget);
      } catch (e) {
        body = String(e?.message ?? e);
        status = 400;
      }
      if (mine !== epoch) return new Response("Query abandoned\n", { status: 503 });
      runningSince = 0;
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
    queue = run.catch(() => {}).finally(() => mine === epoch && pending--);
    // If the client goes away mid-query, its request context would be torn down with the query
    // suspended on a fetch() that then never settles, blocking every later request in the isolate.
    this.ctx.waitUntil(run);
    // Waiting behind a query that never finishes would hang this request too.
    let timer;
    const ready = await Promise.race([
      prev.then(() => true),
      new Promise((resolve) => (timer = setTimeout(resolve, STUCK_MS, false))),
    ]);
    clearTimeout(timer);
    if (!ready) {
      gaveUp = true;
      this.recoverIfStuck();
      return new Response("Gave up waiting behind a stuck query; retry\n", { status: 503 });
    }
    return run;
  }

  recoverIfStuck() {
    if (runningSince && Date.now() - runningSince > STUCK_MS) {
      this.abandon_stuck_query();
      epoch++;
      queue = Promise.resolve();
      pending = 0;
      runningSince = 0;
    }
  }
}

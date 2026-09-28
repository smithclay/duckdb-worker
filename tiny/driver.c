// Minimal C API consumer used by build.sh to size and smoke-test each variant.
#include <stdio.h>
#include "duckdb.h"

int main(int argc, char **argv) {
	const char *sql = argc > 1 ? argv[1] : "SELECT 42 AS answer, 'tiny' AS build";
	duckdb_database db;
	duckdb_connection con;
	duckdb_result res;
	if (duckdb_open(NULL, &db) != DuckDBSuccess) { fprintf(stderr, "open failed\n"); return 1; }
	if (duckdb_connect(db, &con) != DuckDBSuccess) { fprintf(stderr, "connect failed\n"); return 1; }
	if (duckdb_query(con, sql, &res) != DuckDBSuccess) {
		fprintf(stderr, "error: %s\n", duckdb_result_error(&res));
		duckdb_destroy_result(&res);
		return 1;
	}
	idx_t cols = duckdb_column_count(&res), rows = duckdb_row_count(&res);
	for (idx_t r = 0; r < rows; r++) {
		for (idx_t c = 0; c < cols; c++) {
			char *v = duckdb_value_varchar(&res, c, r);
			printf("%s%s", c ? "\t" : "", v ? v : "NULL");
			duckdb_free(v);
		}
		printf("\n");
	}
	duckdb_destroy_result(&res);
	duckdb_disconnect(&con);
	duckdb_close(&db);
	return 0;
}

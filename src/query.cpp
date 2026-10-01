// Runs one caller-supplied statement and streams its rows out as TSV.
//
// The database is shared by every request an isolate serves, so callers only get read-only
// statements: exactly one SELECT or EXPLAIN that modifies no database (the configuration is
// locked separately in src/main.rs). Rows are streamed chunk by chunk rather than materialized,
// and the output stops at MAX_RESULT_BYTES.

#include "duckdb.hpp"
#include "duckdb/main/capi/capi_internal.hpp"
#include "duckdb/main/query_result_stream.hpp"

#include <cstdlib>
#include <cstring>

namespace duckdb {

//! The TSV lives in wasm memory (which never shrinks) and is then copied into a JS string.
static constexpr idx_t MAX_RESULT_BYTES = 8 * 1024 * 1024;

static string RunReadOnly(Connection &con, const string &sql) {
	auto statements = con.ExtractStatements(sql);
	if (statements.size() != 1) {
		throw InvalidInputException("Send exactly one statement (got %llu)", statements.size());
	}
	auto type = statements[0]->type;
	if (type != StatementType::SELECT_STATEMENT && type != StatementType::EXPLAIN_STATEMENT) {
		throw PermissionException("Only SELECT and EXPLAIN statements are allowed (got %s)",
		                          StatementTypeToString(type));
	}
	auto prepared = con.Prepare(std::move(statements[0]));
	if (prepared->HasError()) {
		prepared->GetErrorObject().Throw();
	}
	auto properties = prepared->GetStatementProperties();
	if (!properties.IsReadOnly()) {
		throw PermissionException("Only read-only statements are allowed");
	}
	vector<Value> no_parameters;
	auto submitted = prepared->Submit(no_parameters);
	if (submitted->HasError()) {
		submitted->ThrowError();
	}
	QueryResultStream stream(std::move(submitted));

	string out;
	for (idx_t col = 0; col < stream.ColumnCount(); col++) {
		out += (col ? "\t" : "") + stream.ColumnName(col).GetIdentifierName();
	}
	out += '\n';
	while (auto chunk = stream.Fetch()) {
		for (idx_t row = 0; row < chunk->size(); row++) {
			for (idx_t col = 0; col < chunk->ColumnCount(); col++) {
				if (col) {
					out += '\t';
				}
				out += chunk->GetValue(col, row).ToString();
			}
			out += '\n';
		}
		if (out.size() > MAX_RESULT_BYTES) {
			throw OutOfRangeException("Result is larger than %llu MB: add a LIMIT or aggregate",
			                          MAX_RESULT_BYTES >> 20);
		}
	}
	if (stream.HasError()) {
		stream.GetErrorObject().Throw();
	}
	return out;
}

} // namespace duckdb

//! Runs `sql` on `con`. Returns 0 with TSV in *out, or 1 with an error message in *out.
//! *out is malloc'd (free with duckdb_free).
extern "C" int dw_query(duckdb_connection con, const char *sql, size_t sql_len, char **out, size_t *out_len) {
	int rc = 0;
	std::string text;
	try {
		text = duckdb::RunReadOnly(*reinterpret_cast<duckdb::Connection *>(con), std::string(sql, sql_len));
	} catch (std::exception &ex) {
		rc = 1;
		text = duckdb::ErrorData(ex).Message();
	}
	*out = static_cast<char *>(malloc(text.size()));
	memcpy(*out, text.data(), text.size());
	*out_len = text.size();
	return rc;
}

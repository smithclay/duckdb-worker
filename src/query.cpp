// Runs one caller-supplied statement and streams its rows out as TSV.
//
// The database is shared by every request an isolate serves, so callers only get read-only
// statements: exactly one SELECT or EXPLAIN that modifies no database (the configuration is
// locked separately in src/main.rs). Rows are streamed chunk by chunk rather than materialized,
// and the output stops at MAX_RESULT_BYTES. Columns are cast to VARCHAR a vector at a time.

#include "duckdb.hpp"
#include "duckdb/main/capi/capi_internal.hpp"
#include "duckdb/common/vector_operations/vector_operations.hpp"
#include "duckdb/main/query_result_stream.hpp"

namespace duckdb {

//! The TSV lives in wasm memory (which never shrinks) until it is copied into the JS response.
static constexpr idx_t MAX_RESULT_BYTES = 8 * 1024 * 1024;

//! Appends `chunk` as TSV rows, NULL for nulls, stopping at MAX_RESULT_BYTES (checked per row, since
//! one chunk of wide rows can be far larger than the cap).
static void AppendTsv(DataChunk &chunk, string &out) {
	vector<Vector> text;
	vector<UnifiedVectorFormat> formats(chunk.ColumnCount());
	for (idx_t col = 0; col < chunk.ColumnCount(); col++) {
		auto &source = chunk.data[col];
		if (source.GetType().id() == LogicalTypeId::VARCHAR) {
			source.ToUnifiedFormat(chunk.size(), formats[col]);
			continue;
		}
		text.emplace_back(LogicalType::VARCHAR, chunk.size());
		VectorOperations::DefaultCast(source, text.back(), chunk.size());
		text.back().ToUnifiedFormat(chunk.size(), formats[col]);
	}
	for (idx_t row = 0; row < chunk.size(); row++) {
		for (idx_t col = 0; col < chunk.ColumnCount(); col++) {
			if (col) {
				out += '\t';
			}
			auto &format = formats[col];
			auto idx = format.sel->get_index(row);
			if (!format.validity.RowIsValid(idx)) {
				out += "NULL";
				continue;
			}
			auto value = UnifiedVectorFormat::GetData<string_t>(format)[idx];
			out.append(value.GetData(), value.GetSize());
		}
		out += '\n';
		if (out.size() > MAX_RESULT_BYTES) {
			throw OutOfRangeException("Result is larger than %llu MB: add a LIMIT or aggregate",
			                          MAX_RESULT_BYTES >> 20);
		}
	}
}

static void RunReadOnly(Connection &con, const string &sql, string &out) {
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

	for (idx_t col = 0; col < stream.ColumnCount(); col++) {
		out += (col ? "\t" : "") + stream.ColumnName(col).GetIdentifierName();
	}
	out += '\n';
	while (auto chunk = stream.Fetch()) {
		AppendTsv(*chunk, out);
	}
	if (stream.HasError()) {
		stream.GetErrorObject().Throw();
	}
}

} // namespace duckdb

//! Runs `sql` on `con`, setting *ok and pointing *data/*size at TSV or an error message. Returns the
//! buffer that owns them, to release with dw_free_output once copied out.
extern "C" void *dw_query(duckdb_connection con, const char *sql, size_t sql_len, int *ok, const char **data,
                          size_t *size) {
	auto out = new std::string();
	*ok = 1;
	try {
		duckdb::RunReadOnly(*reinterpret_cast<duckdb::Connection *>(con), std::string(sql, sql_len), *out);
	} catch (std::exception &ex) {
		*ok = 0;
		*out = duckdb::ErrorData(ex).Message();
	}
	*data = out->data();
	*size = out->size();
	return out;
}

extern "C" void dw_free_output(void *out) {
	delete static_cast<std::string *>(out);
}

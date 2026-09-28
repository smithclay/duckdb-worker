// Repro for the LTO exception bug: under -flto a thrown duckdb::Exception escapes DuckDB's own
// catch (std::exception &). Link against a tiny/build/*-lto variant and run with "SELECT nosuchfn(1)".
#include <cstdio>
#include <stdexcept>
#include "duckdb.hpp"
int main(int argc, char **argv) {
	try { throw std::runtime_error("self-test"); } catch (std::exception &e) { printf("caught own: %s\n", e.what()); }
	try {
		duckdb::DuckDB db(nullptr);
		duckdb::Connection con(db);
		auto r = con.Query(argv[1]);
		printf("%s\n", r->ToString().c_str());
	} catch (std::exception &e) { printf("escaped std::exception: %s\n", e.what()); }
	catch (...) { printf("escaped unknown\n"); }
}

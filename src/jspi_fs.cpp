// HTTP range-read FileSystem for DuckDB, backed by the Worker's fetch().
//
// DuckDB file I/O is synchronous. dw_http_size / dw_http_read are implemented in Rust
// (src/jspi.rs) and suspend the wasm stack on a fetch() Promise via JSPI, so from DuckDB's
// point of view these are ordinary blocking reads. Only valid inside a JSPI activation
// (the `query_jspi` export); called anywhere else they trap.

#include "duckdb.hpp"
#include "duckdb/common/file_system.hpp"
#include "duckdb/common/string_util.hpp"
#include "duckdb/main/capi/capi_internal.hpp"

#include <algorithm>
#include <cstring>

extern "C" {
//! Total size of the resource in bytes, or -1 on error.
int64_t dw_http_size(const char *url, size_t url_len);
//! Reads [offset, offset + len) into buf; returns bytes read or -1 on error.
int64_t dw_http_read(const char *url, size_t url_len, uint64_t offset, void *buf, uint64_t len);
//! fetch() calls this invocation may still make (Workers cap subrequests per invocation).
int64_t dw_http_budget_remaining();
//! Copies why the last dw_http_size / dw_http_read failed into buf; returns its length.
size_t dw_http_last_error(char *buf, size_t cap);
}

namespace duckdb {

static string LastHttpError() {
	char buf[512];
	return string(buf, dw_http_last_error(buf, sizeof(buf)));
}

//! Each fetch() is a subrequest, and Workers cap those per invocation (50 on the Free plan).
//! Keeping the count low is DuckDB's job: parquet coalesces the column chunks of a row group into
//! one read when the gap between them is below `parquet_prefetch_column_gap`. This FileSystem
//! stays simple: large reads are fetched exactly as asked, one request each, and small reads
//! (footer, page headers) go through a cache of BLOCK-aligned blocks so they share requests.
static constexpr idx_t BLOCK = 1024 * 1024;
//! Scans move forward, so a couple of cached runs is enough.
static constexpr idx_t CACHED_RUNS = 2;

//! One fetch(): a run of consecutive blocks, kept as a single buffer.
struct FetchedRun {
	idx_t first_block;
	idx_t end_block;
	vector<data_t> bytes;
};

struct RemoteFile {
	idx_t size = 0;
	//! Most recently used first.
	vector<FetchedRun> runs;
};

//! Per-query state shared by every handle, so re-opening a URL costs no requests.
struct RemoteCache {
	unordered_map<string, RemoteFile> files;
};
static RemoteCache &Cache() {
	static RemoteCache cache;
	return cache;
}

static RemoteFile *LookupOrProbe(const string &url) {
	auto &files = Cache().files;
	auto entry = files.find(url);
	if (entry != files.end()) {
		return &entry->second;
	}
	auto size = dw_http_size(url.c_str(), url.size());
	if (size < 0) {
		return nullptr;
	}
	auto &file = files[url];
	file.size = NumericCast<idx_t>(size);
	return &file;
}

class RangeFileHandle : public FileHandle {
public:
	RangeFileHandle(FileSystem &fs, const string &path, FileOpenFlags flags, idx_t size)
	    : FileHandle(fs, path, flags), size(size) {
	}
	void Close() override {
	}

	idx_t size;
	idx_t position = 0;
};

class JspiHttpFileSystem : public FileSystem {
public:
	unique_ptr<FileHandle> OpenFile(const string &path, FileOpenFlags flags,
	                                optional_ptr<FileOpener> opener = nullptr) override {
		if (flags.OpenForWriting()) {
			throw NotImplementedException("JspiHttpFileSystem is read-only: %s", path);
		}
		auto file = LookupOrProbe(path);
		if (!file) {
			if (flags.ReturnNullIfNotExists()) {
				return nullptr;
			}
			throw IOException("Can't open '%s': %s", path, LastHttpError());
		}
		return make_uniq<RangeFileHandle>(*this, path, flags, file->size);
	}

	void Read(FileHandle &handle_p, void *buffer, int64_t nr_bytes, idx_t location) override {
		auto &handle = handle_p.Cast<RangeFileHandle>();
		if (nr_bytes <= 0) {
			return;
		}
		auto len = NumericCast<idx_t>(nr_bytes);
		if (location + len > handle.size) {
			throw IOException("Read past end of '%s' (%llu + %llu > %llu)", handle.path, location, len, handle.size);
		}
		auto &file = *LookupOrProbe(handle.path);
		auto out = static_cast<data_ptr_t>(buffer);
		if (len >= BLOCK) {
			// Column chunks (already coalesced by parquet): straight into DuckDB's buffer.
			CheckedRead(handle.path, file, out, location, len);
			return;
		}
		idx_t first = location / BLOCK;
		idx_t last = (location + len - 1) / BLOCK;
		for (idx_t block = first; block <= last; block++) {
			auto run = FindRun(file, block);
			if (!run) {
				// Fetch this block and any following uncached blocks of the read in one request.
				idx_t run_end = block + 1;
				while (run_end <= last && !FindRun(file, run_end)) {
					run_end++;
				}
				run = FetchRun(handle.path, file, block, run_end);
			}
			idx_t block_start = block * BLOCK;
			idx_t block_end = MinValue(block_start + BLOCK, file.size);
			idx_t copy_start = MaxValue(location, block_start);
			idx_t copy_end = MinValue(location + len, block_end);
			idx_t run_start = run->first_block * BLOCK;
			memcpy(out + (copy_start - location), run->bytes.data() + (copy_start - run_start), copy_end - copy_start);
		}
	}

	int64_t Read(FileHandle &handle_p, void *buffer, int64_t nr_bytes) override {
		auto &handle = handle_p.Cast<RangeFileHandle>();
		auto remaining = NumericCast<int64_t>(handle.size - handle.position);
		auto to_read = MinValue<int64_t>(nr_bytes, remaining);
		Read(handle, buffer, to_read, handle.position);
		handle.position += NumericCast<idx_t>(to_read);
		return to_read;
	}

	int64_t GetFileSize(FileHandle &handle) override {
		return NumericCast<int64_t>(handle.Cast<RangeFileHandle>().size);
	}
	timestamp_t GetLastModifiedTime(FileHandle &handle) override {
		// Unknown over plain HTTP here; only used for cache validation.
		return timestamp_t::epoch();
	}
	string GetVersionTag(FileHandle &handle) override {
		return "";
	}
	FileType GetFileType(FileHandle &handle) override {
		return FileType::FILE_TYPE_REGULAR;
	}
	void Seek(FileHandle &handle, idx_t location) override {
		handle.Cast<RangeFileHandle>().position = location;
	}
	idx_t SeekPosition(FileHandle &handle) override {
		return handle.Cast<RangeFileHandle>().position;
	}
	bool CanSeek() override {
		return true;
	}
	bool OnDiskFile(FileHandle &handle) override {
		return false;
	}
	bool FileExists(const string &filename, optional_ptr<FileOpener> opener = nullptr) override {
		return LookupOrProbe(filename) != nullptr;
	}
	vector<OpenFileInfo> Glob(const string &path, FileOpener *opener = nullptr) override {
		// No listing over HTTP: a URL names exactly one file.
		return {OpenFileInfo(path)};
	}
	bool CanHandleFile(const string &fpath) override {
		return StringUtil::StartsWith(fpath, "https://") || StringUtil::StartsWith(fpath, "http://");
	}
	string GetName() const override {
		return "JspiHttpFileSystem";
	}

private:
	static FetchedRun *FindRun(RemoteFile &file, idx_t block) {
		for (idx_t i = 0; i < file.runs.size(); i++) {
			if (block >= file.runs[i].first_block && block < file.runs[i].end_block) {
				if (i > 0) {
					std::rotate(file.runs.begin(), file.runs.begin() + NumericCast<int64_t>(i),
					            file.runs.begin() + NumericCast<int64_t>(i) + 1);
				}
				return &file.runs[0];
			}
		}
		return nullptr;
	}

	static FetchedRun *FetchRun(const string &url, RemoteFile &file, idx_t first, idx_t end) {
		// Evict before fetching so at most CACHED_RUNS buffers are ever resident.
		while (file.runs.size() > CACHED_RUNS - 1) {
			file.runs.pop_back();
		}
		idx_t start = first * BLOCK;
		idx_t stop = MinValue(end * BLOCK, file.size);
		FetchedRun run {first, end, vector<data_t>(stop - start)};
		CheckedRead(url, file, run.bytes.data(), start, stop - start);
		file.runs.insert(file.runs.begin(), std::move(run));
		return &file.runs[0];
	}

	static void CheckedRead(const string &url, RemoteFile &file, data_ptr_t out, idx_t start, idx_t len) {
		auto got = dw_http_read(url.c_str(), url.size(), start, out, len);
		if (got == NumericCast<int64_t>(len)) {
			return;
		}
		if (dw_http_budget_remaining() <= 0) {
			throw IOException("Subrequest budget exhausted reading '%s' (Workers Free plan: 50 per request). Project "
			                  "fewer columns, raise parquet_prefetch_column_gap, or pass a higher budget on a paid plan",
			                  url);
		}
		throw IOException("HTTP range read of '%s' [%llu, +%llu) failed: %s", url, start, len, LastHttpError());
	}
};

} // namespace duckdb

extern "C" void dw_reset_http_cache() {
	duckdb::Cache().files.clear();
}

extern "C" void dw_register_http_fs(duckdb_database db) {
	auto wrapper = reinterpret_cast<duckdb::DatabaseWrapper *>(db);
	wrapper->database->instance->GetFileSystem().RegisterSubSystem(duckdb::make_uniq<duckdb::JspiHttpFileSystem>());
}

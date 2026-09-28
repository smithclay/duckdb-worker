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

#include <cstring>

extern "C" {
//! Total size of the resource in bytes, or -1 on error.
int64_t dw_http_size(const char *url, size_t url_len);
//! Reads [offset, offset + len) into buf; returns bytes read or -1 on error.
int64_t dw_http_read(const char *url, size_t url_len, uint64_t offset, void *buf, uint64_t len);
}

namespace duckdb {

//! Every read is a fetch() subrequest, and Workers cap those per invocation (50 on the Free
//! plan), so reads are served from one read-ahead window per handle: small reads (footer, page
//! headers) pull in SMALL_WINDOW, large ones (column chunks) at least LARGE_WINDOW, and the
//! following reads of the same row group usually land inside it.
static constexpr idx_t SMALL_WINDOW = 4 * 1024 * 1024;
static constexpr idx_t LARGE_WINDOW = 16 * 1024 * 1024;

class RangeFileHandle : public FileHandle {
public:
	RangeFileHandle(FileSystem &fs, const string &path, FileOpenFlags flags, idx_t size)
	    : FileHandle(fs, path, flags), size(size) {
	}
	void Close() override {
		window.clear();
		window.shrink_to_fit();
	}

	idx_t size;
	idx_t position = 0;
	idx_t window_start = 0;
	vector<data_t> window;
};

class JspiHttpFileSystem : public FileSystem {
public:
	unique_ptr<FileHandle> OpenFile(const string &path, FileOpenFlags flags,
	                                optional_ptr<FileOpener> opener = nullptr) override {
		if (flags.OpenForWriting()) {
			throw NotImplementedException("JspiHttpFileSystem is read-only: %s", path);
		}
		auto size = dw_http_size(path.c_str(), path.size());
		if (size < 0) {
			if (flags.ReturnNullIfNotExists()) {
				return nullptr;
			}
			throw IOException("HTTP request for size of '%s' failed", path);
		}
		return make_uniq<RangeFileHandle>(*this, path, flags, NumericCast<idx_t>(size));
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
		bool hit = location >= handle.window_start && location + len <= handle.window_start + handle.window.size();
		if (!hit) {
			idx_t window = len < SMALL_WINDOW ? SMALL_WINDOW : MaxValue<idx_t>(len, LARGE_WINDOW);
			// Near the end of the file, align the window to the end so the footer and its length share a request.
			idx_t start = location + window > handle.size ? (handle.size > window ? handle.size - window : 0) : location;
			start = MinValue<idx_t>(start, location);
			idx_t end = MinValue<idx_t>(MaxValue<idx_t>(start + window, location + len), handle.size);
			handle.window.clear();
			handle.window.shrink_to_fit(); // release the old window before fetching the next one
			handle.window.resize(end - start);
			FetchExact(handle, handle.window.data(), start, end - start);
			handle.window_start = start;
		}
		memcpy(buffer, handle.window.data() + (location - handle.window_start), len);
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
		return dw_http_size(filename.c_str(), filename.size()) >= 0;
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
	static void FetchExact(RangeFileHandle &handle, data_ptr_t out, idx_t offset, idx_t len) {
		auto got = dw_http_read(handle.path.c_str(), handle.path.size(), offset, out, len);
		if (got != NumericCast<int64_t>(len)) {
			throw IOException("HTTP range read of '%s' [%llu, +%llu) failed (got %lld)", handle.path, offset, len,
			                  static_cast<long long>(got));
		}
	}
};

} // namespace duckdb

extern "C" void dw_register_http_fs(duckdb_database db) {
	auto wrapper = reinterpret_cast<duckdb::DatabaseWrapper *>(db);
	wrapper->database->instance->GetFileSystem().RegisterSubSystem(duckdb::make_uniq<duckdb::JspiHttpFileSystem>());
}

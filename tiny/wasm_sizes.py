#!/usr/bin/env python3
"""Attribute wasm code-section bytes to demangled function names (needs --profiling-funcs)."""
import sys, re, subprocess, collections
data = open(sys.argv[1], 'rb').read()
def uleb(b, i):
    r = s = 0
    while True:
        x = b[i]; i += 1; r |= (x & 0x7f) << s; s += 7
        if x < 0x80: return r, i
i = 8; sections = {}; imports_funcs = 0; bodies = []; names = {}; secsizes = collections.Counter()
while i < len(data):
    sid = data[i]; i += 1; size, i = uleb(data, i); start = i; end = i + size
    if sid == 0:
        nlen, j = uleb(data, i); nm = data[j:j+nlen].decode(); j += nlen
        secsizes['custom:' + nm] += size
        if nm == 'name':
            while j < end:
                sub = data[j]; j += 1; ssz, j = uleb(data, j); sEnd = j + ssz
                if sub == 1:
                    cnt, j = uleb(data, j)
                    for _ in range(cnt):
                        idx, j = uleb(data, j); l, j = uleb(data, j); names[idx] = data[j:j+l].decode('utf8', 'replace'); j += l
                j = sEnd
    else:
        secsizes[{1:'type',2:'import',3:'function',4:'table',5:'memory',6:'global',7:'export',9:'elem',10:'code',11:'data',12:'datacount',13:'tag'}.get(sid, str(sid))] += size
        if sid == 2:
            cnt, j = uleb(data, i)
            for _ in range(cnt):
                l, j = uleb(data, j); j += l; l, j = uleb(data, j); j += l
                kind = data[j]; j += 1
                if kind == 0: _, j = uleb(data, j); imports_funcs += 1
                elif kind == 1: j += 1; fl = data[j]; j += 1; _, j = uleb(data, j); j = uleb(data, j)[1] if fl & 1 else j
                elif kind == 2: fl = data[j]; j += 1; _, j = uleb(data, j); j = uleb(data, j)[1] if fl & 1 else j
                elif kind == 3: j += 2
                elif kind == 4: j += 1; _, j = uleb(data, j)
        if sid == 10:
            cnt, j = uleb(data, i)
            for _ in range(cnt):
                bs, j = uleb(data, j); bodies.append(bs); j += bs
    i = end
print("== sections"); [print(f"{v:>10}  {k}") for k, v in secsizes.most_common()]
funcs = [(names.get(imports_funcs + k, f"f{k}"), s) for k, s in enumerate(bodies)]
def bucket(n):
    n = re.sub(r'\(.*', '', n)
    m = re.match(r'(duckdb::|duckdb_re2::|duckdb_zstd::|duckdb_fmt::|duckdb_mbedtls::|duckdb_miniz::|duckdb_yyjson::|duckdb_fsst::|duckdb_hll::|duckdb_skiplistlib::|std::__2::|icu_)', n)
    return m.group(1) if m else ('C/other')
top = collections.Counter(); byns = collections.Counter()
for n, s in funcs: byns[bucket(n)] += s
print("\n== code by namespace"); [print(f"{v:>10}  {k}") for k, v in byns.most_common(15)]
# group duckdb functions by first identifier after namespace / template head
g = collections.Counter()
for n, s in funcs:
    if n.startswith('duckdb::'):
        key = re.sub(r'<.*', '', re.sub(r'\(.*', '', n)[8:]).split('::')[0]
        g[key] += s
print("\n== top duckdb:: classes/functions"); [print(f"{v:>10}  {k}") for k, v in g.most_common(int(sys.argv[2]) if len(sys.argv) > 2 else 40)]

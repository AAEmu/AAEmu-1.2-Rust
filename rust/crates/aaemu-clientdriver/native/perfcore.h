// Pure parts of the client performance patch set: byte patterns, the patch switch syntax, the
// fast _splitpath, the display-mode key, the pak directory child index and the flush-flag waits.
// No hooking and no game state, so perfpatch-test.cpp can check all of it without a client.
#pragma once
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <string>
#include <unordered_map>
#include <vector>

namespace perfcore {

// ---- byte patterns ("8B 45 ?? 00"; "??" = wildcard for relocated absolute operands) -------------
struct Pattern { std::vector<uint8_t> v, mask; };

inline bool parse_pattern(const char* text, Pattern& out) {
    out.v.clear(); out.mask.clear();
    for (const char* p = text; *p;) {
        if (*p == ' ') { ++p; continue; }
        if (p[0] == '?' && p[1] == '?') { out.v.push_back(0); out.mask.push_back(0); p += 2; continue; }
        char b[3] = { p[0], p[1], 0 };
        if (!p[1]) return false;
        char* end = NULL; unsigned long x = strtoul(b, &end, 16);
        if (end != b + 2) return false;
        out.v.push_back((uint8_t)x); out.mask.push_back(1);
        p += 2;
    }
    return !out.v.empty();
}

inline bool matches(const uint8_t* mem, const Pattern& p) {
    for (size_t i = 0; i < p.v.size(); ++i) if (p.mask[i] && mem[i] != p.v[i]) return false;
    return true;
}

// Bytes that a replacement actually changes. A patch that changes one byte is a single atomic
// store, so it is safe on code another thread may be running (a late attach).
inline size_t changed_bytes(const uint8_t* current, const Pattern& replacement) {
    size_t n = 0;
    for (size_t i = 0; i < replacement.v.size(); ++i) if (current[i] != replacement.v[i]) ++n;
    return n;
}

// The check window with the replacement laid over it matches the memory: the patch is already in
// place (applied by an earlier driver instance or by pakio's diagnostic toggle).
inline bool already_applied(const uint8_t* base, uint32_t checkRva, const Pattern& expect,
                            uint32_t patchRva, const Pattern& replacement) {
    for (size_t i = 0; i < replacement.v.size(); ++i) if (base[patchRva + i] != replacement.v[i]) return false;
    for (size_t i = 0; i < expect.v.size(); ++i) {
        uint32_t at = checkRva + (uint32_t)i;
        if (at >= patchRva && at < patchRva + replacement.v.size()) continue;
        if (expect.mask[i] && base[at] != expect.v[i]) return false;
    }
    return true;
}

// ---- switches --------------------------------------------------------------------------------
// One syntax for aadriver.ini, AADRIVER_PATCHES and -aaemu-patches=: comma- or space-separated tokens,
// applied in order. "default" = table defaults, "none" = everything off, "all" = everything on,
// "id" or "+id" = on, "-id" = off, "key=value" = option.
struct Switches {
    int base = 0;                                   // 0 table default, 1 all on, -1 all off
    std::unordered_map<std::string, bool> explicitly;
    std::unordered_map<std::string, std::string> options;
};

inline std::string lower(std::string s) { for (auto& c : s) c = (char)tolower((unsigned char)c); return s; }

inline void apply_tokens(Switches& s, const std::string& text) {
    std::string tok;
    auto flush = [&]() {
        if (tok.empty()) return;
        std::string t = tok; tok.clear();
        size_t eq = t.find('=');
        if (eq != std::string::npos) { s.options[lower(t.substr(0, eq))] = t.substr(eq + 1); return; }
        t = lower(t);
        if (t == "default") { s.base = 0; s.explicitly.clear(); }
        else if (t == "none") { s.base = -1; s.explicitly.clear(); }
        else if (t == "all") { s.base = 1; s.explicitly.clear(); }
        else if (t[0] == '-') { if (t.size() > 1) s.explicitly[t.substr(1)] = false; }
        else if (t[0] == '+') { if (t.size() > 1) s.explicitly[t.substr(1)] = true; }
        else s.explicitly[t] = true;
    };
    for (char c : text) { if (c == ',' || c == ' ' || c == ';' || c == '\t' || c == '\r' || c == '\n') flush(); else tok += c; }
    flush();
}

inline bool enabled(const Switches& s, const char* id, bool tableDefault) {
    auto it = s.explicitly.find(id);
    if (it != s.explicitly.end()) return it->second;
    return s.base == 0 ? tableDefault : s.base > 0;
}

inline int option_int(const Switches& s, const char* key, int fallback, int lo, int hi) {
    auto it = s.options.find(key);
    if (it == s.options.end()) return fallback;
    char* end = NULL; long v = strtol(it->second.c_str(), &end, 10);
    if (!end || *end || v < lo || v > hi) return fallback;
    return (int)v;
}

// "-aaemu-patches=a,-b" style token from a command line; empty when absent. Several tokens are joined.
inline std::string command_line_value(const wchar_t* cmd, const wchar_t* key) {
    std::string out;
    size_t klen = wcslen(key);
    for (const wchar_t* p = cmd; p && (p = wcsstr(p, key)) != NULL; p += klen) {
        if (p != cmd && p[-1] != L' ' && p[-1] != L'"' && p[-1] != L'\t') continue;
        const wchar_t* v = p + klen;
        std::string part;
        while (*v && *v != L' ' && *v != L'"' && *v != L'\t') { part += (char)(*v < 128 ? *v : '?'); ++v; }
        if (!out.empty()) out += ',';
        out += part;
    }
    return out;
}

// ---- fast _splitpath (crysystem's pak tree builder calls it for every directory level) ---------
// Same results as the MSVCR100 version: "X:" drive when the second character is a colon; the last
// '/' or '\' ends the directory (kept in dir); the last '.' after it starts the extension; a trail
// byte of a double-byte character is never taken for a separator. Lead bytes come from a table
// built once from the multibyte code page instead of a per-character locale lookup. Returns false
// when a component would not fit its _MAX_* buffer: the CRT then clears every output and sets
// errno, so the caller must let the original handle that input (nothing has been written yet).
struct LeadBytes { bool lead[256]; bool any; };

inline void lead_bytes_from_codepage(LeadBytes& t, UINT cp) {
    memset(&t, 0, sizeof t);
    CPINFO ci = {};
    if (!GetCPInfo(cp, &ci)) return;
    for (int i = 0; i + 1 < MAX_LEADBYTES && (ci.LeadByte[i] || ci.LeadByte[i + 1]); i += 2)
        for (int b = ci.LeadByte[i]; b <= ci.LeadByte[i + 1]; ++b) { t.lead[b] = true; t.any = true; }
}

inline void copy_part(char* out, const char* from, size_t len) {
    if (!out) return;
    memcpy(out, from, len);
    out[len] = 0;
}

inline bool fast_splitpath(const LeadBytes& t, const char* path, char* drive, char* dir, char* fname, char* ext) {
    if (!path) return false;
    const char* s = path;
    bool hasDrive = s[0] && s[1] == ':';
    if (hasDrive) s += 2;
    const char* lastSlash = NULL; const char* dot = NULL; const char* p = s;
    if (t.any) {
        for (; *p; ++p) {
            unsigned char c = (unsigned char)*p;
            if (t.lead[c]) { if (p[1]) ++p; continue; }
            if (c == '/' || c == '\\') lastSlash = p + 1; else if (c == '.') dot = p;
        }
    } else {
        for (; *p; ++p) { char c = *p; if (c == '/' || c == '\\') lastSlash = p + 1; else if (c == '.') dot = p; }
    }
    const char* name = lastSlash ? lastSlash : s;
    bool hasExt = dot && dot >= name;
    size_t dirLen = (size_t)(name - s), nameLen = (size_t)((hasExt ? dot : p) - name), extLen = hasExt ? (size_t)(p - dot) : 0;
    if (dirLen >= _MAX_DIR || nameLen >= _MAX_FNAME || extLen >= _MAX_EXT) return false;
    if (drive) { if (hasDrive) copy_part(drive, path, 2); else *drive = 0; }
    copy_part(dir, s, dirLen);
    copy_part(fname, name, nameLen);
    copy_part(ext, name + nameLen, extLen);
    return true;
}

// ---- display-mode test cache key ---------------------------------------------------------------
// CSystem's resolution list test-switches every enumerated mode with CDS_TEST|CDS_FULLSCREEN while
// only filling width/height/bpp: identical questions get identical answers during one run.
inline uint64_t mode_key(DWORD fields, DWORD w, DWORD h, DWORD bpp, DWORD hz, DWORD flags) {
    if (!(fields & DM_DISPLAYFREQUENCY)) hz = 0;
    if (!(fields & DM_BITSPERPEL)) bpp = 0;
    if (!(fields & DM_PELSWIDTH)) w = 0;
    if (!(fields & DM_PELSHEIGHT)) h = 0;
    return ((uint64_t)(w & 0xFFFF) << 48) | ((uint64_t)(h & 0xFFFF) << 32) | ((uint64_t)(bpp & 0xFF) << 24) |
           ((uint64_t)(hz & 0x3FF) << 14) | ((uint64_t)((fields >> 16) & 0xFF) << 6) | (flags & 0x3F);
}

// ---- pak directory child index -----------------------------------------------------------------
// crysystem DirNode (r208022 FUN_36517720 = FindChild): +0x10 std::list sentinel {next, prev, value},
// +0x14 child count, child name = *(const char**)(child + 4). Children are only appended at the tail
// (FUN_365177d0 inserts before the sentinel and increments the count) and names are unique per
// parent. The index therefore only has to add the newest (count - known) entries, walking back from
// sentinel->prev. A changed sentinel or a shrinking count rebuilds it. Lookup semantics equal the
// original's: count 0, a NULL or empty name -> 0; otherwise the first child with that exact name.
struct ListNode { ListNode* next; ListNode* prev; uint8_t* value; };

inline uint8_t* find_child_linear(const uint8_t* node, const char* name) {
    if (*(const int*)(node + 0x14) == 0 || !name || !*name) return NULL;
    ListNode* head = *(ListNode* const*)(node + 0x10);
    for (ListNode* it = head->next; it != head; it = it->next)
        if (strcmp(*(const char**)(it->value + 4), name) == 0) return it->value;
    return NULL;
}

class ChildIndex {
public:
    uint8_t* find(const uint8_t* node, const char* name) {
        int count = *(const int*)(node + 0x14);
        if (count == 0 || !name || !*name) return NULL;
        ListNode* head = *(ListNode* const*)(node + 0x10);
        Entry& e = nodes_[node];
        if (e.head != head || count < e.known) { e.head = head; e.known = 0; e.byName.clear(); }
        if (count > e.known) {
            // walk back over the new tail entries, then insert them oldest first
            std::vector<uint8_t*> fresh;
            fresh.reserve((size_t)(count - e.known));
            ListNode* it = head->prev;
            for (int i = e.known; i < count && it != head; ++i, it = it->prev) fresh.push_back(it->value);
            for (size_t i = fresh.size(); i-- > 0;) e.byName.emplace(*(const char**)(fresh[i] + 4), fresh[i]);
            e.known = count;
            ++rebuilds_;
        }
        auto hit = e.byName.find(name);
        return hit == e.byName.end() ? NULL : hit->second;
    }
    size_t nodes() const { return nodes_.size(); }
    size_t updates() const { return rebuilds_; }
private:
    struct Entry { ListNode* head = NULL; int known = 0; std::unordered_map<std::string, uint8_t*> byName; };
    std::unordered_map<const uint8_t*, Entry> nodes_;
    size_t rebuilds_ = 0;
};

// ---- flush-flag waits (render-thread event sync) -----------------------------------------------
// SRenderThread::m_nFlush (+0x20): the main thread waits while it is non-zero, the render thread
// while it is zero. A short PAUSE spin keeps the common short wait cheap, then the thread blocks in
// WaitOnAddress. The timeout bounds a missed wake (quit paths clear the flag without a waker).
typedef BOOL (WINAPI* WaitOnAddressFn)(volatile VOID*, PVOID, SIZE_T, DWORD);

inline void wait_while(volatile LONG* flag, LONG busy, int spins, DWORD timeoutMs, WaitOnAddressFn wait) {
    for (int i = 0; i < spins; ++i) { if (*flag != busy) return; YieldProcessor(); }
    LONG expected = busy;
    if (*flag == busy) wait(flag, &expected, sizeof(LONG), timeoutMs);
}
inline void wait_while_nonzero(volatile LONG* flag, int spins, DWORD timeoutMs, WaitOnAddressFn wait) {
    for (int i = 0; i < spins; ++i) { if (*flag == 0) return; YieldProcessor(); }
    LONG v = *flag;
    if (v != 0) wait(flag, &v, sizeof(LONG), timeoutMs);
}

} // namespace perfcore

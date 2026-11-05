// ============================================================================
//  tracecore.h - the parts of the skill trace that need no game memory.
//
//  * a bounded, lock-protected ring buffer of JSON rows (oldest rows are overwritten and
//    counted as dropped; nothing allocates after construction);
//  * the `skillparity/v1` JSON row builder (escaping, locale-proof numbers, UTF-8 safe);
//  * the Unix-microsecond wall clock;
//  * the trace kind mask;
//  * the vtable-slot patch with its build check.
//
//  Header only so that skilltrace-test.cpp can exercise it without a game.
// ============================================================================
#pragma once
#define WIN32_LEAN_AND_MEAN
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <string>

namespace tracecore {

// ---- wall clock -------------------------------------------------------------------------
// FILETIME counts 100 ns since 1601-01-01; the trace wants Unix microseconds.
inline int64_t unix_us_from_filetime(uint64_t filetime100ns) {
    return (int64_t)(filetime100ns / 10) - 11644473600000000LL;
}
inline int64_t now_unix_us() {
    FILETIME ft;
    GetSystemTimePreciseAsFileTime(&ft);
    return unix_us_from_filetime(((uint64_t)ft.dwHighDateTime << 32) | ft.dwLowDateTime);
}

// ---- kinds ------------------------------------------------------------------------------
enum Kind : unsigned { K_ANIM = 1, K_FX = 2, K_SOUND = 4, K_PRESS = 8, K_FRAME = 16 };
static const unsigned kAllKinds = K_ANIM | K_FX | K_SOUND | K_PRESS | K_FRAME;
// `frame` rows are one per Present, so they are opt-in.
static const unsigned kDefaultKinds = K_ANIM | K_FX | K_SOUND | K_PRESS;

inline const char* kind_name(unsigned kind) {
    switch (kind) {
    case K_ANIM: return "anim";
    case K_FX: return "fx";
    case K_SOUND: return "sound";
    case K_PRESS: return "press";
    case K_FRAME: return "frame";
    }
    return "";
}

// Parses a comma separated kind list ("anim,fx"). Empty text selects the default kinds.
// Returns false (and leaves *mask alone) on an unknown or duplicate name.
inline bool parse_kinds(const std::string& text, unsigned* mask) {
    if (text.empty()) { *mask = kDefaultKinds; return true; }
    unsigned out = 0;
    size_t pos = 0;
    while (pos <= text.size()) {
        size_t comma = text.find(',', pos);
        if (comma == std::string::npos) comma = text.size();
        std::string name = text.substr(pos, comma - pos);
        unsigned bit = 0;
        for (unsigned k = 1; k <= K_FRAME; k <<= 1) if (name == kind_name(k)) bit = k;
        if (!bit || (out & bit)) return false;
        out |= bit;
        pos = comma + 1;
    }
    *mask = out;
    return true;
}

// ---- ring buffer ------------------------------------------------------------------------
// Rows are stored as [u16 length][bytes]. A full buffer evicts the oldest rows. All methods
// are safe to call from any thread; push() never allocates.
class Ring {
public:
    explicit Ring(size_t capacity) : cap_(capacity), buf_((unsigned char*)malloc(capacity)) {}
    ~Ring() { free(buf_); }
    Ring(const Ring&) = delete;
    Ring& operator=(const Ring&) = delete;

    bool push(const char* row, size_t len) {
        if (!buf_) return false;
        Guard g(&lock_);
        ++rows_;
        size_t need = len + 2;
        if (len == 0 || len > 0xFFFF || need > cap_) { ++dropped_; return false; }
        while (cap_ - used_ < need) evict_oldest();
        unsigned char header[2] = { (unsigned char)(len & 0xFF), (unsigned char)(len >> 8) };
        size_t tail = (head_ + used_) % cap_;
        copy_in(tail, header, 2);
        copy_in((tail + 2) % cap_, (const unsigned char*)row, len);
        used_ += need;
        ++count_;
        return true;
    }

    // Appends every buffered row, each followed by '\n', to *out and empties the buffer.
    // Returns the number of rows moved.
    size_t drain(std::string* out) {
        Guard g(&lock_);
        size_t n = count_;
        out->reserve(out->size() + used_);
        while (count_) {
            size_t len = peek_len();
            size_t at = (head_ + 2) % cap_;
            size_t first = len < cap_ - at ? len : cap_ - at;
            out->append((const char*)buf_ + at, first);
            if (first < len) out->append((const char*)buf_, len - first);
            out->push_back('\n');
            head_ = (head_ + 2 + len) % cap_;
            used_ -= 2 + len;
            --count_;
        }
        return n;
    }

    // Forgets the buffered rows and zeroes the counters.
    void reset() {
        Guard g(&lock_);
        head_ = used_ = count_ = 0;
        rows_ = dropped_ = 0;
    }

    uint64_t rows() { Guard g(&lock_); return rows_; }          // rows ever pushed since reset
    uint64_t dropped() { Guard g(&lock_); return dropped_; }    // rows lost (overwritten or too long)
    size_t buffered() { Guard g(&lock_); return count_; }
    size_t buffered_bytes() { Guard g(&lock_); return used_; }
    size_t capacity() const { return cap_; }

private:
    struct Guard {
        SRWLOCK* l;
        explicit Guard(SRWLOCK* lock) : l(lock) { AcquireSRWLockExclusive(l); }
        ~Guard() { ReleaseSRWLockExclusive(l); }
    };
    size_t peek_len() const { return (size_t)buf_[head_] | ((size_t)buf_[(head_ + 1) % cap_] << 8); }
    void evict_oldest() {
        size_t len = peek_len();
        head_ = (head_ + 2 + len) % cap_;
        used_ -= 2 + len;
        --count_;
        ++dropped_;
    }
    void copy_in(size_t at, const unsigned char* src, size_t n) {
        size_t first = n < cap_ - at ? n : cap_ - at;
        memcpy(buf_ + at, src, first);
        if (first < n) memcpy(buf_, src + first, n - first);
    }
    SRWLOCK lock_ = SRWLOCK_INIT;
    size_t cap_;
    unsigned char* buf_;
    size_t head_ = 0, used_ = 0, count_ = 0;
    uint64_t rows_ = 0, dropped_ = 0;
};

// ---- JSON -------------------------------------------------------------------------------
// Appends s (at most max bytes, stopping at NUL) as the inside of a JSON string. Quotes,
// backslashes and control characters are escaped; invalid UTF-8 becomes U+FFFD so every row
// is valid JSON whatever the game's strings contain. lower folds ASCII letters (the schema
// compares names lowercase).
inline void json_escape(std::string* out, const char* s, size_t max, bool lower) {
    size_t i = 0;
    while (i < max && s[i]) {
        unsigned char c = (unsigned char)s[i];
        if (c == '"') { out->append("\\\""); ++i; }
        else if (c == '\\') { out->append("\\\\"); ++i; }
        else if (c < 0x20) { char b[8]; _snprintf_s(b, sizeof(b), _TRUNCATE, "\\u%04x", c); out->append(b); ++i; }
        else if (c < 0x80) { out->push_back(lower && c >= 'A' && c <= 'Z' ? (char)(c + 32) : (char)c); ++i; }
        else {
            size_t n = (c >= 0xC2 && c <= 0xDF) ? 2 : (c >= 0xE0 && c <= 0xEF) ? 3 : (c >= 0xF0 && c <= 0xF4) ? 4 : 0;
            bool ok = n && i + n <= max;
            for (size_t k = 1; ok && k < n; ++k) ok = ((unsigned char)s[i + k] & 0xC0) == 0x80;
            if (ok && n == 3) {
                unsigned char c1 = (unsigned char)s[i + 1];
                if ((c == 0xE0 && c1 < 0xA0) || (c == 0xED && c1 > 0x9F)) ok = false;   // overlong / surrogate
            }
            if (ok && n == 4) {
                unsigned char c1 = (unsigned char)s[i + 1];
                if ((c == 0xF0 && c1 < 0x90) || (c == 0xF4 && c1 > 0x8F)) ok = false;
            }
            if (ok) { out->append(s + i, n); i += n; }
            else { out->append("\\ufffd"); ++i; }
        }
    }
}

// One JSON object on one line. Strings are length-limited by the caller (str's `max`), so a
// row stays far below the ring's 64 KiB record limit.
class Row {
public:
    // Starts {"src":"orig","wall_us":..,"frame":..,"kind":".."
    Row(const char* kind, int64_t wall_us, uint32_t frame) {
        s_.reserve(768);
        s_ = "{\"src\":\"orig\",\"wall_us\":";
        num(wall_us);
        s_ += ",\"frame\":";
        num((int64_t)frame);
        s_ += ",\"kind\":\"";
        json_escape(&s_, kind, 32, false);
        s_ += '"';
    }
    Row& str(const char* key, const char* value, bool lower = false, size_t max = 240) {
        key_(key); s_ += '"'; json_escape(&s_, value ? value : "", max, lower); s_ += '"'; return *this;
    }
    Row& i(const char* key, int64_t v) { key_(key); num(v); return *this; }
    Row& u(const char* key, uint64_t v) { key_(key); char b[32]; _snprintf_s(b, sizeof(b), _TRUNCATE, "%llu", (unsigned long long)v); s_ += b; return *this; }
    Row& b(const char* key, bool v) { key_(key); s_ += v ? "true" : "false"; return *this; }
    Row& f(const char* key, double v) { key_(key); real(v); return *this; }
    // A float printed with the fewest digits (6..9) that read back as the same float, so 0.8f is
    // "0.8" and not "0.800000012".
    Row& fl(const char* key, float v) { key_(key); real_float(v); return *this; }
    // A pointer or other machine word as "0x%08x" (lowercase hex).
    Row& hex(const char* key, uintptr_t v) {
        key_(key); char b[24]; _snprintf_s(b, sizeof(b), _TRUNCATE, "\"0x%08x\"", (unsigned)v); s_ += b; return *this;
    }
    Row& vec3(const char* key, float x, float y, float z) {
        key_(key); s_ += '['; real_float(x); s_ += ','; real_float(y); s_ += ','; real_float(z); s_ += ']'; return *this;
    }
    Row& vec4(const char* key, float x, float y, float z, float w) {
        key_(key); s_ += '['; real_float(x); s_ += ','; real_float(y); s_ += ','; real_float(z); s_ += ','; real_float(w);
        s_ += ']'; return *this;
    }
    // Closes the object. The row has no trailing newline.
    const std::string& finish() { if (!done_) { s_ += '}'; done_ = true; } return s_; }

private:
    void key_(const char* key) { s_ += ",\""; json_escape(&s_, key, 48, false); s_ += "\":"; }
    void num(int64_t v) { char b[32]; _snprintf_s(b, sizeof(b), _TRUNCATE, "%lld", (long long)v); s_ += b; }
    void real(double v) {
        if (v != v || v > 1e300 || v < -1e300) { s_ += "null"; return; }
        char b[40];
        _snprintf_s(b, sizeof(b), _TRUNCATE, "%.9g", v);
        for (char* p = b; *p; ++p) if (*p == ',') *p = '.';     // a comma-decimal C locale
        s_ += b;
    }
    void real_float(float v) {
        if (v != v || v > 3e38f || v < -3e38f) { s_ += "null"; return; }
        char b[40];
        for (int precision = 6; precision <= 9; ++precision) {
            _snprintf_s(b, sizeof(b), _TRUNCATE, "%.*g", precision, (double)v);
            if ((float)strtod(b, nullptr) == v) break;
        }
        for (char* p = b; *p; ++p) if (*p == ',') *p = '.';
        s_ += b;
    }
    std::string s_;
    bool done_ = false;
};

// ---- vtable slot patch ------------------------------------------------------------------
struct SlotPatch {
    void** slot = nullptr;
    void* original = nullptr;
    void* hook = nullptr;
    bool installed = false;
};

// Reads *p without trusting it: returns false instead of faulting.
inline bool read_ptr(void* const* p, void** out) {
    __try { *out = *p; return true; } __except (EXCEPTION_EXECUTE_HANDLER) { return false; }
}

// Replaces *slot with hook, but only when it currently holds exactly `expected` (the address
// of the known function in the verified build). Any other value means a different build or a
// foreign patch: nothing is changed and *why says so.
inline bool patch_install(SlotPatch* p, void** slot, void* expected, void* hook, std::string* why) {
    char text[160];
    if (p->installed) { *why = "already installed"; return false; }
    if (!slot || ((uintptr_t)slot & (sizeof(void*) - 1)) != 0) { *why = "misaligned vtable slot"; return false; }
    void* current = nullptr;
    if (!read_ptr(slot, &current)) { *why = "vtable slot is not readable"; return false; }
    if (current != expected) {
        _snprintf_s(text, sizeof(text), _TRUNCATE, "vtable slot holds %p, expected %p (not the verified build)", current, expected);
        *why = text;
        return false;
    }
    DWORD old = 0;
    if (!VirtualProtect(slot, sizeof(void*), PAGE_READWRITE, &old)) {
        _snprintf_s(text, sizeof(text), _TRUNCATE, "VirtualProtect failed (%lu)", GetLastError());
        *why = text;
        return false;
    }
    // Atomic and conditional: a concurrent foreign patch between the check and the store wins.
    void* prior = InterlockedCompareExchangePointer(slot, hook, expected);
    DWORD ignored;
    VirtualProtect(slot, sizeof(void*), old, &ignored);
    if (prior != expected) { *why = "vtable slot changed while patching"; return false; }
    p->slot = slot; p->original = expected; p->hook = hook; p->installed = true;
    return true;
}

// Puts the original pointer back, but only if the slot still holds our hook (a patch applied
// over ours stays). Returns true when the slot is no longer ours.
inline bool patch_remove(SlotPatch* p) {
    if (!p->installed) return true;
    DWORD old = 0;
    bool restored = false;
    if (VirtualProtect(p->slot, sizeof(void*), PAGE_READWRITE, &old)) {
        restored = InterlockedCompareExchangePointer(p->slot, p->original, p->hook) == p->hook;
        DWORD ignored;
        VirtualProtect(p->slot, sizeof(void*), old, &ignored);
    }
    void* now = nullptr;
    bool still_ours = read_ptr(p->slot, &now) && now == p->hook;
    if (restored || !still_ours) { p->installed = false; return true; }
    return false;
}

} // namespace tracecore

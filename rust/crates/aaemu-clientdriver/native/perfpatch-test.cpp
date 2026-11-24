// Tests for the client performance patch set. No game needed: the byte checks, the switch syntax,
// the fast _splitpath, the display-mode key, the pak child index, the flush-flag waits, the jump
// hooks and the whole per-module apply path run against synthetic module images.
#define _CRT_SECURE_NO_WARNINGS
#include "perfpatch.cpp"
#include "startup.cpp"
#include <stdio.h>
#include <stdlib.h>
#include <process.h>
#include <mbctype.h>

using namespace perfpatch;

static int g_failures;
#define CHECK(c) do { if (!(c)) { fprintf(stderr, "perfpatch test failed: %s (line %d)\n", #c, __LINE__); ++g_failures; } } while (0)

static std::vector<std::string> g_logLines;
static void test_logger(const char* fmt, ...) {
    char buf[1024]; va_list ap; va_start(ap, fmt); _vsnprintf_s(buf, sizeof buf, _TRUNCATE, fmt, ap); va_end(ap);
    g_logLines.push_back(buf);
}
static bool logged(const char* needle) { for (auto& l : g_logLines) if (l.find(needle) != std::string::npos) return true; return false; }

// ---- patterns ----------------------------------------------------------------------------------
static void test_patterns() {
    perfcore::Pattern p;
    CHECK(perfcore::parse_pattern("8B 45 ?? 00", p) && p.v.size() == 4 && p.mask[2] == 0 && p.v[0] == 0x8B);
    CHECK(!perfcore::parse_pattern("8B 4", p));
    CHECK(!perfcore::parse_pattern("zz", p));
    CHECK(!perfcore::parse_pattern("", p));
    uint8_t mem[] = { 0x8B, 0x45, 0x77, 0x00 };
    perfcore::parse_pattern("8B 45 ?? 00", p); CHECK(perfcore::matches(mem, p));
    mem[3] = 1; CHECK(!perfcore::matches(mem, p));
    perfcore::Pattern r; perfcore::parse_pattern("8B 45 77 05", r);
    CHECK(perfcore::changed_bytes(mem, r) == 1);
    uint8_t img[16] = { 0x6A, 0x00, 0x68, 0x00, 0x00, 0x01, 0x00, 0x52 };
    perfcore::Pattern expect, bytes;
    perfcore::parse_pattern("6A 00 68 00 00 10 00 52", expect);
    perfcore::parse_pattern("00 00 01 00", bytes);
    CHECK(perfcore::already_applied(img, 0, expect, 3, bytes));
    img[7] = 0x53; CHECK(!perfcore::already_applied(img, 0, expect, 3, bytes));
    // every site row parses, byte patches change at least one byte of their own check window
    for (const perfsites::Site& s : perfsites::kSites) {
        perfcore::Pattern e, b;
        CHECK(perfcore::parse_pattern(s.expect, e));
        if (!s.bytes) continue;
        CHECK(perfcore::parse_pattern(s.bytes, b));
        CHECK(s.patchRva >= s.rva && s.patchRva + b.v.size() <= s.rva + e.v.size());
        size_t diff = 0;
        for (size_t i = 0; i < b.v.size(); ++i) { size_t k = s.patchRva - s.rva + i; if (!e.mask[k] || e.v[k] != b.v[i]) ++diff; }
        CHECK(diff > 0);
        CHECK(index_of(s.id) >= 0 && perfpatch::kPatches[index_of(s.id)].kind == perfpatch::BYTES);
    }
    for (const perfsites::Import& i : perfsites::kImports) CHECK(index_of(i.id) >= 0);
}

// ---- switches ----------------------------------------------------------------------------------
static void test_switches() {
    perfcore::Switches s;
    perfcore::apply_tokens(s, "frame-latency, -fast-splitpath frame_latency=2;+Diag-Resources");
    CHECK(perfcore::enabled(s, "frame-latency", false));
    CHECK(!perfcore::enabled(s, "fast-splitpath", true));
    CHECK(perfcore::enabled(s, "diag-resources", false));
    CHECK(perfcore::enabled(s, "pak-readahead-64k", true) && !perfcore::enabled(s, "pak-prefetch", false));
    CHECK(perfcore::option_int(s, "frame_latency", 1, 1, 16) == 2);
    CHECK(perfcore::option_int(s, "missing", 7, 1, 16) == 7);
    perfcore::apply_tokens(s, "frame_latency=99");
    CHECK(perfcore::option_int(s, "frame_latency", 1, 1, 16) == 1);
    perfcore::apply_tokens(s, "none,shader-async-poll-5ms");
    CHECK(!perfcore::enabled(s, "pak-readahead-64k", true) && perfcore::enabled(s, "shader-async-poll-5ms", true));
    CHECK(!perfcore::enabled(s, "fast-splitpath", true));          // "none" cleared the earlier explicit switch too
    perfcore::apply_tokens(s, "all,-instancing-no-wc-readback");
    CHECK(perfcore::enabled(s, "pak-prefetch", false) && !perfcore::enabled(s, "instancing-no-wc-readback", true));
    perfcore::apply_tokens(s, "default");
    CHECK(perfcore::enabled(s, "instancing-no-wc-readback", true) && !perfcore::enabled(s, "pak-prefetch", false));
    const wchar_t* cmd = L"\"C:\\c\\bin32\\archeage.exe\" -aaemu-fast-pak -aaemu-patches=frame-latency,-fast-splitpath +r_width 800 x-aaemu-patches=bad -aaemu-patches=diag-resources";
    CHECK(perfcore::command_line_value(cmd, L"-aaemu-patches=") == "frame-latency,-fast-splitpath,diag-resources");
    CHECK(perfcore::command_line_value(cmd, L"-aaemu-cvars=").empty());
    // cvar list validation (names [A-Za-z0-9_], numeric values)
    g_cvars.clear();
    add_cvars("min_time_step:0;e_view_dist_ratio_vegetation=20, bad-name:1;r_x:1);System.Quit();x");
    CHECK(g_cvars.size() == 2 && g_cvars[0].first == "min_time_step" && g_cvars[1].second == "20");
    add_cvars("MIN_TIME_STEP:0.0066");
    CHECK(g_cvars.size() == 2 && g_cvars[0].second == "0.0066");
}

// ---- fast _splitpath vs the CRT ----------------------------------------------------------------
static void compare_split(const perfcore::LeadBytes& t, const char* path) {
    char d1[_MAX_DRIVE], p1[_MAX_DIR], f1[_MAX_FNAME], e1[_MAX_EXT];
    char d2[_MAX_DRIVE] = "?", p2[_MAX_DIR] = "?", f2[_MAX_FNAME] = "?", e2[_MAX_EXT] = "?";
    _splitpath(path, d1, p1, f1, e1);
    if (!perfcore::fast_splitpath(t, path, d2, p2, f2, e2)) { CHECK(!"fast_splitpath declined a fitting path"); return; }
    bool same = !strcmp(d1, d2) && !strcmp(p1, p2) && !strcmp(f1, f2) && !strcmp(e1, e2);
    if (!same) fprintf(stderr, "splitpath mismatch for '%s': crt [%s|%s|%s|%s] fast [%s|%s|%s|%s]\n", path, d1, p1, f1, e1, d2, p2, f2, e2);
    CHECK(same);
    // NULL outputs are allowed
    CHECK(perfcore::fast_splitpath(t, path, NULL, p2, NULL, e2) && !strcmp(p1, p2) && !strcmp(e1, e2));
}

static void test_splitpath() {
    perfcore::LeadBytes ascii; memset(&ascii, 0, sizeof ascii);
    const char* cases[] = {
        "", "a", ".", "..", "a.b", "a.b.c", ".hidden", "dir/", "dir\\", "/", "\\\\server\\share\\x.y",
        "C:", "C:x", "C:\\", "C:\\a\\b.c", "c:/a/b/c.d.e", "game/worlds/main_world/paths/x01_y02/",
        "game/db/compact.sqlite3", "a/b.c/d", "a/b.c/d.", "a/.b", "dir.ext\\file", "x:y:z", "/a//b\\\\c.",
        "game\\objects\\characters\\nuian\\male\\face\\nu_m_face01_mid.chr",
    };
    for (const char* c : cases) compare_split(ascii, c);
    // components that do not fit go to the original
    std::string longName(300, 'n');
    char d[_MAX_DRIVE], p[_MAX_DIR], f[_MAX_FNAME], e[_MAX_EXT];
    CHECK(!perfcore::fast_splitpath(ascii, longName.c_str(), d, p, f, e));
    CHECK(!perfcore::fast_splitpath(ascii, (std::string("a/") + longName + "/b").c_str(), d, p, f, e));
    CHECK(!perfcore::fast_splitpath(ascii, (std::string("a.") + longName).c_str(), d, p, f, e));
    CHECK(!perfcore::fast_splitpath(ascii, NULL, d, p, f, e));
    std::string fits = std::string(_MAX_FNAME - 1, 'f');
    compare_split(ascii, fits.c_str());
    // double-byte code page: a trail byte equal to '\' is not a separator
    if (_setmbcp(932) == 0) {
        perfcore::LeadBytes sjis; perfcore::lead_bytes_from_codepage(sjis, 932);
        CHECK(sjis.any && sjis.lead[0x81] && !sjis.lead['a']);
        const char* mb[] = { "x\x81\\y.txt", "dir\\\x83\\name\\f.e", "\x81\\\x81\\", "a\x81" };
        for (const char* c : mb) compare_split(sjis, c);
        _setmbcp(_MB_CP_ANSI);
    } else {
        puts("note: code page 932 unavailable, double-byte splitpath cases skipped");
    }
}

// ---- display-mode key --------------------------------------------------------------------------
static void test_mode_key() {
    const DWORD f = DM_PELSWIDTH | DM_PELSHEIGHT | DM_BITSPERPEL;
    CHECK(perfcore::mode_key(f, 1920, 1080, 32, 60, 6) == perfcore::mode_key(f, 1920, 1080, 32, 144, 6));
    CHECK(perfcore::mode_key(f, 1920, 1080, 32, 0, 6) != perfcore::mode_key(f, 1920, 1200, 32, 0, 6));
    CHECK(perfcore::mode_key(f, 1920, 1080, 32, 0, 6) != perfcore::mode_key(f, 1920, 1080, 16, 0, 6));
    CHECK(perfcore::mode_key(f | DM_DISPLAYFREQUENCY, 1920, 1080, 32, 60, 6) != perfcore::mode_key(f | DM_DISPLAYFREQUENCY, 1920, 1080, 32, 144, 6));
    CHECK(perfcore::mode_key(f, 1920, 1080, 32, 0, 6) != perfcore::mode_key(f, 1920, 1080, 32, 0, 4));
    CHECK(perfcore::mode_key(f, 1920, 1080, 32, 0, 6) != perfcore::mode_key(f | DM_DISPLAYFREQUENCY, 1920, 1080, 32, 0, 6));
}

// ---- pak child index ---------------------------------------------------------------------------
struct FakeDir {
    uint8_t node[0x20];
    perfcore::ListNode head;
    std::vector<perfcore::ListNode*> links;
    std::vector<uint8_t*> children;
    std::vector<std::string*> names;
    FakeDir() { memset(node, 0, sizeof node); head.next = head.prev = &head; head.value = NULL; *(perfcore::ListNode**)(node + 0x10) = &head; }
    ~FakeDir() { for (auto* l : links) delete l; for (auto* c : children) delete[] c; for (auto* n : names) delete n; }
    uint8_t* add(const std::string& name) {               // FUN_365177d0: insert before the sentinel, count++
        auto* s = new std::string(name); names.push_back(s);
        uint8_t* child = new uint8_t[0x20](); *(const char**)(child + 4) = s->c_str(); children.push_back(child);
        auto* l = new perfcore::ListNode{ &head, head.prev, child }; links.push_back(l);
        head.prev->next = l; head.prev = l;
        ++*(int*)(node + 0x14);
        return child;
    }
};

static void test_child_index() {
    FakeDir dir; perfcore::ChildIndex idx;
    CHECK(idx.find(dir.node, "a") == NULL);                // count 0
    srand(7);
    std::vector<std::string> names;
    for (int i = 0; i < 3000; ++i) {
        char b[32]; _snprintf_s(b, sizeof b, _TRUNCATE, "x%03d_y%03d", rand() % 400, i);
        names.push_back(b); dir.add(b);
        if (i % 97 == 0) {                                   // interleave lookups with appends
            for (int k = 0; k < 20; ++k) {
                const std::string& q = names[rand() % names.size()];
                CHECK(idx.find(dir.node, q.c_str()) == perfcore::find_child_linear(dir.node, q.c_str()));
            }
        }
    }
    for (auto& n : names) CHECK(idx.find(dir.node, n.c_str()) == perfcore::find_child_linear(dir.node, n.c_str()));
    CHECK(idx.find(dir.node, "missing") == NULL && perfcore::find_child_linear(dir.node, "missing") == NULL);
    CHECK(idx.find(dir.node, "") == NULL && idx.find(dir.node, NULL) == NULL);
    // duplicate names: first in list order wins, like the linear search
    uint8_t* first = dir.add("dup"); dir.add("dup");
    CHECK(idx.find(dir.node, "dup") == first && perfcore::find_child_linear(dir.node, "dup") == first);
    // count shrink and sentinel change rebuild the entry
    FakeDir other; other.add("p"); uint8_t* q = other.add("q");
    CHECK(idx.find(other.node, "q") == q);
    *(perfcore::ListNode**)(dir.node + 0x10) = &other.head; *(int*)(dir.node + 0x14) = 2;
    CHECK(idx.find(dir.node, "q") == q && idx.find(dir.node, "dup") == NULL);
    CHECK(idx.nodes() == 2);
}

// ---- flush-flag waits --------------------------------------------------------------------------
static volatile LONG g_flag;
static perfcore::WaitOnAddressFn g_testWait;
static VOID (WINAPI* g_testWake)(PVOID);
static unsigned __stdcall flip_later(void*) { Sleep(30); InterlockedExchange(&g_flag, 0); g_testWake((PVOID)&g_flag); return 0; }

static void test_waits() {
    HMODULE kb = GetModuleHandleW(L"kernelbase.dll");
    g_testWait = kb ? (perfcore::WaitOnAddressFn)GetProcAddress(kb, "WaitOnAddress") : NULL;
    g_testWake = kb ? (VOID (WINAPI*)(PVOID))GetProcAddress(kb, "WakeByAddressAll") : NULL;
    if (!g_testWait || !g_testWake) { puts("note: WaitOnAddress unavailable, wait tests skipped"); return; }
    g_flag = 0;
    ULONGLONG t0 = GetTickCount64();
    perfcore::wait_while_nonzero(&g_flag, 10, 1000, g_testWait);        // already clear: returns at once
    perfcore::wait_while(&g_flag, 1, 10, 1000, g_testWait);              // not busy: returns at once
    CHECK(GetTickCount64() - t0 < 100);
    // blocks until the other thread clears and wakes (the main-thread side)
    g_flag = 1;
    HANDLE t = (HANDLE)_beginthreadex(NULL, 0, flip_later, NULL, 0, NULL);
    t0 = GetTickCount64();
    while (g_flag != 0) perfcore::wait_while_nonzero(&g_flag, 0, 5000, g_testWait);
    ULONGLONG waited = GetTickCount64() - t0;
    CHECK(waited >= 15 && waited < 2000);
    WaitForSingleObject(t, INFINITE); CloseHandle(t);
    // timeout without a waker (quit path): returns within about the timeout
    g_flag = 0;
    t0 = GetTickCount64();
    perfcore::wait_while(&g_flag, 0, 0, 20, g_testWait);
    CHECK(GetTickCount64() - t0 < 1000);
    // the stubs: [ebp-0x14] / EDI point at an object whose +0x20 is the flag
    g_waitOnAddress = g_testWait; g_spins = 4;
    static struct { BYTE pad[0x20]; volatile LONG flag; } obj = {};
    obj.flag = 0;
    void* objp = &obj;
    BOOL r1 = 0, r2 = 0;
    __asm {
        mov ecx, objp
        push ebp
        mov ebp, esp
        sub esp, 0x20
        mov [ebp - 0x14], ecx
        call main_yield_stub
        mov esp, ebp
        pop ebp
        mov r1, eax
    }
    obj.flag = 1;                                          // render side: not idle, returns at once
    __asm {
        push edi
        mov edi, objp
        call render_yield_stub
        mov r2, eax
        pop edi
    }
    CHECK(r1 == 1 && r2 == 1);
}

// ---- jump hooks --------------------------------------------------------------------------------
// A function with the FindChild prologue shape (push ebp; mov ebp,esp; push ecx; push esi) that
// returns its stack argument + 1, built in executable memory.
static int __stdcall plus_one_hook(int x);
typedef int (__stdcall* IntFn)(int);
static IntFn g_plusTrampoline;
static int __stdcall plus_one_hook(int x) { return g_plusTrampoline(x) * 10; }

static void test_jump_hook() {
    if (!g_thunks) g_thunks = (BYTE*)VirtualAlloc(NULL, 4096, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READ);
    BYTE* code = (BYTE*)VirtualAlloc(NULL, 4096, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE);
    const BYTE fn[] = { 0x55, 0x8B, 0xEC, 0x51, 0x56,      // push ebp; mov ebp,esp; push ecx; push esi
                        0x8B, 0x45, 0x08, 0x40,            // mov eax,[ebp+8]; inc eax
                        0x5E, 0x59, 0x5D, 0xC2, 0x04, 0x00 };  // pop esi; pop ecx; pop ebp; ret 4
    memcpy(code, fn, sizeof fn);
    IntFn f = (IntFn)code;
    CHECK(f(4) == 5);
    g_plusTrampoline = (IntFn)jump_hook(code, 5, (void*)&plus_one_hook);
    CHECK(g_plusTrampoline != NULL);
    CHECK(code[0] == 0xE9);
    CHECK(f(4) == 50 && g_plusTrampoline(4) == 5);
    VirtualFree(code, 0, MEM_RELEASE);
}

static uint8_t* __cdecl linear_wrapper(uint8_t* node, const char* name) { return perfcore::find_child_linear(node, name); }
// FindChild thunk with a custom-convention stand-in for the original (EAX node, [esp+4] name, ret 4).
static __declspec(naked) void linear_find_child() {
    __asm {
        push dword ptr [esp + 4]
        push eax
        call linear_wrapper
        add esp, 8
        ret 4
    }
}
static uint8_t* call_thunk(uint8_t* node, const char* name) {
    uint8_t* r;
    __asm {
        push name
        mov eax, node
        call find_child_thunk
        mov r, eax
    }
    return r;
}
static void test_find_child_thunk() {
    if (g_tls == TLS_OUT_OF_INDEXES) g_tls = TlsAlloc();
    g_findChildTrampoline = (void*)&linear_find_child;
    FakeDir dir; uint8_t* a = dir.add("alpha"); uint8_t* b = dir.add("beta");
    TlsSetValue(g_tls, NULL);
    CHECK(call_thunk(dir.node, "beta") == b && call_thunk(dir.node, "gamma") == NULL);   // no index: the original
    perfcore::ChildIndex idx;
    TlsSetValue(g_tls, &idx);
    SetLastError(1234);
    CHECK(call_thunk(dir.node, "alpha") == a && call_thunk(dir.node, "gamma") == NULL);
    CHECK(GetLastError() == 1234);
    CHECK(idx.nodes() == 1);
    TlsSetValue(g_tls, NULL);
}

// ---- the per-module apply path against synthetic images ----------------------------------------
static BYTE* fake_module(int m) {
    const perfsites::ModuleId& id = perfsites::kModules[m];
    BYTE* b = (BYTE*)VirtualAlloc(NULL, id.sizeOfImage, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE);
    auto dos = (IMAGE_DOS_HEADER*)b; dos->e_magic = IMAGE_DOS_SIGNATURE; dos->e_lfanew = 0x80;
    auto nt = (IMAGE_NT_HEADERS32*)(b + 0x80);
    nt->Signature = IMAGE_NT_SIGNATURE; nt->FileHeader.Machine = IMAGE_FILE_MACHINE_I386;
    nt->FileHeader.TimeDateStamp = id.stamp; nt->OptionalHeader.SizeOfImage = id.sizeOfImage;
    for (const perfsites::Site& s : perfsites::kSites) {
        if (s.module != m) continue;
        perfcore::Pattern p; perfcore::parse_pattern(s.expect, p);
        for (size_t i = 0; i < p.v.size(); ++i) b[s.rva + i] = p.mask[i] ? p.v[i] : 0xCC;
    }
    return b;
}
static void reset_status(const char* onList) {
    perfcore::Switches s; perfcore::apply_tokens(s, onList);
    for (int i = 0; i < kPatchCount; ++i) {
        g_status[i] = Status();
        g_status[i].on = perfcore::enabled(s, kPatches[i].id, false);
        g_status[i].state = g_status[i].on ? PENDING : OFF;
    }
    for (int m = 0; m < MODULE_COUNT; ++m) g_mods[m] = ModuleState();
}
static State state_of(const char* id) { return g_status[index_of(id)].state; }

static void test_apply_path() {
    if (!g_thunks) g_thunks = (BYTE*)VirtualAlloc(NULL, 4096, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READ);
    InitializeCriticalSection(&g_lock);
    g_log = test_logger;
    // fresh modules: every byte patch applies, IAT hooks refuse (no import table in the fixture)
    reset_status("pak-readahead-64k,cpuspeed-sleep10,instancing-no-wc-readback,shader-async-poll-5ms,dispmode-cache");
    BYTE* sys = fake_module(perfsites::CRYSYSTEM);
    BYTE* ren = fake_module(perfsites::CRYRENDER);
    handle_module(perfsites::CRYSYSTEM, sys, "test");
    handle_module(perfsites::CRYRENDER, ren, "test");
    CHECK(state_of("pak-readahead-64k") == APPLIED && sys[0x14CC7] == 0x01 && sys[0x14CC6] == 0x00);
    CHECK(state_of("cpuspeed-sleep10") == APPLIED && sys[0x237B8] == 0x0A);
    CHECK(state_of("instancing-no-wc-readback") == APPLIED && ren[0x5DA45 + 8] == 0x8D && ren[0x5DA45 + 70] == 0x00);
    CHECK(state_of("shader-async-poll-5ms") == APPLIED && ren[0x8DA26] == 0x05);
    CHECK(state_of("dispmode-cache") == REFUSED);
    CHECK(state_of("fast-splitpath") == OFF);
    // a second instance sees its own bytes: present, nothing rewritten
    reset_status("pak-readahead-64k,shader-async-poll-5ms");
    handle_module(perfsites::CRYSYSTEM, sys, "again");
    handle_module(perfsites::CRYRENDER, ren, "again");
    CHECK(state_of("pak-readahead-64k") == PRESENT && state_of("shader-async-poll-5ms") == PRESENT);
    // late attach: one-byte patches apply, the 71-byte instancing rewrite and code hooks refuse
    VirtualFree(ren, 0, MEM_RELEASE); ren = fake_module(perfsites::CRYRENDER);
    reset_status("instancing-no-wc-readback,shader-async-poll-5ms,render-thread-event-sync");
    g_mods[perfsites::CRYRENDER].late = true;
    BYTE before[71]; memcpy(before, ren + 0x5DA45, 71);
    handle_module(perfsites::CRYRENDER, ren, "late");
    CHECK(state_of("shader-async-poll-5ms") == APPLIED);
    CHECK(state_of("instancing-no-wc-readback") == REFUSED && memcmp(before, ren + 0x5DA45, 71) == 0);
    CHECK(state_of("render-thread-event-sync") == REFUSED);
    // a different build: everything for that module refuses, nothing is written
    VirtualFree(sys, 0, MEM_RELEASE); sys = fake_module(perfsites::CRYSYSTEM);
    ((IMAGE_NT_HEADERS32*)(sys + 0x80))->FileHeader.TimeDateStamp ^= 1;
    reset_status("pak-readahead-64k,cpuspeed-sleep10");
    handle_module(perfsites::CRYSYSTEM, sys, "other build");
    CHECK(state_of("pak-readahead-64k") == REFUSED && state_of("cpuspeed-sleep10") == REFUSED && sys[0x14CC7] == 0x10);
    CHECK(logged("not the r208022 build"));
    // one wrong byte inside a check window refuses only that patch
    ((IMAGE_NT_HEADERS32*)(sys + 0x80))->FileHeader.TimeDateStamp ^= 1;
    sys[0x237B5] = 0x90;
    reset_status("pak-readahead-64k,cpuspeed-sleep10");
    handle_module(perfsites::CRYSYSTEM, sys, "one byte off");
    CHECK(state_of("pak-readahead-64k") == APPLIED && state_of("cpuspeed-sleep10") == REFUSED && sys[0x237B8] == 0x64);
    // render-thread-event-sync on a fresh image: refused before any write when the import is missing
    ren = fake_module(perfsites::CRYRENDER);
    g_waitOnAddress = g_testWait ? g_testWait : (perfcore::WaitOnAddressFn)1; g_wakeAll = g_testWake ? g_testWake : (VOID (WINAPI*)(PVOID))1;
    reset_status("render-thread-event-sync,pak-findchild-index");
    BYTE entry[12]; memcpy(entry, ren + 0x199610, 12);
    handle_module(perfsites::CRYRENDER, ren, "no imports");
    CHECK(state_of("render-thread-event-sync") == REFUSED && memcmp(entry, ren + 0x199610, 12) == 0);
    // pak-findchild-index brings pak-catalog-scope with it
    VirtualFree(sys, 0, MEM_RELEASE); sys = fake_module(perfsites::CRYSYSTEM);
    if (g_tls == TLS_OUT_OF_INDEXES) g_tls = TlsAlloc();
    handle_module(perfsites::CRYSYSTEM, sys, "index");
    CHECK(state_of("pak-catalog-scope") == APPLIED && state_of("pak-findchild-index") == APPLIED);
    CHECK(sys[0x13C60] == 0xE9 && sys[0x17720] == 0xE9);
    // status text lists every id
    std::string st = perfpatch::control("status");
    CHECK(st[0] == '0' && st.find("pak-findchild-index applied") != std::string::npos && st.find("diag-resources off") != std::string::npos);
    CHECK(perfpatch::control("bogus")[0] == '2');
    VirtualFree(sys, 0, MEM_RELEASE); VirtualFree(ren, 0, MEM_RELEASE);
}

static void test_first_tick() {
    g_tickStage = 0; g_cvars.clear(); add_cvars("min_time_step:0");
    reset_status("pak-readahead-64k");
    std::string lua = perfpatch::first_tick_lua("");
    CHECK(lua.find("System.Log('[aadriver] perf patches: pak-readahead-64k=pending')") != std::string::npos);
    CHECK(lua.find("local n, v = 'min_time_step', 0") != std::string::npos);
    CHECK(perfpatch::first_tick_lua("0min_time_step=0.0066->0").empty());
    CHECK(logged("cvars: min_time_step=0.0066->0"));
    CHECK(perfpatch::first_tick_lua("").empty());
}

static void test_resources_line() {
    g_log = test_logger; g_logLines.clear();
    log_resources();
    CHECK(g_logLines.size() == 1);
    const std::string& l = g_logLines.empty() ? std::string() : g_logLines[0];
    CHECK(l.find("handles=") != std::string::npos && l.find("largest_free=") != std::string::npos && l.find(" lfh=") != std::string::npos);
    CHECK(l.find("heaps=0 ") == std::string::npos);
}

// ---- physics-role-guard: engine MarkAsPhysThread semantics on a fake CryPhysics image ----------
static BYTE* g_fakePhys;
static HANDLE g_ownerReady, g_ownerGo, g_ownerDone, g_ownerBlocked;
static volatile LONG g_ownerQuit;
static int g_ownerSaw = -1;
static DWORD g_interfered;
// The engine's MarkAsPhysThread(1) (0x350827D0) and TimeStep's inline takes (+0xAFB3D, +0xB01C6),
// with the cell base of the fake image: on a TLS/cur mismatch, move the role to partner(cur).
static void fake_mark(BYTE* base) {
    DWORD idx = *(DWORD*)(base + kPhysTlsIndex);
    void** cur = (void**)(base + kPhysRoleCurrent);
    uintptr_t x = (uintptr_t)(base + kPhysRoleCells);
    if (TlsGetValue(idx) != *cur) {
        LONG* c = (LONG*)*cur;
        LONG* other = (LONG*)(x + (((uintptr_t)c - x) ^ 4));
        *c = 4; *other = 0; *cur = other; TlsSetValue(idx, other);
    }
}
static int fake_icaller(BYTE* base) { void* p = TlsGetValue(*(DWORD*)(base + kPhysTlsIndex)); return p ? *(LONG*)p : 4; }
static volatile LONG g_ownerCmd;       // 0 TimeStep(dt > 0) through the guard, 1 unmark through the guard, 2 TracePendingRays through the guard
// Starts an owner role path while this thread is inside a guarded call and waits until the owner
// either blocks on the role lock (serialised) or has finished (it ran inside the parked window).
static void interfere(LONG cmd) {
    g_ownerCmd = cmd; g_ownerSaw = -1; SetEvent(g_ownerGo);
    HANDLE h[2] = { g_ownerBlocked, g_ownerDone };
    g_interfered = WaitForMultipleObjects(2, h, FALSE, 5000);
}
// TracePendingRays: MarkAsPhysThread(1), then use iCaller.
//   arg 7: the owner steps after the mark, while this call's query still runs (it must keep iCaller 4);
//   arg 8: the owner steps after the park but before the mark (the reviewed window);
//   arg 9: the owner's own TracePendingRays takes the role in that window.
static int __fastcall fake_trace_rays(void*, void*, int arg) {
    if (arg == 8) interfere(0);
    if (arg == 9) interfere(2);
    fake_mark(g_fakePhys);
    int me = fake_icaller(g_fakePhys);
    if (arg == 7) { interfere(0); if (fake_icaller(g_fakePhys) != me) me = -50; }
    return 100 + me;
}
// TimeStep: the inline take when dt > 0.
static void __fastcall fake_step(void*, void*, float dt, int) { if (dt > 0) fake_mark(g_fakePhys); }
// The engine's unmark (world slot 34, 0x35088520): hand the caller's role to the partner cell.
static void __fastcall fake_unmark(void*, void*) {
    BYTE* base = g_fakePhys;
    DWORD idx = *(DWORD*)(base + kPhysTlsIndex);
    void** cur = (void**)(base + kPhysRoleCurrent);
    uintptr_t x = (uintptr_t)(base + kPhysRoleCells);
    LONG* mine = (LONG*)TlsGetValue(idx);
    if (mine == *cur) { LONG* other = (LONG*)(x + (((uintptr_t)mine - x) ^ 4)); *mine = 4; *cur = other; *other = 0; TlsSetValue(idx, nullptr); }
}
static volatile DWORD g_ownerTid;
static void owner_blocked() { if (GetCurrentThreadId() == g_ownerTid) SetEvent(g_ownerBlocked); }
static unsigned __stdcall owner_thread(void*) {
    LONG* cells = (LONG*)(g_fakePhys + kPhysRoleCells);
    TlsSetValue(*(DWORD*)(g_fakePhys + kPhysTlsIndex), &cells[0]);
    g_ownerTid = GetCurrentThreadId();
    SetEvent(g_ownerReady);
    while (WaitForSingleObject(g_ownerGo, 5000) == WAIT_OBJECT_0 && !g_ownerQuit) {
        if (g_ownerCmd == 1) hk_unmark(nullptr, nullptr);
        else if (g_ownerCmd == 2) hk_trace_rays(nullptr, nullptr, 0);
        else hk_timestep(nullptr, nullptr, 0.016f, 0);   // the owner's own step takes the role inline
        g_ownerSaw = fake_icaller(g_fakePhys);
        SetEvent(g_ownerDone);
    }
    return 0;
}
static void owner_do(LONG cmd) { g_ownerCmd = cmd; g_ownerSaw = -1; SetEvent(g_ownerGo); WaitForSingleObject(g_ownerDone, 5000); }
static bool owner_finished() { return WaitForSingleObject(g_ownerDone, 5000) == WAIT_OBJECT_0; }
static void test_physics_role_guard() {
    g_fakePhys = (BYTE*)VirtualAlloc(NULL, 0x220000, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    DWORD idx = TlsAlloc();
    *(DWORD*)(g_fakePhys + kPhysTlsIndex) = idx;
    LONG* cells = (LONG*)(g_fakePhys + kPhysRoleCells);
    cells[0] = 0; cells[1] = 4;
    *(void**)(g_fakePhys + kPhysRoleCurrent) = &cells[0];
    g_physBase = g_fakePhys; g_raysOriginal = (RaysFn)(void*)&fake_trace_rays; g_unmarkOriginal = (VoidFn)(void*)&fake_unmark;
    g_stepOriginal = (StepFn)(void*)&fake_step;
    InitializeCriticalSection(&g_roleLock);
    g_roleWaitHook = owner_blocked;
    g_ownerReady = CreateEventW(NULL, FALSE, FALSE, NULL); g_ownerGo = CreateEventW(NULL, FALSE, FALSE, NULL);
    g_ownerDone = CreateEventW(NULL, FALSE, FALSE, NULL); g_ownerBlocked = CreateEventW(NULL, FALSE, FALSE, NULL);
    HANDLE t = (HANDLE)_beginthreadex(NULL, 0, owner_thread, NULL, 0, NULL);
    WaitForSingleObject(g_ownerReady, 5000);
    g_stepper = g_ownerTid;              // the owner thread is the stepping (physics) thread
    void** cur = (void**)(g_fakePhys + kPhysRoleCurrent);
    g_roleGuard = true;
    // 1. guarded: this thread runs as iCaller 4, the owner keeps the role and its cell
    CHECK(hk_trace_rays(nullptr, nullptr, 0) == 104);
    CHECK(*cur == &cells[0] && cells[0] == 0 && cells[1] == 4 && TlsGetValue(idx) == nullptr && g_guarded == 1);
    // 2. the owner steps after the mark: its step waits for the guarded call, whose query keeps iCaller 4
    CHECK(hk_trace_rays(nullptr, nullptr, 7) == 104);
    CHECK(g_interfered == WAIT_OBJECT_0 && owner_finished() && g_ownerSaw == 0);
    CHECK(*cur == &cells[0] && cells[0] == 0 && cells[1] == 4 && g_guardMoved == 0 && TlsGetValue(idx) == nullptr && g_roleWaits == 1);
    // 3. the owner steps before the original's MarkAsPhysThread (after the park): it waits as well, so
    //    the mark sees "already the owner" and the call still runs as iCaller 4 (unserialised, the owner
    //    moved the park and the mark took the role back: iCaller 0 next to the owner's query)
    CHECK(hk_trace_rays(nullptr, nullptr, 8) == 104);
    CHECK(g_interfered == WAIT_OBJECT_0 && owner_finished() && g_ownerSaw == 0);
    CHECK(*cur == &cells[0] && cells[0] == 0 && cells[1] == 4 && g_guardMoved == 0 && TlsGetValue(idx) == nullptr && g_roleWaits == 2);
    CHECK(g_park[0][0] == 4 && g_park[0][1] == 4 && g_park[1][0] == 4 && g_park[1][1] == 4);
    // 4. the owner itself is never parked, and without contention nothing waits
    owner_do(0);
    CHECK(g_ownerSaw == 0 && *cur == &cells[0]);
    owner_do(2);
    CHECK(g_ownerSaw == 0 && *cur == &cells[0] && g_roleWaits == 2);
    // 5. the owner unmarks: the role is parked ownerless instead of going to the partner cell
    owner_do(1);
    CHECK(g_ownerSaw == 4 && *cur == (void*)&g_phantom[0] && g_phantom[0] == 0 && cells[0] == 4 && cells[1] == 4 && g_unmarksParked == 1);
    CHECK(hk_trace_rays(nullptr, nullptr, 0) == 104 && *cur == (void*)&g_phantom[0]);   // not the stepper: parked
    // 6. the stepper's own TracePendingRays would take the ownerless role inside a parked call's window:
    //    it waits, takes it afterwards, and the guarded call ran as iCaller 4
    CHECK(hk_trace_rays(nullptr, nullptr, 9) == 104);
    CHECK(g_interfered == WAIT_OBJECT_0 && owner_finished() && g_ownerSaw == 0);
    CHECK(*cur == (void*)&g_phantom[1] && g_phantom[0] == 4 && g_phantom[1] == 0 && TlsGetValue(idx) == nullptr && g_guardMoved == 0);
    // 7. a step with dt == 0 (the main thread's per-frame call) does not take the role or the lock
    hk_timestep(nullptr, nullptr, 0.0f, 0);
    CHECK(TlsGetValue(idx) == nullptr && *cur == (void*)&g_phantom[1] && g_stepper == g_ownerTid);
    // 8. guard off: the engine behaviour, this thread takes iCaller 0 from the owner
    g_roleGuard = false;
    CHECK(hk_trace_rays(nullptr, nullptr, 0) == 100 && TlsGetValue(idx) == *cur && g_phantom[1] == 4);
    g_ownerQuit = 1; SetEvent(g_ownerGo); WaitForSingleObject(t, 5000); CloseHandle(t);
    g_roleWaitHook = nullptr;
    TlsSetValue(idx, nullptr); TlsFree(idx);
    std::string st = perfpatch::control("status");
    CHECK(st.find("role_waits=3") != std::string::npos && st.find("guard_moved=0") != std::string::npos);
}

// ---- old start-up paths follow the resolved switches ---------------------------------------------
static void resolve(const wchar_t* cmd, const char* ini = "", const char* env = "") {
    resolve_switches(cmd, ini, env, perfcore::command_line_value(cmd, L"-aaemu-patches="));
}
static bool status_says(const char* id, const char* state) {
    return perfpatch::control("status").find(std::string("\n") + id + " " + state) != std::string::npos ||
           perfpatch::control("status").find(std::string("0") + id + " " + state) == 0;
}
static void test_switch_resolution() {
    g_log = test_logger;
    const char* pak = "pak-readahead-64k";
    const char* cal = "cpu-calibration-normal-priority";
    g_attached = false;
    resolve(L"x.exe -aaemu-fast-pak -aaemu-smooth-start");
    CHECK(!perfpatch::enabled(pak) && !perfpatch::enabled(cal));               // nothing before attach
    g_attached = true;
    // the launcher's default profile: both aliases on
    resolve(L"x.exe -aaemu-fast-pak -aaemu-smooth-start");
    CHECK(perfpatch::enabled(pak) && perfpatch::enabled(cal) && status_says(pak, "pending") && status_says(cal, "pending"));
    // --patch none: both off, and the status says so
    resolve(L"x.exe -aaemu-fast-pak -aaemu-smooth-start -aaemu-patches=none");
    CHECK(!perfpatch::enabled(pak) && !perfpatch::enabled(cal) && status_says(pak, "off") && status_says(cal, "off"));
    CHECK(!perfpatch::enabled("physics-role-guard") && !perfpatch::enabled("fast-splitpath"));
    // --no-patch for one id leaves the other alias alone
    resolve(L"x.exe -aaemu-fast-pak -aaemu-smooth-start -aaemu-patches=-pak-readahead-64k");
    CHECK(!perfpatch::enabled(pak) && perfpatch::enabled(cal) && status_says(pak, "off"));
    resolve(L"x.exe -aaemu-fast-pak -aaemu-smooth-start -aaemu-patches=-cpu-calibration-normal-priority");
    CHECK(perfpatch::enabled(pak) && !perfpatch::enabled(cal) && status_says(cal, "off"));
    // --no-fast-pak / --no-smooth-start: the launcher omits the aliases
    resolve(L"x.exe -aaemu-hidden");
    CHECK(!perfpatch::enabled(pak) && !perfpatch::enabled(cal));
    // explicit ids win over a missing alias, and ini/env layer under the command line
    resolve(L"x.exe -aaemu-hidden -aaemu-patches=pak-readahead-64k,+cpu-calibration-normal-priority");
    CHECK(perfpatch::enabled(pak) && perfpatch::enabled(cal));
    resolve(L"x.exe -aaemu-fast-pak -aaemu-smooth-start", "none");
    CHECK(!perfpatch::enabled(pak) && !perfpatch::enabled(cal));
    resolve(L"x.exe -aaemu-fast-pak -aaemu-smooth-start", "none", "pak-readahead-64k");
    CHECK(perfpatch::enabled(pak) && !perfpatch::enabled(cal));
    // a late inject (no launcher flags): the table defaults
    resolve(L"x.exe");
    CHECK(perfpatch::enabled(pak) && !perfpatch::enabled(cal) && perfpatch::enabled("physics-role-guard"));
    // aadriver.cpp's start-up paths ask the resolved switches, not the raw flags
    std::string src, here = __FILE__;
    FILE* f = fopen((here.substr(0, here.find_last_of("\\/") + 1) + "aadriver.cpp").c_str(), "rb");
    if (f) { char b[4096]; size_t n; while ((n = fread(b, 1, sizeof b, f)) > 0) src.append(b, n); fclose(f); }
    CHECK(!src.empty());
    CHECK(src.find("L\"-aaemu-fast-pak\"") == std::string::npos && src.find("L\"-aaemu-smooth-start\"") == std::string::npos);
    CHECK(src.find("perfpatch::enabled(\"pak-readahead-64k\")") != std::string::npos &&
          src.find("perfpatch::enabled(\"cpu-calibration-normal-priority\")") != std::string::npos);
    resolve(L"x.exe -aaemu-fast-pak -aaemu-smooth-start");
}

// ---- CVar values: one bad entry is dropped, the rest stays, the chunk gets canonical numbers -------
static bool canon(const char* in, const char* want) { std::string out; return perfcore::canonical_number(in, out) && out == want; }
static bool rejected(const char* in) { std::string out; return !perfcore::canonical_number(in, out); }
static void test_cvar_values() {
    CHECK(canon("0", "0") && canon("20", "20") && canon("150", "150") && canon("-3", "-3"));
    CHECK(canon("0.0066", "0.0066") && canon("0.1", "0.1") && canon("123456789.25", "123456789.25"));
    CHECK(canon("+5", "5") && canon("-0", "0") && canon("1e3", "1000") && canon("1.5E-3", "0.0015"));
    CHECK(canon(".5", "0.5") && canon("5.", "5") && canon("2.50", "2.5") && canon("1e-7", "1e-07"));
    CHECK(canon("1000000000", "1000000000") && canon("-1e9", "-1000000000"));
    for (const char* bad : { "", "-", "+", ".", "1.2.3", "1e", "e5", "1e+", "inf", "-inf", "nan", "NaN", "infinity", "0x10", "1,5",
                             " 1", "1 ", "1e400", "-1e400", "1000000001", "1e10", "1..2", "--1", "1-", "5)", "0;os.exit()" })
        CHECK(rejected(bad));
    // the client may switch the CRT locale; the value keeps its decimal point
    if (setlocale(LC_ALL, "de-DE")) { CHECK(canon("0.5", "0.5") && canon("1.25e2", "125")); setlocale(LC_ALL, "C"); }
    g_log = test_logger; g_logLines.clear(); g_cvars.clear();
    add_cvars("min_time_step:1.2.3;r_good:5;e_inf:inf;e_plus:+2;e_frac:0.25;noValue;e_big:1e12");
    CHECK(g_cvars.size() == 3 && g_cvars[0].first == "r_good" && g_cvars[0].second == "5");
    CHECK(g_cvars[1].first == "e_plus" && g_cvars[1].second == "2" && g_cvars[2].second == "0.25");
    CHECK(logged("cvar 'min_time_step:1.2.3' ignored") && logged("cvar 'e_inf:inf' ignored") && logged("cvar 'noValue' ignored") && logged("cvar 'e_big:1e12' ignored"));
    add_cvars("R_GOOD=7");                                // a later source replaces the value of the same name
    CHECK(g_cvars.size() == 3 && g_cvars[0].second == "7");
    g_tickStage = 0;
    std::string lua = perfpatch::first_tick_lua("");
    CHECK(lua.find("local n, v = 'r_good', 7\n") != std::string::npos && lua.find("local n, v = 'e_plus', 2\n") != std::string::npos);
    CHECK(lua.find("local n, v = 'e_frac', 0.25\n") != std::string::npos && lua.find("1.2.3") == std::string::npos && lua.find("inf") == std::string::npos);
    g_tickStage = 0; g_cvars.clear();
}

// ---- PE mapping (perf-sites.exe): damaged files are reported, never read out of bounds ----------
struct PeFixture {
    std::vector<uint8_t> f;
    IMAGE_DOS_HEADER* dos() { return (IMAGE_DOS_HEADER*)f.data(); }
    IMAGE_NT_HEADERS32* nt() { return (IMAGE_NT_HEADERS32*)(f.data() + 0x80); }
    IMAGE_SECTION_HEADER* sec() { return (IMAGE_SECTION_HEADER*)(f.data() + 0x80 + sizeof(IMAGE_NT_HEADERS32)); }
};
// headers 0x400, one section: VA 0x1000, raw 0x400..0x600 (an import table), SizeOfImage 0x3000
static PeFixture pe_fixture() {
    PeFixture p; p.f.assign(0x600, 0);
    p.dos()->e_magic = IMAGE_DOS_SIGNATURE; p.dos()->e_lfanew = 0x80;
    IMAGE_NT_HEADERS32* nt = p.nt();
    nt->Signature = IMAGE_NT_SIGNATURE; nt->FileHeader.Machine = IMAGE_FILE_MACHINE_I386; nt->FileHeader.NumberOfSections = 1;
    nt->FileHeader.SizeOfOptionalHeader = sizeof(IMAGE_OPTIONAL_HEADER32); nt->FileHeader.TimeDateStamp = 0x12345678;
    nt->OptionalHeader.Magic = IMAGE_NT_OPTIONAL_HDR32_MAGIC; nt->OptionalHeader.ImageBase = 0x10000000;
    nt->OptionalHeader.SizeOfImage = 0x3000; nt->OptionalHeader.SizeOfHeaders = 0x400;
    nt->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT].VirtualAddress = 0x1000;
    IMAGE_SECTION_HEADER* s = p.sec();
    memcpy(s->Name, ".idata", 6); s->VirtualAddress = 0x1000; s->Misc.VirtualSize = 0x200; s->PointerToRawData = 0x400; s->SizeOfRawData = 0x200;
    uint8_t* d = p.f.data() + 0x400;                      // file offset of RVA 0x1000
    IMAGE_IMPORT_DESCRIPTOR desc = {}; desc.OriginalFirstThunk = 0x1100; desc.Name = 0x1180; desc.FirstThunk = 0x1140;
    memcpy(d, &desc, sizeof desc);
    uint32_t hint = 0x11A0; memcpy(d + 0x100, &hint, 4); memcpy(d + 0x140, &hint, 4);
    strcpy((char*)d + 0x180, "KERNEL32.dll"); strcpy((char*)d + 0x1A2, "Sleep");
    return p;
}
static bool pe_ok(PeFixture& p, perfcore::PeImage& img) { std::string why; return perfcore::map_pe(p.f.data(), p.f.size(), img, why); }
static bool pe_bad(PeFixture p, const char* reason) {
    perfcore::PeImage img; std::string why;
    bool ok = perfcore::map_pe(p.f.data(), p.f.size(), img, why);
    return !ok && img.mem.empty() && why.find(reason) != std::string::npos;
}
static void test_pe_mapping() {
    PeFixture p = pe_fixture();
    perfcore::PeImage img;
    CHECK(pe_ok(p, img) && img.size == 0x3000 && img.mem.size() == 0x3000 && img.stamp == 0x12345678 && img.base == 0x10000000);
    CHECK(perfcore::pe_import_at(img, 0x1140) == "KERNEL32.dll!Sleep" && perfcore::pe_import_at(img, 0x1144).empty());
    // every truncation of the file is reported (the section's raw data ends at 0x600)
    for (size_t n = 0; n < p.f.size(); ++n) {
        perfcore::PeImage t; std::string why;
        CHECK(!perfcore::map_pe(p.f.data(), n, t, why) && !why.empty());
    }
    { PeFixture q = p; q.dos()->e_magic = 0; CHECK(pe_bad(q, "MZ")); }
    { PeFixture q = p; q.dos()->e_lfanew = 0x7FFFFFF0; CHECK(pe_bad(q, "e_lfanew")); }
    { PeFixture q = p; q.dos()->e_lfanew = -4; CHECK(pe_bad(q, "e_lfanew")); }
    { PeFixture q = p; q.dos()->e_lfanew = 0x600 - 8; CHECK(pe_bad(q, "e_lfanew")); }
    { PeFixture q = p; q.nt()->Signature = 0; CHECK(pe_bad(q, "32-bit PE")); }
    { PeFixture q = p; q.nt()->OptionalHeader.Magic = IMAGE_NT_OPTIONAL_HDR64_MAGIC; CHECK(pe_bad(q, "32-bit PE")); }
    { PeFixture q = p; q.nt()->FileHeader.SizeOfOptionalHeader = 8; CHECK(pe_bad(q, "optional header")); }
    { PeFixture q = p; q.nt()->OptionalHeader.SizeOfImage = 0xFFFFFFFF; CHECK(pe_bad(q, "SizeOfImage")); }
    { PeFixture q = p; q.nt()->OptionalHeader.SizeOfImage = 0; CHECK(pe_bad(q, "SizeOfImage")); }
    { PeFixture q = p; q.nt()->OptionalHeader.SizeOfHeaders = 0x601; CHECK(pe_bad(q, "SizeOfHeaders")); }
    { PeFixture q = p; q.nt()->OptionalHeader.SizeOfImage = 0x200; CHECK(pe_bad(q, "SizeOfHeaders")); }
    { PeFixture q = p; q.nt()->FileHeader.NumberOfSections = 0xFFFF; CHECK(pe_bad(q, "section table")); }
    { PeFixture q = p; q.nt()->FileHeader.SizeOfOptionalHeader = 0xFFFF; CHECK(pe_bad(q, "section table")); }
    { PeFixture q = p; q.sec()->PointerToRawData = 0xFFFFFF00; CHECK(pe_bad(q, "raw data outside")); }
    { PeFixture q = p; q.sec()->SizeOfRawData = 0xFFFFFFFF; CHECK(pe_bad(q, "raw data outside")); }
    { PeFixture q = p; q.sec()->PointerToRawData = 0x500; CHECK(pe_bad(q, "raw data outside")); }
    { PeFixture q = p; q.sec()->VirtualAddress = 0x3000; CHECK(pe_bad(q, "outside SizeOfImage")); }
    { PeFixture q = p; q.sec()->VirtualAddress = 0xFFFFF000; CHECK(pe_bad(q, "outside SizeOfImage")); }
    // raw data that runs past the image end is padding: copied up to SizeOfImage only
    { PeFixture q = p; q.sec()->VirtualAddress = 0x2F00; perfcore::PeImage t; CHECK(pe_ok(q, t) && t.mem.size() == 0x3000 && t.mem[0x2F00 + 0x180] == 0); }
    // a damaged import table gives "" instead of reading outside the image
    { PeFixture q = p; q.nt()->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT].VirtualAddress = 0x2FF8;
      perfcore::PeImage t; CHECK(pe_ok(q, t) && perfcore::pe_import_at(t, 0x1140).empty()); }
    { PeFixture q = p; q.nt()->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT].VirtualAddress = 0xFFFFFFF0;
      perfcore::PeImage t; CHECK(pe_ok(q, t) && perfcore::pe_import_at(t, 0x1140).empty()); }
    { PeFixture q = p; IMAGE_IMPORT_DESCRIPTOR* d = (IMAGE_IMPORT_DESCRIPTOR*)(q.f.data() + 0x400); d->OriginalFirstThunk = 0x2FFE;
      perfcore::PeImage t; CHECK(pe_ok(q, t) && perfcore::pe_import_at(t, 0x1140).empty()); }
    { PeFixture q = p; uint32_t beyond = 0x7FFFFFF0; memcpy(q.f.data() + 0x500, &beyond, 4);
      perfcore::PeImage t; CHECK(pe_ok(q, t) && perfcore::pe_import_at(t, 0x1140) == "KERNEL32.dll!"); }
    { PeFixture q = p; memset(q.f.data() + 0x580, 'A', 0x80);   // names without their NUL run on to the zero image tail
      perfcore::PeImage t; CHECK(pe_ok(q, t) && perfcore::pe_import_at(t, 0x1140).find("AAAA") == 0); }
    { perfcore::PeImage empty; CHECK(perfcore::pe_import_at(empty, 0x1140).empty() && perfcore::pe_string(empty, 0).empty()); }
    { perfcore::PeImage tail; tail.mem = { 0x61, 0x62, 0x63 }; CHECK(perfcore::pe_string(tail, 0).empty() && perfcore::pe_string(tail, 3).empty()); }
}

int main() {
    test_patterns();
    test_switches();
    test_splitpath();
    test_mode_key();
    test_child_index();
    test_waits();
    test_jump_hook();
    test_find_child_thunk();
    test_apply_path();
    test_first_tick();
    test_resources_line();
    test_physics_role_guard();
    test_switch_resolution();
    test_cvar_values();
    test_pe_mapping();
    if (g_failures) { fprintf(stderr, "%d perfpatch check(s) failed\n", g_failures); return 1; }
    puts("perfpatch patterns, switches, splitpath, mode key, child index, waits, hooks, apply path, first tick, physics role guard, switch resolution, CVar values and PE mapping passed");
    return 0;
}

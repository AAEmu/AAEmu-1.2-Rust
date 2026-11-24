// Client performance and reliability patch set for the Trion r208022 client.
//
// Every patch is an in-memory change of a loaded engine module, verified byte for byte first; a
// mismatch refuses it. No client file is changed. Sites: perfsites.h; evidence and measurements:
// C:\aa-re\notes\client-perf-r208022.md and C:\aa-re\lanes\client-perf\patches\README.md.
//
// Timing. The launcher injects this DLL into the suspended client, before crysystem.dll and
// cryrenderd3d10.dll load. A loader notification wraps each target module's loader entry point,
// and the patches go in on the loading thread right after the module's DllMain returns: the same
// moment the dev client uses (after LdrLoadDll), before any of the module's code runs on another
// thread. Modules that were already loaded when the driver attached only get the patches that are
// safe on running code (one changed byte, or an aligned IAT pointer); the rest refuse.
//
// Switches (later wins): table defaults < <driver dir>\aadriver.ini [perf] patches= <
// %AADRIVER_PATCHES% < -aaemu-patches= on the client command line. Syntax in perfcore.h.
// CVars: [perf] cvars=, %AADRIVER_CVARS%, -aaemu-cvars=name:value;... (set at the first script tick).
//
// Lock order: no loader call (GetModuleHandle, GetProcAddress, LoadLibrary) while g_lock is held.
// The notification runs under the loader lock and then takes g_lock; the worker resolves modules
// first and takes g_lock afterwards.
#define WIN32_LEAN_AND_MEAN
#define NOMINMAX
#include <windows.h>
#include <d3d11.h>
#include <dxgi.h>
#include <intrin.h>
#include <psapi.h>
#include <stdio.h>
#include <string>
#include <vector>
#include <map>
#include <algorithm>
#include <share.h>
#include "perfcore.h"
#include "perfsites.h"
#include "perfpatch.h"
#include "startup.h"

using namespace perfsites;

namespace perfpatch {

enum State { PENDING = 0, APPLIED, PRESENT, REFUSED, OFF };
static const char* kStateNames[] = { "pending", "applied", "present", "refused", "off" };

enum Kind { BYTES, HOOK, THREAD, EXTERNAL, WORLD /* physics world vtable copy, installed by world_thread */ };
struct Patch {
    const char* id;
    int module;          // perfsites::Module, -1 for none
    Kind kind;
    bool defaultOn;
    bool earlyOnly;      // multi-byte code change: refuse on a module that was running before attach
    const char* effect;
};

// Defaults: on for the six patches the r208022 dev client has applied in live runs, and for
// physics-role-guard (the CryPhysics crash fix, measured live in lane client-perf/measure).
static const Patch kPatches[] = {
    { "pak-readahead-64k", CRYSYSTEM, BYTES, true, false, "small pak reads fill 64 KiB instead of a whole 1 MiB window" },
    { "cpuspeed-sleep10", CRYSYSTEM, BYTES, true, false, "CPU-speed probe sleeps 10 ms instead of 100 ms" },
    { "cpu-calibration-normal-priority", CRYSYSTEM, EXTERNAL, false, false, "CPU clock calibration at normal priority (launcher --smooth-start)" },
    { "instancing-no-wc-readback", CRYRENDER, BYTES, true, true, "instancing loop no longer reads the write-combined instance buffer" },
    { "shader-async-poll-5ms", CRYRENDER, BYTES, true, false, "async shader worker polls every 5 ms instead of 25 ms" },
    { "dispmode-cache", CRYSYSTEM, HOOK, true, false, "identical CDS_TEST display-mode tests answered from a cache" },
    { "fast-splitpath", CRYSYSTEM, HOOK, true, false, "crysystem's _splitpath without a locale lookup per character" },
    { "frame-latency", CRYRENDER, HOOK, false, false, "IDXGIDevice1::SetMaximumFrameLatency(frame_latency, default 1)" },
    { "render-thread-event-sync", CRYRENDER, HOOK, false, true, "main/render flush waits block on the flag instead of SwitchToThread spins" },
    { "pak-catalog-scope", CRYSYSTEM, HOOK, false, true, "times each pak catalogue load (and scopes pak-findchild-index)" },
    { "pak-findchild-index", CRYSYSTEM, HOOK, false, true, "hash index for directory children during the pak catalogue load" },
    { "pak-prefetch", -1, THREAD, false, false, "background read of the pak catalogue area (cold cache only)" },
    { "diag-resources", -1, THREAD, false, false, "logs handles, GDI/USER objects, private bytes and free address space" },
    { "physics-role-guard", CRYPHYSICS, WORLD, true, false, "only the stepping physics thread holds CryPhysics iCaller 0 (TracePendingRays parked, unmark ownerless)" },
    { "diag-physics-role", CRYPHYSICS, WORLD, false, false, "logs which threads step physics and take the physics-thread role" },
    { "diag-frame-trace", CRYRENDER, HOOK, false, true, "frame phases and main/render stack samples during frames over trace_budget_ms (frametrace-<pid>.tsv)" },
    { "diag-fulldump", -1, THREAD, false, false, "full-memory dump next to the driver on the first access violation in an engine module" },
};
static const int kPatchCount = sizeof kPatches / sizeof kPatches[0];

struct Status { bool on; State state; std::string why; ULONGLONG tick; BYTE* at; BYTE original[96]; size_t len; };
static Status g_status[kPatchCount];

struct ModuleState {
    BYTE* base;
    bool late;           // loaded before the driver attached
    bool ready;          // identified and handled
    bool wrapped;
    ULONGLONG seenTick;
    BOOL (WINAPI* entry)(HINSTANCE, DWORD, LPVOID);
};
static ModuleState g_mods[MODULE_COUNT];

static Logger g_log;
static CRITICAL_SECTION g_lock;
static bool g_attached;
static perfcore::Switches g_switches;
static std::vector<std::pair<std::string, std::string>> g_cvars;
static int g_tickStage;
static HMODULE g_self;
static BYTE* g_thunks;     // executable page for trampolines
static size_t g_thunkUsed;

// resolved in attach() (DllMain may call the loader; the hooks must not)
static perfcore::WaitOnAddressFn g_waitOnAddress;
static VOID (WINAPI* g_wakeAll)(PVOID);
static void* g_switchToThread[2];
static FARPROC g_getProcAddress[2];

static void plog(const char* fmt, ...) {
    if (!g_log) return;
    char buf[900];
    va_list ap; va_start(ap, fmt);
    _vsnprintf_s(buf, sizeof buf, _TRUNCATE, fmt, ap);
    va_end(ap);
    g_log("perf: %s", buf);
}

static int index_of(const char* id) {
    for (int i = 0; i < kPatchCount; ++i) if (strcmp(kPatches[i].id, id) == 0) return i;
    return -1;
}
static bool is_on(const char* id) { int i = index_of(id); return i >= 0 && g_status[i].on; }

static void set_state(int i, State s, const char* why) {
    g_status[i].state = s; g_status[i].why = why ? why : ""; g_status[i].tick = GetTickCount64();
    if (s == APPLIED || s == PRESENT) plog("%s %s%s%s", kPatches[i].id, kStateNames[s], why && *why ? ": " : "", why ? why : "");
    else if (s == REFUSED) plog("%s refused: %s", kPatches[i].id, why ? why : "");
}

// ---- memory helpers ------------------------------------------------------------------------------
static bool readable(const void* p, size_t n) {
    MEMORY_BASIC_INFORMATION mbi;
    const BYTE* a = (const BYTE*)p;
    while (n) {
        if (!VirtualQuery(a, &mbi, sizeof mbi) || mbi.State != MEM_COMMIT || (mbi.Protect & (PAGE_NOACCESS | PAGE_GUARD))) return false;
        size_t left = (size_t)((BYTE*)mbi.BaseAddress + mbi.RegionSize - a);
        if (left >= n) return true;
        n -= left; a += left;
    }
    return true;
}

static bool write_code(BYTE* at, const void* bytes, size_t n) {
    DWORD old;
    if (!VirtualProtect(at, n, PAGE_EXECUTE_READWRITE, &old)) return false;
    memcpy(at, bytes, n);
    DWORD ignored;
    VirtualProtect(at, n, old, &ignored);
    FlushInstructionCache(GetCurrentProcess(), at, n);
    return memcmp(at, bytes, n) == 0;
}

static bool swap_pointer(void** slot, void* expected, void* value) {
    DWORD old;
    if (!VirtualProtect(slot, sizeof(void*), PAGE_READWRITE, &old)) return false;
    void* previous = InterlockedCompareExchangePointer(slot, value, expected);
    DWORD ignored;
    VirtualProtect(slot, sizeof(void*), old, &ignored);
    return previous == expected;
}

static const Site* site(const char* id, uint32_t rva) {
    for (const Site& s : kSites) if (strcmp(s.id, id) == 0 && s.rva == rva) return &s;
    return nullptr;
}
static bool site_ok(BYTE* base, const char* id, uint32_t rva, std::string& why) {
    const Site* s = site(id, rva);
    perfcore::Pattern p;
    if (!s || !perfcore::parse_pattern(s->expect, p)) { why = "bad site table"; return false; }
    if (!readable(base + rva, p.v.size()) || !perfcore::matches(base + rva, p)) {
        char b[96]; _snprintf_s(b, sizeof b, _TRUNCATE, "bytes at +0x%X differ from r208022", rva); why = b; return false;
    }
    return true;
}

static bool module_is_known(int m, BYTE* base, std::string& why) {
    auto dos = (IMAGE_DOS_HEADER*)base;
    if (!readable(base, 0x400) || dos->e_magic != IMAGE_DOS_SIGNATURE) { why = "no PE header"; return false; }
    auto nt = (IMAGE_NT_HEADERS32*)(base + dos->e_lfanew);
    if (nt->FileHeader.TimeDateStamp != kModules[m].stamp || nt->OptionalHeader.SizeOfImage != kModules[m].sizeOfImage) {
        char b[128];
        _snprintf_s(b, sizeof b, _TRUNCATE, "%s is not the r208022 build (stamp %08lX size %08lX)", kModules[m].name,
                    nt->FileHeader.TimeDateStamp, nt->OptionalHeader.SizeOfImage);
        why = b; return false;
    }
    return true;
}

// IAT slot for dll!function; the table pins the expected RVA.
static void** import_slot(BYTE* base, const char* id) {
    const Import* want = nullptr;
    for (const Import& i : kImports) if (strcmp(i.id, id) == 0) { want = &i; break; }
    if (!want) return nullptr;
    auto nt = (IMAGE_NT_HEADERS32*)(base + ((IMAGE_DOS_HEADER*)base)->e_lfanew);
    DWORD rva = nt->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT].VirtualAddress;
    if (!rva) return nullptr;
    for (auto d = (IMAGE_IMPORT_DESCRIPTOR*)(base + rva); d->Name; ++d) {
        if (_stricmp((char*)(base + d->Name), want->dll) != 0 || !d->OriginalFirstThunk) continue;
        auto names = (IMAGE_THUNK_DATA32*)(base + d->OriginalFirstThunk);
        auto slots = (IMAGE_THUNK_DATA32*)(base + d->FirstThunk);
        for (; names->u1.AddressOfData; ++names, ++slots) {
            if (IMAGE_SNAP_BY_ORDINAL32(names->u1.Ordinal)) continue;
            auto by = (IMAGE_IMPORT_BY_NAME*)(base + names->u1.AddressOfData);
            if (strcmp((char*)by->Name, want->function) == 0)
                return (BYTE*)&slots->u1.Function - base == (ptrdiff_t)want->slotRva ? (void**)&slots->u1.Function : nullptr;
        }
    }
    return nullptr;
}
static bool bound(BYTE* base, int m, void* v) {
    uintptr_t x = (uintptr_t)v;
    return x >= 0x10000 && !(x >= (uintptr_t)base && x < (uintptr_t)base + kModules[m].sizeOfImage);
}

struct SlotHook { void** slot; void* original; void* hook; };
static std::vector<SlotHook> g_slots;

// Returns 1 applied, 0 deferred (slot not bound yet), -1 refused.
static int hook_import(BYTE* base, int m, const char* id, void* hook, void** original, std::string& why) {
    void** slot = import_slot(base, id);
    if (!slot) { why = "import slot not at the r208022 RVA"; return -1; }
    void* cur = *slot;
    if (cur == hook) return 1;
    if (!bound(base, m, cur)) return 0;
    *original = cur;
    if (!swap_pointer(slot, cur, hook)) { why = "IAT slot changed during the swap"; return -1; }
    g_slots.push_back({ slot, cur, hook });
    return 1;
}

// 5-byte jmp at `at` over `steal` bytes of verified prologue; trampoline = stolen bytes + jmp back.
static void* jump_hook(BYTE* at, size_t steal, void* hook) {
    if (!g_thunks || steal < 5 || steal > 16 || g_thunkUsed + steal + 5 > 4096) return nullptr;
    BYTE* t = g_thunks + g_thunkUsed;
    DWORD old;
    if (!VirtualProtect(g_thunks, 4096, PAGE_EXECUTE_READWRITE, &old)) return nullptr;
    memcpy(t, at, steal);
    t[steal] = 0xE9; *(int32_t*)(t + steal + 1) = (int32_t)((at + steal) - (t + steal + 5));
    VirtualProtect(g_thunks, 4096, PAGE_EXECUTE_READ, &old);
    FlushInstructionCache(GetCurrentProcess(), t, steal + 5);
    BYTE jmp[16];
    jmp[0] = 0xE9; *(int32_t*)(jmp + 1) = (int32_t)((BYTE*)hook - (at + 5));
    for (size_t i = 5; i < steal; ++i) jmp[i] = 0x90;
    if (!write_code(at, jmp, steal)) return nullptr;
    g_thunkUsed += (steal + 5 + 15) & ~(size_t)15;
    return t;
}

// ---- byte patches --------------------------------------------------------------------------------
static void apply_bytes(int i, BYTE* base, bool late) {
    const Site* s = nullptr;
    for (const Site& x : kSites) if (strcmp(x.id, kPatches[i].id) == 0 && x.bytes) { s = &x; break; }
    perfcore::Pattern expect, bytes;
    if (!s || !perfcore::parse_pattern(s->expect, expect) || !perfcore::parse_pattern(s->bytes, bytes) || bytes.v.size() > sizeof g_status[i].original) {
        set_state(i, REFUSED, "bad site table"); return;
    }
    if (!readable(base + s->rva, expect.v.size()) || !readable(base + s->patchRva, bytes.v.size())) { set_state(i, REFUSED, "site not readable"); return; }
    if (!perfcore::matches(base + s->rva, expect)) {
        if (perfcore::already_applied(base, s->rva, expect, s->patchRva, bytes)) { set_state(i, PRESENT, "already in place"); return; }
        char b[96]; _snprintf_s(b, sizeof b, _TRUNCATE, "bytes at +0x%X differ from r208022", s->rva);
        set_state(i, REFUSED, b); return;
    }
    if (late && perfcore::changed_bytes(base + s->patchRva, bytes) > 1) { set_state(i, REFUSED, "module was running before the driver attached (multi-byte code change)"); return; }
    // extra check-only rows of the same id (preconditions the patch relies on)
    for (const Site& x : kSites) {
        if (strcmp(x.id, kPatches[i].id) != 0 || x.bytes) continue;
        std::string why;
        if (!site_ok(base, x.id, x.rva, why)) { set_state(i, REFUSED, why.c_str()); return; }
    }
    BYTE* at = base + s->patchRva;
    memcpy(g_status[i].original, at, bytes.v.size());
    g_status[i].len = bytes.v.size(); g_status[i].at = at;
    if (!write_code(at, bytes.v.data(), bytes.v.size())) { set_state(i, REFUSED, "write failed"); return; }
    char b[64]; _snprintf_s(b, sizeof b, _TRUNCATE, "+0x%X, %u bytes", s->patchRva, (unsigned)bytes.v.size());
    set_state(i, APPLIED, b);
}

// ---- dispmode-cache ------------------------------------------------------------------------------
static LONG (WINAPI* g_cdsOriginal)(LPCSTR, DEVMODEA*, HWND, DWORD, LPVOID);
static SRWLOCK g_modeLock = SRWLOCK_INIT;
static std::map<uint64_t, LONG>* g_modeCache;
static volatile LONG g_modeHits, g_modeMisses;

static LONG WINAPI hk_change_display(LPCSTR dev, DEVMODEA* dm, HWND hwnd, DWORD flags, LPVOID param) {
    if (!(flags & CDS_TEST) || !dm || dev || hwnd || param) {
        if (!(flags & CDS_TEST)) { AcquireSRWLockExclusive(&g_modeLock); if (g_modeCache) g_modeCache->clear(); ReleaseSRWLockExclusive(&g_modeLock); }
        return g_cdsOriginal(dev, dm, hwnd, flags, param);
    }
    uint64_t key = perfcore::mode_key(dm->dmFields, dm->dmPelsWidth, dm->dmPelsHeight, dm->dmBitsPerPel, dm->dmDisplayFrequency, flags);
    AcquireSRWLockShared(&g_modeLock);
    auto it = g_modeCache->find(key);
    bool hit = it != g_modeCache->end();
    LONG cached = hit ? it->second : 0;
    ReleaseSRWLockShared(&g_modeLock);
    if (hit) { InterlockedIncrement(&g_modeHits); return cached; }
    LONG r = g_cdsOriginal(dev, dm, hwnd, flags, param);
    AcquireSRWLockExclusive(&g_modeLock); (*g_modeCache)[key] = r; ReleaseSRWLockExclusive(&g_modeLock);
    InterlockedIncrement(&g_modeMisses);
    return r;
}

// ---- fast-splitpath ------------------------------------------------------------------------------
static void (__cdecl* g_splitOriginal)(const char*, char*, char*, char*, char*);
static perfcore::LeadBytes g_lead;
static volatile LONG g_leadState;   // 0 not built, 1 building, 2 ready
static volatile LONG g_splitCalls;

static void build_lead_table() {
    if (g_leadState == 2) return;
    if (InterlockedCompareExchange(&g_leadState, 1, 0) == 0) {
        // First call happens on a game thread with none of our locks held: loader calls are fine.
        UINT cp = GetACP();
        HMODULE crt = GetModuleHandleA("MSVCR100.dll");
        typedef int (__cdecl* GetMbcp)();
        GetMbcp f = crt ? (GetMbcp)GetProcAddress(crt, "_getmbcp") : nullptr;
        if (f) { int v = f(); if (v > 0) cp = (UINT)v; }
        perfcore::lead_bytes_from_codepage(g_lead, cp);
        InterlockedExchange(&g_leadState, 2);
    } else {
        while (g_leadState != 2) YieldProcessor();
    }
}

static void __cdecl hk_splitpath(const char* path, char* drive, char* dir, char* fname, char* ext) {
    InterlockedIncrement(&g_splitCalls);
    build_lead_table();
    if (!perfcore::fast_splitpath(g_lead, path, drive, dir, fname, ext)) g_splitOriginal(path, drive, dir, fname, ext);
}

// ---- frame-latency -------------------------------------------------------------------------------
typedef HRESULT (WINAPI* CreateDeviceFn)(IDXGIAdapter*, D3D_DRIVER_TYPE, HMODULE, UINT, const D3D_FEATURE_LEVEL*, UINT, UINT,
                                         ID3D11Device**, D3D_FEATURE_LEVEL*, ID3D11DeviceContext**);
static FARPROC (WINAPI* g_gpaOriginal)(HMODULE, LPCSTR);
static CreateDeviceFn g_createDevice;
static volatile LONG g_latencySet;
static UINT g_frameLatency = 1;

static HRESULT WINAPI hk_create_device(IDXGIAdapter* a, D3D_DRIVER_TYPE t, HMODULE sw, UINT flags, const D3D_FEATURE_LEVEL* fl,
                                       UINT nfl, UINT sdk, ID3D11Device** dev, D3D_FEATURE_LEVEL* got, ID3D11DeviceContext** ctx) {
    HRESULT hr = g_createDevice(a, t, sw, flags, fl, nfl, sdk, dev, got, ctx);
    if (SUCCEEDED(hr) && dev && *dev) {
        IDXGIDevice1* d1 = nullptr;
        if (SUCCEEDED((*dev)->QueryInterface(__uuidof(IDXGIDevice1), (void**)&d1)) && d1) {
            HRESULT r = d1->SetMaximumFrameLatency(g_frameLatency);
            d1->Release();
            InterlockedIncrement(&g_latencySet);
            plog("frame-latency: SetMaximumFrameLatency(%u) on the game device: 0x%08lX", g_frameLatency, (unsigned long)r);
        }
    }
    return hr;
}

static FARPROC WINAPI hk_get_proc_address(HMODULE m, LPCSTR name) {
    FARPROC r = g_gpaOriginal(m, name);
    if (r && (uintptr_t)name > 0xFFFF && strcmp(name, "D3D11CreateDevice") == 0) {
        g_createDevice = (CreateDeviceFn)r;
        return (FARPROC)&hk_create_device;
    }
    return r;
}

// ---- render-thread-event-sync --------------------------------------------------------------------
static int g_spins = 256;
typedef void (__thiscall* ThisCall)(void*);
static ThisCall g_syncTrampoline, g_waitFlushTrampoline;
static bool g_syncWake;      // render-thread-event-sync wakers on (the hooks are shared with diag-frame-trace)

static void __stdcall main_wait(volatile LONG* flag) { perfcore::wait_while_nonzero(flag, g_spins, 1, g_waitOnAddress); }
static void __stdcall render_wait(volatile LONG* flag) { perfcore::wait_while(flag, 0, g_spins, 1, g_waitOnAddress); }

// Replaces `call [SwitchToThread]` in WaitFlushFinishedCond: [ebp-0x14] is the caller's SRenderThread*.
static __declspec(naked) BOOL WINAPI main_yield_stub() {
    __asm {
        mov eax, [ebp - 0x14]
        add eax, 0x20
        push eax
        call main_wait
        mov eax, 1
        ret
    }
}
// Replaces the SwitchToThread pointer WaitFlushCond keeps in EBX: EDI is the SRenderThread*.
static __declspec(naked) BOOL WINAPI render_yield_stub() {
    __asm {
        lea eax, [edi + 0x20]
        push eax
        call render_wait
        mov eax, 1
        ret
    }
}
static void* g_mainCell = (void*)&main_yield_stub;
static void* g_renderCell = (void*)&render_yield_stub;

// ---- diag-frame-trace ----------------------------------------------------------------------------
// Frame phases from the two render-thread sync points (main: SyncMainWithRender once per frame; render:
// WaitFlushCond, idle until the main thread hands over a frame), and EBP-chain stack samples of the
// main and render threads, taken only while the current frame is over budget. A sampler thread checks
// every trace_period_ms (2): if the running main (or render) frame is older than trace_budget_ms (50),
// it suspends that thread, reads its context, walks saved EBP frames inside the thread's own stack
// bounds (no stack scan, no allocation, no lock while the thread is suspended) and resumes it. When
// the long frame ends it writes the hitch (duration, phases, samples, top return addresses) to
// aadriver.log and every sample to frametrace-<pid>.tsv beside the driver.
static bool g_traceOn;
static LARGE_INTEGER g_qpf;
static volatile LONGLONG g_mainStart, g_mainSyncIn, g_mainLastLen, g_mainLastSync;  // QPC
static volatile LONG g_mainFrames;
static volatile DWORD g_mainThreadId, g_renderThreadId;
static volatile LONGLONG g_renderBusySince, g_renderWaitIn;   // render: busy from WaitFlushCond exit to next entry
static volatile LONG g_renderFrames;
static volatile LONGLONG g_renderLastBusy, g_renderLastWait;

static double qpc_ms(LONGLONG d) { return d * 1000.0 / (double)g_qpf.QuadPart; }
static LONGLONG qpc_now() { LARGE_INTEGER t; QueryPerformanceCounter(&t); return t.QuadPart; }

static void trace_main_sync_enter() {
    if (!g_mainThreadId) g_mainThreadId = GetCurrentThreadId();
    g_mainSyncIn = qpc_now();
}
static void trace_main_sync_leave() {
    LONGLONG now = qpc_now();
    g_mainLastSync = now - g_mainSyncIn;
    g_mainLastLen = g_mainStart ? now - g_mainStart : 0;
    g_mainStart = now;
    InterlockedIncrement(&g_mainFrames);
}
static void trace_render_wait_enter() {
    if (!g_renderThreadId) g_renderThreadId = GetCurrentThreadId();
    LONGLONG now = qpc_now();
    if (g_renderBusySince) g_renderLastBusy = now - g_renderBusySince;
    g_renderBusySince = 0;
    g_renderWaitIn = now;
}
static void trace_render_wait_leave() {
    LONGLONG now = qpc_now();
    g_renderLastWait = now - g_renderWaitIn;
    g_renderBusySince = now;
    InterlockedIncrement(&g_renderFrames);
}

static void __fastcall hk_sync_main(void* self, void*) {
    if (g_traceOn) trace_main_sync_enter();
    g_syncTrampoline(self);
    if (g_wakeAll && g_syncWake) g_wakeAll((BYTE*)self + 0x20);   // m_nFlush just became 1: wake the render thread
    if (g_traceOn) trace_main_sync_leave();
}
static void __fastcall hk_wait_flush(void* self, void*) {
    if (g_traceOn) trace_render_wait_enter();
    if (g_wakeAll && g_syncWake) g_wakeAll((BYTE*)self + 0x20);   // the render loop cleared m_nFlush just before this call
    g_waitFlushTrampoline(self);
    if (g_traceOn) trace_render_wait_leave();
}

// The two jump hooks are shared by render-thread-event-sync (wakers) and diag-frame-trace (timestamps).
static bool install_sync_hooks(BYTE* base, const char* id, std::string& why) {
    if (g_syncTrampoline) return true;
    if (!site_ok(base, id, 0x1958F0, why) || !site_ok(base, id, 0x199610, why)) return false;
    g_waitFlushTrampoline = (ThisCall)jump_hook(base + 0x1958F0, 9, (void*)&hk_wait_flush);
    if (!g_waitFlushTrampoline) { why = "WaitFlushCond hook failed"; return false; }
    g_syncTrampoline = (ThisCall)jump_hook(base + 0x199610, 9, (void*)&hk_sync_main);
    if (!g_syncTrampoline) { why = "SyncMainWithRender hook failed"; return false; }
    return true;
}

static int install_render_sync(BYTE* base, std::string& why) {
    if (!g_waitOnAddress || !g_wakeAll) { why = "WaitOnAddress is not available (Windows 8 or later)"; return -1; }
    static const uint32_t sites[] = { 0x195857, 0x1959FB, 0x1958F0, 0x199610, 0x1998CF };
    for (uint32_t rva : sites) if (!site_ok(base, "render-thread-event-sync", rva, why)) return -1;
    void** slot = import_slot(base, "render-thread-event-sync");
    if (!slot) { why = "SwitchToThread import slot not found"; return -1; }
    if (!bound(base, CRYRENDER, *slot)) { why = "SwitchToThread import not bound"; return -1; }
    if (*slot != g_switchToThread[0] && *slot != g_switchToThread[1]) { why = "SwitchToThread import is hooked by something else"; return -1; }
    void** mainOperand = (void**)(base + 0x195859);
    void** renderOperand = (void**)(base + 0x1959FD);
    if (*mainOperand != (void*)slot || *renderOperand != (void*)slot) { why = "call operands do not point at the SwitchToThread slot"; return -1; }
    // wakers first: a waiting thread without a waker still ends after 1 ms
    g_syncWake = true;
    if (!install_sync_hooks(base, "render-thread-event-sync", why)) return -1;
    void* mainCell = &g_mainCell; void* renderCell = &g_renderCell;
    if (!write_code((BYTE*)mainOperand, &mainCell, 4) || !write_code((BYTE*)renderOperand, &renderCell, 4)) { why = "operand write failed"; return -1; }
    return 1;
}

// Stack bounds of another thread (TEB StackBase +4, StackLimit +8).
typedef LONG (NTAPI* QueryThreadFn)(HANDLE, int, void*, ULONG, ULONG*);
static QueryThreadFn g_queryThread;
static BYTE* thread_teb(HANDLE h) {
    if (!g_queryThread) return nullptr;
    struct { LONG exitStatus; BYTE* teb; DWORD pid, tidc; ULONG_PTR affinity; LONG prio, basePrio; } tbi = {};
    return g_queryThread(h, 0, &tbi, sizeof tbi, NULL) >= 0 ? tbi.teb : nullptr;
}

enum { kTraceDepth = 24, kTraceMaxSamples = 1024 };
struct TraceSample { LONGLONG at; char thread; BYTE depth; uint32_t pc[kTraceDepth]; };
static TraceSample* g_samples;     // kTraceMaxSamples, preallocated
struct TracedThread { DWORD tid; HANDLE h; uintptr_t stackLow, stackHigh; };

static bool open_traced(TracedThread& t, DWORD tid) {
    if (t.tid == tid && t.h) return true;
    if (t.h) CloseHandle(t.h);
    t = TracedThread();
    HANDLE h = OpenThread(THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_LIMITED_INFORMATION, FALSE, tid);
    if (!h) return false;
    BYTE* teb = thread_teb(h);
    if (!teb) { CloseHandle(h); return false; }
    t.tid = tid; t.h = h;
    t.stackHigh = *(uintptr_t*)(teb + 4); t.stackLow = *(uintptr_t*)(teb + 8);
    return true;
}
// Suspend, read EIP/EBP/ESP, walk saved frames within [esp, stackHigh), resume. Nothing else runs
// while the thread is suspended: no allocation, no lock, no call that could wait on it.
static bool sample_thread(TracedThread& t, char tag, TraceSample& s) {
    CONTEXT c; c.ContextFlags = CONTEXT_CONTROL | CONTEXT_INTEGER;
    if (SuspendThread(t.h) == (DWORD)-1) return false;
    bool ok = GetThreadContext(t.h, &c) != 0;
    int n = 0;
    if (ok) {
        s.pc[n++] = c.Eip;
        uintptr_t ebp = c.Ebp, low = c.Esp;
        while (n < kTraceDepth && ebp >= low && ebp + 8 <= t.stackHigh && !(ebp & 3)) {
            uintptr_t next = *(uintptr_t*)ebp, ret = *(uintptr_t*)(ebp + 4);
            if (ret < 0x10000) break;
            s.pc[n++] = (uint32_t)ret;
            if (next <= ebp) break;
            low = ebp + 8; ebp = next;
        }
    }
    ResumeThread(t.h);
    s.at = qpc_now(); s.thread = tag; s.depth = (BYTE)n;
    return ok && n > 0;
}

struct ModRange { uintptr_t lo, hi; char name[24]; };
static ModRange g_modRanges[512];
static int g_modCount;
static void refresh_modules() {
    HMODULE mods[512]; DWORD need = 0;
    if (!EnumProcessModules(GetCurrentProcess(), mods, sizeof mods, &need)) return;
    int n = 0;
    for (DWORD i = 0; i < need / sizeof(HMODULE) && i < 512 && n < 512; ++i) {
        MODULEINFO mi; char name[64];
        if (!GetModuleInformation(GetCurrentProcess(), mods[i], &mi, sizeof mi) || !GetModuleBaseNameA(GetCurrentProcess(), mods[i], name, sizeof name)) continue;
        g_modRanges[n].lo = (uintptr_t)mi.lpBaseOfDll; g_modRanges[n].hi = g_modRanges[n].lo + mi.SizeOfImage;
        strncpy_s(g_modRanges[n].name, name, _TRUNCATE);
        if (char* dot = strrchr(g_modRanges[n].name, '.')) *dot = 0;
        ++n;
    }
    g_modCount = n;
}
static std::string where(uint32_t pc) {
    char b[48];
    for (int i = 0; i < g_modCount; ++i)
        if (pc >= g_modRanges[i].lo && pc < g_modRanges[i].hi) { _snprintf_s(b, sizeof b, _TRUNCATE, "%s+0x%X", g_modRanges[i].name, (unsigned)(pc - g_modRanges[i].lo)); return b; }
    _snprintf_s(b, sizeof b, _TRUNCATE, "0x%08X", pc);
    return b;
}
static bool is_system(uint32_t pc) {
    for (int i = 0; i < g_modCount; ++i)
        if (pc >= g_modRanges[i].lo && pc < g_modRanges[i].hi) {
            const char* n = g_modRanges[i].name;
            return !_stricmp(n, "ntdll") || !_stricmp(n, "KERNELBASE") || !_stricmp(n, "kernel32") || !_stricmp(n, "win32u") || !_stricmp(n, "user32");
        }
    return false;
}

static volatile LONG g_hitches;
static FILE* g_traceFile;
// Summarise one long frame: per thread, the return addresses present in the most samples (inclusive).
// Summarise one long frame: per thread, the most frequent leaf chains (the first three frames outside
// ntdll/kernel/user32 of each sample, innermost first), as a share of that thread's samples.
static void finish_hitch(int frame, double ms, double syncMs, double renderBusyMs, double renderWaitMs, int nSamples) {
    LONG k = InterlockedIncrement(&g_hitches);
    refresh_modules();
    std::string top[2];
    for (int th = 0; th < 2; ++th) {
        char tag = th ? 'R' : 'M';
        std::map<std::string, int> chains; int total = 0;
        for (int i = 0; i < nSamples; ++i) {
            if (g_samples[i].thread != tag) continue;
            ++total;
            std::string key; int used = 0;
            for (int d = 0; d < g_samples[i].depth && used < 3; ++d) {
                uint32_t pc = g_samples[i].pc[d];
                if (is_system(pc)) continue;
                if (used++) key += "<";
                key += where(pc);
            }
            ++chains[key.empty() ? std::string("system") : key];
        }
        if (!total) continue;
        std::vector<std::pair<int, std::string>> v;
        for (auto& kv : chains) v.push_back({ kv.second, kv.first });
        std::sort(v.begin(), v.end(), [](const std::pair<int, std::string>& a, const std::pair<int, std::string>& b) { return a.first > b.first; });
        char b[96];
        _snprintf_s(b, sizeof b, _TRUNCATE, "%s %d samples:", th ? "render" : "main", total);
        top[th] = b;
        for (size_t i = 0; i < v.size() && i < 4; ++i) {
            top[th] += " ["; top[th] += v[i].second;
            _snprintf_s(b, sizeof b, _TRUNCATE, "] %d%%", v[i].first * 100 / total);
            top[th] += b;
        }
    }
    if (k <= 400)
        plog("hitch %ld: main frame %d %.1f ms (in SyncMainWithRender %.1f ms) render busy %.1f ms wait %.1f ms | %s | %s",
             k, frame, ms, syncMs, renderBusyMs, renderWaitMs, top[0].empty() ? "main no samples" : top[0].c_str(), top[1].empty() ? "render no samples" : top[1].c_str());
    if (g_traceFile) {
        for (int i = 0; i < nSamples; ++i) {
            fprintf(g_traceFile, "%ld\t%d\t%.1f\t%c\t%.1f", k, frame, ms, g_samples[i].thread, qpc_ms(g_samples[i].at));
            for (int d = 0; d < g_samples[i].depth; ++d) fprintf(g_traceFile, "\t%s", where(g_samples[i].pc[d]).c_str());
            fputc('\n', g_traceFile);
        }
        fflush(g_traceFile);
    }
}

static DWORD WINAPI trace_thread(LPVOID) {
    SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
    double budget = perfcore::option_int(g_switches, "trace_budget_ms", 50, 5, 10000);
    int period = perfcore::option_int(g_switches, "trace_period_ms", 2, 1, 100);
    int maxSamples = perfcore::option_int(g_switches, "trace_max_samples", 400, 10, kTraceMaxSamples);
    HANDLE timer = CreateWaitableTimerExW(NULL, NULL, 0x2 /* CREATE_WAITABLE_TIMER_HIGH_RESOLUTION */, TIMER_ALL_ACCESS);
    if (!timer) timer = CreateWaitableTimerW(NULL, FALSE, NULL);
    TracedThread mainT = {}, renderT = {};
    LONG frame = g_mainFrames;
    int n = 0;
    LONGLONG frameRenderBusy = 0;
    for (;;) {
        LARGE_INTEGER due; due.QuadPart = -10000LL * period;
        if (timer && SetWaitableTimer(timer, &due, 0, NULL, NULL, FALSE)) WaitForSingleObject(timer, 1000); else Sleep(period);
        LONG f = g_mainFrames;
        if (f != frame) {
            double ms = qpc_ms(g_mainLastLen);
            if (n || ms > budget)
                finish_hitch(f, ms, qpc_ms(g_mainLastSync), qpc_ms(frameRenderBusy ? frameRenderBusy : g_renderLastBusy), qpc_ms(g_renderLastWait), n);
            frame = f; n = 0; frameRenderBusy = 0;
        }
        LONGLONG now = qpc_now(), start = g_mainStart;
        if (!start || n >= maxSamples) continue;
        if (qpc_ms(now - start) > budget && g_mainThreadId && open_traced(mainT, g_mainThreadId) && sample_thread(mainT, 'M', g_samples[n])) ++n;
        LONGLONG busy = g_renderBusySince;
        if (busy && n < maxSamples && qpc_ms(now - busy) > budget && g_renderThreadId && open_traced(renderT, g_renderThreadId) &&
            sample_thread(renderT, 'R', g_samples[n])) { ++n; frameRenderBusy = now - busy; }
    }
}

static int install_frame_trace(BYTE* base, std::string& why) {
    if (!install_sync_hooks(base, "diag-frame-trace", why)) return -1;
    QueryPerformanceFrequency(&g_qpf);
    g_samples = (TraceSample*)VirtualAlloc(NULL, sizeof(TraceSample) * kTraceMaxSamples, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    if (!g_samples) { why = "no sample buffer"; return -1; }
    wchar_t path[MAX_PATH];
    DWORD len = GetModuleFileNameW(g_self, path, MAX_PATH);
    wchar_t* slash = len && len < MAX_PATH ? wcsrchr(path, L'\\') : nullptr;
    if (slash) {
        _snwprintf_s(slash + 1, MAX_PATH - (slash + 1 - path), _TRUNCATE, L"frametrace-%lu.tsv", GetCurrentProcessId());
        g_traceFile = _wfsopen(path, L"w", _SH_DENYWR);
        if (g_traceFile) fputs("hitch\tframe\tframe_ms\tthread\tsample_ms\tstack...\n", g_traceFile);
    }
    g_traceOn = true;
    HANDLE t = CreateThread(NULL, 0, trace_thread, NULL, 0, NULL);
    if (!t) { why = "cannot start the sampler"; return -1; }
    CloseHandle(t);
    char b[120];
    _snprintf_s(b, sizeof b, _TRUNCATE, "+0x199610/+0x1958F0 hooks, budget %d ms", perfcore::option_int(g_switches, "trace_budget_ms", 50, 5, 10000));
    why = b;
    return 1;
}

// ---- pak-catalog-scope / pak-findchild-index -----------------------------------------------------
static DWORD g_tls = TLS_OUT_OF_INDEXES;
typedef bool (__thiscall* CatalogFn)(void*, int*);
static CatalogFn g_catalogTrampoline;
static void* g_findChildTrampoline;
static bool g_indexOn;
static volatile LONG g_indexFallbacks;

static uint8_t* __cdecl find_child_fast(uint8_t* node, const char* name) {
    DWORD err = GetLastError();
    auto idx = (perfcore::ChildIndex*)TlsGetValue(g_tls);
    uint8_t* r = (uint8_t*)-1;
    if (idx) {
        try { r = idx->find(node, name); } catch (...) { r = (uint8_t*)-1; InterlockedIncrement(&g_indexFallbacks); }
    }
    SetLastError(err);
    return r;
}
// FindChild: EAX = node, [esp+4] = name, callee pops 4, result in EAX; ECX/EDX are scratch there too.
static __declspec(naked) void find_child_thunk() {
    __asm {
        push eax
        push dword ptr [esp + 8]
        push eax
        call find_child_fast
        add esp, 8
        cmp eax, 0FFFFFFFFh
        je slow
        add esp, 4
        ret 4
    slow:
        pop eax
        jmp dword ptr [g_findChildTrampoline]
    }
}

static bool catalog_call(void* self, int* counts) {
    bool r = false;
    __try { r = g_catalogTrampoline(self, counts); }
    __finally { TlsSetValue(g_tls, NULL); }
    return r;
}
static bool __fastcall hk_catalog(void* self, void*, int* counts) {
    perfcore::ChildIndex* idx = nullptr;
    if (g_indexOn) { try { idx = new perfcore::ChildIndex(); } catch (...) { idx = nullptr; } }
    TlsSetValue(g_tls, idx);
    LARGE_INTEGER f, t0, t1; QueryPerformanceFrequency(&f); QueryPerformanceCounter(&t0);
    bool r = catalog_call(self, counts);
    QueryPerformanceCounter(&t1);
    plog("pak catalogue load %.1f ms ok=%d entries=%d%s%s", (t1.QuadPart - t0.QuadPart) * 1000.0 / f.QuadPart, (int)r,
        counts ? counts[0] : -1, idx ? " index nodes=" : "", idx ? std::to_string(idx->nodes()).c_str() : "");
    delete idx;
    return r;
}

// ---- threads -------------------------------------------------------------------------------------
static DWORD WINAPI prefetch_thread(LPVOID) {
    SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    wchar_t path[MAX_PATH];
    DWORD n = GetModuleFileNameW(NULL, path, MAX_PATH);
    wchar_t* slash = n && n < MAX_PATH ? wcsrchr(path, L'\\') : nullptr;
    if (slash) { *slash = 0; slash = wcsrchr(path, L'\\'); }
    if (!slash || wcscpy_s(slash + 1, MAX_PATH - (slash + 1 - path), L"game_pak")) { plog("pak-prefetch: no client path"); return 0; }
    HANDLE h = CreateFileW(path, GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, NULL, OPEN_EXISTING,
                           FILE_FLAG_SEQUENTIAL_SCAN, NULL);
    if (h == INVALID_HANDLE_VALUE) { plog("pak-prefetch: cannot open game_pak (%lu)", GetLastError()); return 0; }
    LARGE_INTEGER size = {};
    GetFileSizeEx(h, &size);
    LONGLONG want = (LONGLONG)perfcore::option_int(g_switches, "prefetch_mb", 96, 1, 1024) << 20;
    LONGLONG start = size.QuadPart > want ? (size.QuadPart - want) & ~0xFFFFLL : 0;
    std::vector<BYTE> buf(4 << 20);
    LARGE_INTEGER at; at.QuadPart = start;
    SetFilePointerEx(h, at, NULL, FILE_BEGIN);
    ULONGLONG t0 = GetTickCount64(); LONGLONG total = 0; DWORD got = 0;
    while (ReadFile(h, buf.data(), (DWORD)buf.size(), &got, NULL) && got) total += got;
    CloseHandle(h);
    plog("pak-prefetch: read the last %lld MB of game_pak in %llu ms", total >> 20, GetTickCount64() - t0);
    return 0;
}

// One resources line (diag-resources). Loader calls are fine here: no lock is held.
static void log_resources() {
    typedef DWORD (WINAPI* GuiFn)(HANDLE, DWORD);
    typedef BOOL (WINAPI* MemFn)(HANDLE, PPROCESS_MEMORY_COUNTERS, DWORD);
    HMODULE user = GetModuleHandleW(L"user32.dll");
    GuiFn gui = user ? (GuiFn)GetProcAddress(user, "GetGuiResources") : nullptr;
    MemFn mem = (MemFn)GetProcAddress(GetModuleHandleW(L"kernel32.dll"), "K32GetProcessMemoryInfo");
    {
        DWORD handles = 0; GetProcessHandleCount(GetCurrentProcess(), &handles);
        DWORD gdi = gui ? gui(GetCurrentProcess(), 0) : 0, usr = gui ? gui(GetCurrentProcess(), 1) : 0;
        PROCESS_MEMORY_COUNTERS_EX pmc = {}; pmc.cb = sizeof pmc;
        if (mem) mem(GetCurrentProcess(), (PPROCESS_MEMORY_COUNTERS)&pmc, sizeof pmc);
        SIZE_T freeTotal = 0, freeLargest = 0, reserved = 0, committed = 0; unsigned regions = 0;
        MEMORY_BASIC_INFORMATION mbi;
        for (BYTE* a = (BYTE*)0x10000; VirtualQuery(a, &mbi, sizeof mbi) == sizeof mbi; a = (BYTE*)mbi.BaseAddress + mbi.RegionSize) {
            ++regions;
            if (mbi.State == MEM_FREE) { freeTotal += mbi.RegionSize; if (mbi.RegionSize > freeLargest) freeLargest = mbi.RegionSize; }
            else if (mbi.State == MEM_RESERVE) reserved += mbi.RegionSize; else committed += mbi.RegionSize;
            if ((uintptr_t)mbi.BaseAddress + mbi.RegionSize < (uintptr_t)a) break;
        }
        // heaps and how many use the low-fragmentation front end (HeapCompatibilityInformation 2)
        HANDLE heaps[256]; DWORD nHeaps = GetProcessHeaps(256, heaps), lfh = 0;
        for (DWORD h = 0; h < nHeaps && h < 256; ++h) {
            ULONG mode = 0;
            if (HeapQueryInformation(heaps[h], HeapCompatibilityInformation, &mode, sizeof mode, NULL) && mode == 2) ++lfh;
        }
        plog("resources: handles=%lu gdi=%lu user=%lu private=%lu MB va_committed=%lu MB va_reserved=%lu MB va_free=%lu MB largest_free=%lu MB regions=%u heaps=%lu lfh=%lu",
            handles, gdi, usr, (unsigned long)(pmc.PrivateUsage >> 20), (unsigned long)(committed >> 20), (unsigned long)(reserved >> 20),
            (unsigned long)(freeTotal >> 20), (unsigned long)(freeLargest >> 20), regions, nHeaps, lfh);
    }
}

static DWORD WINAPI diag_thread(LPVOID) {
    int interval = perfcore::option_int(g_switches, "diag_interval_s", 60, 5, 3600);
    for (;;) { log_resources(); Sleep((DWORD)interval * 1000); }
}

// ---- physics-role-guard / diag-physics-role --------------------------------------------------------
// CryPhysics keeps per-caller scratch buffers (contacts, contact areas, BV-tree and triangle buffers)
// indexed by iCaller = *TLS(0x3520D814). The "physics thread" points at a cell holding 0; every other
// thread points at a cell holding 4 (MAX_PHYS_THREADS) or has no TLS value (also 4). Slot 4 is shared
// and its users lock it; slot 0 is not locked, it belongs to one thread. MarkAsPhysThread
// (FUN_350827D0(1)) hands the role to its caller with a plain swap of two cells, without waiting for
// the previous owner:
//     if (TLS != *cur) { **cur = 4; other = partner(*cur); *other = 0; *cur = other; TLS = other; }
// CPhysicalWorld::TracePendingRays (world vtable slot 44, FUN_351C3F70) starts with that call, and
// both the main thread (every frame) and the physics thread (every step) call it (live trace
// 2026-11-23). So the role moves to the main thread and back every frame, and the thread that loses
// it keeps iCaller 0 for the query it is in (iCaller is read once per query). Both threads then fill
// the same unlocked buffers: the CryPhysics+0x1689BE write-0x10 (CGeometry::Intersect: the area
// pointer of contact 0 zeroed by the other thread between two reads), +0xAD1DB (camera sweep) and
// the BV-tree / triangle faults, all while streaming fills the world with new geometry.
//
// physics-role-guard: a TracePendingRays call from a thread that is not the owner, while an owner
// exists (its cell holds 0), runs as an ordinary caller (iCaller 4, the locked shared slot). For the
// call, the role pointer and the caller's TLS are parked on a private cell pair, so MarkAsPhysThread
// sees "already the owner" and swaps nothing; the owner keeps iCaller 0. If the owner takes the role
// itself meanwhile (its own TimeStep), the partner rule moves it into the private pair's other cell,
// which stays valid. Guarded calls are serialized. No image byte changes: the slot is replaced in a
// heap copy of the world's vtable (gEnv->pPhysicalWorld vptr swap); the protector rejects CryPhysics
// code changes.
typedef void (__thiscall* StepFn)(void*, float, int);
typedef int (__thiscall* RaysFn)(void*, int);
static StepFn g_stepOriginal;
static RaysFn g_raysOriginal;
static BYTE* g_physBase;
static void* g_world;
static void** g_worldVtable;      // original
static void** g_worldCopy;        // ours (slot -1 = RTTI locator copied)
static bool g_roleTrace, g_roleGuard;
static volatile DWORD g_mainTid;
struct StepStats { DWORD tid; LONG calls, positive, took, lost, rays, raysTook, raysGuarded; bool held; void* cell; };
static StepStats g_steps[16];
static SRWLOCK g_stepLock = SRWLOCK_INIT;
static CRITICAL_SECTION g_parkLock;
static volatile LONG g_roleTakes, g_roleLogs, g_guarded, g_guardMoved, g_guardSkipped;
// Thread inside TimeStep that held iCaller 0 when the step began (diag-physics-role only), and the
// TracePendingRays calls of other threads that came in meanwhile: unguarded they take the role
// while the owner steps (the race), guarded they are parked.
static volatile DWORD g_stepOwnerTid;
static volatile LONG g_unsafeTakes, g_unsafeParked;
// Two private cell pairs, 8-byte aligned like the engine pair (partner = cell ^ 4 relative to 0x352040F8).
__declspec(align(8)) static volatile LONG g_park[2][2] = { { 4, 4 }, { 4, 4 } };

static void* volatile* role_current() { return (void* volatile*)(g_physBase + kPhysRoleCurrent); }
static DWORD role_tls() { return *(DWORD*)(g_physBase + kPhysTlsIndex); }
static bool in_pair(const void* cell, const volatile LONG* pair) { return cell == (const void*)&pair[0] || cell == (const void*)&pair[1]; }
static bool holds_role(void* mine) { return mine && mine == *role_current(); }

static StepStats* stats_for(DWORD tid) {
    for (StepStats& x : g_steps) if (x.tid == tid || !x.tid) { if (!x.tid) x.tid = tid; return &x; }
    return nullptr;
}
static void note(bool step, bool before, bool after, bool guarded, float dt, int arg) {
    DWORD tid = GetCurrentThreadId();
    bool first = false, took = !before && after;
    AcquireSRWLockExclusive(&g_stepLock);
    StepStats* s = stats_for(tid);
    if (s) {
        if (step) { first = s->calls == 0; ++s->calls; if (dt > 0) ++s->positive; if (took) ++s->took; }
        else { first = s->rays == 0; ++s->rays; if (took) ++s->raysTook; if (guarded) ++s->raysGuarded; }
        if (s->held && !before) ++s->lost;   // another thread took the role since this thread's last call
        s->held = after; s->cell = TlsGetValue(role_tls());
    }
    ReleaseSRWLockExclusive(&g_stepLock);
    const char* main = tid == g_mainTid ? " (main)" : "";
    if (first && step) plog("physics step: thread %lu%s first TimeStep(dt=%.4f, flags=0x%X)", tid, main, dt, arg);
    if (first && !step) plog("physics rays: thread %lu%s first TracePendingRays(%d), holds the role: %d", tid, main, arg, (int)before);
    if (took) {
        InterlockedIncrement(&g_roleTakes);
        if (InterlockedIncrement(&g_roleLogs) <= 60)
            plog("physics role: thread %lu%s took iCaller 0 in %s", tid, main, step ? "TimeStep" : "TracePendingRays");
    }
}

// The thread whose TimeStep (dt > 0) ran last: the physics thread, or the main thread when physics is
// single-threaded. Only it may take the role through TracePendingRays.
static volatile DWORD g_stepper;
static volatile LONG g_unmarksParked;
// Ownerless pair for the slot-34 unmark (8-byte aligned, same partner rule).
__declspec(align(8)) static volatile LONG g_phantom[2] = { 4, 4 };
typedef void (__thiscall* VoidFn)(void*);
static VoidFn g_unmarkOriginal;

static int __fastcall hk_trace_rays(void* self, void*, int doTracing) {
    DWORD err = GetLastError();
    DWORD idx = role_tls(), tid = GetCurrentThreadId();
    void* mine = TlsGetValue(idx);
    void* owner = *role_current();
    bool before = holds_role(mine);
    int r;
    bool guarded = false;
    DWORD stepper = g_stepOwnerTid;
    if (g_roleTrace && !before && stepper && stepper != tid)
        InterlockedIncrement(g_roleGuard ? &g_unsafeParked : &g_unsafeTakes);   // the owner is mid-step right now
    DWORD physThread = g_stepper;
    if (g_roleGuard && !before && owner && physThread && physThread != tid) {
        EnterCriticalSection(&g_parkLock);
        owner = *role_current();
        if (!holds_role(mine) && owner) {
            volatile LONG* pair = in_pair(owner, g_park[0]) ? g_park[1] : g_park[0];
            pair[0] = 4; pair[1] = 4;               // a free pair: only non-owner cells, 4 is their value
            TlsSetValue(idx, (void*)&pair[0]);
            if (InterlockedCompareExchangePointer(role_current(), (void*)&pair[0], owner) == owner) {
                SetLastError(err);
                r = g_raysOriginal(self, doTracing);
                err = GetLastError();
                if (InterlockedCompareExchangePointer(role_current(), owner, (void*)&pair[0]) != (void*)&pair[0])
                    InterlockedIncrement(&g_guardMoved);   // the physics thread took the role meanwhile (now on pair[1])
                TlsSetValue(idx, mine);
                InterlockedIncrement(&g_guarded);
                guarded = true;
            } else {
                TlsSetValue(idx, mine);                  // the role moved before the park: engine behaviour
                InterlockedIncrement(&g_guardSkipped);
                SetLastError(err);
                r = g_raysOriginal(self, doTracing);
                err = GetLastError();
            }
        } else {
            InterlockedIncrement(&g_guardSkipped);
            SetLastError(err);
            r = g_raysOriginal(self, doTracing);
            err = GetLastError();
        }
        LeaveCriticalSection(&g_parkLock);
    } else {
        SetLastError(err);
        r = g_raysOriginal(self, doTracing);
        err = GetLastError();
    }
    if (g_roleTrace) note(false, before, holds_role(TlsGetValue(idx)), guarded, 0, doTracing);
    SetLastError(err);
    return r;
}

// Slot 34 hands the owner's role to the partner cell, which belongs to the previous owner (usually the
// main thread): the main thread silently gets iCaller 0 until the physics thread takes it back, possibly
// in the middle of a main-thread query. With the guard the owner's unmark parks the role on an ownerless
// cell; the next TimeStep (or the physics thread's TracePendingRays) takes it from there, from nobody.
static void __fastcall hk_unmark(void* self, void*) {
    DWORD err = GetLastError();
    DWORD idx = role_tls();
    void* mine = TlsGetValue(idx);
    if (g_roleGuard) {
        EnterCriticalSection(&g_parkLock);
        if (mine && mine == *role_current()) {
            g_phantom[0] = 0; g_phantom[1] = 4;
            InterlockedExchangePointer(role_current(), (void*)&g_phantom[0]);
            *(volatile LONG*)mine = 4;
            TlsSetValue(idx, nullptr);
            InterlockedIncrement(&g_unmarksParked);
            LeaveCriticalSection(&g_parkLock);
            if (g_roleTrace) plog("physics role: thread %lu unmarked; role parked ownerless (the engine hands it to the partner cell)", GetCurrentThreadId());
            SetLastError(err);
            return;
        }
        LeaveCriticalSection(&g_parkLock);
    }
    if (g_roleTrace) plog("physics role: thread %lu%s called unmark (slot 34), holds the role: %d", GetCurrentThreadId(),
                          GetCurrentThreadId() == g_mainTid ? " (main)" : "", (int)(mine && mine == *role_current()));
    SetLastError(err);
    g_unmarkOriginal(self);
}

static void __fastcall hk_timestep(void* self, void*, float dt, int flags) {
    DWORD err = GetLastError();
    DWORD idx = role_tls();
    bool before = holds_role(TlsGetValue(idx));
    if (dt > 0) g_stepper = GetCurrentThreadId();
    SetLastError(err);
    if (before) g_stepOwnerTid = GetCurrentThreadId();
    g_stepOriginal(self, dt, flags);
    if (before) g_stepOwnerTid = 0;
    if (!g_roleTrace) return;
    err = GetLastError();
    note(true, before, holds_role(TlsGetValue(idx)), false, dt, flags);
    SetLastError(err);
}

// TLS value of another thread of this process (TEB TlsSlots +0xE10 / TlsExpansionSlots +0xF94).
static bool thread_tls(DWORD tid, DWORD idx, void** value) {
    if (!g_queryThread) return false;
    HANDLE h = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, FALSE, tid);
    if (!h) return false;
    struct { LONG exitStatus; BYTE* teb; DWORD pid, tidc; ULONG_PTR affinity; LONG prio, basePrio; } tbi = {};
    bool ok = g_queryThread(h, 0, &tbi, sizeof tbi, NULL) >= 0 && tbi.teb;
    CloseHandle(h);
    if (!ok) return false;
    if (idx < 64) { *value = *(void* volatile*)(tbi.teb + 0xE10 + idx * 4); return true; }
    void** exp = *(void***)(tbi.teb + 0xF94);
    if (!exp || idx >= 64 + 1024) return false;
    *value = exp[idx - 64];
    return true;
}
// diag-physics-role sampler: every 2 ms, does more than one stepping thread see iCaller 0?
static volatile LONG g_sampleTotal, g_sampleShared, g_sampleMainOwner;
static DWORD WINAPI role_sampler(LPVOID) {
    for (;;) {
        Sleep(2);
        DWORD tids[16]; int n = 0;
        AcquireSRWLockShared(&g_stepLock);
        for (StepStats& x : g_steps) { if (!x.tid) break; tids[n++] = x.tid; }
        ReleaseSRWLockShared(&g_stepLock);
        if (n < 2) continue;
        DWORD idx = role_tls();
        int zero = 0; bool mainZero = false;
        for (int k = 0; k < n; ++k) {
            void* v = nullptr;
            if (thread_tls(tids[k], idx, &v) && v && readable(v, 4) && *(volatile LONG*)v == 0) { ++zero; if (tids[k] == g_mainTid) mainZero = true; }
        }
        InterlockedIncrement(&g_sampleTotal);
        if (zero > 1) InterlockedIncrement(&g_sampleShared);
        if (mainZero) InterlockedIncrement(&g_sampleMainOwner);
    }
}

static std::string physics_role_summary() {
    std::string out;
    char b[300];
    void* cur = *role_current();
    const LONG* cells = (const LONG*)(g_physBase + kPhysRoleCells);
    _snprintf_s(b, sizeof b, _TRUNCATE, "physics role: takes=%ld guarded=%ld guard_moved=%ld guard_skipped=%ld unmarks_parked=%ld stepper=%lu rays_during_owner_step: unguarded=%ld parked=%ld current=%p cells=[%ld,%ld] park=[%ld,%ld][%ld,%ld]\n",
                g_roleTakes, g_guarded, g_guardMoved, g_guardSkipped, g_unmarksParked, g_stepper, g_unsafeTakes, g_unsafeParked, cur, cells[0], cells[1], g_park[0][0], g_park[0][1], g_park[1][0], g_park[1][1]);
    out += b;
    AcquireSRWLockShared(&g_stepLock);
    for (StepStats& x : g_steps) {
        if (!x.tid) break;
        _snprintf_s(b, sizeof b, _TRUNCATE, "physics thread %lu%s: steps=%ld dt>0=%ld step_took=%ld rays=%ld rays_took=%ld rays_guarded=%ld lost=%ld holds=%d cell=%p\n",
                    x.tid, x.tid == g_mainTid ? " (main)" : "", x.calls, x.positive, x.took, x.rays, x.raysTook, x.raysGuarded, x.lost, (int)x.held, x.cell);
        out += b;
        void* now = nullptr;
        if (thread_tls(x.tid, role_tls(), &now))
            _snprintf_s(b, sizeof b, _TRUNCATE, "  tls now=%p value=%ld\n", now, now && readable(now, 4) ? *(volatile LONG*)now : 4L);
        else _snprintf_s(b, sizeof b, _TRUNCATE, "  tls unreadable\n");
        out += b;
    }
    ReleaseSRWLockShared(&g_stepLock);
    _snprintf_s(b, sizeof b, _TRUNCATE, "role samples (2 ms): total=%ld two_threads_with_icaller0=%ld main_with_icaller0=%ld\n",
                g_sampleTotal, g_sampleShared, g_sampleMainOwner);
    out += b;
    return out;
}

// Installs the wrappers once gEnv->pPhysicalWorld exists. Runs on its own thread: loader calls first,
// g_lock only for the status.
static void set_world_state(State s, const char* why) {
    EnterCriticalSection(&g_lock);
    for (int i = 0; i < kPatchCount; ++i)
        if (kPatches[i].kind == WORLD && g_status[i].on && g_status[i].state == PENDING) set_state(i, s, why);
    LeaveCriticalSection(&g_lock);
}
static DWORD WINAPI world_thread(LPVOID) {
    for (int round = 0; round < 3000; ++round, Sleep(100)) {     // 300 s
        HMODULE css = GetModuleHandleW(L"CryScriptSystem.dll");
        BYTE* env = css ? *(BYTE* volatile*)((BYTE*)css + kCssGEnv) : nullptr;
        void* world = env && readable(env + kGEnvPhysicalWorld, 4) ? *(void* volatile*)(env + kGEnvPhysicalWorld) : nullptr;
        if (!world || !readable(world, 4)) continue;
        void** vt = *(void***)world;
        if ((BYTE*)vt != g_physBase + kPhysWorldVtable || vt[kPhysTimeStepSlot] != g_physBase + kPhysTimeStep ||
            vt[kPhysTraceRaysSlot] != g_physBase + kPhysTraceRays || vt[kPhysUnmarkSlot] != g_physBase + kPhysUnmark) {
            set_world_state(REFUSED, "gEnv->pPhysicalWorld is not the r208022 CPhysicalWorld (vtable, TimeStep or TracePendingRays or unmark slot differs)");
            return 0;
        }
        size_t n = 0;
        while (n < 256 && (BYTE*)vt[n] >= g_physBase && (BYTE*)vt[n] < g_physBase + kModules[CRYPHYSICS].sizeOfImage) ++n;
        void** copy = n > kPhysTraceRaysSlot ? (void**)VirtualAlloc(NULL, (n + 1) * sizeof(void*), MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) : nullptr;
        if (!copy) { set_world_state(REFUSED, "vtable copy failed"); return 0; }
        copy[0] = vt[-1];
        memcpy(copy + 1, vt, n * sizeof(void*));
        g_stepOriginal = (StepFn)vt[kPhysTimeStepSlot];
        g_raysOriginal = (RaysFn)vt[kPhysTraceRaysSlot];
        copy[1 + kPhysTraceRaysSlot] = (void*)&hk_trace_rays;
        copy[1 + kPhysTimeStepSlot] = (void*)&hk_timestep;          // tracks the stepping thread (and traces)
        g_unmarkOriginal = (VoidFn)vt[kPhysUnmarkSlot];
        copy[1 + kPhysUnmarkSlot] = (void*)&hk_unmark;
        DWORD old;
        VirtualProtect(copy, (n + 1) * sizeof(void*), PAGE_READONLY, &old);
        g_world = world; g_worldVtable = vt; g_worldCopy = copy + 1;
        if (InterlockedCompareExchangePointer((void* volatile*)world, (void*)(copy + 1), (void*)vt) != (void*)vt) {
            set_world_state(REFUSED, "world vtable pointer changed during the swap");
            return 0;
        }
        char b[160];
        _snprintf_s(b, sizeof b, _TRUNCATE, "world vtable copy (%u slots, world %p): TimeStep, unmark and TracePendingRays wrapped", (unsigned)n, world);
        set_world_state(APPLIED, b);
        if (g_roleTrace) { HANDLE t = CreateThread(NULL, 0, role_sampler, NULL, 0, NULL); if (t) CloseHandle(t); }
        return 0;
    }
    set_world_state(REFUSED, "gEnv->pPhysicalWorld did not appear within 300 s");
    return 0;
}

// ---- diag-fulldump -------------------------------------------------------------------------------
// One MiniDumpWithFullMemory on the first access violation inside CryPhysics or Cry3DEngine (first
// chance, from a vectored handler; the exception then continues to the engine's own handler).
typedef BOOL (WINAPI* MiniDumpFn)(HANDLE, DWORD, HANDLE, int, void*, void*, void*);
static MiniDumpFn g_miniDump;
static volatile LONG g_dumped;
static wchar_t g_dumpDir[MAX_PATH];
static uintptr_t g_dumpRanges[4];

static LONG CALLBACK fulldump_handler(EXCEPTION_POINTERS* ep) {
    if (!ep || ep->ExceptionRecord->ExceptionCode != EXCEPTION_ACCESS_VIOLATION || !g_miniDump) return EXCEPTION_CONTINUE_SEARCH;
    uintptr_t at = (uintptr_t)ep->ExceptionRecord->ExceptionAddress;
    bool inside = (at >= g_dumpRanges[0] && at < g_dumpRanges[1]) || (at >= g_dumpRanges[2] && at < g_dumpRanges[3]);
    if (!inside || InterlockedCompareExchange(&g_dumped, 1, 0) != 0) return EXCEPTION_CONTINUE_SEARCH;
    wchar_t path[MAX_PATH];
    _snwprintf_s(path, MAX_PATH, _TRUNCATE, L"%sfulldump-%lu-%llu.dmp", g_dumpDir, GetCurrentProcessId(), GetTickCount64());
    HANDLE f = CreateFileW(path, GENERIC_WRITE, 0, NULL, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, NULL);
    if (f == INVALID_HANDLE_VALUE) return EXCEPTION_CONTINUE_SEARCH;
    struct { DWORD tid; EXCEPTION_POINTERS* ep; BOOL client; } mei = { GetCurrentThreadId(), ep, FALSE };
    // MiniDumpWithFullMemory 0x2 | HandleData 0x4 | UnloadedModules 0x20 | FullMemoryInfo 0x800 | ThreadInfo 0x1000
    BOOL ok = g_miniDump(GetCurrentProcess(), GetCurrentProcessId(), f, 0x2 | 0x4 | 0x20 | 0x800 | 0x1000, &mei, NULL, NULL);
    CloseHandle(f);
    plog("diag-fulldump: access violation at %p (thread %lu%s): %s", (void*)at, GetCurrentThreadId(),
         GetCurrentThreadId() == g_mainTid ? ", main" : "", ok ? "full dump written" : "dump failed");
    return EXCEPTION_CONTINUE_SEARCH;
}
static void start_fulldump(HMODULE self) {
    DWORD n = GetModuleFileNameW(self, g_dumpDir, MAX_PATH);
    wchar_t* slash = n && n < MAX_PATH ? wcsrchr(g_dumpDir, L'\\') : nullptr;
    if (!slash) { plog("diag-fulldump: no driver directory"); return; }
    slash[1] = 0;
    HMODULE dbghelp = LoadLibraryW(L"dbghelp.dll");
    g_miniDump = dbghelp ? (MiniDumpFn)GetProcAddress(dbghelp, "MiniDumpWriteDump") : nullptr;
    // engine images: CryPhysics 0x34FF0000 + 0x75B400, Cry3DEngine 0x30000000 + 0x38A000 (fixed bases)
    g_dumpRanges[0] = 0x34FF0000; g_dumpRanges[1] = 0x34FF0000 + 0x75B400;
    g_dumpRanges[2] = 0x30000000; g_dumpRanges[3] = 0x30000000 + 0x38A000;
    if (!g_miniDump || !AddVectoredExceptionHandler(1, fulldump_handler)) { plog("diag-fulldump: not available"); return; }
    plog("diag-fulldump: armed (CryPhysics, Cry3DEngine)");
}

// ---- per-module application ----------------------------------------------------------------------
static void apply_hook(int i, BYTE* base, bool late) {
    const char* id = kPatches[i].id;
    std::string why;
    int r = -1;
    if (late && kPatches[i].earlyOnly) { set_state(i, REFUSED, "module was running before the driver attached (code hook)"); return; }
    if (!strcmp(id, "dispmode-cache")) {
        if (!site_ok(base, id, 0x61F0B, why)) r = -1;
        else { if (!g_modeCache) g_modeCache = new std::map<uint64_t, LONG>(); r = hook_import(base, CRYSYSTEM, id, (void*)&hk_change_display, (void**)&g_cdsOriginal, why); }
    } else if (!strcmp(id, "fast-splitpath")) {
        if (!site_ok(base, id, 0x8C00, why) || !site_ok(base, id, 0x8EF0, why)) r = -1;
        else r = hook_import(base, CRYSYSTEM, id, (void*)&hk_splitpath, (void**)&g_splitOriginal, why);
    } else if (!strcmp(id, "frame-latency")) {
        g_frameLatency = (UINT)perfcore::option_int(g_switches, "frame_latency", 1, 1, 16);
        if (!site_ok(base, id, 0x145C83, why)) r = -1;
        else {
            void** slot = import_slot(base, id);
            if (slot && bound(base, CRYRENDER, *slot) && *slot != (void*)g_getProcAddress[0] && *slot != (void*)g_getProcAddress[1] && *slot != (void*)&hk_get_proc_address) {
                why = "GetProcAddress import is hooked by something else"; r = -1;
            } else r = hook_import(base, CRYRENDER, id, (void*)&hk_get_proc_address, (void**)&g_gpaOriginal, why);
            if (r > 0 && late) why = "late attach: only applies if the renderer creates its device afterwards";
        }
    } else if (!strcmp(id, "render-thread-event-sync")) {
        g_spins = perfcore::option_int(g_switches, "rt_spins", 256, 0, 100000);
        r = install_render_sync(base, why);
    } else if (!strcmp(id, "diag-frame-trace")) {
        r = install_frame_trace(base, why);
    } else if (!strcmp(id, "pak-catalog-scope") || !strcmp(id, "pak-findchild-index")) {
        bool wantIndex = is_on("pak-findchild-index");
        int scope = index_of("pak-catalog-scope"), findc = index_of("pak-findchild-index");
        if (g_status[scope].state == APPLIED && i == findc) {
            // scope already in; the index needs its own hook
        } else if (g_status[scope].state != APPLIED) {
            if (g_tls == TLS_OUT_OF_INDEXES) { why = "no TLS index"; set_state(i, REFUSED, why.c_str()); return; }
            if (!site_ok(base, "pak-catalog-scope", 0x13C60, why)) { set_state(scope, REFUSED, why.c_str()); if (findc >= 0 && g_status[findc].on) set_state(findc, REFUSED, "needs pak-catalog-scope"); return; }
            g_catalogTrampoline = (CatalogFn)jump_hook(base + 0x13C60, 5, (void*)&hk_catalog);
            if (!g_catalogTrampoline) { set_state(scope, REFUSED, "hook failed"); if (g_status[findc].on) set_state(findc, REFUSED, "needs pak-catalog-scope"); return; }
            set_state(scope, APPLIED, g_status[scope].on ? "+0x13C60 entry hook" : "+0x13C60 entry hook (enabled by pak-findchild-index)");
        }
        if (wantIndex && g_status[findc].state == PENDING) {
            if (!site_ok(base, "pak-findchild-index", 0x17720, why) || !site_ok(base, "pak-findchild-index", 0x177D0, why)) { set_state(findc, REFUSED, why.c_str()); return; }
            g_findChildTrampoline = jump_hook(base + 0x17720, 5, (void*)&find_child_thunk);
            if (!g_findChildTrampoline) { set_state(findc, REFUSED, "hook failed"); return; }
            g_indexOn = true;
            set_state(findc, APPLIED, "+0x17720 entry hook, active inside the catalogue load only");
        }
        return;
    }
    if (r > 0) set_state(i, APPLIED, why.empty() ? "IAT slot swapped" : why.c_str());
    else if (r < 0) set_state(i, REFUSED, why.c_str());
}

// CryPhysics loaded: check the rows the wrapper relies on (reads only), then wait for the world on
// world_thread. Caller holds g_lock.
static bool g_worldThread;
static void start_world(int i, BYTE* base) {
    for (const Site& x : kSites) {
        if (strcmp(x.id, kPatches[i].id) != 0) continue;
        std::string why;
        if (!site_ok(base, x.id, x.rva, why)) { set_state(i, REFUSED, why.c_str()); return; }
    }
    g_physBase = base;
    if (!strcmp(kPatches[i].id, "physics-role-guard")) g_roleGuard = true;
    if (!strcmp(kPatches[i].id, "diag-physics-role")) g_roleTrace = true;
    if (!g_worldThread) {
        HANDLE t = CreateThread(NULL, 0, world_thread, NULL, 0, NULL);
        if (!t) { set_state(i, REFUSED, "cannot start the world thread"); return; }
        CloseHandle(t);
        g_worldThread = true;
    }
}

// Caller holds g_lock and must not call the loader.
static void handle_module(int m, BYTE* base, const char* how) {
    ModuleState& ms = g_mods[m];
    if (ms.ready) return;
    ms.ready = true; ms.base = base;
    std::string why;
    bool known = module_is_known(m, base, why);
    plog("%s at %p (%s%s)%s%s", kModules[m].name, base, how, ms.late ? ", loaded before attach" : "", known ? "" : ": ", known ? "" : why.c_str());
    for (int i = 0; i < kPatchCount; ++i) {
        if (kPatches[i].module != m || g_status[i].state != PENDING) continue;
        if (!g_status[i].on && !(i == index_of("pak-catalog-scope") && is_on("pak-findchild-index"))) continue;
        if (!known) { set_state(i, REFUSED, why.c_str()); continue; }
        if (kPatches[i].kind == BYTES) apply_bytes(i, base, ms.late);
        else if (kPatches[i].kind == HOOK) apply_hook(i, base, ms.late);
        else if (kPatches[i].kind == WORLD) start_world(i, base);
        else if (kPatches[i].kind == EXTERNAL) {
            if (startup::install_at(base)) set_state(i, APPLIED, "+0x23776/+0x2378A priorities");
            else set_state(i, REFUSED, "calibration code differs from r208022");
        }
    }
}

// Deferred hooks (IAT not bound at entry time) retry from the worker.
static bool retry_pending() {
    bool left = false;
    for (int i = 0; i < kPatchCount; ++i) {
        if (!g_status[i].on || g_status[i].state != PENDING || kPatches[i].module < 0) continue;
        ModuleState& ms = g_mods[kPatches[i].module];
        if (!ms.ready) { left = true; continue; }
        if (kPatches[i].kind == WORLD) { if (g_mods[kPatches[i].module].ready) continue; left = true; continue; }
        if (kPatches[i].kind == HOOK && kPatches[i].earlyOnly) set_state(i, REFUSED, "code hook not installed at module start (not retried on running code)");
        else if (kPatches[i].kind == HOOK) apply_hook(i, ms.base, ms.late);
        if (g_status[i].state == PENDING) left = true;
    }
    return left;
}

template <int M> static BOOL WINAPI entry_wrapper(HINSTANCE h, DWORD reason, LPVOID p) {
    BOOL ok = g_mods[M].entry(h, reason, p);
    if (reason == DLL_PROCESS_ATTACH && ok) {
        EnterCriticalSection(&g_lock);
        handle_module(M, (BYTE*)h, "after its DllMain");
        LeaveCriticalSection(&g_lock);
    }
    return ok;
}

// x86 LDR_DATA_TABLE_ENTRY: InLoadOrderLinks +0, DllBase +0x18, EntryPoint +0x1C (as archenova.dll).
struct LoaderEntry { LIST_ENTRY a, b, c; void* base; void* entry; };
static LoaderEntry* loader_entry(void* base) {
    BYTE* ldr = *(BYTE**)((BYTE*)__readfsdword(0x30) + 0x0C);
    LIST_ENTRY* head = (LIST_ENTRY*)(ldr + 0x0C);
    for (LIST_ENTRY* l = head->Flink; l != head; l = l->Flink) if (((LoaderEntry*)l)->base == base) return (LoaderEntry*)l;
    return nullptr;
}

struct LdrString { USHORT Length; USHORT MaximumLength; PWSTR Buffer; };
struct LoadedData { ULONG flags; const LdrString* fullName; const LdrString* baseName; void* base; ULONG size; };
static void* g_cookie;

static VOID CALLBACK on_dll_loaded(ULONG reason, const LoadedData* d, void*) {
    if (reason != 1 || !d || !d->baseName || !d->baseName->Buffer) return;
    for (int m = 0; m < MODULE_COUNT; ++m) {
        size_t len = wcslen(kModules[m].wname);
        if (d->baseName->Length != len * sizeof(wchar_t) || _wcsnicmp(d->baseName->Buffer, kModules[m].wname, len) != 0) continue;
        EnterCriticalSection(&g_lock);
        ModuleState& ms = g_mods[m];
        if (!ms.ready && !ms.wrapped) {
            ms.base = (BYTE*)d->base; ms.seenTick = GetTickCount64();
            LoaderEntry* e = loader_entry(d->base);
            if (e && e->entry) {
                ms.entry = (BOOL (WINAPI*)(HINSTANCE, DWORD, LPVOID))e->entry;
                e->entry = m == CRYSYSTEM ? (void*)&entry_wrapper<CRYSYSTEM> : m == CRYRENDER ? (void*)&entry_wrapper<CRYRENDER> : (void*)&entry_wrapper<CRYPHYSICS>;
                ms.wrapped = true;
            } else {
                plog("%s: loader entry not found; the worker applies the late-safe patches", kModules[m].name);
                ms.late = true;
            }
        }
        LeaveCriticalSection(&g_lock);
    }
}

static DWORD WINAPI worker(LPVOID) {
    if (is_on("pak-prefetch")) { HANDLE t = CreateThread(NULL, 0, prefetch_thread, NULL, 0, NULL); if (t) CloseHandle(t); }
    if (is_on("diag-resources")) { HANDLE t = CreateThread(NULL, 0, diag_thread, NULL, 0, NULL); if (t) CloseHandle(t); }
    if (is_on("diag-fulldump")) start_fulldump(g_self);
    EnterCriticalSection(&g_lock);
    for (int i = 0; i < kPatchCount; ++i)
        if (kPatches[i].kind == THREAD && g_status[i].on && g_status[i].state == PENDING)
            set_state(i, !strcmp(kPatches[i].id, "diag-fulldump") && !g_miniDump ? REFUSED : APPLIED,
                      !strcmp(kPatches[i].id, "diag-fulldump") && !g_miniDump ? "MiniDumpWriteDump not available" : "started");
    LeaveCriticalSection(&g_lock);
    for (int round = 0; round < 12000; ++round) {      // 120 s
        BYTE* found[MODULE_COUNT] = {};
        for (int m = 0; m < MODULE_COUNT; ++m) {
            if (g_mods[m].ready) continue;
            bool lateCandidate = g_mods[m].late || (g_mods[m].wrapped && GetTickCount64() - g_mods[m].seenTick > 30000);
            if (lateCandidate) found[m] = (BYTE*)GetModuleHandleW(kModules[m].wname);   // outside g_lock
        }
        EnterCriticalSection(&g_lock);
        for (int m = 0; m < MODULE_COUNT; ++m) if (found[m] && !g_mods[m].ready) { g_mods[m].late = true; handle_module(m, found[m], "worker"); }
        bool left = retry_pending();
        bool waiting = false;
        for (int m = 0; m < MODULE_COUNT; ++m) if (!g_mods[m].ready) waiting = true;
        LeaveCriticalSection(&g_lock);
        if (!left && !waiting) break;
        Sleep(10);
    }
    EnterCriticalSection(&g_lock);
    for (int i = 0; i < kPatchCount; ++i) if (g_status[i].on && g_status[i].state == PENDING && kPatches[i].kind != WORLD) set_state(i, REFUSED, "module never loaded or never became ready");
    LeaveCriticalSection(&g_lock);
    return 0;
}

static std::string ini_value(HMODULE self, const wchar_t* key) {
    wchar_t path[MAX_PATH];
    DWORD n = GetModuleFileNameW(self, path, MAX_PATH);
    wchar_t* slash = n && n < MAX_PATH ? wcsrchr(path, L'\\') : nullptr;
    if (!slash || wcscpy_s(slash + 1, MAX_PATH - (slash + 1 - path), L"aadriver.ini")) return "";
    wchar_t buf[2048] = L"";
    GetPrivateProfileStringW(L"perf", key, L"", buf, 2048, path);
    std::string out;
    for (wchar_t* p = buf; *p; ++p) out += (char)(*p < 128 ? *p : '?');
    return out;
}
static std::string env_value(const wchar_t* name) {
    wchar_t buf[2048];
    DWORD n = GetEnvironmentVariableW(name, buf, 2048);
    std::string out;
    if (n && n < 2048) for (DWORD i = 0; i < n; ++i) out += (char)(buf[i] < 128 ? buf[i] : '?');
    return out;
}

static bool cvar_token_ok(const std::string& s, bool value) {
    if (s.empty() || s.size() > 64) return false;
    for (char c : s) {
        bool ok = value ? (isdigit((unsigned char)c) || c == '.' || c == '-' || c == '+' || c == 'e' || c == 'E')
                        : (isalnum((unsigned char)c) || c == '_');
        if (!ok) return false;
    }
    return true;
}
static void add_cvars(const std::string& text) {
    std::string tok;
    auto flush = [&]() {
        size_t c = tok.find(':'); if (c == std::string::npos) c = tok.find('=');
        if (c != std::string::npos) {
            std::string n = tok.substr(0, c), v = tok.substr(c + 1);
            if (cvar_token_ok(n, false) && cvar_token_ok(v, true)) {
                bool replaced = false;
                for (auto& kv : g_cvars) if (_stricmp(kv.first.c_str(), n.c_str()) == 0) { kv.second = v; replaced = true; }
                if (!replaced) g_cvars.push_back({ n, v });
            } else plog("cvar '%s' ignored (name [A-Za-z0-9_], numeric value)", tok.c_str());
        }
        tok.clear();
    };
    for (char ch : text) { if (ch == ';' || ch == ',' || ch == ' ') flush(); else tok += ch; }
    flush();
}

void attach(HMODULE self, Logger logger) {
    if (g_attached) return;
    g_attached = true;
    g_self = self;
    g_log = logger;
    InitializeCriticalSection(&g_lock);
    InitializeCriticalSection(&g_parkLock);
    const wchar_t* cmd = GetCommandLineW();

    // switches
    std::string fromIni = ini_value(self, L"patches"), fromEnv = env_value(L"AADRIVER_PATCHES");
    std::string fromCmd = perfcore::command_line_value(cmd, L"-aaemu-patches=");
    perfcore::apply_tokens(g_switches, fromIni);
    perfcore::apply_tokens(g_switches, fromEnv);
    perfcore::apply_tokens(g_switches, fromCmd);
    // Launcher profile flags predate the table: --no-fast-pak omits -aaemu-fast-pak, --smooth-start adds
    // -aaemu-smooth-start. Without explicit switches for those ids, follow them.
    bool launched = wcsstr(cmd, L"-aaemu-") != NULL;
    for (int i = 0; i < kPatchCount; ++i) {
        bool def = kPatches[i].defaultOn;
        if (!strcmp(kPatches[i].id, "pak-readahead-64k") && launched) def = wcsstr(cmd, L"-aaemu-fast-pak") != NULL;
        if (!strcmp(kPatches[i].id, "cpu-calibration-normal-priority")) def = wcsstr(cmd, L"-aaemu-smooth-start") != NULL;
        g_status[i].on = perfcore::enabled(g_switches, kPatches[i].id, def);
        g_status[i].state = g_status[i].on ? PENDING : OFF;
    }
    for (auto& kv : g_switches.explicitly) if (index_of(kv.first.c_str()) < 0) plog("unknown patch id '%s' ignored", kv.first.c_str());
    add_cvars(ini_value(self, L"cvars"));
    add_cvars(env_value(L"AADRIVER_CVARS"));
    add_cvars(perfcore::command_line_value(cmd, L"-aaemu-cvars="));

    std::string on, off;
    for (int i = 0; i < kPatchCount; ++i) { std::string& s = g_status[i].on ? on : off; if (!s.empty()) s += ","; s += kPatches[i].id; }
    plog("switches: ini='%s' env='%s' cmd='%s'", fromIni.c_str(), fromEnv.c_str(), fromCmd.c_str());
    plog("on: %s", on.empty() ? "-" : on.c_str());
    plog("off: %s", off.empty() ? "-" : off.c_str());

    // things the hooks need, resolved now (DllMain may use the loader)
    HMODULE kb = GetModuleHandleW(L"kernelbase.dll"), k32 = GetModuleHandleW(L"kernel32.dll");
    g_waitOnAddress = kb ? (perfcore::WaitOnAddressFn)GetProcAddress(kb, "WaitOnAddress") : nullptr;
    g_wakeAll = kb ? (VOID (WINAPI*)(PVOID))GetProcAddress(kb, "WakeByAddressAll") : nullptr;
    g_switchToThread[0] = k32 ? (void*)GetProcAddress(k32, "SwitchToThread") : nullptr;
    g_switchToThread[1] = kb ? (void*)GetProcAddress(kb, "SwitchToThread") : nullptr;
    g_getProcAddress[0] = k32 ? GetProcAddress(k32, "GetProcAddress") : nullptr;
    g_getProcAddress[1] = kb ? GetProcAddress(kb, "GetProcAddress") : nullptr;
    g_tls = TlsAlloc();
    HMODULE nt = GetModuleHandleW(L"ntdll.dll");
    g_queryThread = nt ? (QueryThreadFn)GetProcAddress(nt, "NtQueryInformationThread") : nullptr;
    g_thunks = (BYTE*)VirtualAlloc(NULL, 4096, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READ);

    // modules already in memory (late attach through `aaemu-clientdriver inject`)
    for (int m = 0; m < MODULE_COUNT; ++m) if (GetModuleHandleW(kModules[m].wname)) g_mods[m].late = true;

    HMODULE ntdll = GetModuleHandleW(L"ntdll.dll");
    typedef LONG (NTAPI* RegisterFn)(ULONG, PVOID, PVOID, PVOID*);
    RegisterFn reg = ntdll ? (RegisterFn)GetProcAddress(ntdll, "LdrRegisterDllNotification") : nullptr;
    if (!reg || reg(0, (PVOID)&on_dll_loaded, NULL, &g_cookie) < 0) {
        plog("LdrRegisterDllNotification failed: modules loaded later get late-safe patches only");
        for (int m = 0; m < MODULE_COUNT; ++m) g_mods[m].late = true;
    }
    HANDLE t = CreateThread(NULL, 0, worker, NULL, 0, NULL);
    if (t) CloseHandle(t);
}

void remove() {
    if (!g_attached) return;
    EnterCriticalSection(&g_lock);
    for (auto& s : g_slots) if (swap_pointer(s.slot, s.hook, s.original)) s.hook = s.original;
    for (int i = 0; i < kPatchCount; ++i) {
        Status& st = g_status[i];
        if (st.state == APPLIED && kPatches[i].kind == BYTES && st.at) {
            perfcore::Pattern orig; orig.v.assign(st.original, st.original + st.len);
            if (perfcore::changed_bytes(st.at, orig) <= 1 && write_code(st.at, st.original, st.len)) set_state(i, OFF, "restored");
        }
    }
    // the TimeStep wrapper stays (a step may be running in it); the vtable copy is never freed
    plog("detach: IAT hooks and one-byte patches restored; code hooks and the physics world wrapper stay until exit");
    LeaveCriticalSection(&g_lock);
}

std::string info() {
    int applied = 0, on = 0;
    for (int i = 0; i < kPatchCount; ++i) { if (g_status[i].on) ++on; if (g_status[i].state == APPLIED || g_status[i].state == PRESENT) ++applied; }
    char b[64]; _snprintf_s(b, sizeof b, _TRUNCATE, " perf=%d/%d", applied, on);
    return b;
}

std::string control(const std::string& cmd) {
    if (cmd != "status" && !cmd.empty()) return "2perf expects status";
    std::string out = "0";
    EnterCriticalSection(&g_lock);
    for (int i = 0; i < kPatchCount; ++i) {
        out += kPatches[i].id; out += ' '; out += kStateNames[g_status[i].state];
        if (!g_status[i].why.empty()) { out += " ("; out += g_status[i].why; out += ')'; }
        out += '\n';
    }
    char b[160];
    _snprintf_s(b, sizeof b, _TRUNCATE, "counters: splitpath_calls=%ld mode_hits=%ld mode_misses=%ld latency_set=%ld index_fallbacks=%ld\n",
                g_splitCalls, g_modeHits, g_modeMisses, g_latencySet, g_indexFallbacks);
    out += b;
    if (g_raysOriginal && g_roleTrace) out += physics_role_summary();
    else if (g_raysOriginal) {
        _snprintf_s(b, sizeof b, _TRUNCATE, "physics role guard: guarded=%ld guard_moved=%ld guard_skipped=%ld unmarks_parked=%ld stepper=%lu\n",
                    g_guarded, g_guardMoved, g_guardSkipped, g_unmarksParked, g_stepper);
        out += b;
    }
    LeaveCriticalSection(&g_lock);
    return out;
}

std::string first_tick_lua(const std::string& result) {
    if (g_tickStage == 1) {
        g_tickStage = 2;
        if (!g_cvars.empty()) plog("cvars: %s", result.empty() ? "-" : result.c_str() + (result[0] == '0' || result[0] == '1' ? 1 : 0));
        return "";
    }
    if (g_tickStage != 0) return "";
    g_tickStage = 1;
    g_mainTid = GetCurrentThreadId();
    std::string summary;
    EnterCriticalSection(&g_lock);
    for (int i = 0; i < kPatchCount; ++i) {
        if (!g_status[i].on) continue;
        if (!summary.empty()) summary += ", ";
        summary += kPatches[i].id; summary += '='; summary += kStateNames[g_status[i].state];
    }
    LeaveCriticalSection(&g_lock);
    // ids and states are [a-z0-9-=, ]: safe inside a Lua string literal
    std::string lua = "local out = {}\n"
        "if System and System.Log then System.Log('[aadriver] perf patches: " + (summary.empty() ? std::string("none") : summary) + "') end\n";
    for (auto& kv : g_cvars) {
        lua += "do local n, v = '" + kv.first + "', " + kv.second + "\n"
               "  if System and System.GetCVar and (System.GetCVar(n)) ~= nil then local o = System.GetCVar(n); System.SetCVar(n, v)\n"
               "    out[#out + 1] = n .. '=' .. tostring(o) .. '->' .. tostring(System.GetCVar(n))\n"
               "  else out[#out + 1] = n .. '=missing' end end\n";
    }
    lua += "return table.concat(out, ' ')";
    return lua;
}

} // namespace perfpatch

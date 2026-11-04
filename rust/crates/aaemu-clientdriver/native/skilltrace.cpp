// ============================================================================
//  skilltrace.cpp - observe the original 1.2 client's animation, particle and sound calls.
//
//  Six virtual functions of the unpacked engine DLLs are observed by replacing their vtable
//  slot in memory (the table below; addresses are RVAs from each module's runtime base).
//  Before a slot is touched the build is verified: the slot must currently hold exactly
//  module_base + the known function RVA (and, in the game, the module must have the known PE
//  timestamp and image size). Anything else logs why and leaves that hook inert.
//
//  Every hook only observes: it calls the original with the very same arguments (raw 32-bit
//  stack words, so even a bool's upper bytes pass through), returns the original's result and
//  reads game memory under SEH, so a bad pointer costs a row, never the process.
// ============================================================================
#define WIN32_LEAN_AND_MEAN
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <windows.h>
#include <stdio.h>
#include <string.h>
#include <atomic>
#include <string>
#include "skilltrace.h"
#include "tracecore.h"
#include "visuals.h"

namespace skilltrace {
using tracecore::Row;

// ---- verified build ----------------------------------------------------------------------
enum Module { M_ANIM, M_3D, M_SOUND, M_COUNT };
struct ModuleSpec { const char* file; DWORD stamp; DWORD image_size; };
static const ModuleSpec kModules[M_COUNT] = {
    {"cryanimation.dll",  0x543CB82F, 0x001E8000},   // preferred base 0x31500000
    {"cry3dengine.dll",   0x543CB85C, 0x0038A000},   // preferred base 0x30000000
    {"crysoundsystem.dll", 0x543CB770, 0x000F6000},  // preferred base 0x36000000
};

enum Hook { H_ANIM_NAME, H_ANIM_ID, H_FX_SPAWN, H_FX_FIND, H_SOUND_CREATE, H_SOUND_PLAY, H_COUNT };
struct HookSpec {
    const char* id;        // key in the status JSON
    const char* function;
    int module;
    DWORD vtable_rva;
    int slot;
    DWORD function_rva;
    int stack_bytes;       // the original's `ret N`
};
static const HookSpec kHooks[H_COUNT] = {
    {"anim_start",       "CSkeletonAnim::StartAnimation(name, params)",     M_ANIM,  0x1779DC, 12, 0x59FD0,  8},
    {"anim_start_by_id", "CSkeletonAnim::StartAnimationById(id, params)",   M_ANIM,  0x1779DC, 14, 0x5B350,  8},
    {"fx_spawn",         "CParticleEffect::Spawn(loc, flags)",              M_3D,    0x259350,  3, 0x6EED0,  8},
    {"fx_find",          "C3DEngine::FindParticleEffect(name, src, src2, load)", M_3D, 0x263718, 82, 0x1224E0, 16},
    {"sound_create",     "CSoundSystem::CreateSound(name, flags)",          M_SOUND, 0xA3A48,  19, 0x4FE40,  8},
    {"sound_play",       "CSound::Play(volume, force, ratio, package)",     M_SOUND, 0xA1638,   8, 0x37AC0, 16},
};
static const DWORD kSoundGetNameRva = 0x35220;    // CSound vtable slot 29: const char* GetName()
static const int kSoundGetNameSlot = 29;

// ---- state -------------------------------------------------------------------------------
static const size_t kRingBytes = 4u << 20;
static tracecore::Ring* g_ring;                      // created by the first start, never freed
static std::atomic<unsigned> g_kinds{0};             // 0 = not recording
static bool g_running;
static SRWLOCK g_control = SRWLOCK_INIT;
static tracecore::SlotPatch g_patch[H_COUNT];
static void* g_orig[H_COUNT];
static std::string g_reason[H_COUNT];                // why a hook is inert ("" = installed)
static BYTE* g_sound_base;
static DWORD g_tls = TLS_OUT_OF_INDEXES;             // per-thread "inside StartAnimation" flag
static LONGLONG g_last_present;                      // QPC of the previous Present

struct Bases { BYTE* module[M_COUNT]; bool check_pe; };

// ---- guarded reads (plain C so SEH is allowed) --------------------------------------------
static bool read_mem(const void* p, void* out, size_t n) {
    __try { memcpy(out, p, n); return true; } __except (EXCEPTION_EXECUTE_HANDLER) { return false; }
}
static int read_cstr(const char* p, char* out, int cap) {
    out[0] = 0;
    if (!p || cap < 2) return -1;
    __try {
        int n = 0;
        while (n < cap - 1 && p[n]) { out[n] = p[n]; ++n; }
        out[n] = 0;
        return n;
    } __except (EXCEPTION_EXECUTE_HANDLER) { out[0] = 0; return -1; }
}
static bool pe_identity(const BYTE* b, DWORD* stamp, DWORD* size) {
    __try {
        const IMAGE_DOS_HEADER* dos = (const IMAGE_DOS_HEADER*)b;
        if (dos->e_magic != IMAGE_DOS_SIGNATURE) return false;
        const IMAGE_NT_HEADERS* nt = (const IMAGE_NT_HEADERS*)(b + dos->e_lfanew);
        if (nt->Signature != IMAGE_NT_SIGNATURE) return false;
        *stamp = nt->FileHeader.TimeDateStamp;
        *size = nt->OptionalHeader.SizeOfImage;
        return true;
    } __except (EXCEPTION_EXECUTE_HANDLER) { return false; }
}

// CryCharAnimationParams as the r208022 StartAnimationById reads it: +16 and +24 are read
// by the function itself (layer < 16 check, flags & 0x800), +0 is stored as the layer's
// transition time and +20 as its weight, +4 feeds the start key time.
struct AnimParams { float blend, key_time, speed, extra; int layer; float weight; unsigned flags; };
static_assert(sizeof(AnimParams) == 28, "CryCharAnimationParams head");

static bool read_anim_params(const void* p, AnimParams* out) { return p && read_mem(p, out, sizeof(*out)); }

// Skeleton -> CCharInstance (+1176) -> model (+384) -> animation set (model + 1492) whose
// header table (+36 begin, +40 end, 16 bytes per entry, name pointer first) lists the clips.
static const int kSkelInstance = 1176, kInstModel = 384, kModelAnimSet = 1492;
static const int kSetBegin = 36, kSetEnd = 40, kSetEntry = 16;

static int anim_clip_by_id(const BYTE* skel, int id, char* out, int cap) {
    out[0] = 0;
    if (id < 0) return -1;
    __try {
        const BYTE* inst = *(const BYTE* const*)(skel + kSkelInstance);
        if (!inst) return -1;
        const BYTE* model = *(const BYTE* const*)(inst + kInstModel);
        if (!model) return -1;
        const BYTE* set = model + kModelAnimSet;
        const BYTE* begin = *(const BYTE* const*)(set + kSetBegin);
        const BYTE* end = *(const BYTE* const*)(set + kSetEnd);
        if (!begin || end < begin || (size_t)id >= (size_t)(end - begin) / kSetEntry) return -1;
        const char* name = *(const char* const*)(begin + (size_t)id * kSetEntry);
        return read_cstr(name, out, cap);
    } __except (EXCEPTION_EXECUTE_HANDLER) { out[0] = 0; return -1; }
}

// CParticleEffect + 232: the CryString holding the full `library.group.effect` name.
static int effect_name(const BYTE* effect, char* out, int cap) {
    out[0] = 0;
    if (!effect) return -1;
    __try { return read_cstr(*(const char* const*)(effect + 232), out, cap); }
    __except (EXCEPTION_EXECUTE_HANDLER) { out[0] = 0; return -1; }
}

// ISound::GetName (vtable slot 29) is what CSound::Play's own error message prints. Only
// called when the slot is the verified function.
static int sound_name(void* sound, char* out, int cap) {
    out[0] = 0;
    if (!sound || !g_sound_base) return -1;
    __try {
        void** vt = *(void***)sound;
        if (vt[kSoundGetNameSlot] != (void*)(g_sound_base + kSoundGetNameRva)) return -1;
        typedef const char* (__thiscall *GetName)(void*);
        return read_cstr(((GetName)vt[kSoundGetNameSlot])(sound), out, cap);
    } __except (EXCEPTION_EXECUTE_HANDLER) { out[0] = 0; return -1; }
}

// ---- rows --------------------------------------------------------------------------------
static void emit(Row& row) {
    const std::string& line = row.finish();
    if (g_ring) g_ring->push(line.data(), line.size());
}
static inline bool want(unsigned kind) { return (g_kinds.load(std::memory_order_acquire) & kind) != 0; }

static void log_anim(int64_t wall, unsigned frame, const void* skel, const char* name, int id, bool by_name,
                     const void* params, bool ok) {
    Row row("anim", wall, frame);
    char clip[256];
    int len = by_name ? read_cstr(name, clip, sizeof(clip)) : anim_clip_by_id((const BYTE*)skel, id, clip, sizeof(clip));
    void* inst = nullptr;
    if (skel && read_mem((const BYTE*)skel + kSkelInstance, &inst, sizeof(inst))) row.hex("inst", (uintptr_t)inst);
    row.hex("skel", (uintptr_t)skel);
    if (len > 0) row.str("clip", clip, true);
    AnimParams p;
    if (read_anim_params(params, &p)) {
        row.i("layer", p.layer).fl("speed", p.speed).fl("blend", p.blend)
           .b("loop", (p.flags & 0x2) != 0)               // derived: CA_LOOP_ANIMATION
           .u("flags", p.flags).fl("key_time", p.key_time).fl("weight", p.weight);
    }
    row.str("by", by_name ? "name" : "id");
    if (!by_name) row.i("id", id);
    row.b("ok", ok);
    emit(row);
}

static void log_fx_spawn(int64_t wall, unsigned frame, const void* effect, const void* loc, DWORD flags, void* emitter) {
    Row row("fx", wall, frame);
    row.str("phase", "spawn");
    char name[256];
    if (effect_name((const BYTE*)effect, name, sizeof(name)) > 0) row.str("name", name, true);
    float q[8];
    if (loc && read_mem(loc, q, sizeof(q))) {
        row.vec3("pos", q[4], q[5], q[6]).fl("scale", q[7]).vec4("rot", q[0], q[1], q[2], q[3]);
    }
    row.u("flags", flags).hex("emitter", (uintptr_t)emitter).hex("effect", (uintptr_t)effect);
    emit(row);
}

static void log_fx_find(int64_t wall, unsigned frame, const char* requested, void* effect) {
    Row row("fx", wall, frame);
    row.str("phase", "find");
    char name[256];
    if (read_cstr(requested, name, sizeof(name)) > 0) row.str("name", name, true);
    row.b("found", effect != nullptr).hex("effect", (uintptr_t)effect);
    char resolved[256];
    if (effect && effect_name((const BYTE*)effect, resolved, sizeof(resolved)) > 0) row.str("resolved", resolved, true);
    emit(row);
}

static void log_sound_create(int64_t wall, unsigned frame, const char* requested, DWORD flags, void* sound) {
    Row row("sound", wall, frame);
    row.str("phase", "create");
    char name[256];
    if (read_cstr(requested, name, sizeof(name)) > 0) row.str("name", name, true);
    row.u("flags", flags).hex("sound", (uintptr_t)sound);
    emit(row);
}

static void log_sound_play(int64_t wall, unsigned frame, void* sound, DWORD volume_bits, DWORD force, DWORD ratio) {
    Row row("sound", wall, frame);
    row.str("phase", "play").hex("sound", (uintptr_t)sound);
    char name[256];
    if (sound_name(sound, name, sizeof(name)) > 0) row.str("name", name, true);
    float volume;
    memcpy(&volume, &volume_bits, sizeof(volume));
    row.fl("volume", volume).b("force_active", (force & 0xFF) != 0).b("set_ratio", (ratio & 0xFF) != 0);
    emit(row);
}

// ---- hooks -------------------------------------------------------------------------------
typedef bool  (__thiscall *StartAnimFn)(void* self, const char* name, const void* params);
typedef bool  (__thiscall *StartAnimIdFn)(void* self, int id, const void* params);
typedef void* (__thiscall *SpawnFn)(void* self, const void* loc, DWORD flags);
typedef void* (__thiscall *FindFxFn)(void* self, const char* name, const char* source, const char* source2, DWORD load);
typedef void* (__thiscall *CreateSoundFn)(void* self, const char* name, DWORD flags);
typedef void  (__thiscall *PlayFn)(void* self, DWORD volume, DWORD force, DWORD ratio, void* package);

static bool nested_in_start() { return g_tls != TLS_OUT_OF_INDEXES && TlsGetValue(g_tls) != NULL; }

// StartAnimation resolves the name and calls StartAnimationById through the vtable; the flag
// keeps that inner call from producing a second row on this thread.
static bool call_start_nested(StartAnimFn f, void* self, const char* name, const void* params) {
    if (g_tls == TLS_OUT_OF_INDEXES) return f(self, name, params);
    TlsSetValue(g_tls, (LPVOID)1);
    __try { return f(self, name, params); }
    __finally { TlsSetValue(g_tls, NULL); }
}

static bool __fastcall hook_start_anim(void* self, void* /*edx*/, const char* name, const void* params) {
    StartAnimFn f = (StartAnimFn)g_orig[H_ANIM_NAME];
    if (!want(tracecore::K_ANIM)) return f(self, name, params);
    int64_t wall = tracecore::now_unix_us();
    unsigned frame = visuals::frame_count();
    bool ok = call_start_nested(f, self, name, params);
    log_anim(wall, frame, self, name, -1, true, params, ok);
    return ok;
}

static bool __fastcall hook_start_anim_id(void* self, void* /*edx*/, int id, const void* params) {
    StartAnimIdFn f = (StartAnimIdFn)g_orig[H_ANIM_ID];
    if (!want(tracecore::K_ANIM) || nested_in_start()) return f(self, id, params);
    int64_t wall = tracecore::now_unix_us();
    unsigned frame = visuals::frame_count();
    bool ok = f(self, id, params);
    log_anim(wall, frame, self, nullptr, id, false, params, ok);
    return ok;
}

static void* __fastcall hook_spawn(void* self, void* /*edx*/, const void* loc, DWORD flags) {
    SpawnFn f = (SpawnFn)g_orig[H_FX_SPAWN];
    if (!want(tracecore::K_FX)) return f(self, loc, flags);
    int64_t wall = tracecore::now_unix_us();
    unsigned frame = visuals::frame_count();
    void* emitter = f(self, loc, flags);
    log_fx_spawn(wall, frame, self, loc, flags, emitter);
    return emitter;
}

static void* __fastcall hook_find_effect(void* self, void* /*edx*/, const char* name, const char* source,
                                         const char* source2, DWORD load) {
    FindFxFn f = (FindFxFn)g_orig[H_FX_FIND];
    if (!want(tracecore::K_FX)) return f(self, name, source, source2, load);
    int64_t wall = tracecore::now_unix_us();
    unsigned frame = visuals::frame_count();
    void* effect = f(self, name, source, source2, load);
    log_fx_find(wall, frame, name, effect);
    return effect;
}

static void* __fastcall hook_create_sound(void* self, void* /*edx*/, const char* name, DWORD flags) {
    CreateSoundFn f = (CreateSoundFn)g_orig[H_SOUND_CREATE];
    if (!want(tracecore::K_SOUND)) return f(self, name, flags);
    int64_t wall = tracecore::now_unix_us();
    unsigned frame = visuals::frame_count();
    void* sound = f(self, name, flags);
    log_sound_create(wall, frame, name, flags, sound);
    return sound;
}

static void __fastcall hook_play(void* self, void* /*edx*/, DWORD volume, DWORD force, DWORD ratio, void* package) {
    PlayFn f = (PlayFn)g_orig[H_SOUND_PLAY];
    if (want(tracecore::K_SOUND)) {
        // Logged before the call: Play may release a one-shot sound, and the name is read
        // from the live object.
        log_sound_play(tracecore::now_unix_us(), visuals::frame_count(), self, volume, force, ratio);
    }
    f(self, volume, force, ratio, package);
}

static void* const kHookFns[H_COUNT] = {
    (void*)&hook_start_anim, (void*)&hook_start_anim_id, (void*)&hook_spawn,
    (void*)&hook_find_effect, (void*)&hook_create_sound, (void*)&hook_play,
};

// ---- installation ------------------------------------------------------------------------
// Verifies and patches every hook whose module is present. Returns how many are installed;
// g_reason[h] says why a hook is not.
static int install_hooks(const Bases& bases) {
    int installed = 0;
    for (int h = 0; h < H_COUNT; ++h) {
        const HookSpec& spec = kHooks[h];
        BYTE* base = bases.module[spec.module];
        g_reason[h].clear();
        if (g_patch[h].installed) { ++installed; continue; }
        if (!base) { g_reason[h] = std::string(kModules[spec.module].file) + " is not loaded"; continue; }
        if (bases.check_pe) {
            DWORD stamp = 0, size = 0;
            if (!pe_identity(base, &stamp, &size)) { g_reason[h] = "module header unreadable"; continue; }
            if (stamp != kModules[spec.module].stamp || size != kModules[spec.module].image_size) {
                char text[120];
                _snprintf_s(text, sizeof(text), _TRUNCATE, "%s is not the r208022 build (stamp %08lX size %08lX)",
                            kModules[spec.module].file, stamp, size);
                g_reason[h] = text;
                continue;
            }
        }
        void** slot = (void**)(base + spec.vtable_rva) + spec.slot;
        void* expected = base + spec.function_rva;
        g_orig[h] = expected;                      // visible to the hook the moment it is live
        std::string why;
        if (!tracecore::patch_install(&g_patch[h], slot, expected, kHookFns[h], &why)) { g_reason[h] = why; continue; }
        ++installed;
    }
    g_sound_base = bases.module[M_SOUND];
    return installed;
}

static void remove_hooks() {
    for (int h = 0; h < H_COUNT; ++h) {
        tracecore::patch_remove(&g_patch[h]);
        g_reason[h].clear();
    }
}

// ---- control -----------------------------------------------------------------------------
static void append_u64(std::string* s, uint64_t v) {
    char b[32];
    _snprintf_s(b, sizeof(b), _TRUNCATE, "%llu", (unsigned long long)v);
    *s += b;
}

static std::string status_json() {
    std::string s = "{\"running\":";
    s += g_running ? "true" : "false";
    s += ",\"rows\":";
    append_u64(&s, g_ring ? g_ring->rows() : 0);
    s += ",\"dropped\":";
    append_u64(&s, g_ring ? g_ring->dropped() : 0);
    s += ",\"buffered\":";
    append_u64(&s, g_ring ? g_ring->buffered() : 0);
    s += ",\"kinds\":[";
    unsigned kinds = g_kinds.load();
    bool first = true;
    for (unsigned k = 1; k <= tracecore::K_FRAME; k <<= 1) {
        if (!(kinds & k)) continue;
        if (!first) s += ',';
        first = false;
        s += '"'; s += tracecore::kind_name(k); s += '"';
    }
    s += "],\"hooks\":{";
    for (int h = 0; h < H_COUNT; ++h) {
        if (h) s += ',';
        s += '"'; s += kHooks[h].id; s += "\":\"";
        if (g_patch[h].installed) s += "ok";
        else if (g_reason[h].empty()) s += "off";
        else { s += "inert: "; tracecore::json_escape(&s, g_reason[h].c_str(), 200, false); }
        s += '"';
    }
    s += "}}";
    return s;
}

struct Guard {
    Guard() { AcquireSRWLockExclusive(&g_control); }
    ~Guard() { ReleaseSRWLockExclusive(&g_control); }
};

static std::string start_locked(const std::string& kinds_text, const Bases& bases) {
    unsigned mask = 0;
    if (!tracecore::parse_kinds(kinds_text, &mask)) return "2unknown or duplicate trace kind (anim, fx, sound, press, frame)";
    if (!g_ring) g_ring = new tracecore::Ring(kRingBytes);
    if (g_running) {
        g_kinds.store(mask, std::memory_order_release);
        return "0" + status_json();
    }
    if (g_tls == TLS_OUT_OF_INDEXES) g_tls = TlsAlloc();
    g_ring->reset();
    g_last_present = 0;
    int installed = install_hooks(bases);
    if (!installed) {
        std::string why = "2trace inert, no hook could be installed:";
        for (int h = 0; h < H_COUNT; ++h) { why += ' '; why += kHooks[h].id; why += " ("; why += g_reason[h]; why += ");"; }
        return why;
    }
    g_running = true;
    g_kinds.store(mask, std::memory_order_release);
    return "0" + status_json();
}

static std::string stop_locked() {
    g_kinds.store(0, std::memory_order_release);
    remove_hooks();
    g_running = false;
    return "0" + status_json();
}

#ifdef SKILLTRACE_TEST
static Bases g_test_bases;     // the fixture supplies fake module images
#else
static Bases live_bases() {
    Bases b = {};
    for (int m = 0; m < M_COUNT; ++m) b.module[m] = (BYTE*)GetModuleHandleA(kModules[m].file);
    b.check_pe = true;
    return b;
}
#endif

std::string control(const std::string& command) {
    size_t space = command.find(' ');
    std::string action = command.substr(0, space);
    std::string argument = space == std::string::npos ? "" : command.substr(space + 1);
    Guard guard;
    if (action == "start") {
#ifdef SKILLTRACE_TEST
        return start_locked(argument, g_test_bases);
#else
        return start_locked(argument, live_bases());
#endif
    }
    if (!argument.empty()) return "2trace " + action + " takes no argument";
    if (action == "stop") return stop_locked();
    if (action == "status") return "0" + status_json();
    if (action == "drain") {
        std::string out = "0";
        if (g_ring) g_ring->drain(&out);
        return out;
    }
    return "2trace expects start [kinds], stop, drain or status";
}

void on_present(unsigned frame) {
    if (!want(tracecore::K_FRAME)) return;
    LARGE_INTEGER now, hz;
    QueryPerformanceCounter(&now);
    QueryPerformanceFrequency(&hz);
    LONGLONG previous = g_last_present;
    g_last_present = now.QuadPart;
    if (!previous || hz.QuadPart <= 0) return;
    Row row("frame", tracecore::now_unix_us(), frame);
    row.f("dt_ms", (double)(now.QuadPart - previous) * 1000.0 / (double)hz.QuadPart);
    emit(row);
}

void on_press(const char* key, int modifier_mask) {
    if (!want(tracecore::K_PRESS)) return;
    Row row("press", tracecore::now_unix_us(), visuals::frame_count());
    row.str("key", key);
    // Action bar slots: 1..9 -> 1..9, 0 -> 10, - -> 11, = -> 12.
    int slot = 0;
    if (key[0] && !key[1]) {
        if (key[0] >= '1' && key[0] <= '9') slot = key[0] - '0';
        else if (key[0] == '0') slot = 10;
        else if (key[0] == '-') slot = 11;
        else if (key[0] == '=') slot = 12;
    }
    if (slot) row.i("slot", slot);
    if (modifier_mask) {
        std::string mods;
        if (modifier_mask & 0x11) mods += "ctrl";
        if (modifier_mask & 0x22) { if (!mods.empty()) mods += '+'; mods += "shift"; }
        if (modifier_mask & 0x44) { if (!mods.empty()) mods += '+'; mods += "alt"; }
        row.str("mods", mods.c_str());
    }
    emit(row);
}

void remove() {
    Guard guard;
    stop_locked();
}

std::string info() { return g_running ? " trace=1" : " trace=0"; }
} // namespace skilltrace

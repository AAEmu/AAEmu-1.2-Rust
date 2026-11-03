// Fixture tests for the skill trace: ring buffer, JSON rows, kind parsing, the vtable-slot
// patch with its build check, and the six hooks driven through fake module images whose
// "original functions" are trampolines into this file. No game needed.
#define SKILLTRACE_TEST
#include "skilltrace.cpp"
#include <limits>
#include <stdio.h>
#include <stdlib.h>
#include <thread>
#include <vector>

static int g_failures;
#define CHECK(cond) do { if (!(cond)) { fprintf(stderr, "FAILED %s:%d: %s\n", __FILE__, __LINE__, #cond); ++g_failures; } } while (0)

static unsigned g_frame = 7;
namespace visuals { unsigned frame_count() { return g_frame; } }
using namespace skilltrace;

// ---- a small strict JSON validator (objects, arrays, strings, numbers, literals) -----------
struct Json {
    const char* p; const char* end;
    void ws() { while (p < end && (*p == ' ' || *p == '\t')) ++p; }
    bool lit(const char* s) { size_t n = strlen(s); if ((size_t)(end - p) >= n && !memcmp(p, s, n)) { p += n; return true; } return false; }
    bool str() {
        if (p >= end || *p != '"') return false;
        ++p;
        while (p < end && *p != '"') {
            unsigned char c = (unsigned char)*p;
            if (c < 0x20) return false;
            if (c == '\\') {
                ++p;
                if (p >= end) return false;
                if (*p == 'u') { for (int i = 0; i < 4; ++i) { ++p; if (p >= end || !isxdigit((unsigned char)*p)) return false; } }
                else if (!strchr("\"\\/bfnrt", *p)) return false;
            }
            ++p;
        }
        if (p >= end) return false;
        ++p;
        return true;
    }
    bool num() {
        const char* s = p;
        if (p < end && *p == '-') ++p;
        while (p < end && (isdigit((unsigned char)*p) || *p == '.' || *p == 'e' || *p == 'E' || *p == '+' || *p == '-')) ++p;
        return p > s;
    }
    bool value() {
        ws();
        if (p >= end) return false;
        if (*p == '{') {
            ++p; ws();
            if (p < end && *p == '}') { ++p; return true; }
            for (;;) {
                ws(); if (!str()) return false; ws();
                if (p >= end || *p != ':') return false;
                ++p; if (!value()) return false; ws();
                if (p < end && *p == ',') { ++p; continue; }
                if (p < end && *p == '}') { ++p; return true; }
                return false;
            }
        }
        if (*p == '[') {
            ++p; ws();
            if (p < end && *p == ']') { ++p; return true; }
            for (;;) {
                if (!value()) return false; ws();
                if (p < end && *p == ',') { ++p; continue; }
                if (p < end && *p == ']') { ++p; return true; }
                return false;
            }
        }
        if (*p == '"') return str();
        if (lit("true") || lit("false") || lit("null")) return true;
        return num();
    }
};
static bool valid_json(const std::string& s) { Json j{s.data(), s.data() + s.size()}; bool ok = j.value(); j.ws(); return ok && j.p == j.end; }

static std::vector<std::string> lines(const std::string& text) {
    std::vector<std::string> out;
    size_t pos = 0;
    while (pos < text.size()) {
        size_t nl = text.find('\n', pos);
        if (nl == std::string::npos) nl = text.size();
        out.push_back(text.substr(pos, nl - pos));
        pos = nl + 1;
    }
    return out;
}
static bool has(const std::string& s, const char* part) { return s.find(part) != std::string::npos; }

// ---- ring buffer and rows ------------------------------------------------------------------
static void test_clock_and_kinds() {
    CHECK(tracecore::unix_us_from_filetime(116444736000000000ULL) == 0);          // 1970-01-01
    CHECK(tracecore::unix_us_from_filetime(116444736000000000ULL + 10) == 1);      // +1 us
    int64_t now = tracecore::now_unix_us();
    CHECK(now > 1700000000LL * 1000000LL && now < 4000000000LL * 1000000LL);
    unsigned m = 0;
    CHECK(tracecore::parse_kinds("", &m) && m == tracecore::kDefaultKinds);
    unsigned defaults = tracecore::kDefaultKinds;
    CHECK(!(defaults & tracecore::K_FRAME));
    CHECK(tracecore::parse_kinds("anim,fx", &m) && m == (tracecore::K_ANIM | tracecore::K_FX));
    CHECK(tracecore::parse_kinds("frame", &m) && m == tracecore::K_FRAME);
    CHECK(tracecore::parse_kinds("anim,fx,sound,press,frame", &m) && m == tracecore::kAllKinds);
    m = 99;
    CHECK(!tracecore::parse_kinds("anim,anim", &m) && m == 99);
    CHECK(!tracecore::parse_kinds("anim,", &m));
    CHECK(!tracecore::parse_kinds(",anim", &m));
    CHECK(!tracecore::parse_kinds("mark", &m));
    CHECK(!tracecore::parse_kinds("ANIM", &m));
}

static void test_row_format_and_escaping() {
    tracecore::Row row("anim", 1700000000123456LL, 42);
    row.str("clip", "Walk_Fwd", true).i("layer", 3).fl("speed", 0.8f).f("dt", 16.6667).b("loop", true)
       .u("flags", 4294967295u).hex("inst", 0xDEADBEEF).vec3("pos", 1.5f, -2.f, 0.25f);
    const std::string& s = row.finish();
    CHECK(s == "{\"src\":\"orig\",\"wall_us\":1700000000123456,\"frame\":42,\"kind\":\"anim\","
               "\"clip\":\"walk_fwd\",\"layer\":3,\"speed\":0.8,\"dt\":16.6667,\"loop\":true,"
               "\"flags\":4294967295,\"inst\":\"0xdeadbeef\",\"pos\":[1.5,-2,0.25]}");
    CHECK(valid_json(s));
    CHECK(&row.finish() == &s);                                   // finishing twice adds nothing

    // quotes, backslashes, control characters and invalid UTF-8 never break the row
    tracecore::Row hostile("sound", 1, 2);
    const char* nasty = "a\"b\\c\n\t\x01 d\xff\xc3\x28 \xe2\x82\xac \xed\xa0\x80 \xf4\x90\x80\x80";
    hostile.str("name", nasty, false).str("k\"ey", "v");
    const std::string& h = hostile.finish();
    CHECK(valid_json(h));
    CHECK(has(h, "a\\\"b\\\\c\\u000a\\u0009\\u0001 d\\ufffd\\ufffd(")); // 0xFF and the broken 0xC3 0x28 pair
    CHECK(has(h, "\xe2\x82\xac"));                                // valid 3-byte sequence passes through
    CHECK(!has(h, "\xed\xa0\x80"));                               // surrogate rejected
    CHECK(has(h, "\"k\\\"ey\":\"v\""));

    // non-finite numbers become null, long strings are cut
    tracecore::Row odd("fx", 1, 1);
    odd.f("nan", std::numeric_limits<double>::quiet_NaN()).fl("inf", std::numeric_limits<float>::infinity()).str("long", std::string(1000, 'x').c_str(), false, 10);
    CHECK(valid_json(odd.finish()));
    CHECK(has(odd.finish(), "\"nan\":null"));
    CHECK(has(odd.finish(), "\"inf\":null"));
    CHECK(has(odd.finish(), "\"long\":\"xxxxxxxxxx\""));

    // shortest round-trip float text
    tracecore::Row fl("x", 0, 0);
    fl.fl("a", 1.0f).fl("b", 0.1f).fl("c", 123456.789f).fl("d", 1e-7f).fl("e", -0.f);
    CHECK(valid_json(fl.finish()));
    CHECK(has(fl.finish(), "\"a\":1,\"b\":0.1,\"c\":123456.79,\"d\":1e-07"));
}

static void test_ring() {
    tracecore::Ring r(64);
    std::string out;
    CHECK(r.drain(&out) == 0 && out.empty());
    CHECK(r.push("one", 3) && r.push("two", 3));
    CHECK(r.rows() == 2 && r.dropped() == 0 && r.buffered() == 2 && r.buffered_bytes() == 10);
    CHECK(r.drain(&out) == 2 && out == "one\ntwo\n");
    CHECK(r.buffered() == 0 && r.buffered_bytes() == 0);
    out.clear();

    // eviction: 64 bytes hold 6 rows of 8 bytes (2 + 8 = 10 each); the oldest go first and are counted
    tracecore::Ring e(64);
    for (int i = 0; i < 10; ++i) { char row[9]; snprintf(row, sizeof(row), "row-%04d", i); CHECK(e.push(row, 8)); }
    CHECK(e.rows() == 10 && e.dropped() == 4 && e.buffered() == 6);
    CHECK(e.drain(&out) == 6);
    CHECK(out == "row-0004\nrow-0005\nrow-0006\nrow-0007\nrow-0008\nrow-0009\n");
    out.clear();

    // wrap-around with rows of different sizes: the order and contents survive
    tracecore::Ring w(100);
    std::string expect_tail;
    for (int i = 0; i < 40; ++i) {
        std::string row(1 + (i * 7) % 11, (char)('a' + i % 26));
        w.push(row.data(), row.size());
        if (i % 5 == 4) {
            std::string drained;
            size_t n = w.drain(&drained);
            CHECK(n >= 1);
            for (const std::string& line : lines(drained)) CHECK(!line.empty() && line.find_first_not_of(line[0]) == std::string::npos);
        }
    }
    CHECK(w.rows() == 40 && w.dropped() == 0);                    // drained often enough: nothing lost

    // a row that cannot fit at all, and an empty row, are dropped and counted
    tracecore::Ring t(16);
    char big[32]; memset(big, 'x', sizeof(big));
    CHECK(!t.push(big, sizeof(big)) && !t.push("", 0));
    CHECK(t.rows() == 2 && t.dropped() == 2 && t.buffered() == 0);
    t.push("abc", 3);
    t.reset();
    CHECK(t.rows() == 0 && t.dropped() == 0 && t.buffered() == 0);
    CHECK(t.drain(&out) == 0);

    // concurrent producers and a draining consumer: nothing torn, every row accounted for
    tracecore::Ring c(1 << 16);
    std::vector<std::thread> threads;
    for (int t2 = 0; t2 < 4; ++t2) threads.emplace_back([&c, t2] {
        for (int i = 0; i < 20000; ++i) { char row[32]; int n = snprintf(row, sizeof(row), "T%d-%06d", t2, i); c.push(row, (size_t)n); }
    });
    uint64_t seen = 0;
    bool torn = false;
    auto consume = [&] {
        std::string drained;
        c.drain(&drained);
        for (const std::string& line : lines(drained)) {
            ++seen;
            if (line.size() != 9 || line[0] != 'T' || line[2] != '-') torn = true;
        }
    };
    for (int i = 0; i < 50; ++i) { consume(); Sleep(1); }
    for (size_t i = 0; i < threads.size(); ++i) threads[i].join();
    consume();
    CHECK(!torn);
    CHECK(c.rows() == 80000);
    CHECK(seen + c.dropped() == 80000);
}

// ---- the slot patch ------------------------------------------------------------------------
static void test_slot_patch() {
    // A fake vtable in read-only memory, like the .rdata of the real module.
    void** table = (void**)VirtualAlloc(NULL, 4096, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
    void* original = (void*)0x1000;
    void* hook = (void*)0x2000;
    table[3] = original; table[4] = (void*)0x3000;
    DWORD old;
    VirtualProtect(table, 4096, PAGE_READONLY, &old);

    tracecore::SlotPatch p;
    std::string why;
    // wrong build: the slot does not hold the expected function -> untouched, reason given
    CHECK(!tracecore::patch_install(&p, &table[3], (void*)0x1234, hook, &why));
    CHECK(!p.installed && table[3] == original && has(why, "not the verified build"));
    // misaligned and unreadable slots are refused
    CHECK(!tracecore::patch_install(&p, (void**)((char*)&table[3] + 1), original, hook, &why) && has(why, "misaligned"));
    CHECK(!tracecore::patch_install(&p, (void**)0x40, (void*)0, hook, &why) && has(why, "not readable"));
    CHECK(!tracecore::patch_install(&p, NULL, original, hook, &why));
    // the verified build: swapped, page protection restored
    CHECK(tracecore::patch_install(&p, &table[3], original, hook, &why));
    CHECK(p.installed && table[3] == hook && table[4] == (void*)0x3000 && p.original == original);
    MEMORY_BASIC_INFORMATION mbi;
    VirtualQuery(table, &mbi, sizeof(mbi));
    CHECK(mbi.Protect == PAGE_READONLY);
    CHECK(!tracecore::patch_install(&p, &table[3], original, hook, &why) && has(why, "already installed"));
    // removal restores
    CHECK(tracecore::patch_remove(&p) && !p.installed && table[3] == original);
    CHECK(tracecore::patch_remove(&p));                            // idempotent

    // a foreign patch applied over ours survives removal
    CHECK(tracecore::patch_install(&p, &table[3], original, hook, &why));
    DWORD o2; VirtualProtect(table, 4096, PAGE_READWRITE, &o2);
    table[3] = (void*)0x9999;
    VirtualProtect(table, 4096, PAGE_READONLY, &o2);
    CHECK(tracecore::patch_remove(&p) && table[3] == (void*)0x9999 && !p.installed);
    VirtualFree(table, 0, MEM_RELEASE);
}

// ---- fake engine modules --------------------------------------------------------------------
struct Calls {
    int by_name = 0, by_id = 0, spawn = 0, find = 0, create = 0, play = 0;
    void* last_self = nullptr;
    const char* last_name = nullptr;
    int last_id = -2;
    const void* last_params = nullptr;
    DWORD last_flags = 0;
    DWORD play_volume = 0, play_force = 0, play_ratio = 0;
    void* play_package = nullptr;
    bool by_id_result = true;
} g_calls;

static BYTE* g_anim; static BYTE* g_3d; static BYTE* g_snd;
static void* g_effect;                       // the object FindParticleEffect returns for "known.fx"
static void* g_sound_obj;

// The fakes use the hooks' own calling convention: __fastcall with a dummy edx has the stack
// layout (and callee clean-up) of the real __thiscall function.
static bool __fastcall fake_start_by_id(void* self, void*, int id, const void* params) {
    ++g_calls.by_id; g_calls.last_self = self; g_calls.last_id = id; g_calls.last_params = params;
    return g_calls.by_id_result;
}
static bool __fastcall fake_start_by_name(void* self, void*, const char* name, const void* params) {
    ++g_calls.by_name; g_calls.last_self = self; g_calls.last_name = name; g_calls.last_params = params;
    // like the engine: resolve the name, then StartAnimationById through the vtable
    typedef bool (__thiscall *ById)(void*, int, const void*);
    void** vt = *(void***)self;
    return ((ById)vt[14])(self, 2, params);
}
static void* __fastcall fake_spawn(void* self, void*, const void* loc, DWORD flags) {
    ++g_calls.spawn; g_calls.last_self = self; g_calls.last_flags = flags; g_calls.last_params = loc;
    return (void*)0xE111;
}
static void* __fastcall fake_find(void*, void*, const char* name, const char*, const char*, DWORD load) {
    ++g_calls.find; g_calls.last_name = name; g_calls.last_flags = load;
    return name && !strcmp(name, "known.fx") ? g_effect : nullptr;
}
static void* __fastcall fake_create(void*, void*, const char* name, DWORD flags) {
    ++g_calls.create; g_calls.last_name = name; g_calls.last_flags = flags;
    return g_sound_obj;
}
static void __fastcall fake_play(void* self, void*, DWORD volume, DWORD force, DWORD ratio, void* package) {
    ++g_calls.play; g_calls.last_self = self; g_calls.play_volume = volume; g_calls.play_force = force;
    g_calls.play_ratio = ratio; g_calls.play_package = package;
}
static const char* __fastcall fake_get_name(void*, void*) { return "Group:Event_Name"; }

static void trampoline(BYTE* at, void* target) {
    at[0] = 0xB8; memcpy(at + 1, &target, 4);       // mov eax, target
    at[1 + 4] = 0xFF; at[1 + 5] = 0xE0;             // jmp eax
}

static BYTE* fake_module(DWORD stamp, DWORD image_size) {
    BYTE* b = (BYTE*)VirtualAlloc(NULL, 0x300000, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE);
    IMAGE_DOS_HEADER* dos = (IMAGE_DOS_HEADER*)b;
    dos->e_magic = IMAGE_DOS_SIGNATURE; dos->e_lfanew = 0x80;
    IMAGE_NT_HEADERS* nt = (IMAGE_NT_HEADERS*)(b + 0x80);
    nt->Signature = IMAGE_NT_SIGNATURE;
    nt->FileHeader.TimeDateStamp = stamp;
    nt->OptionalHeader.SizeOfImage = image_size;
    return b;
}
static void set_slot(BYTE* base, DWORD vtable_rva, int slot, DWORD function_rva, void* code) {
    trampoline(base + function_rva, code);
    ((void**)(base + vtable_rva))[slot] = base + function_rva;
}

static void build_fake_engine() {
    g_anim = fake_module(kModules[M_ANIM].stamp, kModules[M_ANIM].image_size);
    g_3d = fake_module(kModules[M_3D].stamp, kModules[M_3D].image_size);
    g_snd = fake_module(kModules[M_SOUND].stamp, kModules[M_SOUND].image_size);
    const void* fns[H_COUNT] = { (void*)&fake_start_by_name, (void*)&fake_start_by_id, (void*)&fake_spawn,
                                 (void*)&fake_find, (void*)&fake_create, (void*)&fake_play };
    BYTE* bases[M_COUNT] = { g_anim, g_3d, g_snd };
    for (int h = 0; h < H_COUNT; ++h)
        set_slot(bases[kHooks[h].module], kHooks[h].vtable_rva, kHooks[h].slot, kHooks[h].function_rva, (void*)fns[h]);
    trampoline(g_snd + kSoundGetNameRva, (void*)&fake_get_name);
    ((void**)(g_snd + 0xA1638))[kSoundGetNameSlot] = g_snd + kSoundGetNameRva;
    g_test_bases = Bases{ { g_anim, g_3d, g_snd }, true };
}

// Fake game objects.
static BYTE* g_skel;
static const char* g_clips[] = { "Stand_Idle", "Run_FWD", "Attack_1H_Slash" };
static BYTE* make_skeleton() {
    BYTE* skel = (BYTE*)calloc(1, 4096);
    BYTE* inst = (BYTE*)calloc(1, 4096);
    BYTE* model = (BYTE*)calloc(1, 4096);
    struct Entry { const char* name; int pad[3]; };
    Entry* entries = (Entry*)calloc(3, sizeof(Entry));
    for (int i = 0; i < 3; ++i) entries[i].name = g_clips[i];
    *(void***)skel = (void**)(g_anim + 0x1779DC);
    *(BYTE**)(skel + kSkelInstance) = inst;
    *(BYTE**)(inst + kInstModel) = model;
    *(Entry**)(model + kModelAnimSet + kSetBegin) = entries;
    *(Entry**)(model + kModelAnimSet + kSetEnd) = entries + 3;
    return skel;
}

typedef bool  (__thiscall *ByName)(void*, const char*, const void*);
typedef bool  (__thiscall *ById)(void*, int, const void*);
typedef void* (__thiscall *Spawn)(void*, const void*, DWORD);
typedef void* (__thiscall *Find)(void*, const char*, const char*, const char*, DWORD);
typedef void* (__thiscall *Create)(void*, const char*, DWORD);
typedef void  (__thiscall *Play)(void*, DWORD, DWORD, DWORD, void*);

static void** vt_anim() { return (void**)(g_anim + 0x1779DC); }
static void** vt_spawn() { return (void**)(g_3d + 0x259350); }
static void** vt_3d() { return (void**)(g_3d + 0x263718); }
static void** vt_create() { return (void**)(g_snd + 0xA3A48); }
static void** vt_sound() { return (void**)(g_snd + 0xA1638); }

static std::string drain() { std::string r = control("drain"); CHECK(r[0] == '0'); return r.substr(1); }

static void test_hooks() {
    build_fake_engine();
    g_skel = make_skeleton();
    // an effect object whose name lives at +232, and a sound object with the real CSound vtable
    BYTE* effect = (BYTE*)calloc(1, 512);
    *(const char**)(effect + 232) = "Lib.Group.Effect_Name";
    g_effect = effect;
    BYTE* sound = (BYTE*)calloc(1, 512);
    *(void***)sound = vt_sound();
    g_sound_obj = sound;
    void* fx_vtable_original[4] = { vt_spawn()[3], vt_anim()[12], vt_anim()[14], vt_3d()[82] };

    CHECK(control("status") == "0{\"running\":false,\"rows\":0,\"dropped\":0,\"buffered\":0,\"kinds\":[],"
          "\"hooks\":{\"anim_start\":\"off\",\"anim_start_by_id\":\"off\",\"fx_spawn\":\"off\","
          "\"fx_find\":\"off\",\"sound_create\":\"off\",\"sound_play\":\"off\"}}");

    // Not recording: the real functions run and nothing changes.
    CHECK(vt_anim()[12] == g_anim + 0x59FD0);
    CHECK(((ByName)vt_anim()[12])(g_skel, "x", nullptr));
    CHECK(g_calls.by_name == 1 && g_calls.by_id == 1);

    // ---- bad arguments ----
    CHECK(control("start bogus")[0] == '2');
    CHECK(control("start anim anim")[0] == '2');
    CHECK(control("stop now")[0] == '2');
    CHECK(control("fly")[0] == '2');

    // ---- the build check: every hook inert -> start refuses ----
    BYTE* saved_slot[2] = { (BYTE*)vt_anim()[12], (BYTE*)vt_anim()[14] };
    const Bases good_bases = g_test_bases;
    {
        Bases wrong = g_test_bases;
        wrong.module[M_3D] = NULL; wrong.module[M_SOUND] = NULL;
        vt_anim()[12] = (void*)0x11111111; vt_anim()[14] = (void*)0x22222222;   // not the known functions
        g_test_bases = wrong;
        std::string r = control("start");
        CHECK(r[0] == '2' && has(r, "anim_start (vtable slot holds") && has(r, "cry3dengine.dll is not loaded"));
        CHECK(!g_running && g_kinds == 0 && vt_anim()[12] == (void*)0x11111111);   // nothing touched
        vt_anim()[12] = saved_slot[0]; vt_anim()[14] = saved_slot[1];
    }
    g_test_bases = good_bases;

    // ---- a different PE build is refused even when the slots look right ----
    {
        ((IMAGE_NT_HEADERS*)(g_snd + 0x80))->FileHeader.TimeDateStamp ^= 1;
        std::string r = control("start");
        CHECK(r[0] == '0' && has(r, "\"sound_create\":\"inert: crysoundsystem.dll is not the r208022 build (stamp"));
        CHECK(has(r, "\"anim_start\":\"ok\"") && has(r, "\"fx_spawn\":\"ok\""));
        CHECK(vt_create()[19] == g_snd + 0x4FE40 && vt_anim()[12] != g_anim + 0x59FD0);
        CHECK(control("stop")[0] == '0');
        CHECK(vt_anim()[12] == g_anim + 0x59FD0);
        ((IMAGE_NT_HEADERS*)(g_snd + 0x80))->FileHeader.TimeDateStamp ^= 1;
    }

    // ---- start: all six hooks live ----
    std::string started = control("start");
    CHECK(started[0] == '0' && valid_json(started.substr(1)));
    CHECK(has(started, "\"running\":true") && has(started, "\"kinds\":[\"anim\",\"fx\",\"sound\",\"press\"]"));
    for (const char* id : { "anim_start", "anim_start_by_id", "fx_spawn", "fx_find", "sound_create", "sound_play" })
        CHECK(has(started, (std::string("\"") + id + "\":\"ok\"").c_str()));
    CHECK(vt_anim()[12] == (void*)&hook_start_anim && vt_anim()[14] == (void*)&hook_start_anim_id);
    CHECK(vt_spawn()[3] == (void*)&hook_spawn && vt_3d()[82] == (void*)&hook_find_effect);
    CHECK(vt_create()[19] == (void*)&hook_create_sound && vt_sound()[8] == (void*)&hook_play);
    CHECK(control("start")[0] == '0');                           // idempotent

    // ---- animation by name: one row, the nested by-id call is suppressed ----
    g_calls = Calls();
    AnimParams ap = { 0.25f, -1.f, 1.5f, 0.f, 4, 0.75f, 0x2u | 0x800u };
    g_frame = 100;
    CHECK(((ByName)vt_anim()[12])(g_skel, "Walk_FWD", &ap) == true);
    CHECK(g_calls.by_name == 1 && g_calls.by_id == 1);           // the original ran, with the same arguments
    CHECK(g_calls.last_name != nullptr && !strcmp(g_calls.last_name, "Walk_FWD") && g_calls.last_params == &ap);
    {
        std::vector<std::string> rows = lines(drain());
        CHECK(rows.size() == 1);
        const std::string& r = rows[0];
        CHECK(valid_json(r));
        CHECK(has(r, "\"src\":\"orig\"") && has(r, "\"frame\":100,\"kind\":\"anim\""));
        CHECK(has(r, "\"clip\":\"walk_fwd\"") && has(r, "\"by\":\"name\"") && !has(r, "\"id\":"));
        CHECK(has(r, "\"layer\":4,\"speed\":1.5,\"blend\":0.25,\"loop\":true,\"flags\":2050,\"key_time\":-1,\"weight\":0.75"));
        CHECK(has(r, "\"ok\":true"));
        char skel_text[40];
        snprintf(skel_text, sizeof(skel_text), "\"skel\":\"0x%08x\"", (unsigned)(uintptr_t)g_skel);
        CHECK(has(r, skel_text));
        BYTE* inst = *(BYTE**)(g_skel + kSkelInstance);
        char inst_text[40];
        snprintf(inst_text, sizeof(inst_text), "\"inst\":\"0x%08x\"", (unsigned)(uintptr_t)inst);
        CHECK(has(r, inst_text));
        // wall_us is the Unix microsecond clock
        size_t at = r.find("\"wall_us\":");
        CHECK(at != std::string::npos);
        long long wall = atoll(r.c_str() + at + 10);
        long long now = tracecore::now_unix_us();
        CHECK(wall > now - 5000000 && wall <= now);
    }

    // ---- animation by id: clip name from the animation set, the result is passed through ----
    g_calls = Calls();
    g_calls.by_id_result = false;
    CHECK(((ById)vt_anim()[14])(g_skel, 1, &ap) == false);
    CHECK(g_calls.by_id == 1 && g_calls.last_id == 1);
    {
        std::vector<std::string> rows = lines(drain());
        CHECK(rows.size() == 1 && valid_json(rows[0]));
        CHECK(has(rows[0], "\"clip\":\"run_fwd\"") && has(rows[0], "\"by\":\"id\",\"id\":1,\"ok\":false"));
    }
    g_calls.by_id_result = true;

    // ---- hostile inputs: bad id, bad object, null/garbage parameters, null name ----
    g_calls = Calls();
    CHECK(((ById)vt_anim()[14])(g_skel, 3, &ap));                 // one past the table
    CHECK(((ById)vt_anim()[14])(g_skel, -1, &ap));
    CHECK(((ById)vt_anim()[14])(g_skel, 2000000000, nullptr));    // null params, huge id
    CHECK(((ById)vt_anim()[14])((void*)0x10, 0, (const void*)0x20));   // unreadable skeleton and params
    CHECK(((ByName)vt_anim()[12])(g_skel, nullptr, nullptr));     // vtable of g_skel is the real fake table
    CHECK(((ByName)vt_anim()[12])(g_skel, (const char*)0x30, nullptr));
    CHECK(g_calls.by_id == 6 && g_calls.by_name == 2);
    {
        std::vector<std::string> rows = lines(drain());
        CHECK(rows.size() == 6);
        for (const std::string& r : rows) CHECK(valid_json(r) && has(r, "\"kind\":\"anim\""));
        CHECK(!has(rows[0], "\"clip\"") && !has(rows[1], "\"clip\""));
        CHECK(has(rows[0], "\"layer\":4") && !has(rows[2], "\"layer\""));
        CHECK(!has(rows[3], "\"inst\"") && !has(rows[3], "\"layer\"") && has(rows[3], "\"skel\":\"0x00000010\""));
    }

    // ---- particles ----
    g_calls = Calls();
    float loc[8] = { 0.f, 0.f, 0.7071068f, 0.7071068f, 10.5f, 20.25f, -3.f, 2.f };
    CHECK(((Spawn)vt_spawn()[3])(effect, loc, 0x15) == (void*)0xE111);
    CHECK(g_calls.spawn == 1 && g_calls.last_self == effect && g_calls.last_params == loc && g_calls.last_flags == 0x15);
    CHECK(((Find)vt_3d()[82])(nullptr, "known.fx", "emitter", "src2", 1) == effect);
    CHECK(((Find)vt_3d()[82])(nullptr, "Unknown.FX", "", "", 0) == nullptr);
    CHECK(g_calls.find == 2 && g_calls.last_flags == 0);
    {
        std::vector<std::string> rows = lines(drain());
        CHECK(rows.size() == 3);
        for (const std::string& r : rows) CHECK(valid_json(r) && has(r, "\"kind\":\"fx\""));
        CHECK(has(rows[0], "\"phase\":\"spawn\",\"name\":\"lib.group.effect_name\""));
        CHECK(has(rows[0], "\"pos\":[10.5,20.25,-3],\"scale\":2,\"rot\":[0,0,0.7071068,0.7071068],\"flags\":21,\"emitter\":\"0x0000e111\""));
        CHECK(has(rows[1], "\"phase\":\"find\",\"name\":\"known.fx\",\"found\":true") && has(rows[1], "\"resolved\":\"lib.group.effect_name\""));
        CHECK(has(rows[2], "\"phase\":\"find\",\"name\":\"unknown.fx\",\"found\":false") && !has(rows[2], "\"resolved\""));
    }
    CHECK(((Spawn)vt_spawn()[3])(effect, nullptr, 0) == (void*)0xE111);          // no location: row without pos
    CHECK(((Spawn)vt_spawn()[3])((void*)0x40, (const void*)0x50, 0) == (void*)0xE111);   // unreadable effect
    {
        std::vector<std::string> rows = lines(drain());
        CHECK(rows.size() == 2 && valid_json(rows[0]) && valid_json(rows[1]));
        CHECK(!has(rows[0], "\"pos\"") && has(rows[0], "\"name\":\"lib.group.effect_name\""));
        CHECK(!has(rows[1], "\"name\"") && !has(rows[1], "\"pos\""));
    }

    // ---- sound ----
    g_calls = Calls();
    CHECK(((Create)vt_create()[19])(nullptr, "Sound/Skills/Swing", 0x81) == sound);
    CHECK(g_calls.create == 1 && !strcmp(g_calls.last_name, "Sound/Skills/Swing") && g_calls.last_flags == 0x81);
    float volume = 0.8f; DWORD volume_bits; memcpy(&volume_bits, &volume, 4);
    ((Play)vt_sound()[8])(sound, volume_bits, 1, 0xFFFFFF00u, (void*)0x77);
    CHECK(g_calls.play == 1 && g_calls.play_volume == volume_bits && g_calls.play_force == 1);
    CHECK(g_calls.play_ratio == 0xFFFFFF00u && g_calls.play_package == (void*)0x77);   // raw stack words pass through
    {
        std::vector<std::string> rows = lines(drain());
        CHECK(rows.size() == 2);
        for (const std::string& r : rows) CHECK(valid_json(r) && has(r, "\"kind\":\"sound\""));
        CHECK(has(rows[0], "\"phase\":\"create\",\"name\":\"sound/skills/swing\",\"flags\":129,\"sound\":\""));
        CHECK(has(rows[1], "\"phase\":\"play\",\"sound\":\"") && has(rows[1], "\"name\":\"group:event_name\""));
        CHECK(has(rows[1], "\"volume\":0.8,\"force_active\":true,\"set_ratio\":false"));
    }
    // A sound object whose vtable slot 29 is not the verified GetName: no name, no call into it.
    {
        void* foreign_vtable[40] = {};
        foreign_vtable[kSoundGetNameSlot] = (void*)0x1234;
        void* foreign[2] = { foreign_vtable, nullptr };
        ((Play)vt_sound()[8])(foreign, volume_bits, 1, 1, nullptr);
        ((Play)vt_sound()[8])((void*)0x60, volume_bits, 1, 1, nullptr);       // unreadable object
        std::vector<std::string> rows = lines(drain());
        CHECK(rows.size() == 2 && valid_json(rows[0]) && valid_json(rows[1]));
        CHECK(!has(rows[0], "\"name\"") && !has(rows[1], "\"name\""));
        CHECK(g_calls.play == 3);
    }

    // ---- presses ----
    on_press("1", 0);
    on_press("0", 0);
    on_press("-", 0);
    on_press("=", 0x11 | 0x22);
    on_press("w", 0x44);
    on_press("f", 0);
    {
        std::vector<std::string> rows = lines(drain());
        CHECK(rows.size() == 6);
        for (const std::string& r : rows) CHECK(valid_json(r) && has(r, "\"kind\":\"press\""));
        CHECK(has(rows[0], "\"key\":\"1\",\"slot\":1}"));
        CHECK(has(rows[1], "\"key\":\"0\",\"slot\":10}"));
        CHECK(has(rows[2], "\"key\":\"-\",\"slot\":11}"));
        CHECK(has(rows[3], "\"key\":\"=\",\"slot\":12,\"mods\":\"ctrl+shift\"}"));
        CHECK(has(rows[4], "\"key\":\"w\",\"mods\":\"alt\"}") && !has(rows[4], "\"slot\""));
        CHECK(has(rows[5], "\"key\":\"f\"}"));
    }

    // ---- frame rows only when requested ----
    on_present(1); on_present(2);
    CHECK(drain().empty());
    CHECK(control("start frame")[0] == '0');                       // already running: kinds change, nothing re-patched
    CHECK(has(control("status"), "\"kinds\":[\"frame\"]"));
    on_present(10);                                                // the first present has no previous one
    Sleep(15);
    on_present(11);
    {
        std::vector<std::string> rows = lines(drain());
        CHECK(rows.size() == 1 && valid_json(rows[0]));
        CHECK(has(rows[0], "\"frame\":11,\"kind\":\"frame\",\"dt_ms\":"));
        double dt = atof(rows[0].c_str() + rows[0].find("\"dt_ms\":") + 8);
        CHECK(dt >= 10.0 && dt < 500.0);
    }
    // with only `frame` selected the other hooks pass through without rows
    g_calls = Calls();
    ((ByName)vt_anim()[12])(g_skel, "x", nullptr);
    ((Spawn)vt_spawn()[3])(effect, loc, 0);
    ((Create)vt_create()[19])(nullptr, "s", 0);
    ((Play)vt_sound()[8])(sound, 0, 0, 0, nullptr);
    CHECK(g_calls.by_name == 1 && g_calls.by_id == 1 && g_calls.spawn == 1 && g_calls.create == 1 && g_calls.play == 1);
    CHECK(drain().empty());

    // ---- counters ----
    {
        std::string status = control("status");
        CHECK(valid_json(status.substr(1)));
        CHECK(has(status, "\"rows\":") && has(status, "\"dropped\":0"));
    }

    // ---- stop restores every slot and keeps the unread rows for one more drain ----
    CHECK(control("start anim,fx")[0] == '0');
    ((ById)vt_anim()[14])(g_skel, 0, &ap);
    std::string stopped = control("stop");
    CHECK(stopped[0] == '0' && has(stopped, "\"running\":false") && has(stopped, "\"buffered\":1"));
    CHECK(vt_anim()[12] == g_anim + 0x59FD0 && vt_anim()[14] == g_anim + 0x5B350);
    CHECK(vt_spawn()[3] == fx_vtable_original[0] && vt_3d()[82] == fx_vtable_original[3]);
    CHECK(vt_create()[19] == g_snd + 0x4FE40 && vt_sound()[8] == g_snd + 0x37AC0);
    CHECK(lines(drain()).size() == 1);
    g_calls = Calls();
    ((ByName)vt_anim()[12])(g_skel, "x", nullptr);
    CHECK(g_calls.by_name == 1 && drain().empty());               // not recording, original only
    CHECK(control("stop")[0] == '0');                              // stopping twice is fine
    CHECK(info() == " trace=0");

    // a restart begins with an empty buffer and zeroed counters
    CHECK(control("start")[0] == '0' && info() == " trace=1");
    CHECK(has(control("status"), "\"rows\":0,\"dropped\":0,\"buffered\":0"));
    skilltrace::remove();                                          // driver unload restores the table
    CHECK(vt_anim()[12] == g_anim + 0x59FD0 && info() == " trace=0");
}

int main() {
    test_clock_and_kinds();
    test_row_format_and_escaping();
    test_ring();
    test_slot_patch();
    test_hooks();
    if (g_failures) { fprintf(stderr, "%d skill trace check(s) failed\n", g_failures); return 1; }
    puts("skill trace ring, rows, escaping, slot patch with build check, six hooks, kinds, presses and restoration passed");
    return 0;
}

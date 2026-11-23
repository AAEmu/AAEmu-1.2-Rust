// r208022 patch and hook sites of the client performance set. Plain data, shared by perfpatch.cpp
// (runtime checks), perfpatch-test.cpp and perfpatch-verify.cpp (static check against the client
// files). The dev client (C:\aa-re\devclient-r208022, src\dll\patches.cpp) uses the same rows for
// the six patches it already has; keep both in sync.
//
// RVAs are module-relative. "??" bytes are relocated absolute operands. Every check runs before
// any write, and a mismatch refuses the patch. Evidence: C:\aa-re\notes\client-perf-r208022.md.
#pragma once
#include <stdint.h>

namespace perfsites {

enum Module { CRYSYSTEM = 0, CRYRENDER = 1, MODULE_COUNT = 2 };

struct ModuleId { const char* name; const wchar_t* wname; uint32_t stamp; uint32_t sizeOfImage; uint32_t preferredBase; };
static const ModuleId kModules[MODULE_COUNT] = {
    { "crysystem.dll", L"crysystem.dll", 0x543CB856, 0x169000, 0x36500000 },
    { "cryrenderd3d10.dll", L"cryrenderd3d10.dll", 0x543CB89C, 0x549000, 0x38000000 },
};

// bytes == nullptr: a check only (hook prologue, call site, loop shape).
struct Site { const char* id; Module module; uint32_t rva; const char* expect; uint32_t patchRva; const char* bytes; const char* what; };

static const Site kSites[] = {
    // ---- byte patches (same checks and writes as the dev client's table) ----
    { "pak-readahead-64k", CRYSYSTEM, 0x14CC2, "6A 00 68 00 00 10 00 52 50 8B 07 50 8B C3 E8",
      0x14CC5, "00 00 01 00", "Pack::Read (FUN_36514B10): read-ahead limit pushed to the window helper, 0x100000 -> 0x10000" },
    { "cpuspeed-sleep10", CRYSYSTEM, 0x237B5, "0F 31 6A 64 89 45 EC 89 55 F0 FF 15 ?? ?? ?? ?? 0F 31",
      0x237B8, "0A", "CPU-speed probe (FUN_36523740): Sleep(100) between two RDTSC/QPC samples -> Sleep(10)" },
    { "instancing-no-wc-readback", CRYRENDER, 0x5DA45,
      "0F 28 40 20 0F 29 47 F4 0F 28 40 30 0F 29 47 04 0F 28 40 40 0F 29 47 14 F3 0F 10 07 F3 0F 5C 85 50 FF FF FF "
      "F3 0F 11 07 F3 0F 10 47 10 F3 0F 5C 85 54 FF FF FF F3 0F 11 47 10 F3 0F 10 47 20 F3 0F 5C 85 58 FF FF FF 41 "
      "F3 0F 11 47 20",
      0x5DA45,
      "0F 28 40 20 0F 29 47 F4 8D 95 50 FF FF FF F3 0F 10 40 2C F3 0F 5C 02 F3 0F 11 07 0F 28 40 30 0F 29 47 04 "
      "F3 0F 10 40 3C F3 0F 5C 42 04 F3 0F 11 47 10 0F 28 40 40 0F 29 47 14 F3 0F 10 40 4C F3 0F 5C 42 08 0F 1F 00",
      "FX_DrawShader_General instancing loop: camera-relative translation from the source matrix, not the mapped VB" },
    { "shader-async-poll-5ms", CRYRENDER, 0x8DA15, "83 3D ?? ?? ?? ?? 00 75 07 68 FA 00 00 00 EB 02 6A 19 FF 15 ?? ?? ?? ??",
      0x8DA26, "05", "async shader worker: Sleep(25) between queue scans -> Sleep(5)" },

    // ---- hook sites (checked, not written) ----
    { "dispmode-cache", CRYSYSTEM, 0x61F0B, "C7 85 90 FE FF FF 00 00 1C 00 FF 15 ?? ?? ?? ?? 85 C0 75 1D", 0, nullptr,
      "resolution list (FUN_36561D60): ChangeDisplaySettingsExA(CDS_TEST|CDS_FULLSCREEN) per enumerated mode, dmFields 0x1C0000" },
    { "fast-splitpath", CRYSYSTEM, 0x8C00, "55 8B EC 81 EC 54 04 00 00 A1", 0, nullptr,
      "FirstDirComponent (FUN_36508C00), first _splitpath caller of the pak tree build" },
    { "fast-splitpath", CRYSYSTEM, 0x8EF0, "55 8B EC 81 EC 54 04 00 00 A1", 0, nullptr,
      "HasMoreLevels (FUN_36508EF0), second _splitpath caller" },
    { "frame-latency", CRYRENDER, 0x145C83, "68 ?? ?? ?? ?? 50 FF D7 A3 ?? ?? ?? ?? A1", 0, nullptr,
      "LoadD3D11 (FUN_38145C50): GetProcAddress(d3d11, \"D3D11CreateDevice\")" },
    { "render-thread-event-sync", CRYRENDER, 0x195857, "FF 15 ?? ?? ?? ?? 8B 45 EC 8B 48 20 85 C9 0F 85", 0, nullptr,
      "WaitFlushFinishedCond (FUN_38195510): call [SwitchToThread]; this = [ebp-0x14], m_nFlush = this+0x20" },
    { "render-thread-event-sync", CRYRENDER, 0x1959FB,
      "8B 1D ?? ?? ?? ?? 80 7F 10 00 75 1F 83 FE 10 7F 11 8B 45 F0 F3 90 83 C0 FF 75 F9 03 F6 89 75 F0 EB 02 FF D3 8B 57 20 85 D2 74 DB",
      0, nullptr, "WaitFlushCond (FUN_381958F0): mov ebx,[SwitchToThread]; loop on edi+0x20 == 0 and quit byte edi+0x10" },
    { "render-thread-event-sync", CRYRENDER, 0x1958F0, "55 8B EC 64 A1 00 00 00 00 6A FF 68 ?? ?? ?? ?? 50 64 89 25 00 00 00 00 83 EC 68",
      0, nullptr, "WaitFlushCond entry (thiscall, no stack arguments): waker hook" },
    { "render-thread-event-sync", CRYRENDER, 0x199610, "55 8B EC 64 A1 00 00 00 00 6A FF 68 ?? ?? ?? ?? 50 64 89 25 00 00 00 00 83 EC 6C",
      0, nullptr, "SyncMainWithRender entry (thiscall, no stack arguments): waker hook" },
    { "render-thread-event-sync", CRYRENDER, 0x1998CF, "89 7E 20", 0, nullptr,
      "SyncMainWithRender: m_nFlush = 1 (the only set; render loop clears it at +0x19CA56 / +0x19CCB0)" },
    { "pak-catalog-scope", CRYSYSTEM, 0x13C60, "55 8B EC 6A FF 68 ?? ?? ?? ?? 64 A1 00 00 00 00 50 81 EC 50 04 00 00", 0, nullptr,
      "Pack catalogue load (FUN_36513C60, thiscall, one stack argument, ret 4)" },
    { "pak-findchild-index", CRYSYSTEM, 0x17720, "55 8B EC 51 56 8B F0 83 7E 14 00 77 09", 0, nullptr,
      "DirNode::FindChild (FUN_36517720): EAX = node, [esp+4] = name, ret 4" },
    { "pak-findchild-index", CRYSYSTEM, 0x177D0, "55 8B EC 6A FF 68", 0, nullptr,
      "DirNode::AddChild (FUN_365177D0): push_back before the list sentinel, count +0x14 incremented" },
};

// IAT slots the hooks replace. The runtime finds the slot by import name and requires it to be
// this RVA, and the static verifier checks the import name at the RVA.
struct Import { const char* id; Module module; uint32_t slotRva; const char* dll; const char* function; };
static const Import kImports[] = {
    { "dispmode-cache", CRYSYSTEM, 0x108558, "USER32.dll", "ChangeDisplaySettingsExA" },
    { "fast-splitpath", CRYSYSTEM, 0x1083F4, "MSVCR100.dll", "_splitpath" },
    { "frame-latency", CRYRENDER, 0x2D3088, "KERNEL32.dll", "GetProcAddress" },
    { "render-thread-event-sync", CRYRENDER, 0x2D3050, "KERNEL32.dll", "SwitchToThread" },
};

} // namespace perfsites

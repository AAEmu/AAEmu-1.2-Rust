// The 1.2 CPU-clock calibration temporarily requests real-time scheduling. Keep
// the measurement intact, but run it at normal priority so the desktop can run.
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <string>
#include "startup.h"

namespace startup {
static BYTE* base;
static volatile bool applied;
static const DWORD processImmediate = 0x23776;
static const DWORD threadImmediate = 0x2378A;
// CrySystem RVA 23740: saved priorities -> push REALTIME_PRIORITY_CLASS ->
// XlSetPriorityClass -> push THREAD_PRIORITY_TIME_CRITICAL -> XlSetThreadPriority.
static const BYTE expected[] = {
    0x68,0x00,0x01,0x00,0x00,0x89,0x45,0xf4,0xff,0xd6,0x50,
    0xff,0x15,0x34,0x87,0x60,0x36,0x83,0xc4,0x08,0x6a,0x0f,
    0xff,0xd7,0x50,0xff,0x15,0x38,0x87,0x60,0x36
};

static bool matches(BYTE* b) {
    BYTE actual[sizeof(expected)];
    memcpy(actual, b + processImmediate - 1, sizeof(actual));
    // Relocated absolute IAT operands; all other bytes must match the known build.
    DWORD relocation = (DWORD)b - 0x36500000;
    DWORD processSlot = 0x36608734 + relocation, threadSlot = 0x36608738 + relocation;
    if (memcmp(actual + 13, &processSlot, 4) || memcmp(actual + 27, &threadSlot, 4)) return false;
    memcpy(actual + 13, expected + 13, 4); memcpy(actual + 27, expected + 27, 4);
    return memcmp(actual, expected, sizeof(actual)) == 0;
}

static bool exchange(BYTE* b, bool smooth) {
    DWORD old;
    if (!VirtualProtect(b + processImmediate - 1, sizeof(expected), PAGE_EXECUTE_READWRITE, &old)) return false;
    // Both halfwords are naturally aligned. Preserve the neighboring call opcode
    // when changing the one-byte thread immediate; concurrent readers see a valid
    // old or new priority value, never a partial instruction.
    auto process = reinterpret_cast<volatile SHORT*>(b + processImmediate);
    auto thread = reinterpret_cast<volatile SHORT*>(b + threadImmediate);
    SHORT pOld = smooth ? 0x0100 : 0x0020, pNew = smooth ? 0x0020 : 0x0100;
    SHORT tOld = smooth ? -241 : -256; // 0xff0f / 0xff00
    SHORT tNew = smooth ? -256 : -241;
    SHORT p = InterlockedCompareExchange16(process, pNew, pOld);
    bool ok = p == pOld;
    if (ok && InterlockedCompareExchange16(thread, tNew, tOld) != tOld) {
        InterlockedCompareExchange16(process, pOld, pNew); ok = false;
    }
    FlushInstructionCache(GetCurrentProcess(), b + processImmediate - 1, sizeof(expected));
    DWORD ignored;
    VirtualProtect(b + processImmediate - 1, sizeof(expected), old, &ignored);
    return ok;
}

// Both immediates already hold the normal priorities (an earlier install through the other path).
static bool smooth(BYTE* b) {
    BYTE copy[sizeof(expected)];
    memcpy(copy, b + processImmediate - 1, sizeof(copy));
    if (*reinterpret_cast<SHORT*>(copy + 1) != 0x0020 || copy[threadImmediate - processImmediate + 1] != 0) return false;
    *reinterpret_cast<SHORT*>(copy + 1) = 0x0100; copy[threadImmediate - processImmediate + 1] = 0x0f;
    DWORD relocation = (DWORD)b - 0x36500000;
    DWORD processSlot = 0x36608734 + relocation, threadSlot = 0x36608738 + relocation;
    if (memcmp(copy + 13, &processSlot, 4) || memcmp(copy + 27, &threadSlot, 4)) return false;
    memcpy(copy + 13, expected + 13, 4); memcpy(copy + 27, expected + 27, 4);
    return memcmp(copy, expected, sizeof(copy)) == 0;
}

// No loader calls: perfpatch calls this under its lock, right after crysystem's DllMain returned.
bool install_at(BYTE* b) {
    if (applied) return true;
    if (!b) return false;
    auto nt = reinterpret_cast<IMAGE_NT_HEADERS*>(b + reinterpret_cast<IMAGE_DOS_HEADER*>(b)->e_lfanew);
    if (nt->FileHeader.TimeDateStamp != 0x543CB856 || nt->OptionalHeader.SizeOfImage != 0x169000) return false;
    if (!smooth(b)) {
        if (!matches(b)) return false;
        if (!exchange(b, true) && !smooth(b)) return false;
    }
    base = b; applied = true;
    return true;
}

bool install() {
    if (applied) return true;
    return install_at(reinterpret_cast<BYTE*>(GetModuleHandleW(L"CrySystem.dll")));
}

void remove() {
    if (applied && exchange(base, false)) applied = false;
}

std::string info() { return applied ? " smooth=1" : " smooth=0"; }
}

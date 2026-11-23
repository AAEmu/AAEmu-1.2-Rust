// Static check of every perfsites.h row against the r208022 client files on disk.
//   perf-sites.exe <client>\bin32        (exit 0 = every row matches, 1 = a mismatch)
// crysystem.dll and cryrenderd3d10.dll are plain on disk at every RVA used, so the file mapped at its
// preferred base equals the loaded image (relocated operands are wildcards anyway). With no argument
// and no %AADRIVER_VERIFY_BIN32%, the check is skipped (build machines without a client).
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <stdio.h>
#include <string.h>
#include <string>
#include <vector>
#include "perfcore.h"
#include "perfsites.h"

using namespace perfsites;

struct Image { std::vector<uint8_t> mem; uint32_t stamp = 0, size = 0, base = 0; bool ok = false; };

static Image map_file(const std::string& path) {
    Image img;
    FILE* f = NULL;
    if (fopen_s(&f, path.c_str(), "rb") || !f) return img;
    std::vector<uint8_t> file;
    fseek(f, 0, SEEK_END); long n = ftell(f); fseek(f, 0, SEEK_SET);
    file.resize((size_t)n);
    size_t got = fread(file.data(), 1, file.size(), f);
    fclose(f);
    if (got != file.size() || file.size() < 0x400) return img;
    auto dos = (IMAGE_DOS_HEADER*)file.data();
    auto nt = (IMAGE_NT_HEADERS32*)(file.data() + dos->e_lfanew);
    img.stamp = nt->FileHeader.TimeDateStamp; img.size = nt->OptionalHeader.SizeOfImage; img.base = nt->OptionalHeader.ImageBase;
    img.mem.assign(img.size, 0);
    memcpy(img.mem.data(), file.data(), nt->OptionalHeader.SizeOfHeaders);
    auto sec = IMAGE_FIRST_SECTION(nt);
    for (int i = 0; i < nt->FileHeader.NumberOfSections; ++i) {
        size_t raw = sec[i].SizeOfRawData, at = sec[i].VirtualAddress;
        if (sec[i].PointerToRawData + raw > file.size()) raw = file.size() - sec[i].PointerToRawData;
        if (at + raw > img.mem.size()) raw = img.mem.size() - at;
        memcpy(img.mem.data() + at, file.data() + sec[i].PointerToRawData, raw);
    }
    img.ok = true;
    return img;
}

static std::string hex(const uint8_t* p, size_t n) {
    std::string s; char b[4];
    for (size_t i = 0; i < n; ++i) { _snprintf_s(b, sizeof b, _TRUNCATE, i ? " %02X" : "%02X", p[i]); s += b; }
    return s;
}

// Import name of the IAT slot at slotRva: "DLL!function", or "" if the RVA is not an IAT slot.
static std::string import_at(const Image& img, uint32_t slotRva) {
    auto nt = (IMAGE_NT_HEADERS32*)(img.mem.data() + ((IMAGE_DOS_HEADER*)img.mem.data())->e_lfanew);
    uint32_t rva = nt->OptionalHeader.DataDirectory[IMAGE_DIRECTORY_ENTRY_IMPORT].VirtualAddress;
    for (auto d = (const IMAGE_IMPORT_DESCRIPTOR*)(img.mem.data() + rva); rva && d->Name; ++d) {
        uint32_t names = d->OriginalFirstThunk, slots = d->FirstThunk;
        for (uint32_t k = 0;; ++k) {
            uint32_t entry = *(const uint32_t*)(img.mem.data() + names + 4 * k);
            if (!entry) break;
            if (slots + 4 * k != slotRva) continue;
            std::string dll = (const char*)(img.mem.data() + d->Name);
            if (entry & 0x80000000u) return dll + "!#ordinal";
            return dll + "!" + (const char*)(img.mem.data() + entry + 2);
        }
    }
    return "";
}

int main(int argc, char** argv) {
    std::string dir = argc > 1 ? argv[1] : "";
    if (dir.empty()) { char b[MAX_PATH]; if (GetEnvironmentVariableA("AADRIVER_VERIFY_BIN32", b, MAX_PATH)) dir = b; }
    if (dir.empty()) { puts("perfpatch-verify: no client bin32 given (argument or AADRIVER_VERIFY_BIN32), skipped"); return 0; }
    int bad = 0, rows = 0;
    Image img[MODULE_COUNT];
    for (int m = 0; m < MODULE_COUNT; ++m) {
        img[m] = map_file(dir + "\\" + kModules[m].name);
        if (!img[m].ok) { printf("MISSING  %s\n", kModules[m].name); ++bad; continue; }
        bool id = img[m].stamp == kModules[m].stamp && img[m].size == kModules[m].sizeOfImage && img[m].base == kModules[m].preferredBase;
        printf("%-8s %s stamp %08X size %08X base %08X\n", id ? "MODULE" : "WRONG", kModules[m].name, img[m].stamp, img[m].size, img[m].base);
        if (!id) { ++bad; img[m].ok = false; }
    }
    for (const Site& s : kSites) {
        if (!img[s.module].ok) continue;
        ++rows;
        const uint8_t* base = img[s.module].mem.data();
        perfcore::Pattern e;
        perfcore::parse_pattern(s.expect, e);
        bool ok = s.rva + e.v.size() <= img[s.module].size && perfcore::matches(base + s.rva, e);
        // the same window must not occur anywhere else in the module (a unique site)
        int hits = 0;
        for (size_t at = 0; ok && at + e.v.size() <= img[s.module].size; ++at) if (perfcore::matches(base + at, e)) ++hits;
        if (!ok) ++bad;
        printf("%-8s %-28s %s+0x%06X check %u bytes%s", ok ? "MATCH" : "MISMATCH", s.id, kModules[s.module].name, s.rva,
               (unsigned)e.v.size(), hits > 1 ? " (pattern not unique; pinned by RVA)" : "");
        if (s.bytes) {
            perfcore::Pattern b; perfcore::parse_pattern(s.bytes, b);
            printf("\n         patch +0x%06X [%s] -> [%s]", s.patchRva, hex(base + s.patchRva, b.v.size()).c_str(), hex(b.v.data(), b.v.size()).c_str());
        }
        printf("\n");
        if (!ok) printf("         found [%s]\n", hex(base + s.rva, e.v.size()).c_str());
    }
    for (const Import& i : kImports) {
        if (!img[i.module].ok) continue;
        ++rows;
        std::string got = import_at(img[i.module], i.slotRva), want = std::string(i.dll) + "!" + i.function;
        bool ok = _stricmp(got.c_str(), want.c_str()) == 0;
        if (!ok) ++bad;
        printf("%-8s %-28s %s IAT +0x%06X = %s\n", ok ? "MATCH" : "MISMATCH", i.id, kModules[i.module].name, i.slotRva, got.empty() ? "(not an IAT slot)" : got.c_str());
    }
    // render-thread-event-sync redirects two absolute operands that must point at the SwitchToThread slot
    if (img[CRYRENDER].ok) {
        const uint8_t* r = img[CRYRENDER].mem.data();
        uint32_t want = kModules[CRYRENDER].preferredBase + 0x2D3050;
        for (uint32_t op : { 0x195859u, 0x1959FDu }) {
            ++rows;
            bool ok = *(const uint32_t*)(r + op) == want;
            if (!ok) ++bad;
            printf("%-8s %-28s operand +0x%06X = %08X (slot %08X)\n", ok ? "MATCH" : "MISMATCH", "render-thread-event-sync", op, *(const uint32_t*)(r + op), want);
        }
    }
    printf("%d rows checked, %d mismatches\n", rows, bad);
    return bad ? 1 : 0;
}

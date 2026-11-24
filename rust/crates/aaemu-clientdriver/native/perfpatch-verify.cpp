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

struct Image : perfcore::PeImage { bool ok = false; std::string why; };

// Reads the whole file and maps it through perfcore::map_pe, which checks every header field
// first: a truncated or malformed file is reported as invalid, never read or written out of bounds.
static Image map_file(const std::string& path) {
    Image img;
    FILE* f = NULL;
    if (fopen_s(&f, path.c_str(), "rb") || !f) { img.why = "cannot open"; return img; }
    std::vector<uint8_t> file;
    bool read = fseek(f, 0, SEEK_END) == 0;
    long n = read ? ftell(f) : -1;
    read = read && n >= 0 && (unsigned long)n <= 0x40000000ul && fseek(f, 0, SEEK_SET) == 0;
    if (read) { file.resize((size_t)n); read = fread(file.data(), 1, file.size(), f) == file.size(); }
    fclose(f);
    if (!read) { img.why = "cannot read"; return img; }
    img.ok = perfcore::map_pe(file.data(), file.size(), img, img.why);
    return img;
}

static std::string hex(const uint8_t* p, size_t n) {
    std::string s; char b[4];
    for (size_t i = 0; i < n; ++i) { _snprintf_s(b, sizeof b, _TRUNCATE, i ? " %02X" : "%02X", p[i]); s += b; }
    return s;
}
// hex of [at, at + n) when it lies inside the image
static std::string hex_at(const Image& img, size_t at, size_t n) {
    return perfcore::pe_range(img.mem.size(), at, n) ? hex(img.mem.data() + at, n) : std::string("(outside the image)");
}

int main(int argc, char** argv) {
    std::string dir = argc > 1 ? argv[1] : "";
    if (dir.empty()) { char b[MAX_PATH]; if (GetEnvironmentVariableA("AADRIVER_VERIFY_BIN32", b, MAX_PATH)) dir = b; }
    if (dir.empty()) { puts("perfpatch-verify: no client bin32 given (argument or AADRIVER_VERIFY_BIN32), skipped"); return 0; }
    int bad = 0, rows = 0;
    Image img[MODULE_COUNT];
    char dumpDir[MAX_PATH] = "";
    GetEnvironmentVariableA("AADRIVER_VERIFY_DUMP", dumpDir, MAX_PATH);
    for (int m = 0; m < MODULE_COUNT; ++m) {
        if (kModules[m].packed) {
            // Packed on disk: only an in-memory image (sections at their RVAs, e.g. the dev client's
            // module_dump) has the code. Without one these rows are checked at run time only.
            if (!*dumpDir) { printf("SKIPPED  %s (packed on disk; set AADRIVER_VERIFY_DUMP to a folder with r208022_%s)\n", kModules[m].name, kModules[m].name); continue; }
            img[m] = map_file(std::string(dumpDir) + "\\r208022_" + kModules[m].name);
        } else {
            img[m] = map_file(dir + "\\" + kModules[m].name);
        }
        if (!img[m].ok) { printf("%-8s %s (%s)\n", img[m].why == "cannot open" ? "MISSING" : "INVALID", kModules[m].name, img[m].why.c_str()); ++bad; continue; }
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
        bool ok = !e.v.empty() && perfcore::pe_range(img[s.module].mem.size(), s.rva, e.v.size()) && perfcore::matches(base + s.rva, e);
        // the same window must not occur anywhere else in the module (a unique site)
        int hits = 0;
        for (size_t at = 0; ok && at + e.v.size() <= img[s.module].mem.size(); ++at) if (perfcore::matches(base + at, e)) ++hits;
        if (!ok) ++bad;
        printf("%-8s %-28s %s+0x%06X check %u bytes%s", ok ? "MATCH" : "MISMATCH", s.id, kModules[s.module].name, s.rva,
               (unsigned)e.v.size(), hits > 1 ? " (pattern not unique; pinned by RVA)" : "");
        if (s.bytes) {
            perfcore::Pattern b; perfcore::parse_pattern(s.bytes, b);
            printf("\n         patch +0x%06X [%s] -> [%s]", s.patchRva, hex_at(img[s.module], s.patchRva, b.v.size()).c_str(), hex(b.v.data(), b.v.size()).c_str());
        }
        printf("\n");
        if (!ok) printf("         found [%s]\n", hex_at(img[s.module], s.rva, e.v.size()).c_str());
    }
    for (const Import& i : kImports) {
        if (!img[i.module].ok) continue;
        ++rows;
        std::string got = perfcore::pe_import_at(img[i.module], i.slotRva), want = std::string(i.dll) + "!" + i.function;
        bool ok = _stricmp(got.c_str(), want.c_str()) == 0;
        if (!ok) ++bad;
        printf("%-8s %-28s %s IAT +0x%06X = %s\n", ok ? "MATCH" : "MISMATCH", i.id, kModules[i.module].name, i.slotRva, got.empty() ? "(not an IAT slot)" : got.c_str());
    }
    // render-thread-event-sync redirects two absolute operands that must point at the SwitchToThread slot
    if (img[CRYRENDER].ok) {
        uint32_t want = kModules[CRYRENDER].preferredBase + 0x2D3050;
        for (uint32_t op : { 0x195859u, 0x1959FDu }) {
            ++rows;
            uint32_t got = 0;
            bool ok = perfcore::pe_u32(img[CRYRENDER], op, got) && got == want;
            if (!ok) ++bad;
            printf("%-8s %-28s operand +0x%06X = %08X (slot %08X)\n", ok ? "MATCH" : "MISMATCH", "render-thread-event-sync", op, got, want);
        }
    }
    printf("%d rows checked, %d mismatches\n", rows, bad);
    return bad ? 1 : 0;
}

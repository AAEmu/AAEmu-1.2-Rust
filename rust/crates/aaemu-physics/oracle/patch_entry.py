#!/usr/bin/env python3
"""Prepare the 1.2 x86 cryphysics runtime for the oracle.

Copies the unpacked 1.2 client DLL (D:/aa-re/cryphysics/unpack_x86/
cryphysics_unpacked.dll) to the untracked runtime directory and sets
AddressOfEntryPoint = 0, so LoadLibrary neither runs DllMain nor the
VMProtect stubs.  The preferred image base is kept (0x34FF0000); relocation
is not needed and the dumped .data holds absolute pointers.

The rebuilt import directory (unpack_x86/addimports.py) labels six ntdll
Rtl* imports as kernel32 imports, which LoadLibrary rejects with error 127.
This script rewrites the import descriptors so those entries import from
ntdll.dll, splitting the kernel32 runs around them.

Also copies the x86 runtime DLLs the physics DLL needs from the read-only
1.2 client (D:/AA1.2/bin32): msvcr100, msvcp100, xlcommon and the latter's
dependencies xldiag, tbb, dbghelp.  Nothing here is committed; the runtime
directory is scratch space.

usage: patch_entry.py [--src DLL] [--client DIR] [--out DIR]
"""
import argparse
import os
import struct
import sys

DEFAULT_SRC = r"D:/aa-re/cryphysics/unpack_x86/cryphysics_unpacked.dll"
DEFAULT_CLIENT = r"D:/AA1.2/bin32"
DEFAULT_OUT = r"D:/aaemu-target/physics/oracle-x86/rt"
RUNTIME_DLLS = ["msvcr100.dll", "msvcp100.dll", "xlcommon.dll", "xldiag.dll", "tbb.dll", "dbghelp.dll"]
PREFERRED_BASE = 0x34FF0000

ENTRY_RVA_OFF = 16
IMPORT_DIR_OFF = 96 + 8  # data directory index 1


def sections(data):
    e = struct.unpack_from("<I", data, 0x3C)[0]
    n = struct.unpack_from("<H", data, e + 6)[0]
    opt = e + 24
    so = struct.unpack_from("<H", data, e + 20)[0]
    out = []
    for i in range(n):
        o = opt + so + 40 * i
        name = data[o : o + 8].rstrip(b"\0").decode(errors="replace")
        vsz, va, rsz, ra = struct.unpack_from("<IIII", data, o + 8)
        out.append({"hdr": o, "name": name, "va": va, "vsz": vsz, "raw": ra, "rsz": rsz})
    return e, opt, out


def rva_to_off(secs, rva):
    for s in secs:
        if s["va"] <= rva < s["va"] + max(s["vsz"], s["rsz"]):
            return s["raw"] + rva - s["va"]
    raise SystemExit("rva %#x is not in any section" % rva)


def cstr(data, off):
    end = data.index(b"\0", off)
    return data[off:end].decode(errors="replace")


def read_imports(data):
    e, opt, secs = sections(data)
    # optional header data directory index 1 sits at opt+96+8
    imp_rva, imp_sz = struct.unpack_from("<II", data, opt + 96 + 8)
    items = []  # (iat_rva, module, name)
    o = rva_to_off(secs, imp_rva)
    while True:
        ilt, ts, fc, name_rva, iat = struct.unpack_from("<IIIII", data, o)
        if not any([ilt, ts, fc, name_rva, iat]):
            break
        module = cstr(data, rva_to_off(secs, name_rva))
        t = rva_to_off(secs, ilt or iat)
        i = 0
        while True:
            w = struct.unpack_from("<I", data, t + 4 * i)[0]
            if w == 0:
                break
            if w & 0x80000000:
                name = "#%d" % (w & 0xFFFF)
            else:
                p = rva_to_off(secs, w & 0x7FFFFFFF)
                name = cstr(data, p + 2)
            items.append((iat + 4 * i, module, name))
            i += 1
        o += 20
    items.sort()
    return items, secs


def rebuild_imports(data):
    """Rewrite .idata2 so Rtl* imports come from ntdll.dll."""
    items, secs = read_imports(data)
    runs = []
    for iat_rva, module, name in items:
        target = "ntdll.dll" if name.startswith("Rtl") else module
        if runs and runs[-1][0] == target and runs[-1][1][-1][0] + 4 == iat_rva:
            runs[-1][1].append((iat_rva, name))
        else:
            runs.append((target, [(iat_rva, name)]))
    idata = next(s for s in secs if s["name"] == ".idata2")
    base_va = idata["va"]
    blob = bytearray()
    blob += b"\0" * (20 * (len(runs) + 1))  # descriptors + terminator
    ilt_offsets = []
    for _, entries in runs:
        while len(blob) % 4:
            blob += b"\0"
        ilt_offsets.append(len(blob))
        positions = []
        for _, _name in entries:
            positions.append(len(blob))
            blob += b"\0\0\0\0"
        blob += b"\0\0\0\0"  # ILT terminator
        for pos, (_, name) in zip(positions, entries):
            if name.startswith("#"):
                struct.pack_into("<I", blob, pos, 0x80000000 | int(name[1:]))
            else:
                if len(blob) % 2:
                    blob += b"\0"
                hint_off = len(blob)
                struct.pack_into("<I", blob, pos, base_va + hint_off)
                blob += b"\0\0"
                blob += name.encode() + b"\0"
    # module name strings, one per distinct module in first-use order
    mod_off = {}
    for module, _ in runs:
        if module in mod_off:
            continue
        if len(blob) % 2:
            blob += b"\0"
        mod_off[module] = len(blob)
        blob += module.encode() + b"\0"
    for i, (module, entries) in enumerate(runs):
        struct.pack_into(
            "<IIIII",
            blob,
            20 * i,
            base_va + ilt_offsets[i],
            0,
            0,
            base_va + mod_off[module],
            entries[0][0],
        )
    if len(blob) > idata["vsz"]:
        raise SystemExit("rebuilt import blob (%d) does not fit .idata2 (%d)" % (len(blob), idata["vsz"]))
    raw = idata["raw"]
    size = max(idata["rsz"], len(blob))
    data[raw : raw + idata["rsz"]] = blob + b"\0" * (idata["rsz"] - len(blob))
    if size > idata["rsz"]:
        data[raw + idata["rsz"] : raw + size] = b"\0" * (size - idata["rsz"])
        struct.pack_into("<I", data, idata["hdr"] + 16, size)  # SizeOfRawData
    struct.pack_into("<I", data, idata["hdr"] + 8, max(idata["vsz"], len(blob)))  # VirtualSize
    e, opt, _ = sections(data)
    struct.pack_into("<II", data, opt + 96 + 8, base_va, 20 * (len(runs) + 1))
    for module, entries in runs:
        print("imports %-14s %3d slots from %#x" % (module, len(entries), entries[0][0]))
    return len(runs)


TIME_STEP_VM_THUNK_VA = 0x35081560  # jmp 0x3537BFB8, in the VMProtect .reloc payload


def patch_time_step_vm_thunk(data):
    """TimeStep starts with two `call 0x35081560` profiler-timer starts.  In 1.2 that thunk is
    a VMProtect stub into the .reloc payload, which cannot run without the packer context.
    With the profiler disabled (gEnv+0x97 = 0) the two calls have no observable effect, so the
    runtime copy turns the thunk into a `ret`."""
    _, _, secs = sections(data)
    rva = TIME_STEP_VM_THUNK_VA - PREFERRED_BASE
    off = rva_to_off(secs, rva)
    if data[off] != 0xE9:
        raise SystemExit("TimeStep VM thunk does not start with a jmp (found %#x)" % data[off])
    data[off : off + 5] = b"\xC3\x90\x90\x90\x90"
    print("patched TimeStep VM thunk %#x -> ret" % TIME_STEP_VM_THUNK_VA)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--src", default=DEFAULT_SRC)
    ap.add_argument("--client", default=DEFAULT_CLIENT)
    ap.add_argument("--out", default=DEFAULT_OUT)
    ap.add_argument("--name", default="cryphysics12.dll")
    args = ap.parse_args()

    with open(args.src, "rb") as f:
        data = bytearray(f.read())
    e_lfanew = struct.unpack_from("<I", data, 0x3C)[0]
    opt = e_lfanew + 24
    magic = struct.unpack_from("<H", data, opt)[0]
    if magic != 0x10B:
        raise SystemExit("not a PE32 image (magic %#x)" % magic)
    image_base = struct.unpack_from("<I", data, opt + 28)[0]
    entry = struct.unpack_from("<I", data, opt + ENTRY_RVA_OFF)[0]
    if image_base != PREFERRED_BASE:
        raise SystemExit("unexpected image base %#x" % image_base)
    if entry != 0:
        struct.pack_into("<I", data, opt + ENTRY_RVA_OFF, 0)
        print("entry point %#x -> 0" % entry)

    nruns = rebuild_imports(data)
    patch_time_step_vm_thunk(data)

    os.makedirs(args.out, exist_ok=True)
    dst = os.path.join(args.out, args.name)
    with open(dst, "wb") as f:
        f.write(data)
    print("patched %s -> %s (base %#x, %d import runs)" % (args.src, dst, image_base, nruns))

    for name in RUNTIME_DLLS:
        src = os.path.join(args.client, name)
        if not os.path.isfile(src):
            raise SystemExit("missing runtime DLL: %s" % src)
        with open(src, "rb") as fi, open(os.path.join(args.out, name), "wb") as fo:
            fo.write(fi.read())
        print("copied %s" % name)
    return 0


if __name__ == "__main__":
    sys.exit(main())

# physoracle: running the real CryPhysics engine headlessly

`physoracle.c` loads a shipped CryPhysics DLL standalone (no game, no `ISystem`), drives it
through its vtables and prints a bit-exact trace in which every float is its raw IEEE-754 bits.
One build targets the x64 10.x zone-server engine, the other (`ORACLE_X86`) the 1.2 client
engine. Scenario syntax and the golden traces for the x64 build are described in the crate
README (`rust/crates/aaemu-physics/README.md`, "The oracle"). The 1.2 findings live in
`rust/docs/physics/CRYPHYSICS-MAP.md` §9b.

## x64 (10.x zone server)

```
oracle\build.bat
physoracle.exe [--rt DIR] tests\scenarios\box_on_ground.scn
physoracle.exe [--rt DIR] --math sinf 1000 6
```

The runtime directory (`--rt`, `%AAEMU_PHYS_RT%`, default `D:\aa-re\cryphysics\harness\rt`)
holds `cryphysics_dedicate.dll`, MSVCR100, MSVCP100, `tbb.dll`, `xldiag.dll`, the original
`xlcommon.dll` and the `tbbmalloc_proxy.dll` stub. Binaries are never committed.

## x86 (1.2 client)

The 1.2 DLL is VMProtect-packed, so it is first unpacked from a client capture
(`D:\aa-re\cryphysics\unpack_x86\`, not part of this repository). The oracle loads that
unpacked image at its preferred base after two small runtime patches.

### Prepare the runtime (untracked)

```
python oracle\patch_entry.py
oracle\build_x86.bat
```

`patch_entry.py` copies `D:\aa-re\cryphysics\unpack_x86\cryphysics_unpacked.dll` to
`D:\aaemu-target\physics\oracle-x86\rt\cryphysics12.dll` (override with `--src/--out`) and:

* sets `AddressOfEntryPoint = 0`, so `LoadLibrary` runs neither `DllMain` nor the VMProtect
  stubs;
* rewrites the recovered import directory: the unpacker labelled six `ntdll` `Rtl*` imports as
  `kernel32` imports, which `LoadLibrary` rejects with error 127. The runs are split so the
  `Rtl*` entries import from `ntdll.dll`;
* replaces the two-call VMProtect profiler-timer stub at VA `0x35081560` with `ret`.

It also copies the x86 runtime DLLs from the read-only 1.2 client (`D:\AA1.2\bin32`):
`msvcr100`, `msvcp100`, `xlcommon`, `xldiag`, `tbb`, `dbghelp`. Nothing is committed; all
binaries live under `D:\aaemu-target\physics\oracle-x86\`.

`smoke_x86.exe` verifies that the patched DLL loads at `0x34FF0000` and that both exports
resolve (they must not be called).

### Run

```
physoracle_x86.exe [--rt DIR] [--fpu 24|53|64] [--static-init none|tls|ctors]
                   [--t0 CAPTURE] SCENARIO|-
```

* `--fpu` sets the x87 precision control before any engine code (`53` = double, the plain
  process default; `24` = single, what the D3D9 client normally uses).
* `--static-init ctors` (default) restores `.data` from the unpacked file, clears the
  process-specific globals (TLS index/table, three captured heap pointers, guard word), runs
  the 35 CRT static ctors from the XC table at `0x351FA18C` and diffs the result against the
  capture. `tls` is the quick fix from the feasibility study: it only allocates a TLS slot and
  leaves the captured post-init `.data` otherwise untouched. Both produce identical traces.
* `--t0` (or `%AAEMU_PHYS_T0%`) points at `capture\t0\cryphysics.dll.mem` for the self-check.
  The self-check goes to stderr; stdout stays a clean trace.
* Diagnostics (`fpu ...`, `static-init ...`, `world ...`) go to stderr.

The world is created by hand: `FUN_351F3BE0(0x53F0)` then the constructor `0x350889B0`
(`__thiscall`), with `DAT_3523F4E8 = 0` (client behaviour) and the fake `gEnv` installed at
`0x3537640C` first. The 1.2 fake `gEnv` needs `+0` = a null object for the profiler label
ctor, `+0x38` = a config object whose slot 21 returns a block with the ray-grid limit at
`+0x100`, bytes `+0x95`/`+0x96`, and `+0x97 = 0` (profiler off). Fake x86 methods must be
`__thiscall` (or `__fastcall` with a dummy EDX): a cdecl fake leaves stack arguments behind,
because x86 vtable calls are callee-popped.

### Result

`box_on_ground` and `box_float_300kg` run deterministically (two runs each) and are
**bit-identical to the x64 10.x golden traces** under both x87 precisions; rest z `0.480746`
and float z `2.214693`. The committed traces are in `tests/golden-12/`. Note that the x86
`.data` is a dump of the client process, so after `--static-init ctors` the self-check reports
five process-specific words plus three initialization-only words (one guard bit, two
self-pointers), and nothing unexpected; `tls` reports only the five process words.

`trace_diff.py A.trace B.trace` compares two traces (normalises CRLF) and prints the first
divergence as step / entity / field.

### Not ported in milestone 1

`ray` (the 1.2 `SRWIParams` and `ray_hit` layouts), heightfield terrain, trimesh geometry
(`geom type=mesh`), non-global areas, `listwhere`/`contacts` debug dumps, `var` (use
`var off86=` if needed; the 1.2 block layout is documented in the map), and all
internal-function (`--math`, `--p1a*`, `--p2*`, `--p4*`) probes. The scenario interpreter,
box/sphere/cylinder/capsule primitives, static/rigid entities, parts, buoyancy areas,
`SetSurfaceParameters`, `TimeStep` and traces are ported.

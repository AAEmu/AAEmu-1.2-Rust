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
physoracle_x86.exe [--rt DIR] [--fpu 24|53|64] [--mxcsr HEX]
                   [--static-init none|tls|ctors] [--t0 CAPTURE] SCENARIO|-
```

* `--fpu` sets the x87 precision control before any engine code (`53` = double, the plain
  process default; `24` = single, what the D3D9 client normally uses).
* `--mxcsr HEX` sets the SSE control word explicitly (default: keep the process value).
  Both the process default and the applied value are printed on stderr, e.g.
  `mxcsr default=0x1f80 set=0x1f80`.  The engine is SSE scalar in the hot paths, so this
  (not the x87 word) is the important control; some helpers are x87 (144 `float10`
  functions), so both modes are still run.
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

`box_on_ground`, `box_float_300kg`, `box_float_700kg`, `exp_buoyancy_same_step`,
`p7_params_queue` and `params_neutral` are **bit-identical to the x64 10.x traces** under
both x87 precisions. The other pointer-free rigid scenarios diverge by 1–6 ulp in a
rotating body's quaternion/velocity (see `tests/golden-12/README.md`); the divergence
reports name step/entity/field. `capsule_drop`, `p7_many_bodies` and `p7_ship_float`
differ between `--fpu 24` and `--fpu 53`, so those three have both traces committed; the
decompile's 144 `float10` helpers explain the exception to the "SSE only" note. The
committed traces are in `tests/golden-12/`.

The x86 `.data` is a dump of the client process, so after `--static-init ctors` the
self-check reports five process-specific words plus three initialization-only words (one
guard bit, two self-pointers), and nothing unexpected; `tls` reports only the five process
words.

`trace_diff.py A.trace B.trace` compares two traces (normalises CRLF) and prints the first
divergence as step / entity / field. `trace_diff.py --all` compares every `golden-12`
trace with its `golden` counterpart and prints one line per scenario plus counts.

### Ported in milestone 2

Params `pe_params_pos`, `pe_params_part`, `pe_params_outer_entity` and
`pe_params_foreign_data` are converted between the x64 scenario image and the 32-bit
layout at the vtable call (dumps stay x64). `pe_geomparams` is converted the same way for
`part` (plain 88 B, car 160 B, articulated 92 B): the scenario's x64 field overrides are
applied to the x64 image first and the whole struct is then translated, so overrides never
land on the wrong 1.2 field. Reverse parameter conversion clears only the x64 struct image,
so a GetParams out-parameter buffer reached through a scenario `buf:` pointer (the car
gear ratios) survives. `peek`, `snapdiff`, `poke`, `peekworld`,
`peekdll` and `listwhere` map the scenario's x64 offsets/addresses to 1.2 (rigid bodies,
the physics-vars block, the size tables, the surface tables, the simclass lists); an
unmapped offset fails loudly. `findptr`/`rawpeek`/`addrof` are x86-only helpers for
deriving further layout offsets. Rays are ported: `SRWIParams` (64 B) and `ray_hit`
(64 B) use the 1.2 layouts derived from `FUN_351C2B00`, and the sine table moves to
`0x35375268`. Heightfield terrain is ported: the 108-byte 1.2 `primitives::heightfield`
(callbacks at +100/+104) and the five terrain entity slots at world+0x52C+4*i.
`pe_params_particle` is ported (pointer at +140, size 156), `player dynamics` (id 4) and
`pe_status_living` (id 2) have their 1.2 layouts, and `var off=` translates the physics-vars
block shift. Every `pe_status` id the suite uses is marshalled like the params (pos
time_back +120 -> +100, living, wheel status 5): the scenario's x64 overrides are applied
first and the whole struct is translated at the vtable call. Wheel status 5 has no wheel
pos/q block; its contact-entity pointer at x86 +100 is zeroed before the reverse conversion
because it is process-specific. `gather`/`abuoy`/`estate`/`poke` use the 1.2 addresses and
76-byte `global_buoyancy` records, so the p4a global-area buoyancy scenarios run. Wheeled
entities are ported: car params (pointer at +84, no x64 +12 field, size 124), identical
wheel params, and the type-1 geomparams wheel block shifted from +112..+183 to +88..+159 by
the same whole-struct conversion. See `tests/golden-12/README.md` for the evidence.

**Rare startup crash.** About 1 in 20-100 runs aborts before the first step with
`EXCEPTION c0000005 at cryphysics VA 6a071250 addr 6a071250` (exit code 3), independent of
the scenario; retrying the same command succeeds and successful runs are deterministic.
Golden regeneration should retry on exit code 3.

### Not ported yet

Trimesh geometry (`geom type=mesh`, 16 scenarios): the 1.2 geom-manager `CreateMesh` is slot
11 (`0x35153730`), but `p1c_mesh.c` still builds the x64 mesh/BV arguments and is disabled
for `ORACLE_X86`; CGF also needs `AAEMU_PHYS_CGF_DIR`. Non-global areas
(spline/geometry/polygon) and the CheckAreas/PWI/RTE/CEWP/beam query helpers, `contacts`,
`var off86=` remains available for native 1.2 offsets, and the internal-function
(`--math`, `--p1a*`, `--p2*`, `--p4*`) probes are not ported. The scenario interpreter,
box/sphere/cylinder/capsule primitives, static/rigid/particle/living/wheeled entities,
parts, global buoyancy areas, `SetSurfaceParameters`, `TimeStep`, rays and traces are
ported.

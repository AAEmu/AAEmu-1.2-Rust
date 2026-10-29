# 1.2 client oracle goldens

Traces produced by the x86 build of `physoracle` from the real 1.2 client
`cryphysics.dll` (unpacked, image base `0x34FF0000`). They are the 1.2 counterpart of the
x64 10.x traces in `../golden/` and use the same scenario language.

| file | scenario | x87 precision |
|---|---|---|
| `box_on_ground.fpu53.trace` | `../scenarios/box_on_ground.scn` | 53-bit (double) |
| `box_on_ground.fpu24.trace` | same | 24-bit (single) |
| `box_float_300kg.fpu53.trace` | `../scenarios/box_float_300kg.scn` | 53-bit |
| `box_float_300kg.fpu24.trace` | same | 24-bit |
| `vars.trace` | `dumpvars` after world creation | 53-bit |

Both FPU modes produce byte-identical traces for these scenarios. Two runs of each
scenario/precision are also byte-identical (determinism).

Comparison with the x64 goldens: `box_on_ground` and `box_float_300kg` are **bit-identical**
to `../golden/*.trace` after normalising line endings (the x86 oracle writes LF). There is no
first divergence. Rest z is `0x3ef62457` (0.480746) and float z is `0x400dbd88` (2.214693) in
both engines.

`vars.trace` holds the 1.2 physics-vars block (`vars 72`, 135 words). Its first 29 words match
the x64 block (`../golden/engine_tables.trace`, `vars 104`); two extra zero words follow at
world `+188/+192`, then x64's words from `+220` onward are shifted to x86 `+196` and on, with
one extra zero at the end (`+608`). The only value difference is the `dedicated` flag at
`+588` (0 = client, x64 golden has 1 because it runs with `gEnv == NULL`). See
`docs/physics/CRYPHYSICS-MAP.md` §9b.

Regenerate (runtime preparation: `oracle/README.md`):

```
python oracle/patch_entry.py
oracle/build_x86.bat
set AAEMU_PHYS_RT=D:\aaemu-target\physics\oracle-x86\rt
physoracle_x86.exe --fpu 53 --static-init ctors ^
  --t0 D:\aa-re\cryphysics\unpack_x86\capture\t0\cryphysics.dll.mem ^
  tests\scenarios\box_on_ground.scn > tests\golden-12\box_on_ground.fpu53.trace

echo dumpvars | physoracle_x86.exe --fpu 53 - > tests\golden-12\vars.trace
```

Use `--fpu 24` for the other file. `oracle/trace_diff.py A B` prints the first diverging
step / entity / field.

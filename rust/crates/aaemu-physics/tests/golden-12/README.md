# 1.2 client oracle goldens

Traces produced by the x86 build of `physoracle` from the real 1.2 client
`cryphysics.dll` (unpacked, image base `0x34FF0000`). They are the 1.2 counterpart of the
x64 10.x traces in `../golden/` and use the same scenario language. Every trace is the
scenario run with `--fpu 53 --mxcsr 0x1F80` (the process default MXCSR) after
`--static-init ctors`; the MXCSR default on this machine is `0x1F80` in every run.

Compare with `python ../oracle/trace_diff.py --all` (one line per trace file, first
divergence as step / entity / field). Its counts are per file: scenarios with a `.fpu24`
trace are listed twice, and `vars.trace` is compared with `engine_tables.trace` (its x64
counterpart starts with the same `dumpvars` line). Traces whose `--fpu 24` run differs from
`--fpu 53` also have a `.fpu24.trace`; the other scenarios are byte-identical under both
precisions.

## Results (milestone 2)

| scenario | status | first divergence (`--fpu 53` vs the x64 golden) |
|---|---|---|
| `box_on_ground` | **identical** | — |
| `box_float_300kg` | **identical** | — |
| `box_float_700kg` | **identical** | — |
| `exp_buoyancy_same_step` | **identical** | — |
| `p7_params_queue` | **identical** | — |
| `params_neutral` | **identical** | — |
| `box_float_1500kg` | divergent | step 149 `box` `q[0]`: `af789998` vs `af789996` |
| `box_on_box` | divergent | step 1 `upper` `q[2]`: `3e31d0d5` vs `3e31d0d3` |
| `sphere_drop` | divergent | step 29 `ball` `q[1]`: `3d56433e` vs `3d56433f` |
| `capsule_drop` | divergent | step 23 `cap` `q[0]`: `3eae68ce` vs `3eae68cd` |
| `cylinder_drop` | divergent | step 1 `cyl` `q[1]`: `3ad9edf0` vs `3ad9edef` |
| `p2a1_box_edge_drop` | divergent | step 1 `vertex` `q[0]`: `3ebabaa5` vs `3ebabaa6` |
| `p2a1_box_stack_rotated` | divergent | step 1 `c` `q[0]`: `3dcccccf` vs `3dccccce` |
| `p2a1_capsule_on_box` | divergent | step 1 `across` `q[1]`: `3f3504f4` vs `3f3504f2` |
| `p2a1_cylinder_on_box` | divergent | step 1 `tilted` `q[1]`: `3dcccb20` vs `3dcccb1f` |
| `p2a1_sphere_on_box` | divergent | step 19 `b1` `q[1]`: `3dde5fb9` vs `3dde5fb7` |
| `p7_body_add` | divergent | `peek mix 744` (body `q`/`Ibody` words, 1–6 ulp) |
| `p7_ccd_thin_wall` | divergent | step 2 `p2` `v[0]`: `bf58e75b` vs `bf58e75a` |
| `p7_many_bodies` | divergent | step 5 `m1` `q[0]`: `3d62ce99` vs `3d62ce98` |
| `p7_params_base` | divergent | `ret params st 26`: x64 `1`, x86 `0` (1.2 has no params id 26) |
| `p7_params_rigid_act` | divergent | status 8 after the impulses: `v`/`w` 1 ulp and denormal words |
| `p7_params_rigid` | divergent | status 8 after the simulation params: `v` 6 ulp |
| `p7_ship_float` | divergent | step 61 `ship` `q[0]`: `bc853ab3` vs `bc853ab2` |
| `p7_stack10` | divergent | step 1 `b2` `q[2]`: `3d4cb6fd` vs `3d4cb6fc` |
| `exp_quat_order` | divergent | step 1 committed `q[2]`: `3eb51eb5` vs `3eb51eb4` |
| `exp_simclass_lists` | divergent | step 60 `box` `q[0]`: `2f1b8cfb` vs `2f00618a` (denormal) |
| `engine_tables` | divergent | line 2 `vars`: 1.2 block is 540 B at world+72, x64 is 632 B at +104 |
| `ray_box` | **identical** | — |
| `p2a1_rwi_box` | **identical** | — |
| `p2a4_ray_round` | divergent | hit 0 `ball` `n[0]`: `bf5bdcad` vs `bf5bdcac` (5 of 34 hit lines, 1 ulp) |
| `p2a4_sintab` | divergent | sine table word 510: `3f347690` vs `3f34768f` (1 of 1025) |
| `p7_ray_part` | divergent | step 12 `cart` `q[0]`: `31d7cdaa` vs `31d7cdb2` (denormal) |
| `p7_rays_dynamic` | divergent | step 15 `m1` `q[0]`: `3d62ce99` vs `3d62ce98` (same as p7_many_bodies; 30/160 hit lines inherit it) |
| `ray_heightfield` | divergent | `slant` hit `d`: `41660871` vs `4166086e` (3 ulp; all other hit fields equal) |
| `p1c_drop_terrain_flat` | divergent | step 1 `boxtilt` `q[0]`: `3e066377` vs `3e066376` |
| `p1c_drop_terrain_slope` | divergent | step 1 `boxtilt` `q[0]`: `3e066377` vs `3e066376` |
| `p1c_drop_terrain_bumps` | divergent | step 1 `boxtilt` `q[0]`: `3e066377` vs `3e066376` |
| `p1c_rays_terrain` | divergent | first hit `terrain` `n[0]`: `beea3ce2` vs `beea3ce1` (1 ulp) |
| `p5_particle_bounce` | divergent | step 2 `a` `q[0]`: `bea94cd1` vs `bea94cd0` |
| `p5_particle_carry` | divergent | step 31 `c` `q[1]`: `bebe6403` vs `bebe6402` |
| `p5_particle_collide` | divergent | step 1 hit `b` normal `n[0]`: `b3000000` vs `00000000` (denormal) |
| `p5_particle_flags` | divergent | step 1 `e` `q[0]`: `3e07e97d` vs `3e07e97c` |
| `p5_particle_flight` | divergent | step 0 `b` `q[1]`: `be64f92f` vs `be64f92e` |
| `p5_particle_params` | divergent | step 1 `a` `q[0]`: `3cb8803c` vs `3cb8803d` |
| `p5_particle_rigid` | divergent | step 2 `b` `q[1]`: `3bbd0477` vs `3bbd0475` |
| `p5_particle_slide` | divergent | step 2 `d` `q[3]`: `3f64a32e` vs `3f64a32d` |
| `p5_particle_spawn` | divergent | step 2 `a` `q[2]`: `bf32c5c1` vs `bf32c5c0` |
| `p5_particle_water` | divergent | step 2 `b` `q[0]`: `beab8b4e` vs `beab8b4f` |
| `p5_particle_terrain` | blocked | `geom type=mesh` (trimesh not ported) |
| `p5_living_dims` | **identical** | — |
| `p5_living_jump` | **identical** | — |
| `p5_living_slope` | **identical** | — |
| `p5_living_stairs` | **identical** | — |
| `p5_living_swim` | **identical** | — |
| `p5_living_walk` | **identical** | — |
| `p5_living_bumps` | divergent | step 31 `a` `v[1]`: `3e37749d` vs `3e37749c` |
| `p5_living_net` | divergent | step 101 `a` `q[2]`: `3ec3ef17` vs `3ec3ef15` |
| `p5_living_pair` | divergent | timing: the same push of `c` happens at step 19 on x86 vs step 20 on x64 (identical before; ~1 ulp at shifted steps, e.g. `d.v[0]` `403dd9f8` vs `403dd9f9` at step 19) |
| `p5_living_push` | divergent | timing: first difference is the sleep state at step 33 (`box` `sc` 1 vs 2, `aw` 0 vs 1); the box then follows one step apart |
| `p5_living_ride` | divergent | transient/timing from the impulse step: step 31 `p` `v[0]`: `3f97cb71` vs `3f85429d`; positions then differ by <=1.7 mm |
| `p5_living_rigid` | divergent | transient/timing from the impulse step: step 11 `p` `v[0]`: `3fde1d06` vs `3fc62bc0`; positions then differ by <=5.7 mm |
| `p5_living_terrain` | divergent | step 31 `a` `v[1]`: `bb7f7eac` vs `bb7f7ead` |
| `p5_living_mesh` | blocked | `geom type=mesh` (trimesh not ported) |
| `p4a_buoy_multipart` | **identical** | — |
| `p4a_buoy_basic` | divergent | `ab b3` body `L`/`v`: 1 ulp after ApplyBuoyancy |
| `p4a_buoy_variants` | divergent (fpu53 only) | `ab b1` body `L`: `bc186749` vs `bc184035`; min damping now matches (`3dd97a70`). `--fpu 24` is **bit-identical** to x64, so this is x87/SSE precision in ApplyBuoyancy, not a layout error |
| `p4a_air` | blocked | non-global (geometry) areas not ported |
| `p4a_gather` | blocked | non-global (geometry) areas not ported |
| `p3_areas_big` | blocked | non-global areas / CheckAreas not ported |
| `p3_areas_callback` | blocked | non-global areas / event-11 callback not ported |
| `p3_areas_ellipsoid` | blocked | non-global areas not ported |
| `p3_areas_spline` | blocked | non-global areas not ported |
| `p3q_basic` | blocked | PWI/RTE/CEWP/beam query helpers not ported |
| `p3q_queue_water` | blocked | queued-query/PWI helpers not ported |
| `p3q_stress_a` | blocked | PWI/query helpers not ported |
| `p3q_stress_b` | blocked | PWI/query helpers not ported |
| `p3q_stress_c` | blocked | PWI/query helpers not ported |
| `p5_wheeled_brake` | divergent | step 10 `car` `q[0]`: `2e31f198` vs `2d96a774` (denormal) |
| `p5_wheeled_drive` | divergent | step 4 `car` `q[0]`: `31e9afc1` vs `31e0f77a` (denormal) |
| `p5_wheeled_gears` | divergent | step 10 `car` `q[0]`: `2e31f198` vs `2d96a774` (denormal) |
| `p5_wheeled_params` | divergent | step 10 `car` `q[0]`: `2e31f198` vs `2d96a774` (denormal) |
| `p5_wheeled_platform` | divergent | step 5 `car` `p[2]`: `3fb739f8` vs `3fb739f2` |
| `p5_wheeled_settle` | divergent | car params `dump=96,11`: x64 +132 `3f800000` vs 1.2 `00000000` (no engine-brake field) |
| `p5_wheeled_settle_ray` | divergent | same car params tail difference |
| `p5_wheeled_slope` | divergent | step 10 `car` `p[2]`: `3f5f6b66` vs `3f5f6b65` |
| `p5_wheeled_tank` | divergent | step 10 `car` `q[0]`: `33300c50` vs `3344d206` |
| `p5_wheeled_terrain_bumps` | divergent | step 5 `car` `q[0]`: `39a869fd` vs `39a86a05` |
| `p5_wheeled_terrain_slope` | divergent | step 10 `car` `q[0]`: `3b0a4f1a` vs `3b0a4f22` |
| `p5_wheeled_turn` | divergent | step 10 `car` `q[0]`: `2e31f198` vs `2d96a774` (denormal) |
| `p5_wheeled_mesh` | blocked | `geom type=mesh` (trimesh not ported) |
| `p1c_mesh_gen` | blocked | trimesh `CreateMesh` (geom manager slot 11, x86 `0x35153730`) not ported |
| `p1c_mesh_pair` | blocked | trimesh `CreateMesh` not ported |
| `p1c_mesh_on_meshgrid` | blocked | trimesh `CreateMesh` not ported |
| `p1c_mesh_on_meshobj` | blocked | trimesh `CreateMesh` not ported |
| `p1c_mesh_on_terrain` | blocked | trimesh `CreateMesh` not ported |
| `p1c_drop_meshgrid` | blocked | trimesh `CreateMesh` not ported |
| `p1c_drop_terrainmesh` | blocked | trimesh `CreateMesh` not ported |
| `p1c_drop_bumpy` | blocked | trimesh `CreateMesh` not ported |
| `p1c_drop_cgf` | blocked | trimesh `CreateMesh` / CGF loader not ported |
| `p1c_cgf_rigid` | blocked | trimesh `CreateMesh` / CGF loader not ported |
| `p1c_cgf_terrain` | blocked | trimesh `CreateMesh` / CGF loader not ported |
| `p1c_rays_mesh` | blocked | trimesh `CreateMesh` not ported |
| `p1c_rays_cgf` | blocked | trimesh `CreateMesh` / CGF loader not ported |

Counts (unique scenarios): **15 identical, 61 divergent, 27 blocked, 21 not applicable**
after group 5 triage. The traces were regenerated after the geomparams-override and
GetParams-buffer fixes: `p5_particle_bounce`, `p5_particle_flags`, `p5_particle_slide`,
`p7_body_add`, `p5_wheeled_gears`, `p5_wheeled_settle` and `p5_wheeled_settle_ray` changed
(the corrected fields now match x64, e.g. the `getparams` part flags and the gear-ratio
dumps), but their first divergences are unchanged 1-ulp float differences and the wheeled
denormal/car-tail differences remain, so no status or count changes. The traces were
regenerated again when statuses were marshalled like params (`pe_status_pos` time_back +120
-> +100, `pe_status_living`, `pe_status_wheel`); the eleven wheeled scenarios with status-5
dumps changed (`p5_wheeled_brake`, `drive`, `params`, `platform`, `settle`, `settle_ray`, `slope`,
`tank`, `terrain_bumps`, `terrain_slope`, `turn`) — their status-5 lines now compare the
same fields (except the x64-only wheel pos/q block) instead of a 28-byte-shifted dump, but
their first divergences are unchanged (the earlier physics ulp/denormal difference), so the
counts still do not change. The
`p7_params_rigid` `f120=1` status now reaches the 1.2 engine, but that scenario's current and
previous poses happen to be bit-identical in 1.2, so its trace is unchanged. All scenarios
were run twice; the traces are deterministic. The 21
not-applicable scenarios are the `p1a_*` internal-function
probes (`--gprobe`, `--gphysblob`, protocol/pair-function addresses): they call x64
internal functions directly and have no 1.2 equivalent in this harness.

| not applicable (`p1a_*`) | reason |
|---|---|
| `p1a_basic`, `p1a_buoyancy`, `p1a_drag`, `p1a_float`, `p1a_inside`, `p1a_press`, `p1a_save`, `p1a_entity` | call x64 internal geometry functions (`--gprobe`, `--gphysblob`) at fixed x64 VAs |
| `p1a_closest`, `p1a_closest_crt`, `p1a_closest_cyl` | x64 `FindClosestPoint`/pair probe entry points |
| `p1a_misc`, `p1a_misc_cube`, `p1a_misc_rast`, `p1a_misc_rpos`, `p1a_misc_wire` | x64 internal math/geometry probes |
| `p1a_protocol_*` (5) | x64 protocol/validator probes |

The trimesh blocker: `oracle/p1c_mesh.c` is still `#ifndef ORACLE_X86`. The 1.2 geom
manager `CreateMesh` is slot 11 (`0x35153730`), but its mesh/BV argument layout and the
`CMesh` builder in p1c_mesh.c are x64-derived; CGF additionally needs
`AAEMU_PHYS_CGF_DIR`. Nothing was attempted beyond confirming `unknown geom type mesh`.

Most divergences are 1–6 ulp differences of a rotating body's quaternion/velocity or a
hit normal, consistent with x86-vs-x64 compiler/libm codegen, not harness/layout errors:
the x86 `peek` probes reproduce the x64 body words and the `params_neutral` snapdiffs are
bit-identical. But **not every divergence is a 1–6 ulp float difference**: the genuine
differences are the 1.2 struct/param layouts (params id 26 absent, car engine-brake /
rear-steer fields x64-only), the physics-vars block size, one sine-table word, and the
living pair/push/ride/rigid collision/sleep timing (unproven whether genuinely different
behaviour). See the family summary and the caveats above.

`engine_tables` is listed as divergent because the physics-vars block is laid out
differently in 1.2 (540 B, `dedicated` at +588; x64 632 B). The three size tables and the
eight surface-table defaults are logically identical at the mapped addresses:

| table | x64 address | x86 address | result |
|---|---|---|---|
| params sizes (27 / 26 ints) | `0x3537C640` | `0x35376380` | 1.2 has 26 entries; every entry is the x64 size minus the 32-bit pointer delta (e.g. pos 0x48→0x38, part 0xA0→0x80) |
| action sizes (19 ints) | `0x3537C5F0` | `0x35376330` | same pattern (e.g. impulse 0x50→0x50, add-constraint 0x80→0x74) |
| geomparams sizes (3 ints) | `0x3537C5D8` | `0x3537626C` | `{0x70,0xB8,0x78}` → `{0x58,0xA0,0x5C}` |
| surface defaults (idx 0) | world `+4456/+6504/+8552/+18792/+10600/+12648/+14696/+16744` | world `-0x208` each | all eight values identical (`0`, `3f99999a`, `3f4ccccd`, `0`, …) |

The missing params id 26 is the 10.x "byte flag" param (size 8); 1.2's SetParams dispatch
has no case for it and returns 0 (p7_params_base's first divergence).

## x87 versus SSE

`dynamics.md` §6 says both builds use SSE scalar single precision, so the x87 precision
control word should not matter. That holds for most scenarios, but **not all**:
`capsule_drop`, `p7_many_bodies`, `p7_ship_float`, `p7_ray_part`, `p7_rays_dynamic`,
`p1c_drop_terrain_slope`, `p5_particle_spawn`, `p5_living_pair`, `p4a_buoy_basic` and
`p4a_buoy_variants` differ between `--fpu 24` and `--fpu 53` (e.g. `capsule_drop` step 35
`cap` `v[1]`: `3968136f` vs `396813ed`). Those have both traces committed. The wheeled
family is **not** x87-dependent: its `--fpu 24` and `--fpu 53` traces are byte-identical.
The decompile has 144 `float10` functions, so 1.2 is "SSE in the hot
rigid path, x87 in some helpers", not SSE-only; the docs have been corrected. For the
bit-identical scenarios the 24-bit and 53-bit runs are byte-identical, which is expected
from SSE code and is not by itself evidence of a divergence.

## 1.2 layouts used by the harness

Only the evidence-backed mappings are enabled; an unmapped offset fails loudly instead of
reading the wrong field. Sources are in `cryphysics.c` (x86 1.2, base `0x34FF0000`).

* **params** (converted to/from the x64 image at the vtable call): `pe_params_pos`
  (`FUN_3507b720` case 0; pos 4, q 16, scale 32, mtx3x4 36, mtx3x3 40, simClass 44,
  recalc 48, byte 52), `pe_params_part` (case 6 + GetParams `FUN_3505c460` case 6; two
  pointer pairs at 48/52 and 88/92, mass 76, density 80, minContactDist 84, idmat 96,
  pLattice 100, idSkeleton 104, pMatMapping 108, nMats 112, bAddrefGeoms 120, idParent
  124), `pe_params_outer_entity` (9), `pe_params_foreign_data` (11). `pe_geomparams`
  (`part`) is converted the same way, after the scenario's x64 field overrides are applied
  to the x64 image: mtx3x4 48->44, mtx3x3 56->48, surface 64->52, flags 68->56, collider
  72->60, minContactDist 76->64, idmat 80->68, lattice 88->72, matMapping 96->76, nMats
  104->80, recalcBBox 108->84 (plain 88 B); car type 1 keeps the wheel block at
  112..183 -> 88..159 (160 B), articulated type 2 has id_body 112->88 (92 B). Reverse
  conversion clears only the x64 struct image, so GetParams output buffers written through
  a scenario `buf:` pointer (the wheeled gear ratios at `buf:400`) are preserved. All
  other params used in this group are pointer-free and keep the x64 offsets.
* **entities**: x86 base pos +120, q +132, scale +176, id +56, simclass +59, flags +76,
  parts +236 (pointer), nParts +240, ground planes +256, structure +272; rigid body at
  entity+532 (`GetRigidBody` 0x350b6e40) with the pending pose at +276 (`ctor 0x350b69a0`
  sets `+0x2d` = `+0x114`), deferred P/L at +740, gravity at +412, `bAwake` bits +336.
  `peek`/`snapdiff`/`poke` scenario offsets stay x64 and are mapped at the command.
* **world**: simclass list heads x86 +628+4k with next at entity+0x54 (x64 +664+8k,
  next +104); the two extra heads +676/+680 correspond to x64 +760/+816. Surface tables
  x86 +0xF60…, x64 +0x1168… (offset delta `-0x208`).
* **rays**: x86 `SRWIParams` is 64 B (`FUN_351C2B00`): +0 foreign data (2 words) +
  iForeignData, org +12, dir +24, objtypes +36, flags +40, hits* +44, nMaxHits +48,
  phitLast +52, nSkipEnts +56, pSkipEnts +60 (the zero-direction test reads +24..+32 and
  the queued copy +0/+4/+8, +12..+20, +24..+32, +36, +40, +44, +48, +52, +56, +60).
  The `ray_hit` record is 64 B: dist +0, pCollider +4, ipart +8, partid +12, int16
  +16/+18, surface +20, +24, pt +28, n +40, order +52, iPrim +56, pool ptr +60 (hit
  initialiser `hits + 0x10` floats, `local_24[5]` = surface, `local_24[7..9]` = pt,
  `local_24[0xE]` = iPrim). The quarter-wave sine table moved from x64 `0x3537B450` to
  x86 `0x35375268` (written by 0x351F42D0).
* **heightfield terrain**: `primitives::heightfield` keeps the x64 offsets through
  `+0x60` (Basis 0, center 40, step 52, invstep 60, size 68, stride 76, width 80, hscale
  84, typemask 88, heightmask 90, typehole 92, `+96`) because no pointer precedes them;
  the two callbacks move from +104/+112 (8-byte) to +100/+104 (4-byte), so the 1.2
  primitive is 108 B (FUN_3515CE70/FUN_3510CCA0 copy +0x34..+0x68, geom-manager
  CreatePrimitive case 2 0x351547B0). `SetHeightfieldData` (0x35093C00) creates five
  terrain entities at world+0x52C+4*i (x64 world+1568+8*i) and stores the caller's
  basis/center at world+0x544..+0x570.
* **particles**: `pe_params_particle` (id 3) keeps the x64 offsets through minVel +136;
  because the x64 build pads the pointer to +144, the 1.2 pointer `pColliderToIgnore`
  sits at +140 and the tail is pierceability +144, collTypes +148, areaCheckPeriod +152
  (size 156; CParticleEntity::SetParams 0x35041F80 reads `param_2[0x23]..[0x26]`).
  The `var off=` translation uses the physics-vars block shift (x64 104->x86 72;
  x64 220 onwards -> x86 -24).
* **living**: `player_dimensions` (id 1, 64 B) and `action_move` (id 1, 24 B) are
  pointer-free and keep the x64 layout; `player dynamics` (id 4) is identical through
  `collTypes +72`, then `pColliderToIgnore` is +76 (x64 +80) and `bActive` +84 (x64 +92)
  (CLivingEntity::SetParams 0x3502F5D0). `pe_status_living` (id 2) is emitted in a
  different order by `FUN_35030500`: x86 +4..83 -> x64 +4..83, +84 -> +88, +88..119 ->
  +92..123, x64 +124 has no x86 word, +120..135 -> +128..143, +136 -> +144, +140 (ptr)
  -> +152, +144 -> +164. `pe_status_check_stance` (21) is pointer-free and matches.
  With that mapping six living scenarios (walk/dims/jump/slope/stairs/swim) are
  bit-identical; `p5_living_stairs` crashed once in seven runs at a heap VA
  (`0x6a071250`, step 0) and is deterministic in the successful runs.
* **buoyancy probes**: 1.2 `CRigidEntity::GatherBuoyancy` is `0x350CE9B0` and
  `ApplyBuoyancy` is `0x350D1D80` (entity in ECX, dt/buffers on the stack). The 1.2
  `global_buoyancy` record is 76 B (x64 80 B), so the trace packs each record to 80 B and
  pads +76 with the x64 sentinel `ffbfffff`. `p3_print_state` maps the rigid body fields
  (772->560, 784->572, 796->584, 808->596, 956->740, 968->752), damping 684->480,
  dampingFreefall 688->484, min damping 692->488, submerged fraction 1136->908, the flag
  word 504->332, minAwakeTime 608->404, timeIdle 252->212, simclass 71->59, awake bits
  508->336 and the last-contact step 1104->884. Evidence: rigid SetParams 0x350D4920 maps
  sim +16 damping to E+0x1E0 (480) and sim +32 dampingFreefall to E+0x1E4 (484), and
  ApplyBuoyancy `0x350D1D80` blends with E+0x1E0 at ~98431. The same -204 shift gives
  692->488. The 1.2 ctor initialises E+480 (damping) to **0**, not the x64 default 0.1
  (`cryphysics.c` ~84568; x64 default per entities.md), while freefall damping is 0.1.
  `p4a_buoy_variants` pokes +684=0.05, now correctly written to E+480; `--fpu 24` matches
  x64 bit-for-bit, `--fpu 53` differs only in the x87 ApplyBuoyancy arithmetic.
* **wheeled**: `pe_params_wheel` (16, 80 B) is identical to x64. Car params (2) has no
  x64 +12 field in 1.2, so x64 +16..+87 map to +12..+83, the gear-ratio pointer is at
  +84 (x64 +88), and the tail +96..+131 maps to +88..+123 (size 124; SetParams
  `0x35116CD0`, GetParams `0x35113120`). The x64 +132 engine-brake / +136 rear-steer
  fields are absent, which is the first difference in the settle scenarios. Wheeled
  `part` records are geomparams type 1: the x64 wheel block at +112..+183 moves to the
  1.2 +88..+159 in the geomparams conversion (AddGeometry `0x35119FC0` reads bDriving at
  `param_3[0x16]`). Vehicle status 4 keeps the x64 layout. Wheel status 5 is marshalled
  (`0x351184D0`): 1.2 drops the x64 wheel pos/q block at +16..43, so x64 +44..127 map to
  x86 +16..99, the x64 contact-entity pointer +128 becomes x86 +100 (zeroed before the
  reverse conversion because it is process-specific) and mass share x64 +136 becomes x86
  +104. The rest of the x64 tail is present shifted: brake torque/steer/rotation
  (+116..+124) are x86 +88..96. Only the x64 wheel pos/q block at +16..43 has no 1.2
  counterpart, so those words are zero in the 1.2 traces: a genuine layout difference,
  not an ulp one. The status-5 dumps now compare the same fields in both traces except
  that x64-only block; the old raw dump was shifted by 28 bytes.

## Regenerate (runtime preparation: `oracle/README.md`)

```
python oracle/patch_entry.py
oracle/build_x86.bat
set AAEMU_PHYS_RT=D:\aaemu-target\physics\oracle-x86\rt
physoracle_x86.exe --fpu 53 --mxcsr 0x1F80 --static-init ctors ^
  --t0 D:\aa-re\cryphysics\unpack_x86\capture\t0\cryphysics.dll.mem ^
  tests\scenarios\box_on_ground.scn > tests\golden-12\box_on_ground.fpu53.trace

echo dumpvars | physoracle_x86.exe --fpu 53 --mxcsr 0x1F80 - > tests\golden-12\vars.trace
python oracle/trace_diff.py --all
```

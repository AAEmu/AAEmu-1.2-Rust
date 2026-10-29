/* physoracle: scenario-driven oracle for the ArcheAge 10.x zone-server physics.
 *
 * Loads the real x64 `cryphysics_dedicate.dll` (10.0.2.13) standalone, drives it through its
 * vtables and prints a bit-exact trace (every float as its raw IEEE-754 bits). The Rust port
 * (`aaemu-physics`) diffs against these traces.
 *
 *   physoracle.exe [--rt DIR] SCENARIO.txt|-        run a scenario, trace on stdout
 *   physoracle.exe [--rt DIR] --math NAME COUNT SEED   probe an internal math routine
 *   physoracle.exe [--rt DIR] --p2b                    P2b unprojection probe (p2b_probe.c)
 *   physoracle.exe [--rt DIR] --p4b                    P4b contact solver probe (p4b_probe_solver.c)
 *   physoracle.exe [--rt DIR] --pairfn VA NA NB FILE   call an INTERSECT pair function per line
 *
 * DIR defaults to %AAEMU_PHYS_RT% or D:\aa-re\cryphysics\harness\rt. It must hold
 * cryphysics_dedicate.dll, MSVCR100.dll, MSVCP100.dll, tbb.dll, xldiag.dll, xlcommon.dll (the
 * *original* xlcommon, not the r575 forwarding proxy) and the tbbmalloc_proxy stub (tbbstub.c).
 *
 * Every slot number, struct offset and internal address used here comes from the RE notes in
 * rust/docs/physics/ (derived from the shipped binary only). Scenario syntax: see README.md of
 * the aaemu-physics crate ("Oracle").
 *
 * Single-threaded, deterministic: two runs of the same scenario give identical output. Pointers
 * are never printed (entities are printed by scenario name).
 */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <io.h>
#include <fcntl.h>
#include <math.h>
#include <float.h>

#ifdef ORACLE_X86
/* 1.2 client cryphysics.dll, unpacked image (see CRYPHYSICS-MAP.md s9b). */
#define IMAGE_BASE 0x34FF0000u
#else
#define IMAGE_BASE 0x35000000ULL
#endif
#define VA(h, va) ((void *)((char *)(h) + ((va) - IMAGE_BASE)))
#define VT(obj) (*(void ***)(obj))

/* ---- IPhysicalWorld slots ---- */
#ifdef ORACLE_X86
/* x64 -> x86 shifts from CRYPHYSICS-MAP.md s9b / func_match.csv; the medium-confidence slots
   (18, 22, 43, 80) were verified against the vtable at 0x351FC598 and the x86 decompile. */
enum {
  W_GetGeomManager = 10, W_SetupEntityGrid = 12, W_GetPhysVars = 21, W_CreatePhysicalEntity = 22,
  W_DestroyPhysicalEntity = 25, W_TimeStep = 33, W_RayWorldIntersection = 43, W_AddGlobalArea = 80,
  W_SetHeightfieldData = 18, W_SetSurfaceParameters = 30
};
#else
/* x64 10.x, CRYPHYSICS-MAP.md section 4 */
enum {
  W_GetGeomManager = 9, W_SetupEntityGrid = 11, W_GetPhysVars = 20, W_CreatePhysicalEntity = 21,
  W_DestroyPhysicalEntity = 24, W_TimeStep = 32, W_RayWorldIntersection = 42, W_AddGlobalArea = 80,
  W_SetHeightfieldData = 17, W_SetSurfaceParameters = 29
};
#endif
/* ---- IGeomManager slots (same in both builds) ---- */
enum { G_CreatePrimitive = 7, G_RegisterGeometry = 9 };
/* ---- IPhysicalEntity slots (same in both builds) ---- */
enum { E_SetParams = 7, E_GetParams = 8, E_GetStatus = 9, E_Action = 10, E_AddGeometry = 11 };

#define UNUSED_F_BITS 0xFFBFFFFFu
#define UNUSED_I 0x80000000u

#ifdef ORACLE_X86
/* x86 vtable calls are __thiscall: `this` in ECX and every declared argument on the stack.
   __fastcall gives ECX = first parameter, EDX = second, the rest on the stack, so a dummy
   second parameter reproduces __thiscall exactly (callee pops the stack args). */
typedef void *(__fastcall *fn_p)(void *, void *);
typedef void *(__fastcall *fn_prim)(void *, void *, int, void *);
typedef void *(__fastcall *fn_reg)(void *, void *, void *, int, int *, int);
typedef int (__fastcall *fn_setp)(void *, void *, void *, int);
typedef int (__fastcall *fn_stat)(void *, void *, void *);
typedef int (__fastcall *fn_addg)(void *, void *, void *, void *, int, int);
typedef int (__fastcall *fn_rwi)(void *, void *, void *, const char *, int);
typedef int (__fastcall *fn_surf)(void *, void *, int, float, float, unsigned);
typedef void (__fastcall *fn_hfdata)(void *, void *, void *, int *, int);
typedef void (__fastcall *fn_timestep)(void *, void *, float, int);
typedef void (__fastcall *fn_grid)(void *, void *, int, float, float, float, int, int, float, float, int);
typedef void *(__fastcall *fn_create)(void *, void *, int, float, void *, void *, int, int, void *);
/* direct (non-vtable) entry points */
typedef void *(__cdecl *fn_alloc)(int);
typedef void *(__fastcall *fn_worldctor)(void *);
typedef void (__cdecl *fn_void0)(void);
typedef void (__fastcall *fn_this0)(void *, void *);
typedef void (__fastcall *fn_this1)(void *, void *, int);
#else
typedef void *(*fn_p)(void *);
typedef void (*fn_grid)(void *, int, float *, int, int, float, float, int, int);
typedef void *(*fn_create)(void *, int, float, void *, void *, int, int, void *, unsigned);
typedef int (*fn_timestep)(void *, float, int);
typedef void *(*fn_prim)(void *, int, void *);
typedef void *(*fn_reg)(void *, void *, int, int *, int);
typedef int (*fn_setp)(void *, void *, int);
typedef int (*fn_stat)(void *, void *);
typedef int (*fn_addg)(void *, void *, void *, int, int);
typedef int (*fn_rwi)(void *, void *, const char *, int);
typedef void (*fn_hfdata)(void *, void *, int *, int);
typedef int (*fn_surf)(void *, int, float, float, unsigned);
#endif

/* Vtable wrappers shared by the scenario interpreter. */
static void *m_call0(void *o, int slot) {
#ifdef ORACLE_X86
  return ((fn_p)VT(o)[slot])(o, 0);
#else
  return ((fn_p)VT(o)[slot])(o);
#endif
}
static int m_status(void *o, int slot, void *b) {
#ifdef ORACLE_X86
  return ((fn_stat)VT(o)[slot])(o, 0, b);
#else
  return ((fn_stat)VT(o)[slot])(o, b);
#endif
}
static int m_setp(void *o, int slot, void *b, int ts) {
#ifdef ORACLE_X86
  return ((fn_setp)VT(o)[slot])(o, 0, b, ts);
#else
  return ((fn_setp)VT(o)[slot])(o, b, ts);
#endif
}
static int m_addg(void *o, int slot, void *g, void *gp, int id, int recalc) {
#ifdef ORACLE_X86
  return ((fn_addg)VT(o)[slot])(o, 0, g, gp, id, recalc);
#else
  return ((fn_addg)VT(o)[slot])(o, g, gp, id, recalc);
#endif
}
static void *m_create(void *o, int type, void *pp, int id) {
#ifdef ORACLE_X86
  if (getenv("AAEMU_PHYS_DEBUG")) fprintf(stderr, "m_create type=%d pp=%p id=%d\n", type, pp, id);
  void *r = ((fn_create)VT(o)[W_CreatePhysicalEntity])(o, 0, type, 0.0f, pp, 0, 0, id, 0);
  if (getenv("AAEMU_PHYS_DEBUG")) fprintf(stderr, "m_create -> %p vtable %p\n", r, r ? *(void **)r : 0);
  return r;
#else
  return ((fn_create)VT(o)[W_CreatePhysicalEntity])(o, type, 0.0f, pp, 0, 0, id, 0, 0);
#endif
}
static void m_grid(void *o, int axisz, const float *org, int nx, int ny, float sx, float sy, int pod, int cyclic) {
#ifdef ORACLE_X86
  ((fn_grid)VT(o)[W_SetupEntityGrid])(o, 0, axisz, org[0], org[1], org[2], nx, ny, sx, sy, cyclic);
#else
  ((fn_grid)VT(o)[W_SetupEntityGrid])(o, axisz, (float *)org, nx, ny, sx, sy, pod, cyclic);
#endif
}
static void m_step(void *o, float dt, int flags) {
#ifdef ORACLE_X86
  ((fn_timestep)VT(o)[W_TimeStep])(o, 0, dt, flags);
#else
  ((fn_timestep)VT(o)[W_TimeStep])(o, dt, flags);
#endif
}
static int m_rwi(void *o, void *p, const char *name, int caller) {
#ifdef ORACLE_X86
  return ((fn_rwi)VT(o)[W_RayWorldIntersection])(o, 0, p, name, caller);
#else
  return ((fn_rwi)VT(o)[W_RayWorldIntersection])(o, p, name, caller);
#endif
}
static int m_surf(void *o, int idx, float bounce, float friction, unsigned flags) {
#ifdef ORACLE_X86
  return ((fn_surf)VT(o)[W_SetSurfaceParameters])(o, 0, idx, bounce, friction, flags);
#else
  return ((fn_surf)VT(o)[W_SetSurfaceParameters])(o, idx, bounce, friction, flags);
#endif
}
static void m_hfdata(void *o, void *prim, int *mm, int nm) {
#ifdef ORACLE_X86
  ((fn_hfdata)VT(o)[W_SetHeightfieldData])(o, 0, prim, mm, nm);
#else
  ((fn_hfdata)VT(o)[W_SetHeightfieldData])(o, prim, mm, nm);
#endif
}
static void *m_prim(void *gm, int type, void *prim) {
#ifdef ORACLE_X86
  return ((fn_prim)VT(gm)[G_CreatePrimitive])(gm, 0, type, prim);
#else
  return ((fn_prim)VT(gm)[G_CreatePrimitive])(gm, type, prim);
#endif
}
static void *m_reg(void *gm, void *g) {
#ifdef ORACLE_X86
  return ((fn_reg)VT(gm)[G_RegisterGeometry])(gm, 0, g, 0, 0, 0);
#else
  return ((fn_reg)VT(gm)[G_RegisterGeometry])(gm, g, 0, 0, 0);
#endif
}

static HMODULE g_phys;
static void *g_world, *g_gm;
static int g_step;
static uint32_t g_snap[4096];
static int g_snap_n;
static void *g_snap_obj;

/* ------------------------------------------------------------------ fake gEnv */
typedef struct NullObj { void **vt; char pad[4096]; } NullObj;
static NullObj g_null, g_cfg;
static void *g_nullvt[1024], *g_cfgvt[1024];
#ifdef ORACLE_X86
/* x86 vtable calls are __thiscall: the callee pops the stack arguments.  A cdecl fake would
   leave them on the stack and corrupt the caller (the profiler label ctor calls slot 1 with
   one argument).  __fastcall with a dummy EDX reproduces __thiscall and pops what it declares. */
static void *__fastcall nullfn(void *self, void *dummy) { (void)self; (void)dummy; return &g_null; }
static void *__fastcall nullfn1(void *self, void *dummy, void *a1) { (void)self; (void)dummy; (void)a1; return &g_null; }
static void *__fastcall nullfn2(void *self, void *dummy, void *a1, void *a2) { (void)self; (void)dummy; (void)a1; (void)a2; return &g_null; }
#else
static void *nullfn(void *self) { (void)self; return &g_null; }
#endif
/* x64: gEnv+112 -> slot 20 -> config block, ray limit at +248. x86 1.2: gEnv+0x38 ->
   slot 21 -> config block, ray limit at +0x100 (FUN_35131540/I+0x54). The object returns
   itself, so the int lives inside g_cfg. */
#ifdef ORACLE_X86
static void *__fastcall cfgfn(void *self, void *dummy) { (void)self; (void)dummy; return &g_cfg; }
#else
static void *cfgfn(void *self) { (void)self; return &g_cfg; }
#endif
static struct { void *ptrs[64]; unsigned char bytes[4096]; } g_env;
#ifdef ORACLE_X86
#define ENV_OFF_CONFIG 0x38
#define ENV_OFF_RAYMAX 0x100
#define ENV_OFF_BYPASS 0x95 /* DrawRayOnGrid bypass (x64: +289) */
#define ENV_OFF_B2 0x96     /* copied into world+0x130 (x64: +290) */
#define ENV_OFF_PROFILER 0x97
#else
#define ENV_OFF_CONFIG 112
#define ENV_OFF_RAYMAX 248
#define ENV_OFF_BYPASS 289
#define ENV_OFF_B2 290
#define ENV_OFF_PROFILER 0 /* x64 uses byte 289/290 and object pointers; no profiler byte */
#endif
static void install_fake_env(HMODULE h) {
  for (int i = 0; i < 1024; i++) g_nullvt[i] = (void *)nullfn;
#ifdef ORACLE_X86
  g_nullvt[1] = (void *)nullfn1;
  g_nullvt[2] = (void *)nullfn2;
#endif
  g_null.vt = g_nullvt;
  for (int i = 0; i < 1024; i++) g_cfgvt[i] = (void *)cfgfn;
  g_cfg.vt = g_cfgvt;
  *(int *)((char *)&g_cfg + ENV_OFF_RAYMAX) = 1000; /* default ray grid limit, see "env raymax=" */
  memset(&g_env, 0, sizeof g_env);
#ifdef ORACLE_X86
  *(void **)((char *)&g_env + 0x00) = &g_null; /* +0 ISystem* (profiler label ctor) */
  *(void **)((char *)&g_env + ENV_OFF_CONFIG) = &g_cfg;
  ((unsigned char *)&g_env)[ENV_OFF_PROFILER] = 0;
  *(void **)VA(h, 0x3537640C) = &g_env;
#else
  g_env.ptrs[0] = &g_null;        /* +0   ISystem* */
  g_env.ptrs[112 / 8] = &g_cfg;   /* +112 config (ray grid limit at slot-20 result +248) */
  g_env.ptrs[152 / 8] = &g_null;  /* +152 log */
  g_env.ptrs[160 / 8] = &g_null;  /* +160 slot-19 void call in TimeStep */
  *(void **)VA(h, 0x3537C6D8ULL) = &g_env;
#endif
}

static void die(const char *msg, const char *arg);

#ifdef ORACLE_X86
/* ------------------------------------------------------------------ 1.2 static init
 * The unpacked image's .data is the t0 capture (post-CRT-init, pre-world), so it still holds
 * the client process's TLS index, per-thread world table, a few heap pointers and guard bits.
 * Route "ctors" restores .data from the (unpacked) file, zeroes those process-specific globals,
 * runs the 35 CRT static ctors from the XC table and diffs the result against t0.  Route "tls"
 * only allocates a TLS slot (the quick fix in the study).  Diagnostics go to stderr so stdout
 * stays a clean trace. */
#define X86_TLS_COUNT 0x3520D810u
#define X86_TLS_INDEX 0x3520D814u
#define X86_TLS_TABLE 0x3520D818u
#define X86_TLS_TABLE_N 64
#define X86_GUARD 0x35219930u
#define X86_HEAP1 0x35208D3Cu
#define X86_HEAP2 0x35252BC0u
#define X86_XC_TABLE 0x351FA18Cu
#define X86_XC_COUNT 35
#define X86_TLSALLOC_CTOR 0x351F8E30u
#define X86_WORLD_ALLOC 0x351F3BE0u
#define X86_WORLD_CTOR 0x350889B0u
#define X86_DEDICATED_FLAG 0x3523F4E8u
#define X86_DATA_RVA 0x214000u
#define X86_DATA_VSZ 0x172848u

static int g_static_route; /* 0 none, 1 tls, 2 ctors */
static unsigned g_tls_index = 0xFFFFFFFFu;

static int known_runtime_word(unsigned va) {
  if (va >= X86_TLS_COUNT && va < X86_TLS_TABLE + 4 * X86_TLS_TABLE_N) return 1;
  if (va == X86_HEAP1 || va == X86_HEAP2 || va == X86_GUARD) return 1;
  /* allocator bookkeeping written by FUN_351F3BE0's init (CryMalloc wrapper) */
  if (va >= 0x353763E0u && va < 0x35376410u) return 1;
  return 0;
}

/* Route "ctors" re-runs the two ctors that also call _atexit without the atexit half; the
   first pass over the captured .data leaves these three initialization words different
   (a guard bit and two self-pointers).  They are not physics state. */
static int init_only_word(unsigned va) {
  return va == 0x35208D18u || va == 0x35208E60u || va == 0x35376424u;
}

static void restore_data_from_file(HMODULE h) {
  char path[MAX_PATH];
  if (!GetModuleFileNameA(h, path, sizeof path)) die("GetModuleFileName failed", 0);
  FILE *f = fopen(path, "rb");
  if (!f) die("cannot reopen the DLL for .data", path);
  unsigned char *file;
  long sz;
  fseek(f, 0, SEEK_END);
  sz = ftell(f);
  fseek(f, 0, SEEK_SET);
  file = (unsigned char *)malloc((size_t)sz);
  if (!file || fread(file, 1, (size_t)sz, f) != (size_t)sz) die("cannot read the DLL", path);
  fclose(f);
  unsigned char *base = (unsigned char *)h;
  unsigned e = *(unsigned *)(base + 0x3C);
  unsigned nsec = *(unsigned short *)(base + e + 6);
  unsigned so = *(unsigned short *)(base + e + 20);
  unsigned opt = e + 24;
  for (unsigned i = 0; i < nsec; i++) {
    unsigned char *sh = base + opt + so + 40 * i;
    if (!memcmp(sh, ".data", 5)) {
      unsigned vsz = *(unsigned *)(sh + 8), va = *(unsigned *)(sh + 12);
      unsigned rsz = *(unsigned *)(sh + 16), ra = *(unsigned *)(sh + 20);
      unsigned char *dst = base + va;
      unsigned copy = rsz < vsz ? rsz : vsz;
      memcpy(dst, file + ra, copy);
      if (copy < vsz) memset(dst + copy, 0, vsz - copy);
      free(file);
      return;
    }
  }
  free(file);
  die(".data section not found", 0);
}

static void x86_static_init(const char *route, const char *t0_path) {
  unsigned char *base = (unsigned char *)g_phys;
  unsigned char *t0 = 0;
  long t0_sz = 0;
  if (!strcmp(route, "none")) {
    fprintf(stderr, "static-init route=none tls=%u\n", *(unsigned *)VA(g_phys, X86_TLS_INDEX));
    return;
  }
  if (!strcmp(route, "ctors")) {
    restore_data_from_file(g_phys);
  }
  if (t0_path && *t0_path) {
    FILE *f = fopen(t0_path, "rb");
    if (f) {
      fseek(f, 0, SEEK_END);
      t0_sz = ftell(f);
      fseek(f, 0, SEEK_SET);
      t0 = (unsigned char *)malloc((size_t)t0_sz);
      if (t0 && fread(t0, 1, (size_t)t0_sz, f) != (size_t)t0_sz) {
        free(t0);
        t0 = 0;
      }
      fclose(f);
    }
    if (!t0) fprintf(stderr, "static-init t0 capture %s not readable\n", t0_path);
  }
  unsigned char *before = (unsigned char *)malloc(X86_DATA_VSZ);
  if (!before) die("out of memory", 0);
  memcpy(before, base + X86_DATA_RVA, X86_DATA_VSZ);

  *(unsigned *)VA(g_phys, X86_TLS_COUNT) = 0;
  *(unsigned *)VA(g_phys, X86_TLS_INDEX) = 0;
  memset(VA(g_phys, X86_TLS_TABLE), 0, 4 * X86_TLS_TABLE_N);
  *(unsigned *)VA(g_phys, X86_GUARD) = 0;
  *(unsigned *)VA(g_phys, X86_HEAP1) = 0;
  *(unsigned *)VA(g_phys, X86_HEAP2) = 0;

  int ctors = 0;
  if (!strcmp(route, "ctors")) {
    unsigned *table = (unsigned *)VA(g_phys, X86_XC_TABLE);
    for (int i = 0; i < X86_XC_COUNT; i++) {
      unsigned fn = table[i];
      if (fn < 0x34FF1000u || fn >= 0x34FF1000u + 0x208400u) {
        fprintf(stderr, "static-init bad XC[%d]=%#x\n", i, fn);
        continue;
      }
      if (getenv("AAEMU_PHYS_CTOR_TRACE")) fprintf(stderr, "ctor %d %#x\n", i, fn);
      /* Four ctors end in _atexit().  The captured .data is post-CRT-init of the client
         process, so its per-module atexit list points into the client's heap; re-running
         _atexit here corrupts our heap (STATUS_HEAP_CORRUPTED inside _onexit).  Do the
         non-atexit work by hand and skip the process-exit registration. */
      if (fn == 0x351F8DB0u) { ctors++; continue; } /* only _atexit(...) */
      if (fn == 0x351F8E00u) { /* FUN_35059590(&DAT_35208CE0, 0) then _atexit */
        ((fn_this1)(base + (0x35059590u - IMAGE_BASE)))(VA(g_phys, 0x35208CE0u), 0, 0);
        ctors++;
        continue;
      }
      if (fn == 0x351F91A0u) { /* FUN_351F4A10(&DAT_35376414) then _atexit */
        ((fn_this0)(base + (0x351F4A10u - IMAGE_BASE)))(VA(g_phys, 0x35376414u), 0);
        ctors++;
        continue;
      }
      if (fn == 0x351F9240u) { /* DAT_35376474 = 1 then _atexit */
        *(unsigned char *)VA(g_phys, 0x35376474u) = 1;
        ctors++;
        continue;
      }
      ((fn_void0)(base + (fn - IMAGE_BASE)))();
      ctors++;
    }
  }
  /* route tls, and also the first ctor when the table was unavailable */
  if (ctors == 0) {
    unsigned fn = X86_TLSALLOC_CTOR;
    ((fn_void0)(base + (fn - IMAGE_BASE)))();
  }
  g_tls_index = *(unsigned *)VA(g_phys, X86_TLS_INDEX);
  g_static_route = ctors ? 2 : 1;

  unsigned diffs = 0, known = 0, init_only = 0, unknown = 0;
  unsigned char *after = base + X86_DATA_RVA;
  for (unsigned off = 0; off < X86_DATA_VSZ; off += 4) {
    if (!memcmp(after + off, before + off, 4)) continue;
    unsigned va = IMAGE_BASE + X86_DATA_RVA + off;
    unsigned o = *(unsigned *)(before + off), n = *(unsigned *)(after + off);
    diffs++;
    int k = known_runtime_word(va);
    if (k) known++;
    else if (init_only_word(va)) init_only++;
    else {
      unknown++;
      if (unknown <= 16) fprintf(stderr, "static-init unexpected diff %#x: %08x -> %08x\n", va, o, n);
    }
    (void)o;
    (void)n;
  }
  unsigned t0_diffs = 0xFFFFFFFFu, t0_unknown = 0xFFFFFFFFu, t0_init_only = 0;
  if (t0 && t0_sz >= (long)(X86_DATA_RVA + X86_DATA_VSZ)) {
    t0_diffs = 0;
    t0_unknown = 0;
    unsigned char *r = t0 + X86_DATA_RVA;
    for (unsigned off = 0; off < X86_DATA_VSZ; off += 4) {
      if (!memcmp(after + off, r + off, 4)) continue;
      t0_diffs++;
      if (known_runtime_word(IMAGE_BASE + X86_DATA_RVA + off)) {
      } else if (init_only_word(IMAGE_BASE + X86_DATA_RVA + off)) {
        t0_init_only++;
      } else {
        t0_unknown++;
        if (t0_unknown <= 16)
          fprintf(stderr, "static-init t0 diff %#x: t0=%08x now=%08x\n", IMAGE_BASE + X86_DATA_RVA + off,
                  *(unsigned *)(r + off), *(unsigned *)(after + off));
      }
    }
  }
  fprintf(stderr, "static-init route=%s ctors=%d tls=%u data-diffs-vs-file=%u known=%u init-only=%u unknown=%u",
          route, ctors, g_tls_index, diffs, known, init_only, unknown);
  if (t0) fprintf(stderr, " t0-diffs=%u t0-init-only=%u t0-unknown=%u", t0_diffs, t0_init_only, t0_unknown);
  fprintf(stderr, "\n");
  free(before);
  free(t0);
}

static void *x86_create_world(void) {
  *(unsigned *)VA(g_phys, X86_DEDICATED_FLAG) = 0; /* client behaviour (dedicated-like flag) */
  void *w = ((fn_alloc)((char *)g_phys + (X86_WORLD_ALLOC - IMAGE_BASE)))(0x53F0);
  if (!w) die("world allocation failed", 0);
  void *r = ((fn_worldctor)((char *)g_phys + (X86_WORLD_CTOR - IMAGE_BASE)))(w);
  if (r != w) fprintf(stderr, "world-ctor returned %p for %p\n", r, w);
  return w;
}

static void set_fpu_precision(int bits) {
  unsigned short cw = 0;
  if (bits == 24) _control87(_PC_24, _MCW_PC);
  else if (bits == 53) _control87(_PC_53, _MCW_PC);
  else if (bits == 64) _control87(_PC_64, _MCW_PC);
  _control87(_RC_NEAR, _MCW_RC);
  __asm fnstcw cw;
  fprintf(stderr, "fpu precision=%d x87-control-word=%#06x\n", bits, (unsigned)cw);
}
#endif /* ORACLE_X86 */

static LONG CALLBACK veh(PEXCEPTION_POINTERS e) {
#ifdef ORACLE_X86
  DWORD64 ip = e->ContextRecord->Eip;
  fprintf(stderr, "x86 exception eip=%08lx esp=%08lx ebp=%08lx ret=%08lx eax=%08lx ecx=%08lx\n",
          (unsigned long)e->ContextRecord->Eip, (unsigned long)e->ContextRecord->Esp,
          (unsigned long)e->ContextRecord->Ebp,
          (unsigned long)(e->ContextRecord->Esp ? *(unsigned long *)e->ContextRecord->Esp : 0),
          (unsigned long)e->ContextRecord->Eax, (unsigned long)e->ContextRecord->Ecx);
#else
  DWORD64 ip = e->ContextRecord->Rip;
#endif
  if (e->ExceptionRecord->ExceptionCode == 0x406D1388) return EXCEPTION_CONTINUE_SEARCH; /* thread naming */
  printf("EXCEPTION %08lx at cryphysics VA %llx addr %llx (step %d)\n", e->ExceptionRecord->ExceptionCode,
         (unsigned long long)(ip - (DWORD64)g_phys + IMAGE_BASE),
         (unsigned long long)e->ExceptionRecord->ExceptionInformation[1], g_step);
  fflush(stdout);
  ExitProcess(3);
  return EXCEPTION_CONTINUE_SEARCH;
}

/* ------------------------------------------------------------------ helpers */
static uint32_t fbits(float f) { uint32_t u; memcpy(&u, &f, 4); return u; }
static float bitsf(uint32_t u) { float f; memcpy(&f, &u, 4); return f; }
static void die(const char *msg, const char *arg) {
  printf("ERROR %s %s\n", msg, arg ? arg : "");
  fflush(stdout);
  exit(2);
}
static void out_f(const float *v, int n) {
  for (int i = 0; i < n; i++) printf(" %08x", fbits(v[i]));
}

/* value parsing: floats accept decimal, "U" (unused sentinel) or "h:XXXXXXXX" (raw bits) */
static float parse_f(const char *s) {
  if (!strcmp(s, "U")) return bitsf(UNUSED_F_BITS);
  if (!strncmp(s, "h:", 2)) return bitsf((uint32_t)strtoul(s + 2, 0, 16));
  return (float)strtod(s, 0);
}
static int parse_fv(const char *s, float *out, int maxn) {
  int n = 0;
  char buf[512];
  strncpy(buf, s, sizeof buf - 1);
  buf[sizeof buf - 1] = 0;
  for (char *t = strtok(buf, ","); t && n < maxn; t = strtok(0, ",")) out[n++] = parse_f(t);
  return n;
}
static uint32_t parse_u(const char *s) {
  if (!strcmp(s, "U")) return UNUSED_I;
  return (uint32_t)strtoll(s, 0, 0);
}

/* ------------------------------------------------------------------ named objects */
#define MAXN 1024
typedef struct { char name[32]; void *p; int kind; /* 0 geom(phys_geometry*), 1 entity, 2 area */ } Named;
static Named g_obj[MAXN];
static int g_nobj;
static void *find_obj(const char *name, int kind) {
  for (int i = 0; i < g_nobj; i++)
    if (g_obj[i].kind == kind && !strcmp(g_obj[i].name, name)) return g_obj[i].p;
  return 0;
}
static void *find_target(const char *name) {
  void *p = find_obj(name, 1);
  if (!p) p = find_obj(name, 2);
  if (!p) die("unknown entity/area", name);
  return p;
}
static const char *name_of(void *p) {
  if (!p) return "-";
  for (int i = 0; i < g_nobj; i++)
    if (g_obj[i].p == p && g_obj[i].kind != 0) return g_obj[i].name;
  return "?";
}
static void add_obj(const char *name, void *p, int kind) {
  if (g_nobj >= MAXN) die("too many objects", name);
  strncpy(g_obj[g_nobj].name, name, 31);
  g_obj[g_nobj].p = p;
  g_obj[g_nobj].kind = kind;
  g_nobj++;
}

/* ------------------------------------------------------------------ tokens */
#define MAXTOK 64
typedef struct { int n; char *k[MAXTOK]; char *v[MAXTOK]; } Toks;
static void tokenize(char *line, Toks *t) {
  t->n = 0;
  for (char *s = strtok(line, " \t\r\n"); s && t->n < MAXTOK; s = strtok(0, " \t\r\n")) {
    char *eq = strchr(s, '=');
    if (eq) { *eq = 0; t->v[t->n] = eq + 1; } else t->v[t->n] = 0;
    t->k[t->n++] = s;
  }
}
static const char *get(const Toks *t, const char *key, const char *def) {
  for (int i = 1; i < t->n; i++)
    if (!strcmp(t->k[i], key)) return t->v[i] ? t->v[i] : "";
  return def;
}
static const char *need(const Toks *t, const char *key) {
  const char *v = get(t, key, 0);
  if (!v) die("missing key", key);
  return v;
}

/* ------------------------------------------------------------------ struct templates
 * x64 layouts and "unused" sentinels from entities.md / dynamics.md (see the Rust structs in
 * aaemu-physics/src/params). Only what the oracle needs; everything else is set per field. */
static void fill_unused_f(unsigned char *b, int from, int to) {
  for (int o = from; o + 4 <= to; o += 4) *(uint32_t *)(b + o) = UNUSED_F_BITS;
}
#define SETI(b, off, v) (*(uint32_t *)((b) + (off)) = (uint32_t)(v))
#ifdef ORACLE_X86
#define SETP(b, off, v) (*(uint32_t *)((b) + (off)) = (uint32_t)(uintptr_t)(v))
#else
#define SETP(b, off, v) (*(uint64_t *)((b) + (off)) = (uint64_t)(v))
#endif
static int tmpl_params(int type, unsigned char *b) {
  memset(b, 0, 512);
  SETI(b, 0, type);
  switch (type) {
#ifdef ORACLE_X86
  case 0: /* pe_params_pos 56: pos 4, q 16, scale 32, mtx3x4 36, mtx3x3 40, sim 44, recalc 48 */
    fill_unused_f(b, 4, 36);
    SETP(b, 36, 0); SETP(b, 40, 0); SETI(b, 44, UNUSED_I); SETI(b, 48, 1); b[52] = 0;
    return 56;
  case 6: /* pe_params_part: pointer layout not ported (see CRYPHYSICS-MAP s9b) */
  case 2: /* car params: pointer layout not ported */
  case 16: /* wheel params: pointer layout not ported */
    return 0;
#else
  case 0: /* pe_params_pos 72 */
    fill_unused_f(b, 4, 40);
    SETP(b, 40, 0); SETP(b, 48, 0); SETI(b, 56, UNUSED_I); SETI(b, 60, 1); b[64] = 0;
    return 72;
  case 6: /* pe_params_part 160 */
    SETI(b, 4, UNUSED_I); SETI(b, 8, UNUSED_I); SETI(b, 12, 1);
    fill_unused_f(b, 16, 48);
    SETP(b, 48, 0); SETP(b, 56, 0);
    SETI(b, 64, UNUSED_I); SETI(b, 68, 0); SETI(b, 72, 0xFFFFFFFF); SETI(b, 76, 0); SETI(b, 80, 0xFFFFFFFF);
    fill_unused_f(b, 84, 96);
    SETP(b, 96, -1); SETP(b, 104, -1); SETI(b, 112, UNUSED_I); SETP(b, 120, -1); SETI(b, 128, UNUSED_I);
    SETP(b, 136, -1); SETI(b, 144, 0); SETI(b, 152, 0); SETI(b, 156, UNUSED_I);
    return 160;
  case 2: /* car params 144 (P5 wheeled) */
    fill_unused_f(b, 4, 144);
    SETI(b, 28, UNUSED_I); SETI(b, 56, UNUSED_I); SETI(b, 84, UNUSED_I); SETP(b, 88, -1);
    SETI(b, 96, UNUSED_I); SETI(b, 100, UNUSED_I); SETI(b, 128, UNUSED_I); SETI(b, 136, UNUSED_I);
    return 144;
  case 16: /* wheel params 80 (P5 wheeled) */
    fill_unused_f(b, 4, 80);
    SETI(b, 4, UNUSED_I); SETI(b, 8, UNUSED_I); SETI(b, 12, UNUSED_I); SETI(b, 16, UNUSED_I);
    SETI(b, 20, UNUSED_I); SETI(b, 52, UNUSED_I); SETI(b, 56, UNUSED_I);
    return 80;
#endif
  case 10: /* simulation 84 (pointer-free, identical) */
    fill_unused_f(b, 4, 84);
    SETI(b, 4, UNUSED_I); SETI(b, 60, UNUSED_I); SETI(b, 64, UNUSED_I);
    return 84;
  case 12: /* buoyancy 80 (pointer-free, identical) */
    fill_unused_f(b, 4, 80);
    SETI(b, 72, UNUSED_I);
    return 80;
  case 14: /* bbox 28 */
    fill_unused_f(b, 4, 28);
    return 28;
  case 15: /* flags 16 */
    SETI(b, 4, UNUSED_I); SETI(b, 8, UNUSED_I); SETI(b, 12, UNUSED_I);
    return 16;
  case 23: /* timeout 12 */
    fill_unused_f(b, 4, 12);
    return 12;
  }
  return 0;
}
static int tmpl_action(int type, unsigned char *b) {
  memset(b, 0, 512);
  SETI(b, 0, type);
  switch (type) {
  case 2: /* action_impulse 80 */
    fill_unused_f(b, 4, 64);
    SETI(b, 64, UNUSED_I); SETI(b, 68, UNUSED_I); SETI(b, 72, 0); SETI(b, 76, 0);
    return 80;
  case 3: /* drive 36 (P5 wheeled) */
    fill_unused_f(b, 4, 24); SETI(b, 24, UNUSED_I); SETI(b, 28, UNUSED_I); SETI(b, 32, UNUSED_I);
    return 36;
  case 4: /* reset 8 */
    SETI(b, 4, 0);
    return 8;
  case 8: /* awake 12 */
    SETI(b, 4, 1); fill_unused_f(b, 8, 12);
    return 12;
  case 10: /* set_velocity 40 */
    SETI(b, 4, UNUSED_I); SETI(b, 8, UNUSED_I);
    fill_unused_f(b, 12, 36); SETI(b, 36, 0);
    return 40;
  }
  return 0;
}
/* field setters shared by params/action/status/geomparams: f<off> i<off> u<off> v<off> q<off> p<off> b<off> hex */
static void apply_fields(const Toks *t, unsigned char *b, int cap) {
  for (int i = 1; i < t->n; i++) {
    const char *k = t->k[i], *v = t->v[i];
    if (!v) continue;
    if (!strcmp(k, "hex")) {
      int n = (int)strlen(v) / 2;
      if (n > cap) die("hex too long", k);
      for (int j = 0; j < n; j++) {
        char x[3] = {v[2 * j], v[2 * j + 1], 0};
        b[j] = (unsigned char)strtoul(x, 0, 16);
      }
      continue;
    }
    char c = k[0];
    if (!strchr("fiuvqpb", c) || k[1] < '0' || k[1] > '9') continue;
    int off = atoi(k + 1);
    if (off < 0 || off + 16 > cap) die("field offset out of range", k);
    switch (c) {
    case 'f': *(float *)(b + off) = parse_f(v); break;
    case 'i': case 'u': *(uint32_t *)(b + off) = parse_u(v); break;
    case 'b': b[off] = (unsigned char)strtoul(v, 0, 0); break;
    case 'v': { float f[3] = {0}; parse_fv(v, f, 3); memcpy(b + off, f, 12); } break;
    case 'q': { float f[4] = {0}; parse_fv(v, f, 4); memcpy(b + off, f, 16); } break;
    case 'p':
      if (!strcmp(v, "null")) SETP(b, off, 0);
      else if (!strcmp(v, "unused")) SETP(b, off, -1);
      else if (!strncmp(v, "ent:", 4)) SETP(b, off, find_target(v + 4));
      else if (!strncmp(v, "buf:", 4)) SETP(b, off, b + atoi(v + 4)); /* in-image pointer (P5 wheeled gear ratios) */
      else if (!strncmp(v, "geom:", 5)) { void *g = find_obj(v + 5, 0); if (!g) die("unknown geom", v); SETP(b, off, g); }
      else die("bad pointer value", v);
      break;
    }
  }
}

/* P1a primitive-geometry probes (gprobe, gphysblob): owned by package P1a.
   Milestone 1 of the x86 (1.2 client) oracle drops the internal-function probes. */
#ifndef ORACLE_X86
#include "p1a_probes.c"
#include "p3_queries.c" /* P3 query commands (additive) */
#endif

/* ------------------------------------------------------------------ heightfield callbacks */
static struct { int mode; float h0, kx, ky, amp; } g_hf;
static float hf_height(int ix, int iy) {
  switch (g_hf.mode) {
  case 1: return g_hf.h0 + g_hf.kx * (float)ix + g_hf.ky * (float)iy;   /* slope */
  case 2: return g_hf.h0 + g_hf.amp * (float)((ix * 7 + iy * 13) % 5); /* bumps */
  default: return g_hf.h0;                                               /* flat */
  }
}
static unsigned short hf_surf(int ix, int iy) { (void)ix; (void)iy; return 0; }

/* ------------------------------------------------------------------ trace */
#define MAXTRACE 32
static void *g_trace[MAXTRACE];
static int g_ntrace;
static void print_entity(void *e) {
  unsigned char sp[256], sd[256];
  memset(sp, 0, sizeof sp);
  SETI(sp, 0, 1); SETI(sp, 4, 0xFFFFFFFF); SETI(sp, 8, 0xFFFFFFFF);
  m_status(e, E_GetStatus, sp);
  memset(sd, 0, sizeof sd);
  SETI(sd, 0, 8); SETI(sd, 4, 0xFFFFFFFF); SETI(sd, 8, 0xFFFFFFFF);
  int dyn = m_status(e, E_GetStatus, sd);
  unsigned char sa[16];
  memset(sa, 0, sizeof sa);
  SETI(sa, 0, 7);
  int awake = m_status(e, E_GetStatus, sa);
  printf("e %s p", name_of(e)); out_f((float *)(sp + 24), 3);
  printf(" q"); out_f((float *)(sp + 60), 4);
  if (dyn) { printf(" v"); out_f((float *)(sd + 12), 3); printf(" w"); out_f((float *)(sd + 24), 3); }
  printf(" sc %d aw %d\n", *(int *)(sp + 80), awake);
}
static void print_trace(void) {
  printf("step %d\n", g_step);
  for (int i = 0; i < g_ntrace; i++) print_entity(g_trace[i]);
}

/* ------------------------------------------------------------------ commands */
static void *box_prim_geom(const Toks *t, int type) {
  unsigned char prim[256];
  memset(prim, 0, sizeof prim);
  float c[3] = {0, 0, 0};
  const char *cs = get(t, "center", 0);
  if (cs) parse_fv(cs, c, 3);
  switch (type) {
  case 0: { /* box: Basis[9], bOriented, center, size */
    float *f = (float *)prim;
    f[0] = f[4] = f[8] = 1;
    const char *bs = get(t, "basis", 0);
    if (bs) parse_fv(bs, f, 9);
    SETI(prim, 36, bs ? 1 : 0);
    memcpy(prim + 40, c, 12);
    parse_fv(need(t, "size"), (float *)(prim + 52), 3);
  } break;
  case 4: /* sphere: center, r */
    memcpy(prim, c, 12);
    *(float *)(prim + 12) = parse_f(need(t, "r"));
    break;
  case 5: case 6: { /* cylinder/capsule: center, axis, r, hh */
    float ax[3] = {0, 0, 1};
    const char *as = get(t, "axis", 0);
    if (as) parse_fv(as, ax, 3);
    memcpy(prim, c, 12);
    memcpy(prim + 12, ax, 12);
    *(float *)(prim + 24) = parse_f(need(t, "r"));
    *(float *)(prim + 28) = parse_f(need(t, "hh"));
  } break;
  case 3: { /* ray: origin, dir */
    parse_fv(need(t, "org"), (float *)prim, 3);
    parse_fv(need(t, "dir"), (float *)(prim + 12), 3);
  } break;
  case 2: { /* heightfield primitive, 120 B (x64 layout; the x86 one is not ported) */
#ifdef ORACLE_X86
    die("heightfield primitives are not ported to the 1.2 oracle yet", 0);
#endif
    float *f = (float *)prim;
    f[0] = f[4] = f[8] = 1;
    int n[2] = {64, 64};
    float st[2] = {1, 1};
    sscanf(need(t, "n"), "%d,%d", &n[0], &n[1]);
    parse_fv(need(t, "step"), st, 2);
    memcpy(prim + 40, c, 12);
    memcpy(prim + 52, st, 8);
    *(float *)(prim + 60) = 1.0f / st[0];
    *(float *)(prim + 64) = 1.0f / st[1];
    SETI(prim, 68, n[0]); SETI(prim, 72, n[1]);
    SETI(prim, 76, 1); SETI(prim, 80, n[0]);
    *(float *)(prim + 84) = parse_f(get(t, "hscale", "1"));
    *(uint16_t *)(prim + 88) = 0x8000; /* typemask (must be non-zero: the ctor derives typepower from it) */
    *(uint16_t *)(prim + 90) = 0xFFFF; /* heightmask */
    SETI(prim, 92, 1);                 /* typehole: unreachable with typemask 0x8000 and surf 0 */
    SETI(prim, 96, 15);
    const char *fnm = get(t, "fn", "flat");
    g_hf.mode = !strcmp(fnm, "slope") ? 1 : !strcmp(fnm, "bumps") ? 2 : 0;
    g_hf.h0 = parse_f(get(t, "h0", "0"));
    g_hf.kx = parse_f(get(t, "kx", "0"));
    g_hf.ky = parse_f(get(t, "ky", "0"));
    g_hf.amp = parse_f(get(t, "amp", "0"));
    SETP(prim, 104, (void *)hf_height);
    SETP(prim, 112, (void *)hf_surf);
  } break;
  }
  void *g = m_prim(g_gm, type, prim);
  if (!g) die("CreatePrimitive failed", 0);
  if (type == 2 && get(t, "worldhf", 0)) {
#ifndef ORACLE_X86
    /* world slot 17 SetHeightfieldData(heightfield primitive*, matMapping, nMats) */
    m_hfdata(g_world, prim, 0, 0);
    /* it (re)creates one terrain entity per caller slot at world+1568+8*i; name them all "terrain" */
    for (int i = 0; i < 5; i++) {
      void *te = *(void **)((char *)g_world + 1568 + 8 * i);
      if (te && !find_obj("terrain", 1)) add_obj("terrain", te, 1);
      else if (te) { int known = 0; for (int k = 0; k < g_nobj; k++) known |= g_obj[k].p == te; if (!known) add_obj("terrain", te, 1); }
    }
#else
    die("heightfield terrain is not ported to the 1.2 oracle yet", 0);
#endif
  }
  return m_reg(g_gm, g);
}
static int geom_type_id(const char *s) {
  if (!strcmp(s, "box")) return 0;
  if (!strcmp(s, "heightfield")) return 2;
  if (!strcmp(s, "ray")) return 3;
  if (!strcmp(s, "sphere")) return 4;
  if (!strcmp(s, "cylinder")) return 5;
  if (!strcmp(s, "capsule")) return 6;
  die("unknown geom type", s);
  return -1;
}
static int ent_type_id(const char *s) {
  if (!strcmp(s, "static")) return 1;
  if (!strcmp(s, "rigid")) return 2;
  if (!strcmp(s, "wheeled")) return 3;
  if (!strcmp(s, "living")) return 4;
  if (!strcmp(s, "particle")) return 5;
  return atoi(s);
}

#ifndef ORACLE_X86
#include "p1c_mesh.c" /* P1c: trimesh geometry (type=mesh, geominfo) */
#endif

static void cmd_rwi(const Toks *t) {
#ifdef ORACLE_X86
  /* The 1.2 SRWIParams (64 B) and ray_hit (pointer-bearing) layouts are not ported in
     milestone 1; the box scenarios do not use rays. */
  (void)t;
  die("ray is not ported to the 1.2 oracle yet", 0);
#else
  unsigned char p[96];
  unsigned char hits[80 * 16];
  memset(p, 0, sizeof p);
  memset(hits, 0, sizeof hits);
  int nmax = atoi(get(t, "max", "1"));
  if (nmax < 1) nmax = 1;
  if (nmax > 16) nmax = 16;
  parse_fv(need(t, "org"), (float *)(p + 24), 3);
  parse_fv(need(t, "dir"), (float *)(p + 36), 3);
  SETI(p, 48, parse_u(get(t, "types", "0x13F")));
  SETI(p, 52, parse_u(get(t, "flags", "0")));
  SETP(p, 56, hits);
  SETI(p, 64, nmax);
  SETP(p, 72, 0);
  SETI(p, 80, 0);
  SETP(p, 88, 0);
  int n = ((fn_rwi)VT(g_world)[W_RayWorldIntersection])(g_world, p, "physoracle", atoi(get(t, "caller", "4")));
  printf("ray %s n %d\n", get(t, "name", "-"), n);
  for (int i = 0; i < nmax; i++) {
    unsigned char *h = hits + 80 * i;
    if (*(float *)h < 0 && i > 0) continue;
    printf("h %d d %08x e %s ip %d pid %d s %d p", i, fbits(*(float *)h), name_of(*(void **)(h + 8)), *(int *)(h + 16),
           *(int *)(h + 20), *(int *)(h + 28));
    out_f((float *)(h + 36), 3);
    printf(" n");
    out_f((float *)(h + 48), 3);
    if (atoi(get(t, "ext", "0"))) /* P1c: +24/+26 int16, +32, +60 order index, +64 iPrim (geom-local id) */
      printf(" x %d %d %d %d %d", *(short *)(h + 24), *(short *)(h + 26), *(int *)(h + 32), *(int *)(h + 60), *(int *)(h + 64));
    printf("\n");
  }
#endif /* !ORACLE_X86 */
}

/* P3 areas / P4a water probes (package P3-areas) */
#ifndef ORACLE_X86
#include "p3_areas.c"
#endif

static void run_line(char *line) {
  Toks t;
  char *hash = strchr(line, '#');
  if (hash) *hash = 0;
  tokenize(line, &t);
  if (!t.n) return;
  const char *c = t.k[0];
#ifndef ORACLE_X86
  if (p3_command(c, &t)) return;
#endif
  if (!strcmp(c, "grid")) {
    float org[3] = {0, 0, -100}, st[2] = {4, 4};
    int n[2] = {64, 64};
    parse_fv(get(&t, "org", "0,0,-100"), org, 3);
    sscanf(get(&t, "n", "64,64"), "%d,%d", &n[0], &n[1]);
    parse_fv(get(&t, "step", "4,4"), st, 2);
    m_grid(g_world, atoi(get(&t, "axisz", "2")), org, n[0], n[1], st[0], st[1],
           atoi(get(&t, "pod", "0")), atoi(get(&t, "cyclic", "0")));
  } else if (!strcmp(c, "env")) { /* fake gEnv knobs: raymax=N (int), b289/b95=0|1, b290/b96=0|1 */
    const char *v;
    if ((v = get(&t, "raymax", 0))) *(int *)((char *)&g_cfg + ENV_OFF_RAYMAX) = atoi(v);
#ifdef ORACLE_X86
    if ((v = get(&t, "b95", 0))) *((unsigned char *)&g_env + ENV_OFF_BYPASS) = (unsigned char)atoi(v);
    if ((v = get(&t, "b96", 0))) *((unsigned char *)&g_env + ENV_OFF_B2) = (unsigned char)atoi(v);
    if ((v = get(&t, "b97", 0))) *((unsigned char *)&g_env + ENV_OFF_PROFILER) = (unsigned char)atoi(v);
#else
    if ((v = get(&t, "b289", 0))) *((unsigned char *)&g_env + ENV_OFF_BYPASS) = (unsigned char)atoi(v);
    if ((v = get(&t, "b290", 0))) *((unsigned char *)&g_env + ENV_OFF_B2) = (unsigned char)atoi(v);
#endif
  } else if (!strcmp(c, "var")) {
#ifdef ORACLE_X86
    int off = atoi(need(&t, "off86")); /* x86 world offset 72..612: the layout shifts after +116 */
    if (off < 72 || off > 612) die("var off86 outside the x86 physics-vars block", need(&t, "off86"));
#else
    int off = atoi(need(&t, "off"));
    if (off < 104 || off > 632) die("var offset outside the physics-vars block", need(&t, "off"));
#endif
    const char *f = get(&t, "f", 0), *i = get(&t, "i", 0);
    if (f) *(float *)((char *)g_world + off) = parse_f(f);
    else if (i) *(uint32_t *)((char *)g_world + off) = parse_u(i);
    else die("var needs f= or i=", 0);
  } else if (!strcmp(c, "dumpvars")) {
    void *vars = m_call0(g_world, W_GetPhysVars);
    printf("vars %d", (int)((char *)vars - (char *)g_world));
#ifdef ORACLE_X86
    for (int o = 0; o < 540; o += 4) printf(" %08x", *(uint32_t *)((char *)vars + o));
#else
    for (int o = 0; o < 632 - 104; o += 4) printf(" %08x", *(uint32_t *)((char *)vars + o));
#endif
    printf("\n");
  } else if (!strcmp(c, "geom")) {
#ifndef ORACLE_X86
    if (!strcmp(need(&t, "type"), "mesh")) add_obj(need(&t, "name"), p1c_geom_mesh(&t), 0); /* P1c */
    else
#endif
    add_obj(need(&t, "name"), box_prim_geom(&t, geom_type_id(need(&t, "type"))), 0);
  } else if (!strcmp(c, "geominfo")) { /* P1c: summary of a registered geometry */
#ifndef ORACLE_X86
    void *g = find_obj(need(&t, "name"), 0);
    if (!g) die("unknown geom", need(&t, "name"));
    p1c_geominfo(need(&t, "name"), g);
#else
    die("geominfo is not ported to the 1.2 oracle yet", 0);
#endif
  } else if (!strcmp(c, "entity")) {
    unsigned char pp[512];
    tmpl_params(0, pp);
    parse_fv(get(&t, "pos", "0,0,0"), (float *)(pp + 4), 3);
    float q[4] = {0, 0, 0, 1};
    parse_fv(get(&t, "q", "0,0,0,1"), q, 4);
    memcpy(pp + 16, q, 16);
    apply_fields(&t, pp, 512);
    void *e = m_create(g_world, ent_type_id(need(&t, "type")), pp, atoi(get(&t, "id", "-1")));
    if (!e) die("CreatePhysicalEntity failed", need(&t, "name"));
    add_obj(need(&t, "name"), e, 1);
  } else if (!strcmp(c, "part")) {
    void *e = find_target(need(&t, "ent"));
    void *g = find_obj(need(&t, "geom"), 0);
    if (!g) die("unknown geom", need(&t, "geom"));
    unsigned char gp[512];
    memset(gp, 0, sizeof gp);
    float *f = (float *)gp;
    SETI(gp, 0, 0);
    f[1] = parse_f(get(&t, "density", "0"));
    f[2] = parse_f(get(&t, "mass", "0"));
    parse_fv(get(&t, "pos", "0,0,0"), f + 3, 3);
    float q[4] = {0, 0, 0, 1};
    parse_fv(get(&t, "q", "0,0,0,1"), q, 4);
    memcpy(gp + 24, q, 16);
    f[10] = parse_f(get(&t, "scale", "1"));
#ifdef ORACLE_X86
    /* pe_geomparams 88: surface 52, flags 56, collider 60, mincontact 64, idmat 68,
       lattice 72, matMapping 76, nMats 80, recalcBBox 84 (verified in FUN_35061170) */
    SETI(gp, 52, UNUSED_I);
    SETI(gp, 56, parse_u(get(&t, "flags", "0x1809F")));
    SETI(gp, 60, parse_u(get(&t, "collider", "0x1")));
    *(float *)(gp + 64) = parse_f(get(&t, "mincontact", "U"));
    SETI(gp, 68, -1);
    SETI(gp, 84, 1);
#else
    SETI(gp, 64, UNUSED_I);
    SETI(gp, 68, parse_u(get(&t, "flags", "0x1809F"))); /* = DEFAULT_PART_FLAGS in the Rust crate */
    SETI(gp, 72, parse_u(get(&t, "collider", "0x1")));
    *(float *)(gp + 76) = parse_f(get(&t, "mincontact", "U"));
    SETI(gp, 80, -1);
    SETI(gp, 108, 1);
#endif
    apply_fields(&t, gp, 512);
    int id = m_addg(e, E_AddGeometry, g, gp, atoi(get(&t, "id", "-1")), 1);
    printf("ret part %s %d\n", need(&t, "ent"), id);
  } else if (!strcmp(c, "surface")) { /* SetSurfaceParameters(idx, bounciness, friction, flags) */
    int r = m_surf(g_world, atoi(need(&t, "idx")), parse_f(get(&t, "bounce", "0")),
                   parse_f(get(&t, "friction", "h:3f99999a")), parse_u(get(&t, "flags", "0")));
    printf("ret surface %d\n", r);
  } else if (!strcmp(c, "area")) {
    const char *kind = get(&t, "kind", "global");
    if (strcmp(kind, "global")) die("only kind=global areas are supported", kind);
    void *a = m_call0(g_world, W_AddGlobalArea);
    add_obj(need(&t, "name"), a, 2);
  } else if (!strcmp(c, "params") || !strcmp(c, "action") || !strcmp(c, "status") || !strcmp(c, "getparams")) {
    void *e = find_target(need(&t, "target"));
    int type = atoi(need(&t, "type"));
    __declspec(align(16)) unsigned char b[1024];
    int sz = 0;
    if (!strcmp(get(&t, "tmpl", "auto"), "auto")) sz = (c[0] == 'a') ? tmpl_action(type, b) : (c[0] == 'p') ? tmpl_params(type, b) : 0;
    if (!sz) { memset(b, 0, sizeof b); SETI(b, 0, type); }
    apply_fields(&t, b, 512);
    int r;
    int ts = atoi(get(&t, "ts", "1")); /* bThreadSafe of SetParams / Action (<= 0 may queue) */
    if (c[0] == 'p') r = m_setp(e, E_SetParams, b, ts);
    else if (c[0] == 'a') r = m_setp(e, E_Action, b, ts);
    else if (c[0] == 'g') r = m_status(e, E_GetParams, b);
    else r = m_status(e, E_GetStatus, b);
    printf("ret %s %s %d %d", c, need(&t, "target"), type, r);
    const char *dump = get(&t, "dump", 0); /* dump=OFF,NWORDS of the struct after the call */
    if (dump) {
      int off = 0, n = 0;
      sscanf(dump, "%d,%d", &off, &n);
      for (int i = 0; i < n && off + 4 * i < 512; i++) printf(" %08x", *(uint32_t *)(b + off + 4 * i));
    }
    printf("\n");
  } else if (!strcmp(c, "trace")) {
    g_ntrace = 0;
    char buf[512];
    strncpy(buf, need(&t, "ents"), sizeof buf - 1);
    buf[sizeof buf - 1] = 0;
    for (char *s = strtok(buf, ","); s && g_ntrace < MAXTRACE; s = strtok(0, ",")) g_trace[g_ntrace++] = find_target(s);
  } else if (!strcmp(c, "step")) {
    int n = atoi(get(&t, "n", "1"));
    float dt = parse_f(get(&t, "dt", "h:3d088889")); /* 1/30 */
    int flags = (int)parse_u(get(&t, "flags", "0x9F"));
    int every = atoi(get(&t, "every", "1"));
    for (int i = 0; i < n; i++) {
      m_step(g_world, dt, flags);
      g_step++;
      if (every > 0 && (g_step % every == 0 || i == n - 1)) print_trace();
    }
  } else if (!strcmp(c, "print")) {
    print_trace();
  } else if (!strcmp(c, "ray")) {
    cmd_rwi(&t);
  } else if (!strcmp(c, "peek")) { /* raw words of an entity/area object (RE experiments) */
    void *e = find_target(need(&t, "target"));
    int off = atoi(need(&t, "off")), n = atoi(get(&t, "n", "1"));
    printf("peek %s %d", need(&t, "target"), off);
    for (int i = 0; i < n; i++) printf(" %08x", *(uint32_t *)((char *)e + off + 4 * i));
    printf("\n");
  } else if (!strcmp(c, "snap")) { /* remember n words of an object (pointers make raw dumps non-reproducible) */
    void *e = find_target(need(&t, "target"));
    g_snap_n = atoi(get(&t, "n", "300"));
    if (g_snap_n > 4096) g_snap_n = 4096;
    g_snap_obj = e;
    memcpy(g_snap, e, 4 * (size_t)g_snap_n);
  } else if (!strcmp(c, "snapdiff")) { /* byte offsets of the words that changed since snap */
    void *e = find_target(need(&t, "target"));
    if (e != g_snap_obj) die("snapdiff target differs from snap target", need(&t, "target"));
    printf("snapdiff %s", need(&t, "target"));
    for (int i = 0; i < g_snap_n; i++)
      if (((uint32_t *)e)[i] != g_snap[i]) printf(" %d", 4 * i);
    printf("\n");
  } else if (!strcmp(c, "peekworld")) {
    int off = atoi(need(&t, "off")), n = atoi(get(&t, "n", "1"));
    printf("peekworld %d", off);
    for (int i = 0; i < n; i++) printf(" %08x", *(uint32_t *)((char *)g_world + off + 4 * i));
    printf("\n");
  } else if (!strcmp(c, "peekdll")) { /* raw words of the DLL image at VA (e.g. the params size tables) */
    uint64_t va = strtoull(need(&t, "va"), 0, 0);
    int n = atoi(get(&t, "n", "1"));
    printf("peekdll %llx", (unsigned long long)va);
    for (int i = 0; i < n; i++) printf(" %08x", *(uint32_t *)VA(g_phys, va + 4 * i));
    printf("\n");
  } else if (!strcmp(c, "listwhere")) { /* which world entity list (+664+8k, k=0..7, and +760/+816) holds the entity */
    void *e = find_target(need(&t, "target"));
    int offs[10] = {664, 672, 680, 688, 696, 704, 712, 720, 760, 816};
    printf("listwhere %s", need(&t, "target"));
    for (int k = 0; k < 10; k++) {
      int found = 0, guard = 0;
      for (char *p = *(char **)((char *)g_world + offs[k]); p && guard < 100000; p = *(char **)(p + 104), guard++)
        if (p == e) { found = 1; break; }
      if (found) printf(" %d", offs[k]);
    }
    printf("\n");
  } else if (!strcmp(c, "contacts")) { /* the rigid entity's contact list (E+536): non-pointer fields of each record */
    void *e = find_target(need(&t, "target"));
    char *head = (char *)e + 536;
    int n = 0;
    for (char *k = *(char **)head; k && k != head && n < 1000; k = *(char **)k, n++) {
      printf("c %d", n);
      for (int o = 24; o < 64; o += 4) printf(" %08x", *(uint32_t *)(k + o));
      printf(" e %s %s", name_of(*(void **)(k + 64)), name_of(*(void **)(k + 72)));
      for (int o = 80; o < 88; o += 4) printf(" %08x", *(uint32_t *)(k + o));
      for (int o = 112; o < 168; o += 4) printf(" %08x", *(uint32_t *)(k + o));
      printf("\n");
    }
    printf("contacts %s %d\n", need(&t, "target"), n);
  } else if (!strcmp(c, "echo")) {
    printf("echo");
    for (int i = 1; i < t.n; i++) printf(" %s", t.k[i]);
    printf("\n");
#ifndef ORACLE_X86
  } else if (p1a_command(c, &t)) {
  } else if (p3q_command(c, &t)) {
#endif
  } else {
    die("unknown command", c);
  }
}

/* The internal-function / pair probes below use hard-coded x64 addresses and calling
   conventions; milestone 1 of the 1.2 oracle only ports the scenario loop. */
#ifndef ORACLE_X86
/* ------------------------------------------------------------------ math probes */
static uint64_t g_rng;
static uint64_t splitmix(void) {
  uint64_t z = (g_rng += 0x9E3779B97F4A7C15ULL);
  z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9ULL;
  z = (z ^ (z >> 27)) * 0x94D049BB133111EBULL;
  return z ^ (z >> 31);
}
/* uniform float in [lo, hi): 24 random bits, computed in single precision so Rust can reproduce it */
static float frand(float lo, float hi) { return lo + (hi - lo) * ((float)(splitmix() >> 40) * (1.0f / 16777216.0f)); }
static void rand_quat(float *q) {
  float n2;
  do {
    for (int i = 0; i < 4; i++) q[i] = frand(-1, 1);
    n2 = q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3];
  } while (n2 < 0.01f || n2 > 1.0f);
  float inv = 1.0f / sqrtf(n2);
  for (int i = 0; i < 4; i++) q[i] *= inv;
}
typedef void (*fn_q2m)(float *, const float *);
typedef void (*fn_qv)(float *, const float *, const float *);
typedef void (*fn_bstep)(void *, float);
typedef void (*fn_bupd)(void *);
typedef float (*fn_f1)(float);
typedef float (*fn_f2)(float, float);
typedef double (*fn_d1)(double);
typedef double (*fn_d2)(double, double);

/* ------------------------------------------------------------------ P2a-4 pair probes
 * (round + ray pairs). `--math p2a4_<pair> COUNT SEED` calls one INTERSECT / OVERLAP cell of the
 * DLL directly on generated primitives. Row: "in <A words> <B words> out <ret> <prim_inters
 * words 0..13> <nborderpt> <border points>"; the output struct is pre-filled with 0xCD bytes and
 * nborderpt with 0xCDCDCDCD, so untouched fields are visible. Overlap rows: "out <ret>". */
typedef int (*fn_pair3)(void *, void *, void *);
static const struct { const char *name; uint64_t va; int ta, tb, ovl; } g_p2a4[] = {
  {"p2a4_sph_sph", 0x35160CB0ULL, 4, 4, 0}, {"p2a4_sph_ray", 0x35160EA0ULL, 4, 3, 0},
  {"p2a4_ray_sph", 0x35161180ULL, 3, 4, 0}, {"p2a4_cyl_sph", 0x3515FA20ULL, 5, 4, 0},
  {"p2a4_sph_cyl", 0x3515FDF0ULL, 4, 5, 0}, {"p2a4_cyl_ray", 0x3515FE40ULL, 5, 3, 0},
  {"p2a4_ray_cyl", 0x35160970ULL, 3, 5, 0}, {"p2a4_cyl_cyl", 0x3516B7F0ULL, 5, 5, 0},
  {"p2a4_cyl_cap", 0x3516D2E0ULL, 5, 6, 0}, {"p2a4_cap_cyl", 0x3516E320ULL, 6, 5, 0},
  {"p2a4_cap_sph", 0x35161C80ULL, 6, 4, 0}, {"p2a4_sph_cap", 0x35161EA0ULL, 4, 6, 0},
  {"p2a4_cap_ray", 0x35161EF0ULL, 6, 3, 0}, {"p2a4_ray_cap", 0x35162A50ULL, 3, 6, 0},
  {"p2a4_cap_cap", 0x35161550ULL, 6, 6, 0}, {"p2a4_ovl_sph_sph", 0x35196030ULL, 4, 4, 1},
  {"p2a4_ovl_hf_sph", 0x351975E0ULL, 2, 4, 1}, {"p2a4_ovl_sph_hf", 0x35197860ULL, 4, 2, 1},
};
static float g_p2a4_axis[3] = {0, 0, 1};
static void p2a4_unit(float *v) {
  float n2;
  do {
    for (int k = 0; k < 3; k++) v[k] = frand(-1, 1);
    n2 = v[0] * v[0] + v[1] * v[1] + v[2] * v[2];
  } while (n2 < 0.01f || n2 > 1.0f);
  float inv = 1.0f / sqrtf(n2);
  for (int k = 0; k < 3; k++) v[k] *= inv;
}
/* A primitive of `type` near `c` (spread `sc`); `first` marks the first solid of the row. */
static int p2a4_gen(int type, float *p, const float *c, float sc, int i, int first) {
  switch (type) {
  case 4:
    for (int k = 0; k < 3; k++) p[k] = c[k] + frand(-sc, sc);
    p[3] = frand(0.05f, 1.5f);
    return 4;
  case 5: case 6: {
    for (int k = 0; k < 3; k++) p[k] = c[k] + frand(-sc, sc);
    int m = (i / 2) % 8;
    if (m == 0) { p[3] = 0; p[4] = 0; p[5] = 1; }
    else if (m == 1) { p[3] = 1; p[4] = 0; p[5] = 0; }
    else if (!first && m == 2) memcpy(p + 3, g_p2a4_axis, 12);                         /* parallel */
    else if (!first && m == 3) for (int k = 0; k < 3; k++) p[3 + k] = -g_p2a4_axis[k]; /* antiparallel */
    else if (!first && m == 4) {                                                       /* nearly parallel */
      float n2 = 0;
      for (int k = 0; k < 3; k++) { p[3 + k] = g_p2a4_axis[k] + frand(-0.004f, 0.004f); n2 += p[3 + k] * p[3 + k]; }
      float inv = 1.0f / sqrtf(n2);
      for (int k = 0; k < 3; k++) p[3 + k] *= inv;
    } else p2a4_unit(p + 3);
    if (first) memcpy(g_p2a4_axis, p + 3, 12);
    p[6] = frand(0.05f, 1.2f);
    p[7] = frand(0.05f, 1.5f);
    return 8;
  }
  case 3: {
    float u[3], tg[3];
    p2a4_unit(u);
    float dist = frand(0.3f, 4.0f);
    for (int k = 0; k < 3; k++) p[k] = c[k] + u[k] * dist;
    if (i % 13 == 5) for (int k = 0; k < 3; k++) p[k] = c[k] + frand(-0.2f, 0.2f); /* starts inside */
    for (int k = 0; k < 3; k++) tg[k] = c[k] + frand(-sc, sc);
    float len = frand(0.3f, 2.5f);
    for (int k = 0; k < 3; k++) p[3 + k] = (tg[k] - p[k]) * len;
    if (i % 9 == 4) { /* axis-aligned direction */
      int big = 0;
      for (int k = 1; k < 3; k++) if (fabsf(p[3 + k]) > fabsf(p[3 + big])) big = k;
      for (int k = 0; k < 3; k++) if (k != big) p[3 + k] = 0;
    }
    if (i % 31 == 7) for (int k = 0; k < 3; k++) p[3 + k] = 0; /* zero-length ray */
    return 6;
  }
  }
  return 0;
}
static void p2a4_probe(const char *name, int count) {
  int idx = -1;
  for (int j = 0; j < (int)(sizeof g_p2a4 / sizeof g_p2a4[0]); j++)
    if (!strcmp(g_p2a4[j].name, name)) idx = j;
  if (idx < 0) die("unknown p2a4 probe", name);
  int ta = g_p2a4[idx].ta, tb = g_p2a4[idx].tb;
  void *fn = VA(g_phys, g_p2a4[idx].va);
  static const float zero[3] = {0, 0, 0};
  for (int i = 0; i < count; i++) {
    __declspec(align(16)) float pa[32], pb[32];
    __declspec(align(16)) unsigned char hfp[128];
    memset(pa, 0, sizeof pa);
    memset(pb, 0, sizeof pb);
    if (ta == 2 || tb == 2) { /* heightfield vs sphere: random grid, the "bumps" height function */
      float *f = (float *)hfp;
      memset(hfp, 0, sizeof hfp);
      f[0] = f[4] = f[8] = 1;
      if (i % 3 == 1) { float a = frand(-3.1f, 3.1f), cs = cosf(a), sn = sinf(a); f[0] = cs; f[1] = -sn; f[3] = sn; f[4] = cs; }
      for (int k = 0; k < 3; k++) f[10 + k] = frand(-5, 5);
      f[13] = frand(0.5f, 2.0f); f[14] = frand(0.5f, 2.0f);
      f[15] = 1.0f / f[13]; f[16] = 1.0f / f[14];
      int nx = 4 + (int)(splitmix() % 12), ny = 4 + (int)(splitmix() % 12);
      SETI(hfp, 68, nx); SETI(hfp, 72, ny); SETI(hfp, 76, 1); SETI(hfp, 80, nx);
      f[21] = frand(0.5f, 2.0f);
      *(uint16_t *)(hfp + 88) = 0x8000; *(uint16_t *)(hfp + 90) = 0xFFFF; SETI(hfp, 92, 1); SETI(hfp, 96, 15);
      g_hf.mode = 2; g_hf.h0 = frand(-1, 1); g_hf.amp = frand(0, 1);
      SETP(hfp, 104, (void *)hf_height); SETP(hfp, 112, (void *)hf_surf);
      pb[0] = f[10] + frand(-2, (float)nx * f[13] + 2);
      pb[1] = f[11] + frand(-2, (float)ny * f[14] + 2);
      pb[2] = f[12] + frand(-2, 4);
      pb[3] = frand(0.1f, 2.0f);
      int r = (ta == 2) ? ((fn_pair3)fn)(hfp, pb, 0) : ((fn_pair3)fn)(pb, hfp, 0);
      printf("in");
      out_f(f, 9); out_f(f + 10, 3); out_f(f + 13, 4);
      printf(" %08x %08x", nx, ny);
      out_f(f + 21, 1); out_f(&g_hf.h0, 1); out_f(&g_hf.amp, 1); out_f(pb, 4);
      printf(" out %08x\n", r);
      continue;
    }
    /* the solid (non-ray) operand first, the other one around it */
    int ray_first = (ta == 3);
    int t1 = ray_first ? tb : ta, t2 = ray_first ? ta : tb;
    float *p1 = ray_first ? pb : pa, *p2 = ray_first ? pa : pb;
    int n1 = p2a4_gen(t1, p1, zero, 0.3f, i, 1);
    float spread = (i % 4 == 0) ? 0.6f : 2.0f;
    int n2 = p2a4_gen(t2, p2, p1, spread, i, 0);
    int na = ray_first ? n2 : n1, nb = ray_first ? n1 : n2;
    printf("in");
    out_f(pa, na); out_f(pb, nb);
    if (g_p2a4[idx].ovl) {
      int r = ((fn_pair3)fn)(pa, pb, 0);
      printf(" out %08x\n", r);
      continue;
    }
    __declspec(align(16)) unsigned char pi[256];
    __declspec(align(16)) float border[3 * 64];
    memset(pi, 0xCD, sizeof pi);
    memset(border, 0xCD, sizeof border);
    SETP(pi, 56, border);
    SETI(pi, 68, 64);
    int r = ((fn_pair3)fn)(pa, pb, pi);
    printf(" out %08x", r);
    for (int k = 0; k < 14; k++) printf(" %08x", *(uint32_t *)(pi + 4 * k));
    int nbp = *(int *)(pi + 64);
    printf(" %08x", (uint32_t)nbp);
    if (nbp > 0 && nbp <= 64) out_f(border, 3 * nbp);
    printf("\n");
  }
}
#include "probe_p2a1.c" /* P2a-1 box pair probes (additive) */

static void math_probe(const char *name, int count, uint64_t seed) {
  g_rng = seed;
  HMODULE crt = GetModuleHandleA("MSVCR100.dll");
  printf("physoracle-math 1 %s %d %llu\n", name, count, (unsigned long long)seed);
  if (!strncmp(name, "p2a4_", 5)) { p2a4_probe(name, count); return; }
  if (!strncmp(name, "p2a1_", 5)) { p2a1_probe(name, count); return; }
  if (!strcmp(name, "quat2mat")) {
    /* sub_350247B0: Matrix33 (row-major) from quaternion (x,y,z,w) */
    fn_q2m f = (fn_q2m)VA(g_phys, 0x350247B0ULL);
    for (int i = 0; i < count; i++) {
      float q[4], m[9];
      if (i % 4 == 3) for (int k = 0; k < 4; k++) q[k] = frand(-2, 2); /* also non-unit inputs */
      else rand_quat(q);
      f(m, q);
      printf("in"); out_f(q, 4); printf(" out"); out_f(m, 9); printf("\n");
    }
  } else if (!strcmp(name, "mat2quat")) {
    /* sub_35023EB0: quaternion from a row-major Matrix33 (all four trace branches) */
    fn_q2m q2m = (fn_q2m)VA(g_phys, 0x350247B0ULL);
    fn_q2m f = (fn_q2m)VA(g_phys, 0x35023EB0ULL);
    for (int i = 0; i < count; i++) {
      float q[4], m[9], o[4];
      rand_quat(q);
      if (i % 5 == 1) { q[3] = frand(-0.05f, 0.05f); } /* push trace <= 0 */
      q2m(m, q);
      if (i % 7 == 3) for (int k = 0; k < 9; k++) m[k] += frand(-0.01f, 0.01f); /* slightly non-orthonormal */
      f(o, m);
      printf("in"); out_f(m, 9); printf(" out"); out_f(o, 4); printf("\n");
    }
  } else if (!strcmp(name, "quatrot")) {
    /* sub_35022240: rotate vec3 by quaternion */
    fn_qv f = (fn_qv)VA(g_phys, 0x35022240ULL);
    for (int i = 0; i < count; i++) {
      float q[4], v[3], o[3];
      rand_quat(q);
      for (int k = 0; k < 3; k++) v[k] = frand(-100, 100);
      f(o, q, v);
      printf("in"); out_f(q, 4); out_f(v, 3); printf(" out"); out_f(o, 3); printf("\n");
    }
  } else if (!strcmp(name, "bodystep") || !strcmp(name, "bodyupdate")) {
    /* sub_3509E180 body Step(dt) / sub_3509CE40 UpdateState on a 212-byte RigidBody:
       +0 pos, +12 q, +28 P, +40 L, +52 w, +64 v, +76 M, +80 Minv, +84 V, +88 Ibody, +100 Ibody_inv */
    int step = !strcmp(name, "bodystep");
    for (int i = 0; i < count; i++) {
      __declspec(align(16)) float b[64];
      memset(b, 0, sizeof b);
      for (int k = 0; k < 3; k++) b[k] = frand(-50, 50);
      rand_quat(b + 3);
      float m = frand(0.5f, 2000);
      for (int k = 0; k < 3; k++) b[7 + k] = frand(-50, 50) * m;     /* P */
      float wscale = (i % 3 == 0) ? 0.001f : 5.0f;                   /* hit both rotation branches */
      for (int k = 0; k < 3; k++) b[22 + k] = frand(0.05f, 10) * m; /* Ibody */
      for (int k = 0; k < 3; k++) b[25 + k] = 1.0f / b[22 + k];     /* Ibody_inv */
      for (int k = 0; k < 3; k++) b[10 + k] = frand(-wscale, wscale) * b[22 + k]; /* L ~ I*w */
      b[19] = m;
      b[20] = 1.0f / m;
      b[21] = frand(0.1f, 10);
      float dt = (i % 2) ? (1.0f / 30.0f) : frand(0.001f, 0.02f);
      printf("in"); out_f(b, 28); out_f(&dt, 1);
      if (step) ((fn_bstep)VA(g_phys, 0x3509E180ULL))(b, dt);
      else ((fn_bupd)VA(g_phys, 0x3509CE40ULL))(b);
      printf(" out"); out_f(b, 44); printf("\n");
    }
  } else if (!strcmp(name, "sinf") || !strcmp(name, "cosf") || !strcmp(name, "tanf") || !strcmp(name, "atanf") ||
             !strcmp(name, "asinf") || !strcmp(name, "acosf") || !strcmp(name, "expf") || !strcmp(name, "logf") ||
             !strcmp(name, "sqrtf")) {
    fn_f1 f = (fn_f1)GetProcAddress(crt, name);
    if (!f) die("CRT function not found", name);
    for (int i = 0; i < count; i++) {
      float x;
      if (!strcmp(name, "asinf") || !strcmp(name, "acosf")) x = frand(-1, 1);
      else if (!strcmp(name, "logf") || !strcmp(name, "sqrtf")) x = frand(0, 1000) + 1e-6f;
      else if (!strcmp(name, "expf")) x = frand(-20, 20);
      else x = (i % 2) ? frand(-4, 4) : frand(-200, 200);
      float y = f(x);
      printf("in %08x out %08x\n", fbits(x), fbits(y));
    }
  } else if (!strcmp(name, "atan2f") || !strcmp(name, "powf")) {
    fn_f2 f = (fn_f2)GetProcAddress(crt, name);
    for (int i = 0; i < count; i++) {
      float x = frand(-10, 10), y = frand(-10, 10);
      if (!strcmp(name, "powf")) x = frand(0, 10);
      float r = f(x, y);
      printf("in %08x %08x out %08x\n", fbits(x), fbits(y), fbits(r));
    }
  } else if (!strcmp(name, "sin") || !strcmp(name, "cos") || !strcmp(name, "exp") || !strcmp(name, "log") ||
             !strcmp(name, "asin")) {
    fn_d1 f = (fn_d1)GetProcAddress(crt, name);
    for (int i = 0; i < count; i++) {
      double x = !strcmp(name, "asin") ? frand(-1, 1) : !strcmp(name, "log") ? frand(0, 1000) + 1e-6 : frand(-20, 20);
      double y = f(x);
      uint64_t a, b;
      memcpy(&a, &x, 8);
      memcpy(&b, &y, 8);
      printf("in %016llx out %016llx\n", (unsigned long long)a, (unsigned long long)b);
    }
  } else if (!strcmp(name, "atan2")) {
    fn_d2 f = (fn_d2)GetProcAddress(crt, name);
    for (int i = 0; i < count; i++) {
      double x = frand(-10, 10), y = frand(-10, 10), r = f(x, y);
      uint64_t a, b, c;
      memcpy(&a, &x, 8); memcpy(&b, &y, 8); memcpy(&c, &r, 8);
      printf("in %016llx %016llx out %016llx\n", (unsigned long long)a, (unsigned long long)b, (unsigned long long)c);
    }
  } else if (!strcmp(name, "rng")) {
    for (int i = 0; i < count; i++) { float x = frand(-1, 1); printf("in %d out %08x\n", i, fbits(x)); }
  } else {
    die("unknown math probe", name);
  }
}

/* ------------------------------------------------------------------ P2a-2 pair probes */
/* `physoracle --p2a2 < LINES` (added by P2a-2, triangle pairs). Each stdin line is
 *   NAME W0 W1 ...    (hex 32-bit words)
 * holding the operands' raw primitive words (tri 12, box 16, sph 4, ray 6, hf see below) and,
 * for INTERSECT functions, the 24 initial words of the prim_inters buffer. The probe calls the
 * DLL function directly and prints `NAME in <words> out <ret> [<24 buffer words>]`.
 * Heightfield operands: 30 primitive words (the two callback pointers are replaced), then
 * gx gy and gx*gy heights; getHeight(ix,iy) returns grid[iy*gx+ix] inside the grid and
 * (float)(ix*7+iy*13)*0.25f outside. */
typedef int (*fn_pair3)(void *, void *, void *);
static float p2a2_grid[4096];
static int p2a2_gx, p2a2_gy;
static float p2a2_height(int ix, int iy) {
  if (ix >= 0 && iy >= 0 && ix < p2a2_gx && iy < p2a2_gy) return p2a2_grid[iy * p2a2_gx + ix];
  return (float)(ix * 7 + iy * 13) * 0.25f;
}
static unsigned short p2a2_surf(int ix, int iy) { (void)ix; (void)iy; return 0; }
static const struct { const char *name; uint32_t va; char a, b; int inters; } p2a2_fns[] = {
  {"p2a2_i_tri_tri", 0x35159410, 't', 't', 1}, {"p2a2_i_tri_sph", 0x35159DE0, 't', 's', 1},
  {"p2a2_i_sph_tri", 0x3515AAA0, 's', 't', 1}, {"p2a2_i_ray_tri", 0x3515AB20, 'r', 't', 1},
  {"p2a2_i_tri_ray", 0x3515B1D0, 't', 'r', 1}, {"p2a2_i_tri_box", 0x35162AA0, 't', 'b', 1},
  {"p2a2_i_box_tri", 0x35164780, 'b', 't', 1}, {"p2a2_o_box_tri", 0x351968F0, 'b', 't', 0},
  {"p2a2_o_tri_box", 0x35196CC0, 't', 'b', 0}, {"p2a2_o_tri_sph", 0x35197530, 't', 's', 0},
  {"p2a2_o_sph_tri", 0x35197590, 's', 't', 0}, {"p2a2_o_box_hf", 0x351986E0, 'b', 'h', 0},
  {"p2a2_o_hf_box", 0x35199120, 'h', 'b', 0}, {"p2a2_o_hf_sph", 0x351975E0, 'h', 's', 0},
  {"p2a2_o_sph_hf", 0x35197860, 's', 'h', 0},
};
static int p2a2_words(char k) { return k == 't' ? 12 : k == 'b' ? 16 : k == 's' ? 4 : k == 'r' ? 6 : 30; }
static void p2a2_probe(void) {
  static char line[1 << 16];
  while (fgets(line, sizeof line, stdin)) {
    char *tok = strtok(line, " \t\r\n");
    if (!tok) continue;
    int fi = -1;
    for (int i = 0; i < (int)(sizeof p2a2_fns / sizeof p2a2_fns[0]); i++)
      if (!strcmp(tok, p2a2_fns[i].name)) fi = i;
    if (fi < 0) die("unknown p2a2 probe", tok);
    static uint32_t w[8192];
    int n = 0;
    while ((tok = strtok(0, " \t\r\n")) && n < 8192) w[n++] = (uint32_t)strtoul(tok, 0, 16);
    __declspec(align(16)) uint32_t a[64], b[64], o[64];
    memset(a, 0, sizeof a); memset(b, 0, sizeof b); memset(o, 0, sizeof o);
    int k = 0;
    for (int side = 0; side < 2; side++) {
      char t = side ? p2a2_fns[fi].b : p2a2_fns[fi].a;
      uint32_t *d = side ? b : a;
      int nw = p2a2_words(t);
      if (k + nw > n) die("p2a2: short line", p2a2_fns[fi].name);
      memcpy(d, w + k, nw * 4); k += nw;
      if (t == 'h') {
        p2a2_gx = (int)w[k]; p2a2_gy = (int)w[k + 1]; k += 2;
        if (p2a2_gx * p2a2_gy > 4096 || k + p2a2_gx * p2a2_gy > n) die("p2a2: bad grid", "");
        memcpy(p2a2_grid, w + k, p2a2_gx * p2a2_gy * 4); k += p2a2_gx * p2a2_gy;
        void *cb = (void *)p2a2_height, *cs = (void *)p2a2_surf;
        memcpy((char *)d + 104, &cb, 8); memcpy((char *)d + 112, &cs, 8);
      }
    }
    if (p2a2_fns[fi].inters) {
      if (k + 24 > n) die("p2a2: missing prim_inters words", p2a2_fns[fi].name);
      memcpy(o, w + k, 24 * 4); k += 24;
    }
    /* Border buffer: words 14/15 (ptborder) == B0B0B0B0 select a zeroed 64-point buffer;
       nborderpt/nbordersz (words 16/17) are taken as given. The pointer is printed back as the
       marker and the first 16 points follow the buffer words. */
    static float border[64 * 3];
    int use_border = p2a2_fns[fi].inters && o[14] == 0xB0B0B0B0u && o[15] == 0xB0B0B0B0u;
    if (use_border) {
      float *bp = border;
      memset(border, 0, sizeof border);
      memcpy(&o[14], &bp, 8);
    }
    int r = ((fn_pair3)VA(g_phys, (uint64_t)p2a2_fns[fi].va))(a, b, o);
    if (use_border) o[14] = o[15] = 0xB0B0B0B0u;
    printf("%s in", p2a2_fns[fi].name);
    for (int i = 0; i < n; i++) printf(" %08x", w[i]);
    printf(" out %d", r);
    if (p2a2_fns[fi].inters) for (int i = 0; i < 24; i++) printf(" %08x", o[i]);
    if (use_border) {
      printf(" border");
      out_f(border, 48);
    }
    printf("\n");
  }
}

/* ------------------------------------------------------------------ pair-function probe (P2a-3) */
/* --pairfn VA NA NB FILE: call the INTERSECT-table function at VA (`int f(primA*, primB*,
   prim_inters*)`) once per input line. A line holds NA + NB + 22 hex dwords: primA, primB and the
   initial 88-byte prim_inters (dwords 14/15, the ptborder pointer, are replaced by a 64-point
   buffer pre-filled with 0x7fc0dead; dword 16 = nborderpt, 17 = nbordersz). Output per line:
   "in <the input words> out <ret> <22 dwords, ptborder as 0> b <points 0..nborderpt>". */
static void pairfn_probe(uint64_t va, int na, int nb, const char *path) {
  typedef int (*fn_pair)(void *, void *, void *);
  fn_pair f = (fn_pair)VA(g_phys, va);
  FILE *in = fopen(path, "rb");
  if (!in) die("cannot open pair input", path);
  printf("physoracle-pairfn 1 %llx %d %d\n", (unsigned long long)va, na, nb);
  char line[8192];
  while (fgets(line, sizeof line, in)) {
    uint32_t w[256];
    int n = 0;
    for (char *t = strtok(line, " \t\r\n"); t && n < 256; t = strtok(0, " \t\r\n")) w[n++] = (uint32_t)strtoul(t, 0, 16);
    if (n == 0) continue;
    if (n != na + nb + 22) die("pair input: wrong word count", path);
    __declspec(align(16)) uint32_t a[64], b[64], pi[32], border[64 * 3 + 8];
    memcpy(a, w, 4 * na);
    memcpy(b, w + na, 4 * nb);
    memcpy(pi, w + na + nb, 4 * 22);
    for (int i = 0; i < 64 * 3 + 8; i++) border[i] = 0x7fc0deadu;
    void *pb = border;
    memcpy(&pi[14], &pb, 8);
    int r = f(a, b, pi);
    printf("in");
    for (int i = 0; i < n; i++) printf(" %08x", w[i]);
    printf(" out %d", r);
    pi[14] = pi[15] = 0;
    for (int i = 0; i < 22; i++) printf(" %08x", pi[i]);
    int nb_out = (int)pi[16];
    if (nb_out < 0) nb_out = 0;
    if (nb_out > 64) nb_out = 64;
    printf(" b");
    for (int i = 0; i < 3 * nb_out; i++) printf(" %08x", border[i]);
    printf("\n");
  }
  fclose(in);
}

/* ------------------------------------------------------------------ P2b unprojection probe */
static int p4a_ext_intersect(char *line); /* P4a primitive Intersect probe */
#include "p2b_probe.c"
#include "p4a_probe_intersect.c"
#include "p4b_probe_solver.c" /* P4b contact solver probe */
#endif /* !ORACLE_X86 */

/* ------------------------------------------------------------------ main */
int main(int argc, char **argv) {
  _setmode(_fileno(stdout), _O_BINARY);
  setvbuf(stdout, 0, _IOFBF, 1 << 16);
  AddVectoredExceptionHandler(1, veh);
  const char *rt = getenv("AAEMU_PHYS_RT");
  const char *scenario = 0;
#ifdef ORACLE_X86
  const char *init = "ctors", *t0 = getenv("AAEMU_PHYS_T0");
  int fpu = 53;
  if (!rt) rt = "D:\\aaemu-target\\physics\\oracle-x86\\rt";
  int a = 1;
  while (a < argc) {
    if (!strcmp(argv[a], "--rt") && a + 1 < argc) { rt = argv[a + 1]; a += 2; }
    else if (!strcmp(argv[a], "--fpu") && a + 1 < argc) { fpu = atoi(argv[a + 1]); a += 2; }
    else if (!strcmp(argv[a], "--static-init") && a + 1 < argc) { init = argv[a + 1]; a += 2; }
    else if (!strcmp(argv[a], "--t0") && a + 1 < argc) { t0 = argv[a + 1]; a += 2; }
    else { scenario = argv[a++]; }
  }
  if (!scenario) {
    fprintf(stderr,
            "usage: physoracle_x86 [--rt DIR] [--fpu 24|53|64] [--static-init none|tls|ctors]\n"
            "                     [--t0 CAPTURE] SCENARIO|-\n");
    return 1;
  }
#else
  if (!rt) rt = "D:\\aa-re\\cryphysics\\harness\\rt";
  int a = 1;
  if (a + 1 < argc && !strcmp(argv[a], "--rt")) { rt = argv[a + 1]; a += 2; }
  if (a >= argc) {
    fprintf(stderr, "usage: physoracle [--rt DIR] SCENARIO|-\n       physoracle [--rt DIR] --math NAME COUNT SEED\n");
    return 1;
  }
  scenario = argv[a];
#endif
  char dll[MAX_PATH];
#ifdef ORACLE_X86
  snprintf(dll, sizeof dll, "%s\\cryphysics12.dll", rt);
#else
  snprintf(dll, sizeof dll, "%s\\cryphysics_dedicate.dll", rt);
#endif
  SetDllDirectoryA(rt);
  g_phys = LoadLibraryA(dll);
  if (!g_phys) { printf("ERROR LoadLibrary %s failed (%lu)\n", dll, GetLastError()); return 1; }
#ifdef ORACLE_X86
  if ((void *)g_phys != (void *)IMAGE_BASE) {
    printf("ERROR image loaded at %p, expected %p\n", (void *)g_phys, (void *)IMAGE_BASE);
    return 1;
  }
  {
    /* The unpacked image keeps the VMProtect dispatcher in .reloc (section flags: not
       executable).  The client made it executable during unpack; TimeStep calls a mutated
       thunk that lives there.  Do the same before running any engine code. */
    DWORD old = 0;
    if (!VirtualProtect((void *)g_phys, 0x75D000, PAGE_EXECUTE_READWRITE, &old))
      fprintf(stderr, "VirtualProtect image failed (%lu)\n", GetLastError());
  }
  set_fpu_precision(fpu);
  x86_static_init(init, t0);
  install_fake_env(g_phys);
  g_world = x86_create_world();
  g_gm = m_call0(g_world, W_GetGeomManager);
  fprintf(stderr, "world %p vtable=%p geom-manager %p\n", g_world, *(void **)g_world, g_gm);
#else
  if (!strcmp(argv[a], "--p2b")) { /* P2b: unprojection / helper probe, commands on stdin */
    p2b_run();
    return 0;
  }
  if (!strcmp(argv[a], "--p4b")) { /* P4b: contact solver probe, commands on stdin */
    p4b_run();
    return 0;
  }
  if (!strcmp(argv[a], "--p2a2")) { /* P2a-2 pair probes, see p2a2_probe */
    p2a2_probe();
    fflush(stdout);
    return 0;
  }
  if (!strcmp(argv[a], "--pairfn")) { /* P2a-3 probe, see pairfn_probe */
    if (a + 4 >= argc) return 1;
    pairfn_probe(strtoull(argv[a + 1], 0, 16), atoi(argv[a + 2]), atoi(argv[a + 3]), argv[a + 4]);
    fflush(stdout);
    return 0;
  }
  if (!strcmp(argv[a], "--math")) {
    if (a + 3 >= argc) return 1;
    math_probe(argv[a + 1], atoi(argv[a + 2]), strtoull(argv[a + 3], 0, 0));
    fflush(stdout);
    return 0;
  }
  g_world = ((void *(*)(void *))GetProcAddress(g_phys, "CreatePhysicalWorld"))(NULL);
  if (!g_world) { printf("ERROR CreatePhysicalWorld failed\n"); return 1; }
  install_fake_env(g_phys);
  g_gm = m_call0(g_world, W_GetGeomManager);
#endif

  FILE *f = strcmp(scenario, "-") ? fopen(scenario, "rb") : stdin;
  if (!f) die("cannot open scenario", scenario);
#ifndef ORACLE_X86
  p1c_set_scenario_path(scenario);
#endif
  printf("physoracle 1\n");
  char line[4096];
  while (fgets(line, sizeof line, f)) run_line(line);
  fflush(stdout);
  return 0;
}

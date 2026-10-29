/* smoke_x86: load the entry-point-less 1.2 cryphysics DLL at its preferred base.
 *
 * Step 1 of the 1.2 physics oracle (milestone 1): prove that LoadLibrary succeeds
 * without DllMain, that the image lands at 0x34FF0000 and that the export table
 * resolves.  The exports are VMProtect stubs and must NOT be called.
 *
 *   smoke_x86.exe [RT_DIR]
 *
 * RT_DIR defaults to %AAEMU_PHYS_RT% or D:/aaemu-target/physics/oracle-x86/rt and
 * must contain cryphysics12.dll (patch_entry.py) and the x86 runtime DLLs.
 */
#include <windows.h>
#include <stdio.h>
#include <stdlib.h>

#define IMAGE_BASE ((HMODULE)0x34FF0000)

int main(int argc, char **argv) {
  char dir[MAX_PATH], dll[MAX_PATH];
  const char *rt = argc > 1 ? argv[1] : getenv("AAEMU_PHYS_RT");
  if (!rt || !*rt) rt = "D:\\aaemu-target\\physics\\oracle-x86\\rt";
  snprintf(dir, sizeof dir, "%s", rt);
  snprintf(dll, sizeof dll, "%s\\cryphysics12.dll", rt);
  SetDllDirectoryA(dir);
  HMODULE h = LoadLibraryA(dll);
  if (!h) {
    printf("smoke FAIL LoadLibrary %s error %lu\n", dll, GetLastError());
    return 1;
  }
  {
    unsigned char *b = (unsigned char *)h;
    unsigned e = *(unsigned *)(b + 0x3C);
    unsigned size = *(unsigned *)(b + e + 24 + 56); /* SizeOfImage */
    printf("smoke base %p (want %p) size %#x\n", (void *)h, (void *)IMAGE_BASE, size);
  }
  if (h != IMAGE_BASE) {
    printf("smoke FAIL wrong base\n");
    return 1;
  }
  FARPROC cw = GetProcAddress(h, "CreatePhysicalWorld");
  FARPROC mi = GetProcAddress(h, "CryModuleGetMemoryInfo");
  printf("smoke exports CreatePhysicalWorld %p CryModuleGetMemoryInfo %p\n", (void *)cw, (void *)mi);
  if (!cw || !mi) {
    printf("smoke FAIL missing export\n");
    return 1;
  }
  /* Do not call the exports: they are VMProtect stubs (see CRYPHYSICS-MAP s1). */
  printf("smoke ok\n");
  FreeLibrary(h);
  return 0;
}

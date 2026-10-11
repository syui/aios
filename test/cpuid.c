// EL0 から ID レジスタを読むテスト (カーネル: trap.rs の emulate_mrs): aios の中で動かす。
// Linux と同じく MRS で MIDR_EL1 などを読めて (zig cc は CPU を調べるのにこれを読む)、AT_HWCAP に CPUID がある。
// MPIDR は 1 << 31、名前のない ID レジスタは 0、SVE とポインタ認証は見せない
// (aarch64-linux-gnu-gcc -O2 -static -o cpuid test/cpuid.c。aish-vm の try で file: test/cpuid.c でも)
#include <stdint.h>
#include <stdio.h>
#include <sys/auxv.h>

#define MRS(r) ({ uint64_t v; __asm__ volatile("mrs %0, " r : "=r"(v)); v; })

int main(void) {
  int fail = 0;
  if (!(getauxval(AT_HWCAP) & (1 << 11))) {
    puts("FAIL AT_HWCAP has no CPUID (bit 11)");
    fail = 1;
  }
  uint64_t midr = MRS("midr_el1");
  uint64_t mpidr = MRS("mpidr_el1");
  uint64_t pfr0 = MRS("id_aa64pfr0_el1");
  uint64_t isar0 = MRS("id_aa64isar0_el1");
  uint64_t isar1 = MRS("id_aa64isar1_el1");
  uint64_t mmfr0 = MRS("id_aa64mmfr0_el1");
  // op0 = 3, op1 = 0, CRn = 0, CRm = 7, op2 = 7: 名前のない ID レジスタ
  uint64_t none = MRS("s3_0_c0_c7_7");
  printf("midr %#lx mpidr %#lx pfr0 %#lx isar0 %#lx isar1 %#lx mmfr0 %#lx\n",
         (unsigned long)midr, (unsigned long)mpidr, (unsigned long)pfr0,
         (unsigned long)isar0, (unsigned long)isar1, (unsigned long)mmfr0);
  if (midr == 0 || (midr >> 24) == 0) {
    puts("FAIL MIDR_EL1 has no implementer");
    fail = 1;
  }
  if (mpidr != 1UL << 31) {
    puts("FAIL MPIDR_EL1 is not 1 << 31 (as Linux)");
    fail = 1;
  }
  if (((pfr0 >> 16) & 0xf) == 0xf || ((pfr0 >> 32) & 0xf) != 0) {
    puts("FAIL ID_AA64PFR0: FP missing, or SVE shown");
    fail = 1;
  }
  if ((isar1 & (0xffUL << 4 | 0xffUL << 24)) != 0) {
    puts("FAIL ID_AA64ISAR1 shows pointer authentication");
    fail = 1;
  }
  if (none != 0) {
    puts("FAIL an unnamed ID register is not 0");
    fail = 1;
  }
  puts(fail ? "cpuid: FAIL" : "cpuid: ok");
  return fail;
}

// EL0 から ID レジスタを読むテスト (カーネル: trap.rs の emulate_mrs): aios の中で動かす。
// Linux と同じく MRS で MIDR_EL1 などを読めて (zig cc は CPU を調べるのにこれを読む)、AT_HWCAP に CPUID がある。
// MPIDR は 1 << 31、名前のない ID レジスタは 0、SVE とポインタ認証は見せない。
// AT_HWCAP と AT_HWCAP2 と /proc/cpuinfo の Features が、MRS で読んだ ID レジスタと合う (Linux の arm64_elf_hwcaps と
// 同じ決まり。下の表のものだけ見る: Linux はほかにも evtstrm や sve を出すことがある)
// (aarch64-linux-gnu-gcc -O2 -static -o cpuid test/cpuid.c。aish-vm の try で file: test/cpuid.c でも)
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/auxv.h>

#ifndef AT_HWCAP2
#define AT_HWCAP2 26
#endif

#define MRS(r) ({ uint64_t v; __asm__ volatile("mrs %0, " r : "=r"(v)); v; })

// (名前, AT_HWCAP2 か, ビット, レジスタ, 欄の位置, 最小の値, 符号つき)。レジスタ: 0 PFR0 1 ISAR0 2 ISAR1 3 ISAR2
// 4 MMFR0 5 MMFR1 6 MMFR2
static const struct cap {
  const char *name;
  int word2, bit, reg, shift, min, sign;
} caps[] = {
    {"fp", 0, 0, 0, 16, 0, 1},       {"asimd", 0, 1, 0, 20, 0, 1},      {"aes", 0, 3, 1, 4, 1, 0},
    {"pmull", 0, 4, 1, 4, 2, 0},     {"sha1", 0, 5, 1, 8, 1, 0},        {"sha2", 0, 6, 1, 12, 1, 0},
    {"crc32", 0, 7, 1, 16, 1, 0},    {"atomics", 0, 8, 1, 20, 2, 0},    {"fphp", 0, 9, 0, 16, 1, 1},
    {"asimdhp", 0, 10, 0, 20, 1, 1}, {"asimdrdm", 0, 12, 1, 28, 1, 0},  {"jscvt", 0, 13, 2, 12, 1, 0},
    {"fcma", 0, 14, 2, 16, 1, 0},    {"lrcpc", 0, 15, 2, 20, 1, 0},     {"dcpop", 0, 16, 2, 0, 1, 0},
    {"sha3", 0, 17, 1, 32, 1, 0},    {"sm3", 0, 18, 1, 36, 1, 0},       {"sm4", 0, 19, 1, 40, 1, 0},
    {"asimddp", 0, 20, 1, 44, 1, 0}, {"sha512", 0, 21, 1, 12, 2, 0},    {"asimdfhm", 0, 23, 1, 48, 1, 0},
    {"dit", 0, 24, 0, 48, 1, 0},     {"uscat", 0, 25, 6, 32, 1, 0},     {"ilrcpc", 0, 26, 2, 20, 2, 0},
    {"flagm", 0, 27, 1, 52, 1, 0},   {"sb", 0, 29, 2, 36, 1, 0},        {"dcpodp", 1, 0, 2, 0, 2, 0},
    {"flagm2", 1, 7, 1, 52, 2, 0},   {"frint", 1, 8, 2, 32, 1, 0},      {"i8mm", 1, 13, 2, 52, 1, 0},
    {"bf16", 1, 14, 2, 44, 1, 0},    {"dgh", 1, 15, 2, 48, 1, 0},       {"rng", 1, 16, 1, 60, 1, 0},
    {"ecv", 1, 19, 4, 60, 1, 0},     {"afp", 1, 20, 5, 44, 1, 0},       {"rpres", 1, 21, 3, 4, 1, 0},
    {"ebf16", 1, 32, 2, 44, 2, 0},   {"cssc", 1, 34, 3, 52, 1, 0},      {"rprfm", 1, 35, 3, 48, 1, 0},
    {"hbc", 1, 44, 3, 20, 1, 0},     {"lrcpc3", 1, 46, 2, 20, 3, 0},    {"lse128", 1, 47, 1, 20, 3, 0},
};

// Features の行に name があるか (空白で区切った語として)
static int in_features(const char *line, const char *name) {
  size_t n = strlen(name);
  for (const char *p = line; (p = strstr(p, name)); p += n)
    if ((p == line || p[-1] == ' ' || p[-1] == '\t') && (p[n] == ' ' || p[n] == '\n' || p[n] == 0)) return 1;
  return 0;
}

// 表のものが、ID レジスタ、AT_HWCAP / AT_HWCAP2、/proc/cpuinfo の 3 つで合うか
static int check_caps(uint64_t pfr0, uint64_t isar0, uint64_t isar1) {
  uint64_t regs[7] = {pfr0, isar0, isar1, MRS("s3_0_c0_c6_2"), MRS("id_aa64mmfr0_el1"), MRS("id_aa64mmfr1_el1"), MRS("id_aa64mmfr2_el1")};
  uint64_t hw[2] = {getauxval(AT_HWCAP), getauxval(AT_HWCAP2)};
  char line[4096] = "";
  FILE *f = fopen("/proc/cpuinfo", "r");
  while (f && fgets(line, sizeof line, f))
    if (!strncmp(line, "Features", 8)) break;
  if (f) fclose(f);
  if (strncmp(line, "Features", 8)) {
    puts("FAIL /proc/cpuinfo has no Features line");
    return 1;
  }
  int fail = 0, n = 0;
  printf("features:");
  for (size_t i = 0; i < sizeof caps / sizeof caps[0]; i++) {
    const struct cap *c = &caps[i];
    int v = (regs[c->reg] >> c->shift) & 0xf;
    int want = c->sign && v == 0xf ? 0 : v >= c->min;
    int aux = (hw[c->word2] >> c->bit) & 1;
    int txt = in_features(line, c->name);
    if (want) printf(" %s", c->name), n++;
    if (aux != want || txt != want) {
      printf("\nFAIL %s: ID register says %d, AT_HWCAP%s %d, /proc/cpuinfo %d\n", c->name, want, c->word2 ? "2" : "", aux, txt);
      fail = 1;
    }
  }
  printf(" (%d)\n", n);
  return fail;
}

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
  // Architecture の欄はいつも 0xf (implementer は 0 のこともある: QEMU の max)
  if (((midr >> 16) & 0xf) != 0xf) {
    puts("FAIL MIDR_EL1 has no architecture field 0xf");
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
  fail |= check_caps(pfr0, isar0, isar1);
  puts(fail ? "cpuid: FAIL" : "cpuid: ok");
  return fail;
}

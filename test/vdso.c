// vDSO (AT_SYSINFO_EHDR) を、Go の runtime (vdso_linux.go) と musl と同じやり方で読んで、中身を出す。
// Go は DT_VERDEF があるつもりで版の表を読む (なければ番地 0x2 を読んで SIGSEGV。gitea が起きなかった)。
// 名前と版 (LINUX_2.6.39)、版の表 (verdef と versym)、soname、それで見つけた __kernel_clock_gettime を呼ぶ
// (Linux 7.4 には __kernel_getrandom (vgetrandom) もある。aios にはまだなく、glibc と Go はシステムコールの getrandom にする)。
// PT_NOTE の「Linux」の注釈 (種類 0) の版も (glibc は起きるときにここからカーネルの版を読み、古ければ kernel too old で止まる。
// aios は手本の Linux の版を書く)
// test/ldiff.sh vdso で Linux とくらべる
#include <elf.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <unistd.h>
#include <time.h>

static const char *tag(Elf64_Sxword t) {
  switch (t) {
  case DT_HASH: return "HASH";
  case DT_GNU_HASH: return "GNU_HASH";
  case DT_STRTAB: return "STRTAB";
  case DT_SYMTAB: return "SYMTAB";
  case DT_STRSZ: return "STRSZ";
  case DT_SYMENT: return "SYMENT";
  case DT_SONAME: return "SONAME";
  case DT_VERSYM: return "VERSYM";
  case DT_VERDEF: return "VERDEF";
  case DT_VERDEFNUM: return "VERDEFNUM";
  default: return NULL;
  }
}

static unsigned long elf_hash(const char *s) {
  unsigned long h = 0, g;
  while (*s) {
    h = (h << 4) + (unsigned char)*s++;
    g = h & 0xf0000000;
    if (g) h ^= g >> 24;
    h &= ~g;
  }
  return h;
}

int main(void) {
  int bad = 0; // CI (vm-ctests) のため: 版の表、名前、呼んだ答えのどれかがおかしければ 1
  uintptr_t base = getauxval(AT_SYSINFO_EHDR);
  printf("vdso %s\n", base ? "yes" : "no");
  if (!base) return 1;
  Elf64_Ehdr *eh = (void *)base;
  printf("magic %d class %d type %d machine %d\n", memcmp(eh->e_ident, ELFMAG, SELFMAG) == 0, eh->e_ident[EI_CLASS], eh->e_type, eh->e_machine);
  Elf64_Phdr *ph = (void *)(base + eh->e_phoff);
  uintptr_t load = 0;
  int found = 0, linux_note = 0;
  Elf64_Dyn *dyn = NULL;
  for (int i = 0; i < eh->e_phnum; i++) {
    if (ph[i].p_type == PT_LOAD && !found) {
      found = 1;
      load = base + ph[i].p_offset - ph[i].p_vaddr;
    }
    if (ph[i].p_type == PT_DYNAMIC) dyn = (void *)(base + ph[i].p_offset);
    // 注釈 (Elf64_Nhdr、名前と中身は 4 にそろえる)。glibc の _dl_discover_osversion と同じ読み方
    if (ph[i].p_type == PT_NOTE) {
      char *n = (char *)(base + ph[i].p_offset), *end = n + ph[i].p_filesz;
      while (n + sizeof(Elf64_Nhdr) <= end) {
        Elf64_Nhdr *h = (void *)n;
        char *name = n + sizeof *h, *desc = name + ((h->n_namesz + 3) & ~3);
        if (h->n_namesz == 6 && h->n_type == 0 && h->n_descsz == 4 && memcmp(name, "Linux", 6) == 0) {
          uint32_t v = *(uint32_t *)desc;
          printf("note Linux %u.%u\n", v >> 16, (v >> 8) & 0xff);
          linux_note = v >= (3 << 16 | 7 << 8);
        }
        n = desc + ((h->n_descsz + 3) & ~3);
      }
    }
  }
  printf("load %d dynamic %d\n", found, dyn != NULL);
  bad |= !linux_note;
  if (!found || !dyn) return 1;
  const char *str = NULL;
  Elf64_Sym *sym = NULL;
  uint32_t *hash = NULL;
  uint16_t *versym = NULL;
  Elf64_Verdef *verdef = NULL;
  long soname = -1;
  for (Elf64_Dyn *d = dyn; d->d_tag != DT_NULL; d++) {
    const char *n = tag(d->d_tag);
    if (n) printf("dt %s\n", n);
    uintptr_t p = load + d->d_un.d_ptr;
    switch (d->d_tag) {
    case DT_STRTAB: str = (void *)p; break;
    case DT_SYMTAB: sym = (void *)p; break;
    case DT_HASH: hash = (void *)p; break;
    case DT_VERSYM: versym = (void *)p; break;
    case DT_VERDEF: verdef = (void *)p; break;
    case DT_SONAME: soname = d->d_un.d_val; break;
    }
  }
  if (!str || !sym) return 1;
  if (soname >= 0) printf("soname %s\n", str + soname);
  // 版の表: 番号、印 (BASE)、名前、hash が名前の ELF hash と合うか
  int want = -1;
  for (Elf64_Verdef *v = verdef; v; v = v->vd_next ? (void *)((char *)v + v->vd_next) : NULL) {
    Elf64_Verdaux *a = (void *)((char *)v + v->vd_aux);
    const char *n = str + a->vda_name;
    printf("verdef version %d ndx %d flags %s cnt %d name %s hash %s\n", v->vd_version, v->vd_ndx, v->vd_flags & VER_FLG_BASE ? "BASE" : "0", v->vd_cnt, n,
           v->vd_hash == elf_hash(n) ? "ok" : "bad");
    if (!(v->vd_flags & VER_FLG_BASE) && strcmp(n, "LINUX_2.6.39") == 0) want = v->vd_ndx & 0x7fff;
  }
  // 名前 (DT_HASH の nchain がシンボルの数)。__kernel_ のものだけを、名前の順でなく表の順で
  void *clock_gettime_fn = NULL, *gettimeofday_fn = NULL, *getres_fn = NULL;
  if (hash) {
    uint32_t nchain = hash[1];
    for (uint32_t i = 1; i < nchain; i++) {
      const char *n = str + sym[i].st_name;
      if (strncmp(n, "__kernel_", 9)) continue;
      int ver = versym ? versym[i] & 0x7fff : -1;
      printf("sym %s type %d bind %d defined %d version %s\n", n, ELF64_ST_TYPE(sym[i].st_info), ELF64_ST_BIND(sym[i].st_info), sym[i].st_shndx != SHN_UNDEF,
             ver < 0 ? "none" : ver == want ? "LINUX_2.6.39" : "other");
      // DT_HASH でも引けるか (Go と musl の引き方)
      uint32_t nb = hash[0], *bucket = hash + 2, *chain = bucket + nb;
      int hit = 0;
      for (uint32_t c = bucket[elf_hash(n) % nb]; c; c = chain[c])
        if (c == i) hit = 1;
      printf("  hash lookup %d\n", hit);
      bad |= !hit;
      void *fn = (void *)(load + sym[i].st_value);
      if (ver != want) continue;
      if (strcmp(n, "__kernel_clock_gettime") == 0) clock_gettime_fn = fn;
      if (strcmp(n, "__kernel_gettimeofday") == 0) gettimeofday_fn = fn;
      if (strcmp(n, "__kernel_clock_getres") == 0) getres_fn = fn;
    }
  }
  // 見つけたものを呼んで、システムコールの答えとくらべる
  if (clock_gettime_fn) {
    int (*f)(clockid_t, struct timespec *) = clock_gettime_fn;
    clockid_t ids[] = {CLOCK_REALTIME, CLOCK_MONOTONIC, CLOCK_MONOTONIC_RAW, CLOCK_BOOTTIME, CLOCK_PROCESS_CPUTIME_ID};
    for (unsigned k = 0; k < sizeof ids / sizeof ids[0]; k++) {
      struct timespec a, b;
      int r = f(ids[k], &a);
      syscall(SYS_clock_gettime, ids[k], &b);
      long long d = (long long)b.tv_sec - a.tv_sec;
      int ok = r == 0 && d >= 0 && d < 2 && a.tv_nsec < 1000000000;
      printf("call clock_gettime %d: %d sane %d\n", (int)ids[k], r, ok);
      bad |= !ok;
    }
  }
  if (gettimeofday_fn) {
    int (*f)(struct timeval *, void *) = gettimeofday_fn;
    struct timeval tv;
    struct timezone tz;
    struct timespec b;
    int r = f(&tv, NULL);
    syscall(SYS_clock_gettime, CLOCK_REALTIME, &b);
    long long d = (long long)b.tv_sec - tv.tv_sec;
    int ok = r == 0 && d >= 0 && d < 2 && tv.tv_usec < 1000000;
    printf("call gettimeofday %d sane %d\n", r, ok);
    bad |= !ok;
    printf("call gettimeofday tz %d\n", f(&tv, &tz));
    printf("call gettimeofday null %d\n", f(NULL, NULL));
  }
  if (getres_fn) {
    int (*f)(clockid_t, struct timespec *) = getres_fn;
    struct timespec a = {0}, b = {0};
    int r = f(CLOCK_MONOTONIC, &a);
    syscall(SYS_clock_getres, CLOCK_MONOTONIC, &b);
    int ok = r == 0 && a.tv_sec == b.tv_sec && a.tv_nsec == b.tv_nsec;
    printf("call clock_getres %d same %d\n", r, ok);
    bad |= !ok;
  }
  bad |= want < 0 || !clock_gettime_fn || !gettimeofday_fn || !getres_fn;
  return bad;
}

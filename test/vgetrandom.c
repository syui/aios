// vDSO の __kernel_getrandom (Linux の lib/vdso/getrandom.c) と MAP_DROPPABLE が Linux と同じか:
//   params    (NULL, 0, 0, &params, ~0) で状態の大きさと mmap の prot と flags (MAP_DROPPABLE | MAP_ANONYMOUS)、reserved は 0
//   random    状態を渡すと len バイト (2 回でちがう)、0 バイトは 0、大きさのちがう状態でも (システムコールへ)、知らない flags は
//             -EINVAL (vDSO はエラーを -errno で返す)、GRND_NONBLOCK、ページをまたぐ状態は -EFAULT
//   droppable MAP_DROPPABLE (型 8) の無名のメモリ。fork した子では 0 (VM_WIPEONFORK)、ファイルとは EINVAL、型 0・9 と
//             無名の MAP_SHARED_VALIDATE (3) は EINVAL
#define _GNU_SOURCE
#include <elf.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef MAP_DROPPABLE
#define MAP_DROPPABLE 0x08
#endif

struct params {
  uint32_t size, prot, flags, reserved[13];
};
typedef long (*vgr_t)(void *, size_t, unsigned, void *, size_t);

static int fails;
static void check(const char *name, int ok) {
  printf("%s %s\n", ok ? "ok" : "FAIL", name);
  if (!ok)
    fails++;
}

static void *vdso_sym(const char *want) {
  uintptr_t base = getauxval(AT_SYSINFO_EHDR);
  if (!base)
    return NULL;
  Elf64_Ehdr *eh = (void *)base;
  Elf64_Phdr *ph = (void *)(base + eh->e_phoff);
  uintptr_t load = 0;
  Elf64_Dyn *dyn = NULL;
  int found = 0;
  for (int i = 0; i < eh->e_phnum; i++) {
    if (ph[i].p_type == PT_LOAD && !found) {
      found = 1;
      load = base + ph[i].p_offset - ph[i].p_vaddr;
    }
    if (ph[i].p_type == PT_DYNAMIC)
      dyn = (void *)(base + ph[i].p_offset);
  }
  if (!dyn)
    return NULL;
  const char *str = NULL;
  Elf64_Sym *sym = NULL;
  uint32_t *hash = NULL;
  for (Elf64_Dyn *d = dyn; d->d_tag != DT_NULL; d++) {
    uintptr_t p = load + d->d_un.d_ptr;
    if (d->d_tag == DT_STRTAB)
      str = (void *)p;
    if (d->d_tag == DT_SYMTAB)
      sym = (void *)p;
    if (d->d_tag == DT_HASH)
      hash = (void *)p;
  }
  if (!str || !sym || !hash)
    return NULL;
  for (uint32_t i = 1; i < hash[1]; i++)
    if (!strcmp(str + sym[i].st_name, want))
      return (void *)(load + sym[i].st_value);
  return NULL;
}

static int mm(int flags, int fd) {
  void *p = mmap(NULL, 4096, PROT_READ | PROT_WRITE, flags, fd, 0);
  if (p == MAP_FAILED)
    return -errno;
  munmap(p, 4096);
  return 0;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  vgr_t vgr = (vgr_t)vdso_sym("__kernel_getrandom");
  printf("__kernel_getrandom %s\n", vgr ? "found" : "missing");
  if (vgr) {
    struct params pa;
    memset(&pa, 0xff, sizeof pa);
    long r = vgr(NULL, 0, 0, &pa, ~0UL);
    int zero = 1;
    for (int i = 0; i < 13; i++)
      zero &= pa.reserved[i] == 0;
    printf("params (%ld, size %u, prot %#x, flags %#x, reserved zero %d)\n", r, pa.size, pa.prot, pa.flags, zero);
    char *page = mmap(NULL, 4096, pa.prot, pa.flags, -1, 0);
    check("mmap with the params", page != MAP_FAILED);
    if (page != MAP_FAILED) {
      unsigned char a[32], b[32];
      long r1 = vgr(a, sizeof a, 0, page, pa.size), r2 = vgr(b, sizeof b, 0, page, pa.size);
      long r3 = vgr(a, 0, 0, page, pa.size);
      long r4 = vgr(a, sizeof a, 0, page, pa.size + 1);
      long r5 = vgr(a, sizeof a, 0x100, page, pa.size);
      long r6 = vgr(a, sizeof a, 1, page, pa.size);
      long r7 = vgr(a, sizeof a, 0, page + 4096 - 8, pa.size);
      printf("random (%ld %ld differ %d, 0 bytes %ld, wrong size %ld, flag 0x100 %ld, GRND_NONBLOCK %ld, across the page %ld)\n", r1, r2, memcmp(a, b, 32) != 0, r3, r4, r5, r6, r7);
      check("random", r1 == 32 && r2 == 32 && r3 == 0 && r4 == 32 && r5 == -EINVAL && r6 == 32 && r7 == -EFAULT);
    }
  }
  // MAP_DROPPABLE
  {
    char *d = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_DROPPABLE | MAP_ANONYMOUS, -1, 0);
    int ok = d != MAP_FAILED;
    if (ok)
      d[0] = 0x55;
    int p[2];
    if (pipe(p))
      return 1;
    pid_t k = fork();
    if (k == 0) {
      char v = ok ? d[0] : -1;
      if (write(p[1], &v, 1) != 1)
        _exit(1);
      _exit(0);
    }
    char child = -1;
    if (read(p[0], &child, 1) != 1)
      child = -2;
    waitpid(k, NULL, 0);
    int fd = open("/proc/self/exe", O_RDONLY);
    printf("droppable (mmap %d, parent %#x, child %#x, with a file %d, type 0 %d, type 9 %d, anonymous type 3 %d, private %d)\n", ok, ok ? d[0] & 0xff : 0, child & 0xff,
           mm(MAP_DROPPABLE, fd), mm(MAP_ANONYMOUS, -1), mm(9 | MAP_ANONYMOUS, -1), mm(3 | MAP_ANONYMOUS, -1), mm(MAP_PRIVATE | MAP_ANONYMOUS, -1));
    check("droppable", ok && d[0] == 0x55 && child == 0);
  }
  printf("vgetrandom: %d failed\n", fails);
  return fails != 0;
}

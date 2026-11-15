// /proc/PID/mem、pagemap、map_files が Linux と同じか:
//   mem        自分の変数を pread / pwrite、書けない写し (const) と PROT_NONE と共有で書けないファイルの写しは EIO (手本の
//              Linux は FOLL_FORCE を使わない: PROC_MEM_NO_FORCE)、写っていないところと 0 も EIO、ページの終わりをまたぐ
//              読みは読めたところまで、子のものを読む、init のものは開けない (EACCES)
//   pagemap    8 バイトずつ (そろっていなければ EINVAL)、さわったページは present と exclusive、さわっていないものは 0、
//              ファイルのページは file、fork したあとの共有の無名のページは exclusive でない、PFN は 0 (root でない)
//   map_files  ファイルを写した領域ごとに start-end (maps と同じ数と名前)、lstat は lr-------- のリンク、readlink はできる
//              が、たどって開くのは EPERM (CAP_SYS_ADMIN がない)、まちがった名前 (0x、頭の 0) は ENOENT
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

static int fails;
static void check(const char *name, int ok) {
  printf("%s %s\n", ok ? "ok" : "FAIL", name);
  if (!ok)
    fails++;
}

static long g_var = 0x1122334455667788;
static const long g_const = 0x0102030405060708;

static uint64_t pm(int fd, void *va) {
  uint64_t e = 0;
  if (pread(fd, &e, 8, (uintptr_t)va / 4096 * 8) != 8)
    return ~0ull;
  return e;
}

static const char *bits(uint64_t e) {
  static char b[8][96];
  static int k;
  char *s = b[k++ % 8];
  if (e == ~0ull)
    return "read failed";
  snprintf(s, 96, "present %d swapped %d file %d exclusive %d pfn %s", (int)(e >> 63 & 1), (int)(e >> 62 & 1), (int)(e >> 61 & 1), (int)(e >> 56 & 1),
           (e & ((1ull << 55) - 1)) ? "set" : "0");
  return s;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  // mem
  {
    int fd = open("/proc/self/mem", O_RDWR);
    long v = 0;
    ssize_t a = pread(fd, &v, sizeof v, (uintptr_t)&g_var);
    long nv = 0x5555;
    ssize_t b = pwrite(fd, &nv, sizeof nv, (uintptr_t)&g_var);
    long c = 0x7777;
    errno = 0;
    ssize_t d = pwrite(fd, &c, sizeof c, (uintptr_t)&g_const);
    int de = errno;
    long seen = *(volatile const long *)&g_const;
    errno = 0;
    ssize_t e = pread(fd, &v, sizeof v, 0);
    int ee = errno;
    void *hole = mmap(NULL, 8192, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    munmap((char *)hole + 4096, 4096);
    errno = 0;
    ssize_t f = pread(fd, &v, sizeof v, (uintptr_t)hole + 4096);
    int fe = errno;
    // ページの終わりをまたぐ読み: 読めたところまで
    char buf[16];
    ssize_t g = pread(fd, buf, 16, (uintptr_t)hole + 4096 - 8);
    char *none = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    none[0] = 42;
    mprotect(none, 4096, PROT_NONE);
    char nb = 0;
    errno = 0;
    ssize_t h = pread(fd, &nb, 1, (uintptr_t)none);
    int he = errno;
    int tf = open("/proc/self/exe", O_RDONLY);
    char *shared = mmap(NULL, 4096, PROT_READ, MAP_SHARED, tf, 0);
    errno = 0;
    ssize_t i = pwrite(fd, "X", 1, (uintptr_t)shared);
    int ie = errno;
    printf("mem (read %zd, write %zd now %#lx, write const %zd errno %d now %#lx, at 0 %zd errno %d, hole %zd errno %d, across the end %zd, PROT_NONE %zd errno %d, "
           "shared read-only %zd errno %d)\n",
           a, b, g_var, d, de, seen, e, ee, f, fe, g, h, he, i, ie);
    pid_t k = fork();
    if (k == 0) {
      g_var = 0x4242;
      pause();
      _exit(0);
    }
    usleep(100000);
    char p[64];
    snprintf(p, sizeof p, "/proc/%d/mem", k);
    int cf = open(p, O_RDONLY);
    long cv = 0;
    ssize_t j = pread(cf, &cv, sizeof cv, (uintptr_t)&g_var);
    printf("child mem (open %d, read %zd, value %#lx)\n", cf >= 0, j, cv);
    kill(k, 9);
    waitpid(k, NULL, 0);
    errno = 0;
    int initf = open("/proc/1/mem", O_RDONLY);
    printf("init mem (open %d errno %d)\n", initf, errno);
    check("mem", a == 8 && b == 8 && g_var == 0x5555 && d == -1 && de == EIO && e == -1 && ee == EIO && f == -1 && fe == EIO && g == 8);
  }
  // pagemap
  {
    int fd = open("/proc/self/pagemap", O_RDONLY);
    char *a = mmap(NULL, 3 * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    a[0] = 1;
    uint64_t e;
    errno = 0;
    ssize_t r1 = pread(fd, &e, 8, 3);
    int e1 = errno;
    errno = 0;
    ssize_t r2 = pread(fd, &e, 4, 0);
    int e2 = errno;
    printf("pagemap (offset 3: %zd errno %d, count 4: %zd errno %d)\n", r1, e1, r2, e2);
    printf("pagemap touched: %s\n", bits(pm(fd, a)));
    printf("pagemap untouched: %s\n", bits(pm(fd, a + 4096)));
    // ほかに写していないファイル (プログラムのファイルは、Linux はプログラムの写しとページを分けあうが、aios は exec で
    // 自分のページに読む)
    int tf = open("/tmp/procmem.map", O_RDWR | O_CREAT | O_TRUNC, 0600);
    char zero[4096] = {1};
    if (write(tf, zero, sizeof zero) != sizeof zero)
      return 1;
    volatile char *file = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, tf, 0);
    (void)file[0];
    // exclusive は見ない: aios の tmpfs のページは、ファイルシステムと写しの両方が参照を持つので、参照の数からは
    // 写している数が出ない (doc/aios.md)
    uint64_t fe = pm(fd, (void *)file);
    printf("pagemap file: present %d file %d\n", (int)(fe >> 63 & 1), (int)(fe >> 61 & 1));
    unlink("/tmp/procmem.map");
    a[4096 * 2] = 1;
    pid_t k = fork();
    if (k == 0) {
      usleep(300000);
      _exit(0);
    }
    printf("pagemap after fork: %s\n", bits(pm(fd, a + 8192)));
    waitpid(k, NULL, 0);
    uint64_t far;
    // va 1 << 55: どの VA の大きさ (39、48、52 ビット) よりも上で、タグ (上の 8 ビット) より下
    ssize_t r3 = pread(fd, &far, 8, (off_t)1 << 46);
    // 1 << 52 は va 1 << 61: arm64 は上の 8 ビット (タグ) を外して見る (untagged_addr_remote) ので 0 のページ
    ssize_t r4 = pread(fd, &far, 8, (off_t)1 << 52);
    printf("pagemap beyond the end %zd, in the tag bits %zd (%s)\n", r3, r4, bits(far));
  }
  // map_files
  {
    char *maps = malloc(1 << 16);
    int mf = open("/proc/self/maps", O_RDONLY);
    ssize_t n = read(mf, maps, (1 << 16) - 1);
    maps[n > 0 ? n : 0] = 0;
    int files = 0;
    char first[64] = "";
    for (char *l = maps; l && *l; l = strchr(l, '\n') ? strchr(l, '\n') + 1 : NULL) {
      unsigned long s, e, off;
      char perm[8], dev[16];
      unsigned long ino;
      if (sscanf(l, "%lx-%lx %7s %lx %15s %lu", &s, &e, perm, &off, dev, &ino) == 6 && ino != 0) {
        if (!files)
          snprintf(first, sizeof first, "%lx-%lx", s, e);
        files++;
      }
    }
    DIR *d = opendir("/proc/self/map_files");
    int ents = 0, has_first = 0;
    struct dirent *de;
    while (d && (de = readdir(d)))
      if (de->d_name[0] != '.') {
        ents++;
        has_first |= !strcmp(de->d_name, first);
      }
    if (d)
      closedir(d);
    char p[128], link[256];
    snprintf(p, sizeof p, "/proc/self/map_files/%s", first);
    struct stat st;
    int ls = lstat(p, &st) ? -errno : 0;
    errno = 0;
    ssize_t rl = readlink(p, link, sizeof link);
    int rle = errno;
    errno = 0;
    int of = open(p, O_RDONLY);
    int ofe = errno;
    struct stat dst;
    stat("/proc/self/map_files", &dst);
    int bad1 = lstat("/proc/self/map_files/0x400000-0x401000", &st) ? -errno : 0;
    char lead[80];
    snprintf(lead, sizeof lead, "/proc/self/map_files/0%s", first);
    int bad2 = lstat(lead, &st) ? -errno : 0;
    lstat(p, &st);
    printf("map_files (dir %o, entries are the file maps %d, first listed %d, lstat %d mode %o link %d, readlink ok %d errno %d starts with / %d, open %d errno %d, 0x %d, leading 0 %d)\n",
           dst.st_mode & 07777, ents == files, has_first, ls, st.st_mode & 07777, S_ISLNK(st.st_mode), rl > 0, rle, rl > 0 && link[0] == '/', of, ofe, bad1, bad2);
  }
  printf("procmem: %d failed\n", fails);
  return fails != 0;
}

// 手本の Linux のまちがい (test/sysdiff.c の linux_bugs) のうち、ファイルのものを小さく起こす。Linux の新しい版で直ったかを
// 見るため (直ったら sysdiff -r とふつうの答えが同じになり、linux_bugs から外す):
//   zero-range-eof  ext4 の fallocate ZERO_RANGE が、ディスクにある終わりのブロックをまるごとふくむと、そのブロックの
//                   ファイルの終わりまでが 0 にならない (7.2.11〜7.4-rc4。7.4-rc5 の f0872c44e で直る)。tmpfs は
//                   ZERO_RANGE がない (Linux は EOPNOTSUPP: aios がえらんだちがい zero-range-tmpfs) ので見ない
//   efault-extend   ファイルの終わりより先への write / pwrite / pwritev2 が、さわれない入れ物で EFAULT になっても、
//                   ファイルがその位置まで伸びる (ext4 は 7.4-rc5 の e5db68ec4 で直る。tmpfs はまだ)
//   tmpfs-extend    写し (mmap) でファイルの終わりの先 (同じページの中) に書いたものが、ftruncate で伸ばすと中身になる
//                   (tmpfs。ext4 は 0)
// 引数はディレクトリ (ext4 と tmpfs: test/ldiff.sh fsbugs @home /tmp)。1 行ずつ、正しければ ok、ちがえば Linux の
// まちがいの形を出す。終わりの番号は ok でなかった数
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/uio.h>
#include <unistd.h>

#define TMPFS_MAGIC 0x01021994
#define EXT4_MAGIC 0xEF53

static int bad;

static void say(const char *fs, const char *what, int ok, const char *detail) {
  printf("%-6s %-28s %s\n", fs, what, ok ? "ok" : detail);
  if (!ok)
    bad++;
}

// 0 だけか
static int zeros(const char *b, size_t n) {
  for (size_t i = 0; i < n; i++)
    if (b[i])
      return 0;
  return 1;
}

static void zero_range_eof(const char *fs, const char *dir) {
  char p[512];
  snprintf(p, sizeof p, "%s/.fsbugs-zr", dir);
  int fd = open(p, O_CREAT | O_TRUNC | O_RDWR, 0600);
  char b[4096];
  memset(b, 0x11, 512);
  if (pwrite(fd, b, 512, 0) != 512 || fsync(fd) < 0) {
    say(fs, "zero-range-eof", 0, strerror(errno));
    goto out;
  }
  posix_fadvise(fd, 0, 0, POSIX_FADV_DONTNEED);
  // 終わりのブロック (0..4096) をまるごとふくむ ZERO_RANGE
  if (fallocate(fd, FALLOC_FL_ZERO_RANGE, 0, 4096) < 0) {
    say(fs, "zero-range-eof", 0, strerror(errno));
    goto out;
  }
  struct stat st;
  fstat(fd, &st);
  memset(b, 0x55, sizeof b);
  ssize_t n = pread(fd, b, sizeof b, 0);
  say(fs, "zero-range-eof read", n == 4096 && zeros(b, 4096) && st.st_size == 4096, "old data below the old end");
  // 書き出して落としてから読む (よごれたページが古い中身のまま書かれると、ディスクにも残る)
  fsync(fd);
  posix_fadvise(fd, 0, 0, POSIX_FADV_DONTNEED);
  memset(b, 0x55, sizeof b);
  n = pread(fd, b, sizeof b, 0);
  say(fs, "zero-range-eof after sync", n == 4096 && zeros(b, 4096), "old data on the disk");
out:
  close(fd);
  unlink(p);
}

static void efault_extend(const char *fs, const char *dir) {
  char p[512];
  snprintf(p, sizeof p, "%s/.fsbugs-ef", dir);
  char *no = mmap(NULL, 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  for (int k = 0; k < 3; k++) {
    int fd = open(p, O_CREAT | O_TRUNC | O_RDWR, 0600);
    ssize_t r;
    const char *what;
    if (k == 0) {
      what = "efault-extend pwrite";
      r = pwrite(fd, no, 4096, 1 << 20);
    } else if (k == 1) {
      what = "efault-extend write";
      lseek(fd, 1 << 20, SEEK_SET);
      r = write(fd, no, 4096);
    } else {
      what = "efault-extend pwritev2";
      struct iovec v = {no, 4096};
      r = pwritev2(fd, &v, 1, 1 << 20, 0);
    }
    int e = errno;
    struct stat st;
    fstat(fd, &st);
    char d[64];
    snprintf(d, sizeof d, "%s, size %ld", r < 0 ? (e == EFAULT ? "EFAULT" : strerror(e)) : "wrote", (long)st.st_size);
    say(fs, what, r < 0 && e == EFAULT && st.st_size == 0, d);
    close(fd);
  }
  munmap(no, 4096);
  unlink(p);
}

static void tmpfs_extend(const char *fs, const char *dir) {
  char p[512];
  snprintf(p, sizeof p, "%s/.fsbugs-te", dir);
  int fd = open(p, O_CREAT | O_TRUNC | O_RDWR, 0600);
  char b[100];
  memset(b, 0x22, sizeof b);
  pwrite(fd, b, 100, 0);
  char *m = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  // 終わりの先 (同じページの中) に写しで書く
  memset(m + 100, 0x33, 100);
  munmap(m, 4096);
  ftruncate(fd, 4096);
  char r[100];
  pread(fd, r, 100, 100);
  say(fs, "tmpfs-extend ftruncate", zeros(r, 100), "mapped bytes past the old end became data");
  close(fd);
  unlink(p);
}

int main(int argc, char **argv) {
  setvbuf(stdout, 0, _IOLBF, 0);
  for (int i = 1; i < argc; i++) {
    struct statfs sf;
    if (statfs(argv[i], &sf) < 0) {
      printf("%s: %s\n", argv[i], strerror(errno));
      return 2;
    }
    const char *fs = sf.f_type == TMPFS_MAGIC ? "tmpfs" : sf.f_type == EXT4_MAGIC ? "ext4" : "other";
    if (sf.f_type != TMPFS_MAGIC)
      zero_range_eof(fs, argv[i]);
    efault_extend(fs, argv[i]);
    tmpfs_extend(fs, argv[i]);
  }
  return bad;
}

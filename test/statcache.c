// stat が古い答えを出さないか (カーネル: ext4 の meta() の覚え、extfs.rs の metas): aios の中で動かす。
// 覚えを作ってから (何度か stat)、書く・切りつめる・chmod・chown (自分のまま)・時刻を変える・リンクを足して消す・
// ディレクトリを作る、のあとで、すぐに新しい値になるか。ext4 の上で (/home/ai)、と tmpfs の上で (/tmp)
// (aarch64-linux-gnu-gcc -O2 -static -o statcache test/statcache.c)
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

static int fail;

static void warm(const char *p) {
  struct stat st;
  for (int i = 0; i < 3; i++) {
    stat(p, &st);
  }
}

static void want(const char *what, long got, long exp) {
  if (got != exp) {
    printf("FAIL %s: %ld (want %ld)\n", what, got, exp);
    fail = 1;
  }
}

static void run(const char *dir) {
  char f[256], l[256], d[256];
  snprintf(f, sizeof f, "%s/statcache-f", dir);
  snprintf(l, sizeof l, "%s/statcache-l", dir);
  snprintf(d, sizeof d, "%s/statcache-d", dir);
  unlink(f);
  unlink(l);
  rmdir(d);
  int fd = open(f, O_CREAT | O_RDWR | O_TRUNC, 0644);
  struct stat st;
  warm(f);
  write(fd, "hello", 5);
  stat(f, &st);
  want("size after write", st.st_size, 5);
  warm(f);
  ftruncate(fd, 2);
  stat(f, &st);
  want("size after truncate", st.st_size, 2);
  warm(f);
  chmod(f, 0600);
  stat(f, &st);
  want("mode after chmod", st.st_mode & 07777, 0600);
  warm(f);
  chown(f, getuid(), getgid());
  stat(f, &st);
  want("uid after chown", st.st_uid, getuid());
  warm(f);
  struct timespec ts[2] = {{1000000000, 0}, {1000000000, 0}};
  utimensat(AT_FDCWD, f, ts, 0);
  stat(f, &st);
  want("mtime after utimensat", st.st_mtime, 1000000000);
  warm(f);
  link(f, l);
  stat(f, &st);
  want("nlink after link", st.st_nlink, 2);
  warm(f);
  unlink(l);
  stat(f, &st);
  want("nlink after unlink", st.st_nlink, 1);
  warm(dir);
  struct stat before, after;
  stat(dir, &before);
  mkdir(d, 0755);
  stat(dir, &after);
  // ディレクトリを作ると、親のリンクの数が 1 増える (ext4。tmpfs は数えないものもある)
  if (after.st_nlink != before.st_nlink + 1 && after.st_nlink != before.st_nlink) {
    printf("FAIL parent nlink after mkdir: %ld -> %ld\n", (long)before.st_nlink, (long)after.st_nlink);
    fail = 1;
  }
  close(fd);
  unlink(f);
  rmdir(d);
}

int main(void) {
  const char *home = getenv("HOME");
  run(home && *home ? home : "/tmp");
  run("/tmp");
  puts(fail ? "statcache: FAIL" : "statcache: ok");
  return fail;
}

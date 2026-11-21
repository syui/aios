// 使っているマウントの umount が Linux と同じか (root で): 開いたファイル (O_PATH も、マウントの根のディレクトリも)、
// cwd (ほかのプロセスのものも)、子に受けついだ fd、mmap (fd を閉じても)、そこから動かしているプログラム、下のマウント
// があれば EBUSY、やめれば外せる。重ねたマウントと bind はマウントごとに数える。ほかのマウントの namespace の cwd は
// 数えない。MNT_DETACH は使っていても外し、開いた fd はそのまま読める。MNT_FORCE も EBUSY。MNT_EXPIRE は 1 回目
// EAGAIN、2 回目に外す (使っていれば EBUSY、DETACH か FORCE といっしょなら EINVAL)
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#define D "/tmp/ubt/d"
#define E "/tmp/ubt/e"

static long um(const char *p, int fl) { return umount2(p, fl) ? -errno : 0; }
static int mnt(const char *p) {
  mkdir(p, 0755);
  return mount("tmpfs", p, "tmpfs", 0, NULL) ? -errno : 0;
}
static int mounted(const char *p) {
  FILE *f = fopen("/proc/self/mountinfo", "r");
  char l[1024], pat[256];
  int n = 0;
  snprintf(pat, sizeof pat, " %s ", p);
  while (f && fgets(l, sizeof l, f))
    n += strstr(l, pat) != NULL;
  if (f)
    fclose(f);
  return n;
}
static int mkf(const char *p) {
  int fd = open(p, O_CREAT | O_RDWR | O_TRUNC, 0644);
  if (fd >= 0 && write(fd, "hello", 5) != 5)
    return -1;
  return fd;
}
// 子を作って fn を動かし、用意ができるまで待つ (pid を返す。子は殺されるまで待つ)
static pid_t child(void (*fn)(void)) {
  int p[2];
  if (pipe(p))
    return -1;
  pid_t c = fork();
  if (c == 0) {
    close(p[0]);
    fn();
    if (write(p[1], "r", 1) != 1)
      _exit(1);
    for (;;)
      pause();
  }
  close(p[1]);
  char b;
  if (read(p[0], &b, 1) != 1)
    printf("  child not ready\n");
  close(p[0]);
  return c;
}
static void reap(pid_t c) {
  kill(c, SIGKILL);
  waitpid(c, NULL, 0);
}
static void cd_d(void) {
  if (chdir(D))
    _exit(2);
}
static void newns_cd_d(void) {
  if (unshare(CLONE_NEWNS) || chdir(D))
    _exit(2);
}

int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  // 自分を動かすだけ (そのプログラムのマウントを使う)
  if (argc > 1 && !strcmp(argv[1], "sleep")) {
    if (write(3, "r", 1) != 1)
      return 1;
    for (;;)
      pause();
  }
  if (getuid() != 0) {
    printf("umountbusy: not root, skipped\n");
    return 0;
  }
  mkdir("/tmp/ubt", 0755);
  long r1, r2, r3;

  printf("idle: mount %d, umount %ld, still there %d\n", mnt(D), um(D, 0), mounted(D));
  printf("not a mount: %ld\n", um("/tmp/ubt", 0));

  mnt(D);
  int fd = mkf(D "/f");
  r1 = um(D, 0);
  close(fd);
  printf("open file: %ld, after close %ld\n", r1, um(D, 0));

  mnt(D);
  close(mkf(D "/f"));
  fd = open(D "/f", O_PATH);
  r1 = um(D, 0);
  close(fd);
  fd = open(D, O_RDONLY | O_DIRECTORY);
  r2 = um(D, 0);
  close(fd);
  printf("O_PATH: %ld, the root dir: %ld, after %ld\n", r1, r2, um(D, 0));

  mnt(D);
  if (chdir(D))
    return 1;
  r1 = um(D, 0);
  r2 = um(".", 0);
  if (chdir("/"))
    return 1;
  printf("cwd: %ld, dot %ld, after chdir / %ld\n", r1, r2, um(D, 0));

  mnt(D);
  pid_t c = child(cd_d);
  r1 = um(D, 0);
  reap(c);
  printf("child cwd: %ld, after it exits %ld\n", r1, um(D, 0));

  mnt(D);
  c = child(newns_cd_d);
  r1 = um(D, 0);
  reap(c);
  printf("cwd in another mount namespace: %ld\n", r1);

  mnt(D);
  fd = mkf(D "/f");
  c = child(cd_d);
  reap(c);
  c = fork();
  if (c == 0) {
    for (;;)
      pause();
  }
  close(fd);
  r1 = um(D, 0);
  reap(c);
  printf("fd held by a child: %ld, after it exits %ld\n", r1, um(D, 0));

  mnt(D);
  fd = mkf(D "/f");
  ftruncate(fd, 4096);
  char *m = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  close(fd);
  r1 = um(D, 0);
  munmap(m, 4096);
  printf("mmap after close: %ld, after munmap %ld\n", r1, um(D, 0));

  // そこにあるプログラムを動かしている
  mnt(D);
  {
    int in = open("/proc/self/exe", O_RDONLY), out = open(D "/prog", O_CREAT | O_WRONLY | O_TRUNC, 0755);
    char b[65536];
    ssize_t n;
    while ((n = read(in, b, sizeof b)) > 0)
      if (write(out, b, n) != n)
        break;
    close(in);
    close(out);
    int p[2];
    if (pipe(p))
      return 1;
    c = fork();
    if (c == 0) {
      dup2(p[1], 3);
      execl(D "/prog", "prog", "sleep", (char *)NULL);
      _exit(127);
    }
    close(p[1]);
    char x;
    r2 = read(p[0], &x, 1);
    close(p[0]);
    r1 = um(D, 0);
    reap(c);
    printf("running a program from it: %ld (ready %ld), after it exits %ld\n", r1, r2, um(D, 0));
  }

  mnt(D);
  mnt(D "/sub");
  r1 = um(D, 0);
  r2 = um(D "/sub", 0);
  printf("submount: %ld, the submount %ld, then %ld\n", r1, r2, um(D, 0));

  // 重ねたマウント: 下のマウントの開いたファイルは、上のマウントを止めない
  mnt(D);
  fd = mkf(D "/f");
  mnt(D);
  r1 = um(D, 0);
  r2 = um(D, 0);
  close(fd);
  printf("stacked: top %ld, below with its file open %ld, after close %ld\n", r1, r2, um(D, 0));

  // bind: マウントごと
  mnt(D);
  close(mkf(D "/f"));
  mkdir(E, 0755);
  r3 = mount(D, E, NULL, MS_BIND, NULL) ? -errno : 0;
  fd = open(E "/f", O_RDONLY);
  r1 = um(D, 0);
  r2 = um(E, 0);
  close(fd);
  printf("bind %ld: the source %ld, the bind with a file open %ld, after close %ld\n", r3, r1, r2, um(E, 0));

  mnt(D);
  fd = mkf(D "/f");
  r1 = um(D, MNT_FORCE);
  char b[8] = {0};
  r2 = um(D, MNT_DETACH);
  ssize_t n = pread(fd, b, 5, 0);
  printf("force: %ld, detach %ld, still there %d, read %zd %.5s\n", r1, r2, mounted(D), n, b);
  close(fd);

  mnt(D);
  fd = mkf(D "/f");
  r1 = um(D, MNT_EXPIRE);
  close(fd);
  printf("expire: busy %ld, with detach %ld, with force %ld", r1, um(D, MNT_EXPIRE | MNT_DETACH), um(D, MNT_EXPIRE | MNT_FORCE));
  r1 = um(D, MNT_EXPIRE);
  r2 = um(D, MNT_EXPIRE);
  printf(", first %ld, second %ld, still there %d\n", r1, r2, mounted(D));
  um(D, MNT_DETACH);

  rmdir(D "/sub");
  rmdir(D);
  rmdir(E);
  rmdir("/tmp/ubt");
  return 0;
}

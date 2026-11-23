// ディレクトリの delegation (F_SETDELEG をディレクトリに) と、delegation をこわすもの が Linux と同じか:
//   dir     読む口で開いたディレクトリに F_RDLCK の delegation (F_WRLCK は EINVAL、F_SETLEASE はいまも EINVAL)
//   break   ほかのプロセスがそのディレクトリの中を変える (作る、mkdir、mkfifo、symlink、link、消す、rmdir、名前を変える、
//           外へ動かす) か、ディレクトリそのものの属性を変える (chmod、utimes) と、持ち主に F_SETSIG のシグナル
//           (POLL_MSG、si_fd) を送り、外すまで待つ。F_GETDELEG は F_UNLCK (外すべき型)
//   keep    見るだけ (stat、readdir、中のファイルを読む・書く) と、下のディレクトリの中を変えるのは、こわさない
//   file    ふつうのファイルの delegation も、消す・名前を変える・link・chmod でこわれる (F_SETLEASE のリースはこわれない)
// 引数はためすところ (既定 /tmp)。1 行ずつ出す (aios と Linux で同じか、test/ldiff.sh でくらべる)。終わりの番号は思ったものでなかった数
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef F_GETDELEG
#define F_GETDELEG 1039
#define F_SETDELEG 1040
struct delegation {
  uint32_t d_flags;
  uint16_t d_type;
  uint16_t __pad;
};
#endif

static int bad;
static char base[256], d[300];
static volatile sig_atomic_t got, got_fd, got_band;

static void on_sig(int s, siginfo_t *si, void *u) {
  (void)s;
  (void)u;
  got++;
  got_fd = si->si_fd;
  got_band = si->si_band;
}

static long fc(int fd, int cmd, long arg) {
  long r = fcntl(fd, cmd, arg);
  return r < 0 ? -errno : r;
}
static long setdeleg(int fd, int type) {
  struct delegation x = {0, (uint16_t)type, 0};
  return fc(fd, F_SETDELEG, (long)&x);
}
static const char *typ(int t) {
  return t == F_RDLCK ? "rd" : t == F_WRLCK ? "wr" : t == F_UNLCK ? "un" : "?";
}
static int getdeleg(int fd) {
  struct delegation x = {0, 99, 0};
  fc(fd, F_GETDELEG, (long)&x);
  return x.d_type;
}

static char *at(const char *name) {
  static char b[4][400];
  static int i;
  i = (i + 1) % 4;
  snprintf(b[i], sizeof b[i], "%s/%s", d, name);
  return b[i];
}

static uint64_t now_ms(void) {
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  return t.tv_sec * 1000ull + t.tv_nsec / 1000000;
}

// ほかのプロセスで op をして、シグナルが来たか、そのあいだ op が待っていたか、そのときの F_GETDELEG を見て、外す
static void trial(const char *name, int fd, int (*op)(void)) {
  got = 0;
  got_fd = -1;
  got_band = 0;
  int p[2];
  pipe(p);
  pid_t c = fork();
  if (c == 0) {
    close(p[0]);
    int r = op();
    write(p[1], &r, sizeof r);
    _exit(0);
  }
  close(p[1]);
  uint64_t end = now_ms() + 400;
  while (!got && now_ms() < end)
    usleep(1000);
  int during = getdeleg(fd);
  struct pollfd pf = {p[0], POLLIN, 0};
  int waited = poll(&pf, 1, 30) == 0;
  long un = got ? setdeleg(fd, F_UNLCK) : 0;
  int r = -1;
  read(p[0], &r, sizeof r);
  close(p[0]);
  waitpid(c, NULL, 0);
  printf("%s: signal %d, fd ok %d, band %#x, deleg then %s, op waited %d, unlock %ld, op %d", name, got > 0, got_fd == fd, (unsigned)got_band, typ(during), waited, un, r);
  // つけなおせる (外すと F_SETSIG も消える: Linux の lease_modify)
  long again = setdeleg(fd, F_RDLCK);
  printf(", sig after %ld, again %ld\n", fc(fd, F_GETSIG, 0), again);
  fc(fd, F_SETSIG, SIGRTMIN + 1);
}

static int res(int r) {
  return r < 0 ? -errno : 0;
}
static int op_create(void) {
  int f = open(at("f1"), O_CREAT | O_WRONLY, 0644);
  if (f >= 0)
    close(f);
  return f < 0 ? -errno : 0;
}
static int op_mkdir(void) { return res(mkdir(at("sub"), 0755)); }
static int op_mkfifo(void) { return res(mkfifo(at("fifo"), 0644)); }
static int op_symlink(void) { return res(symlink("f1", at("sym"))); }
static int op_link(void) { return res(link(at("f1"), at("hard"))); }
static int op_unlink(void) { return res(unlink(at("hard"))); }
static int op_rmdir(void) { return res(rmdir(at("sub"))); }
static int op_rename(void) { return res(rename(at("sym"), at("sym2"))); }
static char out[400];
static int op_moveout(void) { return res(rename(at("sym2"), out)); }
static int op_chmod(void) { return res(chmod(d, 0700)); }
static int op_utimes(void) { return res(utimes(d, NULL)); }
static int op_stat(void) {
  struct stat st;
  return res(stat(at("f1"), &st));
}
static int op_readdir(void) {
  DIR *x = opendir(d);
  int n = 0;
  while (x && readdir(x))
    n++;
  if (x)
    closedir(x);
  return n > 0 ? 0 : -1;
}
static int op_rw(void) {
  int f = open(at("f1"), O_RDWR);
  int r = f < 0 ? -errno : 0;
  if (f >= 0) {
    write(f, "x", 1);
    close(f);
  }
  return r;
}
static int op_inner(void) {
  char b[400];
  snprintf(b, sizeof b, "%s/keep/inner", d);
  int f = open(b, O_CREAT | O_WRONLY, 0644);
  if (f >= 0)
    close(f);
  return f < 0 ? -errno : 0;
}

// ふつうのファイルの delegation をこわすもの
static char fpath[400];
static int op_funlink(void) { return res(unlink(fpath)); }
static int op_fchmod(void) { return res(chmod(fpath, 0600)); }
static int op_frename(void) {
  char b[400];
  snprintf(b, sizeof b, "%s.r", fpath);
  int r = res(rename(fpath, b));
  rename(b, fpath);
  return r;
}
static int op_flink(void) {
  char b[400];
  snprintf(b, sizeof b, "%s.l", fpath);
  int r = res(link(fpath, b));
  unlink(b);
  return r;
}

int main(int argc, char **argv) {
  setvbuf(stdout, NULL, _IONBF, 0);
  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_sigaction = on_sig;
  sa.sa_flags = SA_SIGINFO | SA_RESTART;
  sigaction(SIGRTMIN + 1, &sa, NULL);
  sigaction(SIGIO, &sa, NULL);
  snprintf(base, sizeof base, "%s/delegXXXXXX", argc > 1 ? argv[1] : "/tmp");
  if (!mkdtemp(base)) {
    printf("mkdtemp: %s\n", strerror(errno));
    return 1;
  }
  snprintf(d, sizeof d, "%s/d", base);
  snprintf(out, sizeof out, "%s/out", base);
  mkdir(d, 0755);
  mkdir(at("keep"), 0755);
  int fd = open(d, O_RDONLY | O_DIRECTORY);
  fc(fd, F_SETSIG, SIGRTMIN + 1);
  long wr = setdeleg(fd, F_WRLCK);
  long rd = setdeleg(fd, F_RDLCK);
  printf("dir: F_WRLCK %ld, F_RDLCK %ld, get %s, F_SETLEASE %ld, F_GETLEASE %s, owner me %d\n", wr, rd, typ(getdeleg(fd)), fc(fd, F_SETLEASE, F_RDLCK), typ(fc(fd, F_GETLEASE, 0)), fc(fd, F_GETOWN, 0) == getpid());
  if (rd != 0) {
    printf("deleg: %d bad\n", bad);
    return 0;
  }
  trial("create", fd, op_create);
  trial("mkdir", fd, op_mkdir);
  trial("mkfifo", fd, op_mkfifo);
  trial("symlink", fd, op_symlink);
  trial("link", fd, op_link);
  trial("unlink", fd, op_unlink);
  trial("rmdir", fd, op_rmdir);
  trial("rename", fd, op_rename);
  trial("move out", fd, op_moveout);
  trial("chmod dir", fd, op_chmod);
  trial("utimes dir", fd, op_utimes);
  trial("stat", fd, op_stat);
  trial("readdir", fd, op_readdir);
  trial("read and write a file", fd, op_rw);
  trial("create in a subdir", fd, op_inner);
  printf("close: unlock %ld\n", setdeleg(fd, F_UNLCK));
  close(fd);
  // ふつうのファイルの delegation
  snprintf(fpath, sizeof fpath, "%s/file", base);
  close(open(fpath, O_CREAT | O_WRONLY, 0644));
  int f = open(fpath, O_RDONLY);
  fc(f, F_SETSIG, SIGRTMIN + 1);
  printf("file: deleg %ld\n", setdeleg(f, F_RDLCK));
  trial("file chmod", f, op_fchmod);
  trial("file rename", f, op_frename);
  trial("file link", f, op_flink);
  trial("file unlink", f, op_funlink);
  setdeleg(f, F_UNLCK);
  close(f);
  // リース (F_SETLEASE) は名前の変わりではこわれない
  close(open(fpath, O_CREAT | O_WRONLY, 0644));
  f = open(fpath, O_RDONLY);
  fc(f, F_SETSIG, SIGRTMIN + 1);
  long l = fc(f, F_SETLEASE, F_RDLCK);
  got = 0;
  pid_t c = fork();
  if (c == 0) {
    chmod(fpath, 0600);
    char b[400];
    snprintf(b, sizeof b, "%s.l", fpath);
    link(fpath, b);
    unlink(b);
    _exit(0);
  }
  waitpid(c, NULL, 0);
  usleep(50000);
  printf("lease: set %ld, chmod and link signal %d, still %s\n", l, got > 0, typ(fc(f, F_GETLEASE, 0)));
  fc(f, F_SETLEASE, F_UNLCK);
  close(f);
  unlink(fpath);
  char cmd[600];
  snprintf(cmd, sizeof cmd, "rm -rf %s", base);
  system(cmd);
  printf("deleg: %d bad\n", bad);
  return bad;
}

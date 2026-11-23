// dnotify (fcntl の F_NOTIFY: ディレクトリの中が変わったらシグナル) が Linux と同じか:
//   set     ディレクトリでないものは ENOTDIR、0 で外す、持ち主が自分になる、同じ fd にもう一度なら印を足す
//   once    DN_MULTISHOT がなければ 1 度で外れる
//   events  DN_CREATE (作る、名前を変えて入る)、DN_DELETE (消す、名前を変えて出る)、DN_MODIFY (書く)、DN_ACCESS (読む)、
//           DN_ATTRIB (chmod: 中のものとディレクトリ自身)、DN_RENAME。F_SETSIG なら POLL_MSG と si_fd
//   close   fd を閉じると外れる (dup したほうを閉じても)
// 1 行ずつ出す (aios と Linux で同じか、test/ldiff.sh でくらべる)。終わりの番号は思ったものでなかった数
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static int bad;
static char dir[64];

static long fc(int fd, int cmd, long arg) {
  long r = fcntl(fd, cmd, arg);
  return r < 0 ? -errno : r;
}

static int dfd;
static volatile int n, code, fdseen, signo;
static volatile long band;
static void on_sig(int s, siginfo_t *si, void *u) {
  (void)u;
  n++;
  signo = s;
  code = si->si_code;
  band = si->si_band;
  fdseen = si->si_fd;
}

static void settle(void) {
  struct timespec t = {0, 20 * 1000 * 1000};
  while (nanosleep(&t, &t) < 0 && errno == EINTR)
    ;
}

static void show(const char *what) {
  settle();
  printf("%s: %d", what, n);
  if (n)
    printf(" [%s code %d band %#lx fd %s]", signo == SIGIO ? "SIGIO" : signo == SIGRTMIN + 1 ? "RT+1" : "other", code, band, fdseen == dfd ? "dir" : "other");
  printf("\n");
  n = 0;
}

static void path(char *b, const char *name) {
  snprintf(b, 128, "%s/%s", dir, name);
}

static void touch(const char *name) {
  char p[128];
  path(p, name);
  close(open(p, O_WRONLY | O_CREAT, 0600));
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_sigaction = on_sig;
  sa.sa_flags = SA_SIGINFO | SA_RESTART;
  sigaction(SIGIO, &sa, NULL);
  sigaction(SIGRTMIN + 1, &sa, NULL);
  snprintf(dir, sizeof dir, "/tmp/dnotifyXXXXXX");
  mkdtemp(dir);
  FILE *sf = fopen("/proc/sys/fs/dir-notify-enable", "r");
  int en = -1;
  if (sf) {
    fscanf(sf, "%d", &en);
    fclose(sf);
  }
  printf("sysctl: dir-notify-enable %d\n", en);
  char p[128], q[128];
  touch("a");
  path(p, "a");
  int f = open(p, O_RDONLY);
  dfd = open(dir, O_RDONLY | O_DIRECTORY);
  printf("set: file %ld, dir %ld, owner %s, O_ASYNC %d, zero %ld\n", fc(f, F_NOTIFY, DN_CREATE), fc(dfd, F_NOTIFY, DN_CREATE), syscall(SYS_fcntl, dfd, F_GETOWN) == getpid() ? "self" : "none",
         (fcntl(dfd, F_GETFL) & O_ASYNC) != 0, fc(dfd, F_NOTIFY, 0));
  close(f);
  // 1 度だけ (SIGIO)
  fc(dfd, F_NOTIFY, DN_CREATE);
  touch("b");
  show("once: create");
  touch("c");
  show("once: create again");
  // F_SETSIG と DN_MULTISHOT
  fcntl(dfd, F_SETSIG, SIGRTMIN + 1);
  fc(dfd, F_NOTIFY, DN_CREATE | DN_MULTISHOT);
  touch("d");
  show("multi: create");
  touch("e");
  show("multi: create again");
  path(p, "e");
  unlink(p);
  show("multi: delete without DN_DELETE");
  // 同じ fd にもう一度: 印を足す
  fc(dfd, F_NOTIFY, DN_DELETE | DN_MULTISHOT);
  path(p, "d");
  unlink(p);
  show("multi: delete after adding DN_DELETE");
  touch("f");
  show("multi: create still");
  path(p, "f");
  path(q, "g");
  rename(p, q);
  show("multi: rename (moved from and to)");
  fc(dfd, F_NOTIFY, 0);
  fc(dfd, F_NOTIFY, DN_RENAME | DN_MULTISHOT);
  rename(q, p);
  show("rename: rename");
  fc(dfd, F_NOTIFY, 0);
  fc(dfd, F_NOTIFY, DN_MODIFY | DN_ACCESS | DN_MULTISHOT);
  int w = open(p, O_RDWR);
  write(w, "x", 1);
  show("modify: write");
  write(w, "y", 1);
  show("modify: write again");
  char c;
  pread(w, &c, 1, 0);
  show("access: read");
  close(w);
  show("access: close");
  fc(dfd, F_NOTIFY, 0);
  fc(dfd, F_NOTIFY, DN_ATTRIB | DN_MULTISHOT);
  chmod(p, 0640);
  show("attrib: chmod child");
  chmod(dir, 0750);
  show("attrib: chmod dir");
  // 閉じると外れる (dup したほうを閉じても)
  int d2 = dup(dfd);
  close(d2);
  chmod(p, 0600);
  show("close: after closing a dup");
  fc(dfd, F_NOTIFY, DN_ATTRIB | DN_MULTISHOT);
  int other = dfd;
  dfd = dup(other);
  close(other);
  chmod(p, 0640);
  show("close: after closing the fd");
  close(dfd);
  const char *names[] = {"a", "b", "c", "f", "g"};
  for (unsigned i = 0; i < sizeof names / sizeof *names; i++) {
    path(p, names[i]);
    unlink(p);
  }
  rmdir(dir);
  printf("dnotify: %d bad\n", bad);
  return bad;
}

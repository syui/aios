// IPC の namespace (CLONE_NEWIPC) で System V の共有メモリが分かれるか (カーネル: ns.rs の Ipc、shm.rs の表)。
// 新しい namespace からは外の鍵が見えず、番号は 0 から。中で作ったものは外から見えず、/proc/sysvipc/shm も中のものだけ。
// /proc/self/ns/ipc は ipc:[N] で、外とちがう。namespace がなくなれば、中のかたまりも消える
// (aarch64-linux-gnu-gcc -O2 -static -o ipcns test/ipcns.c。test/ldiff.sh ipcns で Linux とくらべる)
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ipc.h>
#include <sys/shm.h>
#include <sys/wait.h>
#include <unistd.h>

static const char *err(void) {
  switch (errno) {
  case ENOENT: return "ENOENT";
  case EINVAL: return "EINVAL";
  case EPERM: return "EPERM";
  default: return strerror(errno);
  }
}

static int lines(void) {
  FILE *f = fopen("/proc/sysvipc/shm", "r");
  if (!f) return -1;
  char l[512];
  int n = -1;
  while (fgets(l, sizeof l, f)) n++;
  fclose(f);
  return n;
}

static void put(const char *p, const char *s) {
  int fd = open(p, O_WRONLY);
  if (fd >= 0) {
    if (write(fd, s, strlen(s)) < 0) {}
    close(fd);
  }
}

static int newipc(void) {
  if (unshare(CLONE_NEWIPC) == 0) return 0;
  uid_t u = getuid();
  gid_t g = getgid();
  if (unshare(CLONE_NEWUSER | CLONE_NEWIPC) < 0) return -1;
  char m[64];
  snprintf(m, sizeof m, "0 %d 1\n", (int)u);
  put("/proc/self/uid_map", m);
  put("/proc/self/setgroups", "deny");
  snprintf(m, sizeof m, "0 %d 1\n", (int)g);
  put("/proc/self/gid_map", m);
  return 0;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  key_t k = 0x1bc0ffee;
  int id = shmget(k, 4096, IPC_CREAT | IPC_EXCL | 0600);
  printf("outer create: %s\n", id >= 0 ? "ok" : err());
  char outer[64] = "", inner[64] = "";
  if (readlink("/proc/self/ns/ipc", outer, sizeof outer - 1) < 0) strcpy(outer, err());
  printf("outer link: %.4s\n", outer);
  int before = lines();
  int p[2];
  if (pipe(p) < 0) return 2;
  pid_t c = fork();
  if (c == 0) {
    close(p[0]);
    if (newipc() < 0) {
      printf("child: unshare %s\n", err());
      _exit(0);
    }
    if (readlink("/proc/self/ns/ipc", inner, sizeof inner - 1) < 0) strcpy(inner, err());
    printf("child: link %.4s differs %d\n", inner, strcmp(inner, outer) != 0);
    printf("child: outer key %s\n", shmget(k, 0, 0) < 0 ? err() : "found");
    printf("child: outer id stat %s\n", shmctl(id, IPC_STAT, &(struct shmid_ds){0}) < 0 ? err() : "ok");
    printf("child: lines %d\n", lines());
    int a = shmget(IPC_PRIVATE, 100, 0600);
    int b = shmget(k, 100, IPC_CREAT | 0600);
    printf("child: ids %d %d\n", a, b);
    printf("child: lines %d\n", lines());
    struct shminfo si;
    printf("child: max index %d\n", shmctl(0, IPC_INFO, (struct shmid_ds *)&si));
    char *m = shmat(b, NULL, 0);
    strcpy(m, "inner");
    if (write(p[1], &b, sizeof b) != sizeof b) _exit(1);
    _exit(0);
  }
  close(p[1]);
  int b = -1;
  if (read(p[0], &b, sizeof b) != sizeof b) b = -1;
  waitpid(c, NULL, 0);
  printf("outer key still: %s\n", shmget(k, 0, 0) == id ? "same" : err());
  printf("outer lines: %d (before %d)\n", lines(), before);
  struct shmid_ds d;
  int r = shmctl(id, IPC_STAT, &d);
  printf("outer stat: %s size %zu\n", r < 0 ? err() : "ok", r < 0 ? 0 : (size_t)d.shm_segsz);
  shmctl(id, IPC_RMID, NULL);
  printf("done\n");
  return 0;
}

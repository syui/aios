// /proc/PID のファイル (見せるもの) が Linux と同じか:
//   auxv          (型, 値) の 16 バイトの並びと AT_NULL。型の順、getauxval と同じ値か
//   syscall       自分 (read の番号と fd と数)、パイプを read で待つ子 (同じ形)、走っている子 (running)、SIGSTOP で止めた子
//   sched         1 行目 (名前、pid、#threads)、2 行目の -、名前の並び (値はうごく)
//   smaps_rollup  1 行目 (maps のはじめから終わり、[rollup])、名前の並び、Rss が smaps の和と同じか
//   mountstats    mounts と同じ並びで device X mounted on Y with fstype Z
//   projid_map    init の user namespace の中身
//   timens_offsets 中身、書く (CAP_SYS_TIME がないので EPERM)
//   net           /proc/net はリンク (self/net)、/proc/self/net の中は /proc/net と同じ名前
//   clear_refs    0200、1〜5 は書ける、0 と 6 と数でないものは EINVAL
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/wait.h>
#include <unistd.h>

static int fails;
static void check(const char *name, int ok) {
  printf("%s %s\n", ok ? "ok" : "FAIL", name);
  if (!ok)
    fails++;
}

static ssize_t slurp(const char *p, char *b, size_t n) {
  int fd = open(p, O_RDONLY);
  if (fd < 0)
    return -errno;
  ssize_t t = 0, r;
  while (t < (ssize_t)n - 1 && (r = read(fd, b + t, n - 1 - t)) > 0)
    t += r;
  int e = r < 0 ? -errno : 0;
  close(fd);
  if (e && !t)
    return e;
  b[t] = 0;
  return t;
}

static int wr(const char *p, const char *v) {
  int fd = open(p, O_WRONLY);
  if (fd < 0)
    return -1000 - errno;
  ssize_t n = write(fd, v, strlen(v));
  int e = n < 0 ? -errno : (int)n;
  close(fd);
  return e;
}

// 行ごとに : の前 (名前) だけ
static void names(const char *tag, char *b, int skip) {
  char *save, *l = strtok_r(b, "\n", &save);
  printf("%s:", tag);
  for (int i = 0; l; l = strtok_r(NULL, "\n", &save), i++) {
    if (i < skip)
      continue;
    char *c = strchr(l, ':');
    if (c) {
      *c = 0;
      char *e = c;
      while (e > l && e[-1] == ' ')
        *--e = 0;
      printf(" %s", l);
    } else
      printf(" [%s]", l);
  }
  printf("\n");
}

static int cmp(const void *a, const void *b) { return strcmp(*(char *const *)a, *(char *const *)b); }
static int list(const char *dir, char **out, int max) {
  DIR *d = opendir(dir);
  if (!d)
    return -errno;
  int n = 0;
  struct dirent *e;
  while ((e = readdir(d)) && n < max)
    if (strcmp(e->d_name, ".") && strcmp(e->d_name, ".."))
      out[n++] = strdup(e->d_name);
  closedir(d);
  qsort(out, n, sizeof *out, cmp);
  return n;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  static char b[1 << 16], c[1 << 16];
  // auxv
  {
    ssize_t n = slurp("/proc/self/auxv", b, sizeof b);
    uint64_t *v = (uint64_t *)b;
    int pairs = n / 16, same = 1, null_last = n >= 16 && v[2 * (pairs - 1)] == 0;
    printf("auxv (%zd bytes, %d pairs, AT_NULL last %d):", n, pairs, null_last);
    for (int i = 0; i < pairs; i++) {
      printf(" %llu", (unsigned long long)v[2 * i]);
      if (v[2 * i] && v[2 * i] != AT_IGNORE && getauxval(v[2 * i]) != v[2 * i + 1])
        same = 0;
    }
    printf("\n");
    check("auxv values are getauxval's", same && null_last && n % 16 == 0);
  }
  // syscall
  {
    ssize_t n = slurp("/proc/self/syscall", b, sizeof b);
    long nr = -9;
    unsigned long a0 = 0, a1 = 0, a2 = 0;
    int f = sscanf(b, "%ld 0x%lx 0x%lx 0x%lx", &nr, &a0, &a1, &a2);
    int words = 0;
    for (char *s = b; *s; s++)
      if (*s == ' ')
        words++;
    printf("syscall self (read %d, nr %ld, fd>=3 %d, count %lu, %d fields)\n", n > 0, nr, a0 >= 3, a2, words + 1);
    int p[2];
    if (pipe(p))
      return 1;
    pid_t k = fork();
    if (k == 0) {
      char ch;
      if (read(p[0], &ch, 1) < 0)
        _exit(1);
      for (;;)
        ;
    }
    usleep(200000);
    char path[64];
    snprintf(path, sizeof path, "/proc/%d/syscall", k);
    slurp(path, b, sizeof b);
    f = sscanf(b, "%ld 0x%lx 0x%lx 0x%lx", &nr, &a0, &a1, &a2);
    printf("syscall read child (nr %ld, fd is the pipe %d, count %lu)\n", nr, a0 == (unsigned long)p[0], a2);
    if (write(p[1], "x", 1) != 1)
      return 1;
    usleep(200000);
    slurp(path, b, sizeof b);
    printf("syscall running child: %s", b);
    kill(k, SIGSTOP);
    usleep(200000);
    slurp(path, b, sizeof b);
    words = 0;
    for (char *s = b; *s; s++)
      if (*s == ' ')
        words++;
    printf("syscall stopped child (starts with -1 %d, %d fields)\n", !strncmp(b, "-1 ", 3), words + 1);
    kill(k, SIGKILL);
    waitpid(k, NULL, 0);
    (void)f;
  }
  // sched
  {
    slurp("/proc/self/sched", b, sizeof b);
    char first[128];
    sscanf(b, "%127[^\n]", first);
    char *nl = strchr(b, '\n');
    int dashes = 0;
    if (nl)
      while (nl[1 + dashes] == '-')
        dashes++;
    char want[128], comm[32] = "";
    slurp("/proc/self/comm", c, sizeof c);
    sscanf(c, "%31[^\n]", comm);
    snprintf(want, sizeof want, "%s (%d, #threads: 1)", comm, getpid());
    printf("sched (first line ok %d, %d dashes)\n", !strcmp(first, want), dashes);
    names("sched names", b, 2);
  }
  // smaps_rollup
  {
    slurp("/proc/self/smaps_rollup", b, sizeof b);
    slurp("/proc/self/maps", c, sizeof c);
    unsigned long rs, re, ms, me = 0, x, y;
    sscanf(b, "%lx-%lx", &rs, &re);
    sscanf(c, "%lx-", &ms);
    for (char *l = c; l && *l; l = strchr(l, '\n') ? strchr(l, '\n') + 1 : NULL)
      if (sscanf(l, "%lx-%lx", &x, &y) == 2)
        me = y;
    char first[160];
    sscanf(b, "%159[^\n]", first);
    char *tag = strstr(first, "[rollup]");
    printf("smaps_rollup (start is maps' %d, end is maps' %d, ---p 00000000 00:00 0 %d, [rollup] at %ld)\n", rs == ms, re == me, strstr(first, " ---p 00000000 00:00 0 ") != NULL,
           tag ? (long)(tag - first) : -1L);
    slurp("/proc/self/smaps", c, sizeof c);
    long sum = 0, v;
    for (char *l = c; l && *l; l = strchr(l, '\n') ? strchr(l, '\n') + 1 : NULL)
      if (sscanf(l, "Rss: %ld kB", &v) == 1)
        sum += v;
    char *r = strstr(b, "\nRss:");
    long roll = r ? strtol(r + 5, NULL, 10) : -1;
    printf("smaps_rollup Rss is the sum of smaps (within 64 kB) %d\n", labs(roll - sum) <= 64);
    names("smaps_rollup names", b, 1);
  }
  // maps: ファイルの行は major:minor と inode (プログラムのものは stat と同じ)、名前は 73 けた目から、[heap] などの名前
  {
    slurp("/proc/self/maps", b, sizeof b);
    struct stat st;
    stat("/proc/self/exe", &st);
    int exe_ok = 0, col_ok = 1, anon_ok = 1;
    char kinds[256] = "";
    char *save, *l = strtok_r(b, "\n", &save);
    for (; l; l = strtok_r(NULL, "\n", &save)) {
      unsigned long s, e, off, ino;
      unsigned maj, min;
      char perm[8];
      int pos = 0;
      if (sscanf(l, "%lx-%lx %7s %lx %x:%x %lu %n", &s, &e, perm, &off, &maj, &min, &ino, &pos) < 7)
        continue;
      char *name = l + pos;
      if (*name) {
        // 名前の前の空白をのぞいた場所
        col_ok &= name - l == 73 || (name - l > 73 && name[-1] == ' ' && name[-2] != ' ');
        if (name[0] == '[' && !strstr(kinds, name)) {
          strcat(kinds, " ");
          strcat(kinds, name);
        }
      } else
        anon_ok &= l[strlen(l) - 1] == ' ';
      if (ino == st.st_ino && makedev(maj, min) == st.st_dev)
        exe_ok = 1;
    }
    printf("maps (exe line has its dev and inode %d, names at column 73 %d, unnamed lines end with a space %d, named:%s)\n", exe_ok, col_ok, anon_ok, kinds);
  }
  // mountstats
  {
    slurp("/proc/self/mountstats", b, sizeof b);
    slurp("/proc/self/mounts", c, sizeof c);
    int lines = 0, same = 1;
    char *sb, *sc, *lb = strtok_r(b, "\n", &sb), *lc = strtok_r(c, "\n", &sc);
    for (; lb && lc; lb = strtok_r(NULL, "\n", &sb), lc = strtok_r(NULL, "\n", &sc)) {
      char dev[256], mnt[256], type[64], want[700];
      sscanf(lc, "%255s %255s %63s", dev, mnt, type);
      snprintf(want, sizeof want, "device %s mounted on %s with fstype %s", dev, mnt, type);
      same &= !strcmp(lb, want);
      lines++;
    }
    printf("mountstats (%d lines, like mounts %d, same count %d)\n", lines, same, !lb && !lc);
  }
  // projid_map と timens_offsets
  {
    slurp("/proc/self/projid_map", b, sizeof b);
    printf("projid_map: %s", b);
    slurp("/proc/self/timens_offsets", b, sizeof b);
    printf("timens_offsets:\n%s", b);
    printf("timens_offsets write %d\n", wr("/proc/self/timens_offsets", "monotonic 1 0\n"));
  }
  // net
  {
    char l[64] = "";
    ssize_t n = readlink("/proc/net", l, sizeof l - 1);
    if (n > 0)
      l[n] = 0;
    struct stat st;
    stat("/proc/self/net", &st);
    char *a[128], *d[128];
    int na = list("/proc/net", a, 128), nd = list("/proc/self/net", d, 128);
    int same = na == nd && na > 0;
    for (int i = 0; same && i < na; i++)
      same = !strcmp(a[i], d[i]);
    printf("net (/proc/net -> %s, self/net dir %d mode %o, same names %d)\n", l, S_ISDIR(st.st_mode), st.st_mode & 07777, same);
  }
  // clear_refs
  {
    struct stat st;
    stat("/proc/self/clear_refs", &st);
    int r = open("/proc/self/clear_refs", O_RDONLY);
    int re = r < 0 ? -errno : 0;
    printf("clear_refs (mode %o, open for read %d, 1 %d, 4 %d, 5 %d, 0 %d, 6 %d, x %d)\n", st.st_mode & 07777, re, wr("/proc/self/clear_refs", "1"), wr("/proc/self/clear_refs", "4\n"),
           wr("/proc/self/clear_refs", "5"), wr("/proc/self/clear_refs", "0"), wr("/proc/self/clear_refs", "6"), wr("/proc/self/clear_refs", "x"));
  }
  printf("procfiles: %d failed\n", fails);
  return fails != 0;
}

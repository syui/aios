// 大きなロックなしの stat (カーネルの statc.rs) が、ほかの CPU で変えたすぐあとに古い答えを出さないか: aios の中で動かす
// (zig cc -target aarch64-linux-musl -O2 -static -o statc test/statc.c)。statcache.c と pathcache.c は 1 つのプロセスで
// 見るので、こちらは変える人と見る人を別の CPU に:
//   1. 順に: 変える人 (親) が 1 つ変えて round を出す。見る人 (子。CPU の数より 1 つ少なく、2 つ以上) はみな、それを見て
//      から何度も stat と statx をして (絶対のパスと cwd からのパスで)、その round の答えと同じかをたしかめ、済んだと返す。親はみなが
//      済むのを待って次へ。答えは親が開いた fd の fstat (stat の道を通らない)。変えるのは: chmod、時刻、大きさ、リンクの数、
//      名前を変えてもどす、消して作りなおす、シンボリックリンクを付けかえる、途中のディレクトリを取りかえる、だれでもは
//      通れないディレクトリの許し (親が先に stat して覚えさせてから)
//   2. 止めずに: 親が時刻を増やしつづけ、名前を次々に変えつづけるあいだ、見る人の答えが出した番号より古くないか
//   3. 1 つのプロセスで: chdir のあとの cwd からのパス。ユーザーの namespace の中の人が覚えた答え (uid が読みかわる) を
//      ほかの人が使わないか、その逆も
// root なら見る人は uid 65534 (覚えたのがほかの人でも、通れない人には EACCES か。DIR はだれでも通れるところに)。
// Linux でも同じに通る。1 行に ok NAME か FAIL NAME、おしまいに statc: N failed
//   statc [DIR...]   DIR (既定 /tmp) ごとに、その下に sc-PID を作って
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int fails;
#define CHECK(name, cond) do { if (cond) printf("ok %s\n", name); else { printf("FAIL %s\n", name); fails++; } } while (0)

// stat の答えのうち、たしかめるもの (err は 0 か -errno)
struct ans {
  long err, ino, mode, nlink, size, mtime;
};

struct shared {
  int round, acks, stop, bad, first;
  // round の問い: path (base からの名前) と、その答え
  char path[200];
  struct ans want;
  char what[48];
  char msg[200];
  // 止めずに変えるとき: 出した番号
  long k;
};

static struct shared *sh;
static char base[256];
static int readers;

static struct ans of(const struct stat *s) {
  struct ans a = {0, (long)s->st_ino, (long)(s->st_mode & 07777), (long)s->st_nlink, (long)s->st_size, (long)s->st_mtime};
  return a;
}

static struct ans stat_of(const char *p) {
  struct stat s;
  struct ans a = {0};
  if (stat(p, &s) != 0) {
    a.err = -errno;
    return a;
  }
  return of(&s);
}

// statx で (Rust の std::fs::metadata はこちら。カーネルの statc.rs は両方に答える)
static struct ans statx_of(const char *p) {
  struct statx x;
  struct ans a = {0};
  if (statx(AT_FDCWD, p, 0, STATX_BASIC_STATS, &x) != 0) {
    a.err = -errno;
    return a;
  }
  struct ans r = {0, (long)x.stx_ino, (long)(x.stx_mode & 07777), (long)x.stx_nlink, (long)x.stx_size, (long)x.stx_mtime.tv_sec};
  return r;
}

static int same(struct ans a, struct ans b) {
  if (a.err || b.err) return a.err == b.err;
  return a.ino == b.ino && a.mode == b.mode && a.nlink == b.nlink && a.size == b.size && a.mtime == b.mtime;
}

static void note(const char *fmt, const char *what, const char *path, struct ans got, struct ans want) {
  __atomic_add_fetch(&sh->bad, 1, __ATOMIC_SEQ_CST);
  if (__atomic_exchange_n(&sh->first, 1, __ATOMIC_SEQ_CST) == 0) {
    snprintf(sh->msg, sizeof sh->msg, fmt, what, path, got.err, got.ino, got.mode, got.mtime, want.err, want.ino, want.mode, want.mtime);
  }
}

#define BAD "%s: %s: got err %ld ino %ld mode %lo mtime %ld, want err %ld ino %ld mode %lo mtime %ld"

// 見る人: round を待って、その問いを 60 回 (偶数は絶対のパス、奇数は cwd からのパス。3 回に 1 回は statx)。始められなければ、しくじりを
// 残して返事だけする (親が待ちつづけないように)
static void reader(int id) {
  int broken = 0;
  if (getuid() == 0) {
    gid_t g = 65534;
    if (setgroups(0, &g) != 0 || setgid(65534) != 0 || setuid(65534) != 0) broken = 1;
  }
  if (!broken && chdir(base) != 0) broken = 2;
  if (broken) {
    struct ans none = {broken == 1 ? -EPERM : -errno};
    note(BAD, broken == 1 ? "reader setuid 65534" : "reader chdir (DIR must be passable by uid 65534 when root)", base, none, none);
  }
  int seen = 0;
  char abs[512], rel[256];
  for (;;) {
    int r;
    while ((r = __atomic_load_n(&sh->round, __ATOMIC_ACQUIRE)) == seen) {
      if (__atomic_load_n(&sh->stop, __ATOMIC_ACQUIRE)) _exit(0);
      sched_yield();
    }
    seen = r;
    if (r < 0) break;
    snprintf(rel, sizeof rel, "%s", sh->path);
    snprintf(abs, sizeof abs, "%s/%s", base, rel);
    struct ans want = sh->want;
    for (int i = 0; i < 60 && !broken; i++) {
      const char *p = (i + id) % 2 ? rel : abs;
      struct ans got = i % 3 == 2 ? statx_of(p) : stat_of(p);
      if (!same(got, want)) {
        note(BAD, sh->what, p, got, want);
        break;
      }
    }
    __atomic_add_fetch(&sh->acks, 1, __ATOMIC_RELEASE);
  }
  if (broken) _exit(1);
  // 止めずに: 時刻 (f の mtime は出した番号より小さくない)
  char f[512];
  snprintf(f, sizeof f, "%s/run/f", base);
  while (!__atomic_load_n(&sh->stop, __ATOMIC_ACQUIRE)) {
    long k = __atomic_load_n(&sh->k, __ATOMIC_ACQUIRE);
    if (k < 0) break;
    struct ans got = k % 2 ? statx_of(f) : stat_of(f);
    if (got.err || got.mtime < 1000 + k) {
      struct ans want = {0, 0, 0, 0, 0, 1000 + k};
      note(BAD, "running-mtime", f, got, want);
      break;
    }
  }
  while (__atomic_load_n(&sh->k, __ATOMIC_ACQUIRE) < 0 && !__atomic_load_n(&sh->stop, __ATOMIC_ACQUIRE)) sched_yield();
  // 止めずに: 名前 (k を出したら n(k-1) はもうない。いまの名前 n(k) を引いて、覚えをわざと作る)
  while (!__atomic_load_n(&sh->stop, __ATOMIC_ACQUIRE)) {
    long k = __atomic_load_n(&sh->k, __ATOMIC_ACQUIRE);
    char cur[512], old[512];
    snprintf(cur, sizeof cur, "%s/run/n%ld", base, k);
    snprintf(old, sizeof old, "%s/run/n%ld", base, k - 1);
    stat_of(cur);
    if (k > 0) {
      struct ans got = k % 2 ? statx_of(old) : stat_of(old);
      if (got.err != -ENOENT) {
        struct ans want = {-ENOENT};
        note(BAD, "running-rename", old, got, want);
        break;
      }
    }
  }
  _exit(0);
}

// 親: 問いを出して、みなが済むのを待つ
static int round_no;
static void ask(const char *what, const char *path, struct ans want) {
  char p[512];
  snprintf(p, sizeof p, "%s/%s", base, path);
  // 親 (root なら見る人とちがう人) が先に何度か引いて覚えさせる
  for (int i = 0; i < 3; i++) stat_of(p);
  snprintf(sh->path, sizeof sh->path, "%s", path);
  snprintf(sh->what, sizeof sh->what, "%s", what);
  sh->want = want;
  __atomic_store_n(&sh->acks, 0, __ATOMIC_SEQ_CST);
  __atomic_store_n(&sh->round, ++round_no, __ATOMIC_RELEASE);
  // 見る人がいなくなっていたら (60 秒)、待たずに
  time_t t0 = time(0);
  while (__atomic_load_n(&sh->acks, __ATOMIC_ACQUIRE) < readers) {
    if (time(0) - t0 > 60) {
      struct ans none = {0};
      note(BAD, "readers do not answer", path, none, none);
      break;
    }
    sched_yield();
  }
}

// path を開いて fstat した答え (stat の道を通らない)
static struct ans fd_ans(const char *path) {
  char p[512];
  snprintf(p, sizeof p, "%s/%s", base, path);
  struct ans a = {0};
  int fd = open(p, O_RDONLY);
  struct stat s;
  if (fd < 0 || fstat(fd, &s) != 0) {
    a.err = -999;
    if (fd >= 0) close(fd);
    return a;
  }
  close(fd);
  return of(&s);
}

static void at(char *out, const char *name) { snprintf(out, 512, "%s/%s", base, name); }

static void touch(const char *name, long mtime) {
  char p[512];
  at(p, name);
  int fd = open(p, O_CREAT | O_WRONLY | O_TRUNC, 0644);
  if (fd >= 0) close(fd);
  struct timespec ts[2] = {{mtime, 0}, {mtime, 0}};
  utimensat(AT_FDCWD, p, ts, 0);
}

static void rounds(int root) {
  char p[512], q[512];
  const int R = 12;
  struct ans gone = {-ENOENT};
  touch("f", 100);
  ask("create", "f", fd_ans("f"));
  at(p, "f");
  for (int r = 0; r < R; r++) {
    chmod(p, r % 2 ? 0640 : 0604);
    ask("chmod", "f", fd_ans("f"));
  }
  for (int r = 0; r < R; r++) {
    struct timespec ts[2] = {{200 + r, 0}, {200 + r, 0}};
    utimensat(AT_FDCWD, p, ts, 0);
    ask("mtime", "f", fd_ans("f"));
  }
  for (int r = 0; r < R; r++) {
    truncate(p, r * 100);
    ask("size", "f", fd_ans("f"));
  }
  at(q, "f-link");
  for (int r = 0; r < R; r++) {
    if (r % 2) unlink(q); else link(p, q);
    ask("nlink", "f", fd_ans("f"));
  }
  at(q, "f-away");
  for (int r = 0; r < R; r++) {
    if (r % 2 == 0) {
      rename(p, q);
      ask("rename-away", "f", gone);
    } else {
      rename(q, p);
      ask("rename-back", "f", fd_ans("f"));
    }
  }
  for (int r = 0; r < R; r++) {
    unlink(p);
    if (r % 2) {
      ask("unlink", "f", gone);
    } else {
      touch("f", 300 + r);
      ask("recreate", "f", fd_ans("f"));
    }
  }
  touch("f", 400);
  // シンボリックリンク l を a と b に付けかえる (新しいリンクを作って名前を変えて上に)
  touch("a", 401);
  touch("b", 402);
  char l[512], tmp[512];
  at(l, "l");
  at(tmp, "l-new");
  for (int r = 0; r < R; r++) {
    unlink(tmp);
    symlink(r % 2 ? "b" : "a", tmp);
    rename(tmp, l);
    ask("symlink", "l", fd_ans(r % 2 ? "b" : "a"));
  }
  // 途中のディレクトリを取りかえる: d/e/f の d を別の d に (前の d は d-old-R に)
  for (int r = 0; r < R; r++) {
    char d[512], e[512], old[512];
    at(d, "d");
    snprintf(old, sizeof old, "%s/d-old-%d", base, r);
    rename(d, old);
    mkdir(d, 0755);
    snprintf(e, sizeof e, "%s/d/e", base);
    mkdir(e, 0755);
    touch("d/e/f", 500 + r);
    ask("parent-swap", "d/e/f", fd_ans("d/e/f"));
  }
  // だれでもは通れないディレクトリ g: 開けたときは g/f の答え、閉じたときは EACCES (root でなければ持ち主にも
  // x のない 0600、root なら持ち主の root だけが通れる 0700: 覚えたのは親の root)
  char g[512];
  at(g, "g");
  mkdir(g, 0755);
  touch("g/f", 600);
  struct ans open_ans = fd_ans("g/f");
  struct ans denied = {-EACCES};
  for (int r = 0; r < R; r++) {
    if (r % 2) {
      chmod(g, root ? 0700 : 0600);
      ask("gate-closed", "g/f", denied);
    } else {
      chmod(g, r % 4 ? 0711 : 0755);
      ask("gate-open", "g/f", open_ans);
    }
  }
  chmod(g, 0755);
}

// 止めずに変える (見る人は reader の後半)
static void running(void) {
  char f[512], cur[512], next[512];
  snprintf(f, sizeof f, "%s/run", base);
  mkdir(f, 0755);
  touch("run/f", 1000);
  __atomic_store_n(&sh->k, 0, __ATOMIC_RELEASE);
  __atomic_store_n(&sh->round, -1, __ATOMIC_RELEASE);
  at(f, "run/f");
  for (long k = 1; k <= 1500; k++) {
    struct timespec ts[2] = {{1000 + k, 0}, {1000 + k, 0}};
    utimensat(AT_FDCWD, f, ts, 0);
    __atomic_store_n(&sh->k, k, __ATOMIC_RELEASE);
  }
  // 名前のほうへ
  __atomic_store_n(&sh->k, -1, __ATOMIC_RELEASE);
  touch("run/n0", 1);
  __atomic_store_n(&sh->k, 0, __ATOMIC_RELEASE);
  for (long k = 1; k <= 1500; k++) {
    snprintf(cur, sizeof cur, "%s/run/n%ld", base, k - 1);
    snprintf(next, sizeof next, "%s/run/n%ld", base, k);
    rename(cur, next);
    __atomic_store_n(&sh->k, k, __ATOMIC_RELEASE);
  }
  __atomic_store_n(&sh->stop, 1, __ATOMIC_RELEASE);
}

// chdir のあとの cwd からのパス
static void chdir_check(void) {
  char x[512], y[512];
  at(x, "cx");
  at(y, "cy");
  mkdir(x, 0755);
  mkdir(y, 0755);
  touch("cx/f", 700);
  touch("cy/f", 701);
  char cwd[512];
  if (!getcwd(cwd, sizeof cwd)) return;
  int ok = 1;
  for (int r = 0; r < 20 && ok; r++) {
    const char *d = r % 2 ? y : x;
    if (chdir(d) != 0) ok = 0;
    struct ans got = stat_of("f");
    for (int i = 0; i < 3; i++) got = stat_of("f");
    if (got.err || got.mtime != (r % 2 ? 701 : 700)) {
      printf("  chdir %s: f mtime %ld err %ld\n", d, got.mtime, got.err);
      ok = 0;
    }
  }
  chdir(cwd);
  CHECK("statc-chdir", ok);
}

// ユーザーの namespace の中では uid が読みかわる (map を書かなければ 65534): 中で覚えたものを外の人が使わないか、逆も
static void userns_check(void) {
  char f[512];
  at(f, "u");
  touch("u", 800);
  struct stat s;
  if (stat(f, &s) != 0) return;
  long real = s.st_uid;
  int fds[2];
  for (int order = 0; order < 2; order++) {
    if (pipe(fds) != 0) return;
    if (order == 1) for (int i = 0; i < 5; i++) stat(f, &s);
    pid_t pid = fork();
    if (pid == 0) {
      close(fds[0]);
      long v = -1;
      if (unshare(CLONE_NEWUSER) == 0) {
        for (int i = 0; i < 5; i++) v = stat(f, &s) == 0 ? (long)s.st_uid : -2;
      } else {
        v = -3;
      }
      write(fds[1], &v, sizeof v);
      _exit(0);
    }
    close(fds[1]);
    long inner = -4;
    read(fds[0], &inner, sizeof inner);
    close(fds[0]);
    waitpid(pid, 0, 0);
    if (inner == -3) {
      printf("ok statc-userns (skipped: unshare(CLONE_NEWUSER): not allowed here)\n");
      return;
    }
    long outer = -1;
    for (int i = 0; i < 5; i++) outer = stat(f, &s) == 0 ? (long)s.st_uid : -2;
    // map のない namespace の中では、持ち主はだれでも 65534 (overflowuid)。外はもとのまま
    if (inner != 65534 || outer != real) {
      printf("  order %d: inside uid %ld (want 65534), outside %ld (want %ld)\n", order, inner, outer, real);
      CHECK("statc-userns", 0);
      return;
    }
  }
  CHECK("statc-userns", 1);
}

static void run(const char *dir) {
  snprintf(base, sizeof base, "%s/sc-%d", dir, getpid());
  if (mkdir(base, 0755) != 0 || chmod(base, 0755) != 0) {
    printf("FAIL statc %s: mkdir: %s\n", base, strerror(errno));
    fails++;
    return;
  }
  memset(sh, 0, sizeof *sh);
  long n = sysconf(_SC_NPROCESSORS_ONLN) - 1;
  readers = n < 2 ? 2 : n;
  pid_t kids[64];
  for (int i = 0; i < readers; i++) {
    kids[i] = fork();
    if (kids[i] == 0) reader(i);
  }
  round_no = 0;
  rounds(getuid() == 0);
  running();
  int dead = 0;
  for (int i = 0; i < readers; i++) {
    int ws;
    if (waitpid(kids[i], &ws, 0) != kids[i] || !WIFEXITED(ws) || WEXITSTATUS(ws) != 0) dead++;
  }
  char name[300];
  snprintf(name, sizeof name, "statc-cpus %s (%d readers, %d rounds)", dir, readers, round_no);
  if (sh->bad || dead) printf("  %d bad answers, %d readers failed: %s\n", sh->bad, dead, sh->msg);
  CHECK(name, sh->bad == 0 && dead == 0);
  chdir_check();
  userns_check();
  char cmd[600];
  snprintf(cmd, sizeof cmd, "rm -rf '%s'", base);
  system(cmd);
}

int main(int argc, char **argv) {
  setvbuf(stdout, 0, _IOLBF, 0);
  sh = mmap(0, sizeof *sh, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
  if (sh == MAP_FAILED) return 2;
  if (argc < 2) {
    run("/tmp");
  }
  for (int i = 1; i < argc; i++) run(argv[i]);
  printf("statc: %d failed\n", fails);
  return fails != 0;
}

// System V の共有メモリ (shmget、shmat、shmdt、shmctl) が Linux と同じに動くか (カーネル: shm.rs)。
// 作る・鍵で探す (EEXIST、ENOENT、大きさ)、IPC_STAT の欄 (大きさ、mode、nattch、cpid、lpid、時刻)、写す (maps の名前、
// SHM_RND、SHM_REMAP、SHM_RDONLY と書いたときの SIGSEGV、許し)、fork で数が増えること、shmdt (分かれた写しもみな)、
// munmap で外す、IPC_RMID (つないでいるあいだは SHM_DEST、鍵は消える)、IPC_SET、IPC_INFO、SHM_INFO、/proc/sysvipc/shm。
// PostgreSQL は小さなかたまりをとり、shm_nattch で前の postmaster がまだいるかを見る
// (aarch64-linux-gnu-gcc -O2 -static -o shm test/shm.c。test/ldiff.sh shm で Linux とくらべる)
#define _GNU_SOURCE
#include <errno.h>
#include <setjmp.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ipc.h>
#include <sys/mman.h>
#include <sys/shm.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static const char *err(int e) {
  switch (e) {
  case EINVAL: return "EINVAL";
  case EEXIST: return "EEXIST";
  case ENOENT: return "ENOENT";
  case EACCES: return "EACCES";
  case EPERM: return "EPERM";
  case EFAULT: return "EFAULT";
  case EIDRM: return "EIDRM";
  default: return strerror(e);
  }
}

#define TRY(what, call) printf("%s: %s\n", what, (call) < 0 ? err(errno) : "ok")

// fork した子が終わったとき、Linux は lpid と dtime をその子に変える (写しの shm_close)。aios は shmat と shmdt でだけ
// 変えるので、fork のまわりでは見せない
static int hide_l;
// 時刻は 0 か、はじめから今までのあいだか (手本の Linux は時計が 1970 年から始まるので、0 か set かはくらべない)
static time_t t0;

static const char *tm(time_t t) {
  return t == 0 || (t >= t0 && t <= time(NULL)) ? "ok" : "bad";
}

static void st(const char *what, int id) {
  struct shmid_ds d;
  if (shmctl(id, IPC_STAT, &d) < 0) {
    printf("%s: stat %s\n", what, err(errno));
    return;
  }
  printf("%s: size %zu mode %o nattch %lu key %s cpid %s lpid %s uid %s times %s %s %s\n", what,
         (size_t)d.shm_segsz, (unsigned)d.shm_perm.mode, (unsigned long)d.shm_nattch,
         d.shm_perm.__key == 0 ? "private" : "set", d.shm_cpid == getpid() ? "self" : "other",
         hide_l ? "-" : d.shm_lpid == 0 ? "0" : d.shm_lpid == getpid() ? "self" : "other",
         d.shm_perm.uid == geteuid() && d.shm_perm.cuid == geteuid() ? "self" : "other", tm(d.shm_atime),
         tm(d.shm_dtime), tm(d.shm_ctime));
}

static void maps(void *a) {
  char line[512];
  FILE *f = fopen("/proc/self/maps", "r");
  unsigned long want = (unsigned long)a;
  while (f && fgets(line, sizeof line, f)) {
    unsigned long s, e;
    char perm[8];
    if (sscanf(line, "%lx-%lx %7s", &s, &e, perm) == 3 && s == want) {
      char *name = strchr(line, '/');
      printf("maps: %lu pages %s %s", (e - s) / 4096, perm, name ? name : "(none)\n");
    }
  }
  if (f) fclose(f);
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  t0 = time(NULL);
  TRY("size 0", shmget(IPC_PRIVATE, 0, 0600));
  TRY("bad id stat", shmctl(12345678, IPC_STAT, &(struct shmid_ds){0}));
  TRY("negative id", shmctl(-1, IPC_STAT, &(struct shmid_ds){0}));

  int id = shmget(IPC_PRIVATE, 10000, IPC_CREAT | 0600);
  printf("create: %s\n", id >= 0 ? "ok" : err(errno));
  st("new", id);
  TRY("bad cmd", shmctl(id, 99, NULL));
  TRY("stat efault", shmctl(id, IPC_STAT, (struct shmid_ds *)8));

  char *a = shmat(id, NULL, 0);
  printf("attach: %s aligned %d\n", a == (void *)-1 ? err(errno) : "ok", ((unsigned long)a & 4095) == 0);
  st("attached", id);
  maps(a);
  memset(a, 'x', 10000);
  a[9999] = 'z';
  // ページの終わりまでは触れる (大きさは 10000、写したのは 3 ページ)
  a[12287] = 'e';

  // fork: 子も同じページを見て、つないでいる数が増える
  int p1[2], p2[2];
  pipe(p1);
  pipe(p2);
  hide_l = 1;
  pid_t c = fork();
  if (c == 0) {
    char b;
    a[0] = 'c';
    write(p1[1], "r", 1);
    read(p2[0], &b, 1);
    _exit(a[9999] == 'z' ? 0 : 1);
  }
  char b;
  read(p1[0], &b, 1);
  printf("child wrote %c\n", a[0]);
  st("forked", id);
  write(p2[1], "g", 1);
  int ws;
  waitpid(c, &ws, 0);
  printf("child exit %d\n", WEXITSTATUS(ws));
  st("child gone", id);
  hide_l = 0;

  // shmdt: 頭でなければ EINVAL、かたまりでないところも EINVAL
  TRY("shmdt +1", shmdt(a + 1));
  TRY("shmdt +4096", shmdt(a + 4096));
  void *m = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  TRY("shmdt anon", shmdt(m));
  TRY("shmdt", shmdt(a));
  st("detached", id);
  TRY("shmdt again", shmdt(a));

  // 外しても中身は残る
  a = shmat(id, NULL, SHM_RDONLY);
  printf("reattach ro: %c%c perm\n", a[1], a[9999]);
  maps(a);
  pid_t w = fork();
  if (w == 0) {
    a[0] = 'w';
    _exit(0);
  }
  waitpid(w, &ws, 0);
  printf("write ro: %s\n", WIFSIGNALED(ws) ? strsignal(WTERMSIG(ws)) : "no signal");
  shmdt(a);

  // 場所を決めて写す: SHM_RND でそろえる、そろっていなければ EINVAL、重なれば EINVAL (SHM_REMAP なら上に)
  char *spot = mmap(NULL, 8 * 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  munmap(spot, 8 * 4096);
  void *r = shmat(id, spot + 100, 0);
  printf("unaligned: %s\n", r == (void *)-1 ? err(errno) : "ok");
  r = shmat(id, spot + 100, SHM_RND);
  printf("rnd: %s at spot %d\n", r == (void *)-1 ? err(errno) : "ok", r == spot);
  void *r2 = shmat(id, spot, 0);
  printf("overlap: %s\n", r2 == (void *)-1 ? err(errno) : "ok");
  r2 = shmat(id, spot, SHM_REMAP);
  printf("remap: %s same %d\n", r2 == (void *)-1 ? err(errno) : "ok", r2 == spot);
  st("remapped", id);
  r2 = shmat(id, NULL, SHM_REMAP);
  printf("remap null: %s\n", r2 == (void *)-1 ? err(errno) : "ok");
  // 中を munmap すると写しが 2 つに分かれる (数は写しごと)。shmdt はみな外す
  munmap(spot + 4096, 4096);
  st("split", id);
  TRY("shmdt split", shmdt(spot));
  st("after split", id);
  // munmap でも外れる
  a = shmat(id, NULL, 0);
  munmap(a, 12288);
  st("munmapped", id);

  // 鍵で探す
  key_t k = 0x5eed1234;
  int kid = shmget(k, 100, IPC_CREAT | IPC_EXCL | 0600);
  printf("key create: %s\n", kid >= 0 ? "ok" : err(errno));
  TRY("key excl again", shmget(k, 100, IPC_CREAT | IPC_EXCL | 0600));
  printf("key find: %s\n", shmget(k, 0, 0) == kid ? "same" : err(errno));
  printf("key creat no excl: %s\n", shmget(k, 50, IPC_CREAT | 0600) == kid ? "same" : err(errno));
  TRY("key too big", shmget(k, 200, 0));
  TRY("key missing", shmget(k + 1, 100, 0));
  st("keyed", kid);

  // IPC_SET (mode の 9 ビットだけ)、許しが足りないと shmat が EACCES
  struct shmid_ds d;
  shmctl(kid, IPC_STAT, &d);
  d.shm_perm.mode = 0400 | 07000;
  TRY("set", shmctl(kid, IPC_SET, &d));
  st("set 0400", kid);
  void *q = shmat(kid, NULL, 0);
  printf("attach rw on 0400: %s\n", q == (void *)-1 ? err(errno) : "ok");
  q = shmat(kid, NULL, SHM_RDONLY);
  printf("attach ro on 0400: %s\n", q == (void *)-1 ? err(errno) : "ok");
  d.shm_perm.mode = 0600;
  shmctl(kid, IPC_SET, &d);

  // つないだまま IPC_RMID: SHM_DEST がつき鍵は消える。番号ではまだ見え、つなげる。みな外すと消える
  TRY("rmid attached", shmctl(kid, IPC_RMID, NULL));
  st("dest", kid);
  TRY("key after rmid", shmget(k, 0, 0));
  void *q2 = shmat(kid, NULL, 0);
  printf("attach dest: %s\n", q2 == (void *)-1 ? err(errno) : "ok");
  st("dest 2", kid);
  shmdt(q);
  shmdt(q2);
  st("dest gone", kid);
  TRY("rmid gone", shmctl(kid, IPC_RMID, NULL));

  // IPC_INFO と SHM_INFO、/proc/sysvipc/shm
  struct shminfo si;
  int mx = shmctl(0, IPC_INFO, (struct shmid_ds *)&si);
  printf("ipc_info: %s shmmin %lu shmmni %lu shmmax big %d\n", mx >= 0 ? "ok" : err(errno), si.shmmin, si.shmmni,
         si.shmmax == ~0UL - (1UL << 24));
  struct shm_info sinfo;
  printf("shm_info: %s used %d tot %lu\n", shmctl(0, SHM_INFO, (struct shmid_ds *)&sinfo) >= 0 ? "ok" : err(errno),
         sinfo.used_ids, sinfo.shm_tot);
  FILE *f = fopen("/proc/sysvipc/shm", "r");
  char line[512];
  int lines = 0;
  if (f) {
    fgets(line, sizeof line, f);
    printf("proc head: %.12s\n", line);
    while (fgets(line, sizeof line, f)) lines++;
    fclose(f);
  }
  printf("proc: %s lines %d\n", f ? "ok" : err(errno), lines);

  TRY("rmid", shmctl(id, IPC_RMID, NULL));
  st("removed", id);
  printf("done\n");
  return 0;
}

// clone3 と clone のフラグのテスト (カーネル): aios の中で動かす。glibc は pthread_create と posix_spawn で先に clone3 を試す
// (aarch64-linux-gnu-gcc -O2 -static -pthread -o clone3 test/clone3.c。aish-vm の try で file: test/clone3.c でも)。
// clone3 があること (小さすぎる size は ENOSYS でなく EINVAL)、スレッドの TLS (tls) と tid、posix_spawn、
// ないプログラムの posix_spawn が ENOENT (glibc は CLONE_VM | CLONE_VFORK の子が、共有のメモリで知らせる)、
// CLONE_VM だけの子の書いたものが親に見えること、vfork の親が子の終わりまで待つこと、いくつものスレッドから同時に posix_spawn
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <spawn.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;
static __thread int tl = 7;
static volatile int shared;

static int vm_child(void *a) {
  shared = 42;
  return 0;
}

// スレッドごとに true を n 回 posix_spawn して、うまくいった数を返す
static void *spawner(void *a) {
  long ok = 0;
  for (int i = 0; i < 50; i++) {
    pid_t p;
    int st = 0;
    char *argv[] = {"true", 0};
    if (posix_spawnp(&p, "true", 0, 0, argv, environ) == 0 && waitpid(p, &st, 0) == p && WIFEXITED(st) && WEXITSTATUS(st) == 0)
      ok++;
  }
  return (void *)ok;
}

static void *f(void *a) {
  tl += (int)(intptr_t)a;
  return (void *)(intptr_t)(tl * 10 + (gettid() != getpid()));
}

int main(void) {
  int fail = 0;
  long r = syscall(435, (void *)0, 0);
  if (r != -1 || errno != EINVAL) {
    printf("FAIL clone3(NULL, 0) = %ld errno %d (want EINVAL)\n", r, errno);
    fail = 1;
  }
  // スレッドごとに tl は 7 から: (7 + i) * 10 + 1 の和
  long sum = 0, want = 0;
  for (int i = 0; i < 64; i++) {
    pthread_t t;
    void *v;
    if (pthread_create(&t, 0, f, (void *)(intptr_t)i)) {
      printf("FAIL pthread_create %d\n", i);
      return 1;
    }
    pthread_join(t, &v);
    sum += (long)(intptr_t)v;
    want += (7 + i) * 10 + 1;
  }
  if (sum != want || tl != 7) {
    printf("FAIL threads: sum %ld (want %ld), main tl %d\n", sum, want, tl);
    fail = 1;
  }
  pid_t p;
  char *argv[] = {"true", 0};
  int st = 0;
  int e = posix_spawnp(&p, "true", 0, 0, argv, environ);
  if (e || waitpid(p, &st, 0) != p || !WIFEXITED(st) || WEXITSTATUS(st)) {
    printf("FAIL posix_spawn %d status %d\n", e, st);
    fail = 1;
  }
  // ないプログラムは ENOENT (子が 127 で終わるのではなく)
  char *missing[] = {"no-such-program", 0};
  e = posix_spawnp(&p, "no-such-program", 0, 0, missing, environ);
  if (e != ENOENT) {
    printf("FAIL posix_spawn of a missing program = %d (want ENOENT)\n", e);
    if (e == 0)
      waitpid(p, &st, 0);
    fail = 1;
  }
  // CLONE_VM だけ (スレッドでない): 同じメモリ
  static char stack[65536];
  pid_t c = clone(vm_child, stack + sizeof stack, CLONE_VM | SIGCHLD, 0);
  if (c < 0 || waitpid(c, &st, 0) != c || shared != 42) {
    printf("FAIL clone(CLONE_VM): the child's write is not seen (%d)\n", shared);
    fail = 1;
  }
  // vfork: 親は子が終わるまで待つ (子が書いたものがもう見える)
  volatile int order = 0;
  pid_t v = vfork();
  if (v == 0) {
    order = 1;
    _exit(0);
  }
  if (order != 1) {
    printf("FAIL vfork: the parent ran before the child exited\n");
    fail = 1;
  }
  waitpid(v, &st, 0);
  // 4 つのスレッドから同時に posix_spawn (vfork の待ちと、共有のメモリ)
  pthread_t ts[4];
  long spawned = 0;
  for (int i = 0; i < 4; i++)
    pthread_create(&ts[i], 0, spawner, 0);
  for (int i = 0; i < 4; i++) {
    void *r;
    pthread_join(ts[i], &r);
    spawned += (long)r;
  }
  if (spawned != 200) {
    printf("FAIL posix_spawn from 4 threads: %ld of 200\n", spawned);
    fail = 1;
  }
  printf("%s\n", fail ? "clone3: FAIL" : "clone3: ok");
  return fail;
}

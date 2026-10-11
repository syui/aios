// clone3 のテスト (カーネル): aios の中で動かす。glibc は pthread_create と posix_spawn で先に clone3 を試す
// (aarch64-linux-gnu-gcc -O2 -static -pthread -o clone3 test/clone3.c。aish-vm の try で file: test/clone3.c でも)。
// clone3 があること (小さすぎる size は ENOSYS でなく EINVAL)、スレッドの TLS (tls) と tid、posix_spawn
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <spawn.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;
static __thread int tl = 7;

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
  printf("%s\n", fail ? "clone3: FAIL" : "clone3: ok");
  return fail;
}

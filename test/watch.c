// aiwatch を試す (わざと見つかるものを作る): aios の中で動かす。作りかたは test/seccomp.c と同じ
// (zig cc -target aarch64-linux-musl -O2 -static -o watch-test test/watch.c)
//   watch-test storm   寝ているスレッドを、値を変えずに FUTEX_WAKE で起こしつづける → wake_storm
//   watch-test futex   futex の値を変えたのに FUTEX_WAKE しない (起こしそこね) → lost_wakeup_futex
// どちらも 60 秒で終わる
#include <linux/futex.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

static atomic_int word;
static volatile int stop;

static long futex(atomic_int *a, int op, int v) { return syscall(SYS_futex, a, op, v, 0, 0, 0); }

static void *sleeper(void *x) {
  (void)x;
  // 値が 0 のあいだ待ちつづける (起こされても値を見て眠りなおす)
  while (!stop && atomic_load(&word) == 0) futex(&word, FUTEX_WAIT_PRIVATE, 0);
  return 0;
}

int main(int argc, char **argv) {
  int storm = argc > 1 && strcmp(argv[1], "storm") == 0;
  pthread_t t;
  pthread_create(&t, 0, sleeper, 0);
  printf("watch-test %s pid %d\n", storm ? "storm" : "futex", getpid());
  fflush(stdout);
  if (storm) {
    // 休まずに起こしつづける (usleep はタイマの刻みにまるめられて遅すぎる)
    for (int i = 0; i < 60 * 200; i++) {
      for (int k = 0; k < 50; k++) {
        futex(&word, FUTEX_WAKE_PRIVATE, 1);
        sched_yield();
      }
      usleep(1);
    }
  } else {
    sleep(1);
    atomic_store(&word, 1);  // 変えたのに起こさない
    sleep(60);
  }
  stop = 1;
  atomic_store(&word, 1);
  futex(&word, FUTEX_WAKE_PRIVATE, 1);
  pthread_join(t, 0);
  return 0;
}

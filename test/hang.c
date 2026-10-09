// わざと止まるプログラム (aish-sys の hang と /proc/ai を試す): aios の中で動かす。作りかたは test/seccomp.c と同じ
// (zig cc -target aarch64-linux-musl -O2 -static -o hang test/hang.c)。
// 代表スレッドはだれも知らせない条件変数で、もう 1 つのスレッドは何も来ないパイプの poll で眠る。
// 別のパイプには読まれないデータを残す。hang は「futex の値が変わっていない (だれも知らせていない)」
// 「kick しても同じところで眠りなおす」「fd 5 に 6 バイト残っている」と答えるはず
#include <poll.h>
#include <pthread.h>
#include <stdio.h>
#include <unistd.h>

static pthread_mutex_t m = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t c = PTHREAD_COND_INITIALIZER;
static int p1[2], p2[2];

static void *poller(void *a) {
  (void)a;
  struct pollfd f = {p1[0], POLLIN};
  poll(&f, 1, -1);
  return 0;
}

int main(void) {
  pipe(p1);
  pipe(p2);
  write(p2[1], "unread", 6);
  pthread_t t;
  pthread_create(&t, 0, poller, 0);
  printf("hang pid %d\n", getpid());
  fflush(stdout);
  pthread_mutex_lock(&m);
  pthread_cond_wait(&c, &m);
  return 0;
}

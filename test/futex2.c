// futex2 (Linux 6.7 の futex_wake、futex_wait、futex_requeue) が Linux と同じか: flags は FUTEX2_SIZE_U32 だけ (ほかの
// 大きさと知らない印は EINVAL)、mask 0 は EINVAL、futex_wake の nr 0 は起こさない、futex_wait は値がちがえば EAGAIN、
// 時間は絶対で CLOCK_MONOTONIC と CLOCK_REALTIME だけ (ほかは EINVAL)、時間切れは ETIMEDOUT、起こされれば 0、
// futex_requeue は flags 0 だけ、waiters[0] の値がちがえば EAGAIN、起こした数と移した数
#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#define FUTEX2_SIZE_U8 0
#define FUTEX2_SIZE_U32 2
#define FUTEX2_SIZE_U64 3
#define FUTEX2_PRIVATE 128
#define M 0xffffffffUL

struct waitv {
  uint64_t val, uaddr;
  uint32_t flags, reserved;
};

static long fw(uint32_t *u, unsigned long mask, int nr, unsigned f) { return syscall(454, u, mask, nr, f) < 0 ? -errno : syscall(454, u, mask, 0, f); }
static long wake(uint32_t *u, unsigned long mask, int nr, unsigned f) {
  long r = syscall(454, u, mask, nr, f);
  return r < 0 ? -errno : r;
}
static long wait2(uint32_t *u, unsigned long v, unsigned long mask, unsigned f, struct timespec *t, int clk) {
  long r = syscall(455, u, v, mask, f, t, clk);
  return r < 0 ? -errno : r;
}
static long requeue(struct waitv *w, unsigned f, int nw, int nq) {
  long r = syscall(456, w, f, nw, nq);
  return r < 0 ? -errno : r;
}

static uint32_t word = 0, word2 = 0;
static volatile int waited = -99;
static void *waiter(void *arg) {
  (void)arg;
  waited = (int)wait2(&word, 0, M, FUTEX2_SIZE_U32 | FUTEX2_PRIVATE, NULL, CLOCK_MONOTONIC);
  return NULL;
}

static struct timespec in_ms(int clk, long ms) {
  struct timespec t;
  clock_gettime(clk, &t);
  t.tv_nsec += ms * 1000000;
  t.tv_sec += t.tv_nsec / 1000000000;
  t.tv_nsec %= 1000000000;
  return t;
}

int main(void) {
  setvbuf(stdout, NULL, _IONBF, 0);
  (void)fw;
  unsigned P = FUTEX2_SIZE_U32 | FUTEX2_PRIVATE;
  printf("futex_wake (mask > u32 %ld, u8 %ld, u64 %ld, flag 0x10 %ld, mask 0 %ld, nr 0 %ld, nr 1 nobody %ld, shared %ld)\n", wake(&word, ~0UL, 1, FUTEX2_SIZE_U32 | FUTEX2_PRIVATE), wake(&word, M, 1, FUTEX2_SIZE_U8), wake(&word, M, 1, FUTEX2_SIZE_U64),
         wake(&word, M, 1, P | 0x10), wake(&word, 0, 1, P), wake(&word, M, 0, P), wake(&word, M, 1, P), wake(&word, M, 1, FUTEX2_SIZE_U32));
  struct timespec t = in_ms(CLOCK_MONOTONIC, 50), r = in_ms(CLOCK_REALTIME, 50);
  printf("futex_wait (value differs %ld, mask 0 %ld, val > u32 %ld, clock BOOTTIME %ld, timeout %ld, REALTIME timeout %ld, past %ld)\n", wait2(&word, 1, M, P, NULL, CLOCK_MONOTONIC),
         wait2(&word, 0, 0, P, NULL, CLOCK_MONOTONIC), wait2(&word, 1UL << 33, M, P, NULL, CLOCK_MONOTONIC), wait2(&word, 0, M, P, &t, CLOCK_BOOTTIME),
         wait2(&word, 0, M, P, &t, CLOCK_MONOTONIC), wait2(&word, 0, M, P, &r, CLOCK_REALTIME), wait2(&word, 0, M, P, &(struct timespec){0, 0}, CLOCK_MONOTONIC));
  pthread_t th;
  pthread_create(&th, NULL, waiter, NULL);
  usleep(200000);
  long woke = wake(&word, M, 5, P);
  pthread_join(th, NULL);
  printf("wake a waiter (%ld, waiter got %d)\n", woke, waited);
  // requeue
  struct waitv w[2] = {{0, (uintptr_t)&word, P, 0}, {0, (uintptr_t)&word2, P, 0}};
  printf("futex_requeue (flags 1 %ld, reserved %ld, value differs %ld, nobody %ld)\n", requeue(w, 1, 1, 1),
         requeue((struct waitv[2]){{0, (uintptr_t)&word, P, 1}, {0, (uintptr_t)&word2, P, 0}}, 0, 1, 1),
         requeue((struct waitv[2]){{9, (uintptr_t)&word, P, 0}, {0, (uintptr_t)&word2, P, 0}}, 0, 1, 1), requeue(w, 0, 1, 1));
  waited = -99;
  pthread_t a, b;
  pthread_create(&a, NULL, waiter, NULL);
  pthread_create(&b, NULL, waiter, NULL);
  usleep(200000);
  long rq = requeue(w, 0, 1, 1);
  usleep(100000);
  long w2 = wake(&word2, M, 5, P);
  pthread_join(a, NULL);
  pthread_join(b, NULL);
  printf("requeue two waiters (%ld, then wake on the second word %ld)\n", rq, w2);
  return 0;
}

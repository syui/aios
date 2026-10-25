// futex の待ちの行列 (kernel/src/futex.rs: key のハッシュの箱と、使いまわす入れもの) を、いくつもの CPU から同時に:
// pthread の mutex (musl は FUTEX_WAIT / WAKE)、cond で受けわたす行列 (signal と broadcast)、時間切れで終わる待ち
// (行列から自分で出る)、生の FUTEX_CMP_REQUEUE (別の key へ移して、移した先で起こす)。数があえば 0 で終わる。起こしが
// 消えれば待つものが眠りつづけるので、alarm で止めて失敗にする。Linux でも同じ答え
// (aarch64-linux-gnu-gcc -O2 -static -pthread -o futexrace test/futexrace.c)。futexrace [SECONDS]
#define _GNU_SOURCE
#include <errno.h>
#include <limits.h>
#include <linux/futex.h>
#include <pthread.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static double secs = 5;
static atomic_int stop;

static double now(void) {
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  return t.tv_sec + t.tv_nsec / 1e9;
}

static long fx(atomic_int *a, int op, int val, long v2, atomic_int *b, int val3) {
  return syscall(SYS_futex, a, op | FUTEX_PRIVATE_FLAG, val, v2, b, val3);
}

// mutex: 4 つを 6 つのスレッドで取りあう
#define NM 4
static pthread_mutex_t mu[NM] = {PTHREAD_MUTEX_INITIALIZER, PTHREAD_MUTEX_INITIALIZER, PTHREAD_MUTEX_INITIALIZER, PTHREAD_MUTEX_INITIALIZER};
static long counted[NM];
static atomic_long locked;

static void *locker(void *arg) {
  long k = (long)arg, n = 0;
  while (!stop) {
    int i = (k + n) % NM;
    pthread_mutex_lock(&mu[i]);
    counted[i]++;
    pthread_mutex_unlock(&mu[i]);
    n++;
  }
  locked += n;
  return 0;
}

// 受けわたし: 4 こまでの行列を、2 つが入れて 2 つが出す
static pthread_mutex_t qm = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t notfull = PTHREAD_COND_INITIALIZER, notempty = PTHREAD_COND_INITIALIZER;
static long q[4], qn, qh;
static atomic_long put_sum, got_sum;
static int producers = 2;

static void *producer(void *arg) {
  long v = (long)arg * 1000000;
  while (!stop) {
    pthread_mutex_lock(&qm);
    while (qn == 4 && !stop) pthread_cond_wait(&notfull, &qm);
    if (qn < 4) {
      q[(qh + qn) % 4] = ++v;
      qn++;
      put_sum += v;
      if (v & 1) pthread_cond_signal(&notempty);
      else pthread_cond_broadcast(&notempty);
    }
    pthread_mutex_unlock(&qm);
  }
  pthread_mutex_lock(&qm);
  producers--;
  pthread_cond_broadcast(&notempty);
  pthread_mutex_unlock(&qm);
  return 0;
}

static void *consumer(void *arg) {
  (void)arg;
  for (;;) {
    pthread_mutex_lock(&qm);
    while (qn == 0 && producers > 0) pthread_cond_wait(&notempty, &qm);
    if (qn == 0) {
      pthread_mutex_unlock(&qm);
      return 0;
    }
    got_sum += q[qh];
    qh = (qh + 1) % 4;
    qn--;
    pthread_cond_broadcast(&notfull);
    pthread_mutex_unlock(&qm);
  }
}

// 時間切れ: だれも起こさない cond と語で 1 ms ずつ待つ (時間切れの答えでなければ失敗)
static pthread_mutex_t tm = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t never = PTHREAD_COND_INITIALIZER;
static atomic_int word;
static atomic_long timeouts, bad_timeouts;

static void *timer_main(void *arg) {
  (void)arg;
  while (!stop) {
    struct timespec ts;
    clock_gettime(CLOCK_REALTIME, &ts);
    ts.tv_nsec += 1000000;
    if (ts.tv_nsec >= 1000000000) ts.tv_sec++, ts.tv_nsec -= 1000000000;
    pthread_mutex_lock(&tm);
    int r = pthread_cond_timedwait(&never, &tm, &ts);
    pthread_mutex_unlock(&tm);
    struct timespec rel = {0, 1000000};
    long r2 = fx(&word, FUTEX_WAIT, 0, (long)&rel, 0, 0);
    if (r == ETIMEDOUT && r2 == -1 && errno == ETIMEDOUT) timeouts++;
    else bad_timeouts++;
  }
  return 0;
}

// 移す: 4 つが g で待ち、1 つが g を進めて、1 つ起こしてのこりを g2 へ移し、g2 をみな起こす
#define NW 4
static atomic_int g, g2, waiters_out;
static atomic_long rounds, requeued;

static void *rq_waiter(void *arg) {
  (void)arg;
  while (!stop) {
    int v = g;
    fx(&g, FUTEX_WAIT, v, 0, 0, 0);
    rounds++;
  }
  waiters_out++;
  return 0;
}

static void *requeuer(void *arg) {
  (void)arg;
  while (!stop) {
    int v = ++g;
    long r = fx(&g, FUTEX_CMP_REQUEUE, 1, INT_MAX, &g2, v);
    if (r > 1) requeued += r - 1;
    fx(&g2, FUTEX_WAKE, INT_MAX, 0, 0, 0);
    usleep(50);
  }
  // 待つものが stop を見てから眠ることがあるので、みな出るまで進めて起こす
  while (waiters_out < NW) {
    ++g;
    fx(&g, FUTEX_WAKE, INT_MAX, 0, 0, 0);
    fx(&g2, FUTEX_WAKE, INT_MAX, 0, 0, 0);
    usleep(1000);
  }
  return 0;
}

static void hung(int s) {
  (void)s;
  static const char m[] = "FAIL futexrace: a waiter was never woken (alarm)\n";
  if (write(1, m, sizeof m - 1) < 0) _exit(2);
  _exit(1);
}

int main(int argc, char **argv) {
  if (argc > 1) secs = atof(argv[1]);
  signal(SIGALRM, hung);
  alarm((unsigned)secs + 30);
  pthread_t t[32];
  int n = 0;
  for (long i = 0; i < 6; i++) pthread_create(&t[n++], 0, locker, (void *)i);
  for (long i = 0; i < 2; i++) pthread_create(&t[n++], 0, producer, (void *)(i + 1));
  for (long i = 0; i < 2; i++) pthread_create(&t[n++], 0, consumer, 0);
  for (long i = 0; i < 2; i++) pthread_create(&t[n++], 0, timer_main, 0);
  for (long i = 0; i < NW; i++) pthread_create(&t[n++], 0, rq_waiter, 0);
  pthread_create(&t[n++], 0, requeuer, 0);
  double end = now() + secs;
  while (now() < end) usleep(10000);
  stop = 1;
  pthread_mutex_lock(&qm);
  pthread_cond_broadcast(&notfull);
  pthread_cond_broadcast(&notempty);
  pthread_mutex_unlock(&qm);
  for (int i = 0; i < n; i++) pthread_join(t[i], 0);
  int fails = 0;
  long sum = 0;
  for (int i = 0; i < NM; i++) sum += counted[i];
  if (sum != locked) printf("FAIL mutex: counted %ld, locked %ld\n", sum, (long)locked), fails++;
  if (put_sum != got_sum) printf("FAIL queue: put %ld, got %ld\n", (long)put_sum, (long)got_sum), fails++;
  if (bad_timeouts) printf("FAIL timeout: %ld waits did not time out\n", (long)bad_timeouts), fails++;
  if (!timeouts || !rounds || !locked) printf("FAIL nothing ran: locks %ld timeouts %ld rounds %ld\n", (long)locked, (long)timeouts, (long)rounds), fails++;
  printf("futexrace: %ld locks, queue sum %ld, %ld timeouts, %ld requeue rounds (%ld moved): %s\n", (long)locked, (long)got_sum, (long)timeouts, (long)rounds,
         (long)requeued, fails ? "FAIL" : "ok");
  return fails;
}

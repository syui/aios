// 代表でないスレッドからの execve のテスト (カーネル: proc.rs の exec_takeover): aios の中で動かす。
// Linux と同じく、ほかのスレッドは消えて、プロセスは同じ PID のまま新しいプログラムになり、親にはその終わりが見える
// (前は代表スレッドが SIGKILL で終わったように見えた。zig cc -MM が make で "Killed" になった)。
// aarch64-linux-gnu-gcc -O2 -static -pthread -o exec-thread test/exec-thread.c。aish-vm の try で file: test/exec-thread.c でも
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static char self[4096];
static char pidarg[32];

static void *run(void *a) {
  (void)a;
  char *argv[] = {self, "after", pidarg, NULL};
  execv(self, argv);
  perror("execv");
  _exit(100);
}

int main(int argc, char **argv) {
  // exec のあと: 同じ PID か
  if (argc == 3 && strcmp(argv[1], "after") == 0) {
    return getpid() == atoi(argv[2]) ? 3 : 4;
  }
  ssize_t n = readlink("/proc/self/exe", self, sizeof self - 1);
  if (n <= 0) {
    puts("exec-thread: FAIL readlink /proc/self/exe");
    return 1;
  }
  self[n] = 0;
  pid_t c = fork();
  if (c == 0) {
    snprintf(pidarg, sizeof pidarg, "%d", getpid());
    pthread_t t;
    pthread_create(&t, NULL, run, NULL);
    // 代表スレッドは待っている (execve で消える)
    pthread_join(t, NULL);
    _exit(101);
  }
  int st;
  if (waitpid(c, &st, 0) != c) {
    puts("exec-thread: FAIL waitpid");
    return 1;
  }
  if (WIFSIGNALED(st)) {
    printf("exec-thread: FAIL killed by signal %d (the leader was killed instead of taking over the execve)\n", WTERMSIG(st));
    return 1;
  }
  if (!WIFEXITED(st) || WEXITSTATUS(st) != 3) {
    printf("exec-thread: FAIL exit status %d (want 3: the same pid after execve)\n", WEXITSTATUS(st));
    return 1;
  }
  puts("exec-thread: ok");
  return 0;
}

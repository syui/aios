// メモリを使いきるプロセスがいても、ほかは生きのこる: 子 (hog) が RAM とスワップより大きい無名のメモリに書いていき、
// 親はそのあいだも少しずつ新しいページに書く。ページが取れなくなったら、カーネルはいちばん大きいもの (hog) を
// SIGKILL で終わらせ、困ったスレッドはやりなおす (oom.rs on_fault)。前は困ったスレッドをそのまま殺していたので、
// たまたまフォルトした init が殺されてカーネルが止まった (init exited with status 9)
//   cc -O2 -static -o oomfault oomfault.c && ./oomfault
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

static size_t meminfo(const char *key) {
    FILE *f = fopen("/proc/meminfo", "r");
    char line[256];
    size_t kb = 0, k = strlen(key);
    while (f && fgets(line, sizeof line, f))
        if (!strncmp(line, key, k) && line[k] == ':') {
            kb = strtoul(line + k + 1, 0, 10);
            break;
        }
    if (f)
        fclose(f);
    return kb << 10;
}

int main(void) {
    long pg = sysconf(_SC_PAGESIZE);
    size_t hog = meminfo("MemTotal") + meminfo("SwapTotal") + (256 << 20);
    pid_t c = fork();
    if (c == 0) {
        char *m = mmap(0, hog, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
        if (m == MAP_FAILED)
            _exit(3);
        for (size_t i = 0; i < hog; i += pg)
            m[i] = 1;
        _exit(0); // ここまで来たら、使いきれなかった
    }
    // 親: hog が終わるまで、1 秒に 1 MiB ずつ新しいページに書く (フォルトしつづける)
    int st = 0;
    size_t mine = 0;
    while (waitpid(c, &st, WNOHANG) == 0) {
        char *p = malloc(1 << 20);
        if (p) {
            memset(p, 2, 1 << 20);
            mine += 1 << 20;
        }
        usleep(200000);
    }
    if (!WIFSIGNALED(st) || WTERMSIG(st) != SIGKILL) {
        printf("FAIL: the hog (%zu MiB) ended with status %#x, not SIGKILL\n", hog >> 20, st);
        return 1;
    }
    printf("oomfault: ok (the %zu MiB hog was killed by SIGKILL; this process wrote %zu MiB meanwhile)\n", hog >> 20, mine >> 20);
    return 0;
}

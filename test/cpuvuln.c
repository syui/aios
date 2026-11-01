// CPU のぜい弱性 (kernel/src/cpuvuln.rs): /sys/devices/system/cpu/vulnerabilities/* の中身と、prctl の
// PR_GET/SET_SPECULATION_CTRL (PR_SPEC_STORE_BYPASS)、/proc/self/status の Speculation_Store_Bypass、PSTATE.SSBS
// (CPU に SSBS があれば MRS で読む)。答えは出すだけ (CPU によってちがう): 同じ QEMU の CPU (cortex-a72 や max) の
// Linux と出力をくらべる。root でなくてよい
//   aios の中で: zig cc -target aarch64-linux-musl -O2 -static -o cpuvuln test/cpuvuln.c; ./cpuvuln
//   Linux で: LINUX_CPU=cortex-a72 test/linux-vm.sh exec latest test/bin/cpuvuln
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/prctl.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef PR_GET_SPECULATION_CTRL
#define PR_GET_SPECULATION_CTRL 52
#define PR_SET_SPECULATION_CTRL 53
#define PR_SPEC_STORE_BYPASS 0
#define PR_SPEC_INDIRECT_BRANCH 1
#define PR_SPEC_ENABLE (1UL << 1)
#define PR_SPEC_DISABLE (1UL << 2)
#define PR_SPEC_FORCE_DISABLE (1UL << 3)
#define PR_SPEC_DISABLE_NOEXEC (1UL << 4)
#endif
#ifndef HWCAP_SSBS
#define HWCAP_SSBS (1 << 28)
#endif

static long r(long v) {
    return v < 0 ? -errno : v;
}

static long get(void) {
    return r(prctl(PR_GET_SPECULATION_CTRL, PR_SPEC_STORE_BYPASS, 0, 0, 0));
}

static long set(unsigned long ctrl) {
    return r(prctl(PR_SET_SPECULATION_CTRL, PR_SPEC_STORE_BYPASS, ctrl, 0, 0));
}

// /proc/self/status の Speculation_Store_Bypass と SpeculationIndirectBranch
static void status(const char *what) {
    FILE *f = fopen("/proc/self/status", "r");
    char l[256];
    int seen = 0;
    while (f && fgets(l, sizeof l, f)) {
        if (!strncmp(l, "Speculation", 11)) {
            printf("%s status %s", what, l);
            seen = 1;
        }
    }
    if (!seen)
        printf("%s status: no Speculation lines\n", what);
    if (f)
        fclose(f);
}

// PSTATE.SSBS (EL0 から MRS で読める。SSBS のない CPU では読まない)
static long ssbs(void) {
    if (!(getauxval(AT_HWCAP) & HWCAP_SSBS))
        return -1;
    unsigned long v;
    __asm__ volatile("mrs %0, s3_3_c4_c2_6" : "=r"(v));
    return (v >> 12) & 1;
}

static void show(const char *what) {
    printf("%s: get %ld ssbs %ld\n", what, get(), ssbs());
}

static void *thread_disable(void *a) {
    (void)a;
    printf("thread set disable %ld\n", set(PR_SPEC_DISABLE));
    show("thread after disable");
    return NULL;
}

int main(int argc, char **argv) {
    // fork した子の行もそのまま (Linux の手本では出力がファイルなので)
    setvbuf(stdout, NULL, _IONBF, 0);
    if (argc > 1 && !strcmp(argv[1], "--after-exec")) {
        show("after exec");
        status("after exec");
        return 0;
    }
    // ファイルの一覧と中身
    DIR *d = opendir("/sys/devices/system/cpu/vulnerabilities");
    if (!d) {
        printf("no vulnerabilities directory: %s\n", strerror(errno));
        return 1;
    }
    char names[64][64];
    int n = 0;
    struct dirent *e;
    while ((e = readdir(d)) && n < 64) {
        if (e->d_name[0] != '.')
            snprintf(names[n++], 64, "%.63s", e->d_name);
    }
    closedir(d);
    qsort(names, n, 64, (int (*)(const void *, const void *))strcmp);
    printf("files %d\n", n);
    for (int i = 0; i < n; i++) {
        char p[128], b[256] = {0};
        snprintf(p, sizeof p, "/sys/devices/system/cpu/vulnerabilities/%s", names[i]);
        FILE *f = fopen(p, "r");
        if (f && fgets(b, sizeof b, f))
            printf("%s: %s", names[i], b);
        else
            printf("%s: (unreadable)\n", names[i]);
        if (f)
            fclose(f);
    }
    // prctl
    show("start");
    status("start");
    printf("get arg3 %ld\n", r(prctl(PR_GET_SPECULATION_CTRL, PR_SPEC_STORE_BYPASS, 1, 0, 0)));
    printf("get indirect %ld\n", r(prctl(PR_GET_SPECULATION_CTRL, PR_SPEC_INDIRECT_BRANCH, 0, 0, 0)));
    printf("get l1d %ld\n", r(prctl(PR_GET_SPECULATION_CTRL, 2, 0, 0, 0)));
    printf("set indirect %ld\n", r(prctl(PR_SET_SPECULATION_CTRL, PR_SPEC_INDIRECT_BRANCH, PR_SPEC_DISABLE, 0, 0)));
    printf("set arg4 %ld\n", r(prctl(PR_SET_SPECULATION_CTRL, PR_SPEC_STORE_BYPASS, PR_SPEC_DISABLE, 1, 0)));
    printf("set bad %ld\n", set(3));
    printf("set disable %ld\n", set(PR_SPEC_DISABLE));
    show("after disable");
    status("after disable");
    // fork は受けつぐ
    pid_t c = fork();
    if (c == 0) {
        show("child");
        _exit(0);
    }
    waitpid(c, NULL, 0);
    printf("set enable %ld\n", set(PR_SPEC_ENABLE));
    show("after enable");
    status("after enable");
    // スレッドごと
    pthread_t t;
    pthread_create(&t, NULL, thread_disable, NULL);
    pthread_join(t, NULL);
    show("main after thread");
    // exec で外れる
    printf("set noexec %ld\n", set(PR_SPEC_DISABLE_NOEXEC));
    show("after noexec");
    status("after noexec");
    fflush(stdout);
    c = fork();
    if (c == 0) {
        execl("/proc/self/exe", argv[0], "--after-exec", (char *)NULL);
        printf("exec failed: %s\n", strerror(errno));
        _exit(1);
    }
    waitpid(c, NULL, 0);
    // 強く止めると戻せない (子で)
    c = fork();
    if (c == 0) {
        printf("set force %ld\n", set(PR_SPEC_FORCE_DISABLE));
        show("after force");
        status("after force");
        printf("force then enable %ld\n", set(PR_SPEC_ENABLE));
        printf("force then noexec %ld\n", set(PR_SPEC_DISABLE_NOEXEC));
        printf("force then disable %ld\n", set(PR_SPEC_DISABLE));
        show("after force tries");
        _exit(0);
    }
    waitpid(c, NULL, 0);
    return 0;
}

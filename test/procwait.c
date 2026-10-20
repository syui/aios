// プロセスとシグナル (kernel/src/proc.rs、signal.rs): 子の終わりと止まり・動きの知らせ (wait4 / waitid)、ゾンビ、
// SIGCHLD の扱い、孤児のプロセスグループ、setpgid / getpgid / getsid、kill / tkill / tgkill のしらべる順が Linux と
// 同じか。aios の中で: zig cc -target aarch64-linux-musl -O2 -static -o procwait test/procwait.c; ./procwait。
// Linux でも同じ答え (test/linux-vm.sh exec latest ...)。しくじった数を終了コードで
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static int fails, checks;

static void check(const char *what, long got, long want) {
    checks++;
    if (got != want) {
        fails++;
        printf("FAIL %s: got %ld, want %ld\n", what, got, want);
    }
}

static void msleep(int ms) {
    struct timespec ts = {ms / 1000, (ms % 1000) * 1000000L};
    while (nanosleep(&ts, &ts) < 0 && errno == EINTR)
        ;
}

// /proc/PID/stat の状態 (なければ X)
static char state(pid_t pid) {
    char p[64], b[512];
    snprintf(p, sizeof p, "/proc/%d/stat", (int)pid);
    int fd = open(p, O_RDONLY);
    if (fd < 0)
        return 'X';
    ssize_t n = read(fd, b, sizeof b - 1);
    close(fd);
    if (n <= 0)
        return 'X';
    b[n] = 0;
    char *q = strrchr(b, ')');
    return q && q[1] == ' ' ? q[2] : '?';
}

// 子が want の状態になるまで (2 秒まで) 待つ
static char waitstate(pid_t pid, const char *want) {
    char s = 0;
    for (int i = 0; i < 400; i++) {
        s = state(pid);
        if (strchr(want, s))
            return s;
        msleep(5);
    }
    return s;
}

static pid_t kid_exit(int code) {
    pid_t p = fork();
    if (p == 0)
        _exit(code);
    waitstate(p, "Z");
    return p;
}

static pid_t kid_pause(void) {
    pid_t p = fork();
    if (p == 0) {
        prctl(PR_SET_PDEATHSIG, SIGKILL);
        for (;;)
            pause();
    }
    waitstate(p, "S");
    return p;
}

static void reap(pid_t p) {
    kill(p, SIGKILL);
    waitpid(p, NULL, 0);
}

static long w4(pid_t pid, int opts, int *st) {
    *st = 0x5a5a;
    long r = syscall(SYS_wait4, pid, st, opts, NULL);
    return r < 0 ? -errno : r;
}

static long wid(int idtype, long id, int opts, siginfo_t *si) {
    memset(si, 0x55, sizeof *si);
    long r = syscall(SYS_waitid, idtype, id, si, opts, NULL);
    return r < 0 ? -errno : r;
}

static void zombies(void) {
    // ゾンビは回収されるまでいる: /proc/PID (状態 Z)、kill は 0、getpgid / getsid も答える
    pid_t z = kid_exit(7);
    check("zombie: /proc/PID/stat says Z", state(z), 'Z');
    check("zombie: kill(sig 0) is 0", kill(z, 0), 0);
    check("zombie: kill(SIGTERM) is 0", kill(z, SIGTERM), 0);
    check("zombie: kill(-pgid) of a group with only it... (its group is mine)", getpgid(z) == getpgrp(), 1);
    check("zombie: getsid is mine", getsid(z) == getsid(0), 1);
    check("zombie: setpgid(zombie, 0) works (Linux finds it)", setpgid(z, 0), 0);
    check("zombie: now its own group", getpgid(z), z);
    check("zombie: kill(-pgid) is 0", kill(-z, SIGTERM), 0);
    int st;
    // WSTOPPED / WCONTINUED だけではゾンビを待たない: ほかにいなければ ECHILD
    siginfo_t si;
    check("zombie: waitid(WSTOPPED) only is ECHILD", wid(P_PID, z, WNOHANG | WSTOPPED, &si), -ECHILD);
    check("zombie: wait4(WUNTRACED) still reaps (wait4 always WEXITED)", w4(z, WNOHANG | WUNTRACED, &st), z);
    check("zombie: exit status 7", WIFEXITED(st) && WEXITSTATUS(st) == 7, 1);
    check("reaped: /proc/PID gone", state(z), 'X');
    check("reaped: kill is ESRCH", kill(z, 0) < 0 ? errno : 0, ESRCH);
    check("reaped: wait4 is ECHILD", w4(z, WNOHANG, &st), -ECHILD);
}

static void order(void) {
    // 待てる子が何人もいれば、古い子から (Linux の子の並び)
    pid_t a = kid_exit(1), b = kid_exit(2), c = kid_exit(3);
    int st;
    check("order: first", w4(-1, WNOHANG, &st), a);
    check("order: second", w4(-1, WNOHANG, &st), b);
    check("order: third", w4(-1, WNOHANG, &st), c);
    check("order: none left", w4(-1, WNOHANG, &st), -ECHILD);
    // WNOWAIT は知らせるだけ
    pid_t d = kid_exit(4);
    siginfo_t si;
    check("WNOWAIT: reports", wid(P_PID, d, WNOHANG | WEXITED | WNOWAIT, &si), 0);
    check("WNOWAIT: pid", si.si_pid, d);
    check("WNOWAIT: code", si.si_code, CLD_EXITED);
    check("WNOWAIT: status", si.si_status, 4);
    check("WNOWAIT: still a zombie", state(d), 'Z');
    check("waitid: reaps", wid(P_ALL, 0, WNOHANG | WEXITED, &si), 0);
    check("waitid: reaped pid", si.si_pid, d);
    check("waitid: none left: si_pid 0", (wid(P_ALL, 0, WNOHANG | WEXITED, &si), 0), 0);
    check("waitid: nothing waits: ECHILD", wid(P_ALL, 0, WNOHANG | WEXITED, &si), -ECHILD);
    // 動いている子しかいなければ WNOHANG は 0、si_pid も 0
    pid_t e = kid_pause();
    check("running kid: wait4 WNOHANG is 0", w4(-1, WNOHANG, &st), 0);
    check("running kid: waitid WNOHANG is 0", wid(P_ALL, 0, WNOHANG | WEXITED, &si), 0);
    check("running kid: si_pid 0", si.si_pid, 0);
    check("running kid: si_signo 0", si.si_signo, 0);
    reap(e);
}

static void stopcont(void) {
    // 止まった・動いたの知らせ: wait4 は WUNTRACED / WCONTINUED、waitid は WSTOPPED / WCONTINUED
    pid_t p = kid_pause();
    int st;
    siginfo_t si;
    check("stop: kill SIGSTOP", kill(p, SIGSTOP), 0);
    check("stop: stopped", waitstate(p, "T"), 'T');
    check("stop: wait4 without WUNTRACED sees nothing", w4(p, WNOHANG, &st), 0);
    check("stop: waitid WSTOPPED|WNOWAIT reports", wid(P_PID, p, WNOHANG | WSTOPPED | WNOWAIT, &si), 0);
    check("stop: CLD_STOPPED", si.si_code, CLD_STOPPED);
    check("stop: status SIGSTOP", si.si_status, SIGSTOP);
    check("stop: wait4 WUNTRACED reports", w4(p, WNOHANG | WUNTRACED, &st), p);
    check("stop: stopped by SIGSTOP", WIFSTOPPED(st) && WSTOPSIG(st) == SIGSTOP, 1);
    check("stop: reported once", w4(p, WNOHANG | WUNTRACED, &st), 0);
    check("cont: kill SIGCONT", kill(p, SIGCONT), 0);
    check("cont: running again", waitstate(p, "S"), 'S');
    check("cont: wait4 WCONTINUED reports", w4(p, WNOHANG | WCONTINUED, &st), p);
    check("cont: continued", WIFCONTINUED(st), 1);
    check("cont: reported once", w4(p, WNOHANG | WCONTINUED, &st), 0);
    // 止まったのを知らせる前に動いたら、止まった知らせは消えて、動いた知らせだけ
    kill(p, SIGSTOP);
    waitstate(p, "T");
    kill(p, SIGCONT);
    waitstate(p, "S");
    check("stop+cont: no stop report left", w4(p, WNOHANG | WUNTRACED, &st), 0);
    check("stop+cont: waitid WCONTINUED reports", wid(P_PID, p, WNOHANG | WCONTINUED, &si), 0);
    check("stop+cont: CLD_CONTINUED", si.si_code, CLD_CONTINUED);
    check("stop+cont: status SIGCONT", si.si_status, SIGCONT);
    // 止まっているあいだのシグナルは、動いてから (SIGKILL はすぐ)
    kill(p, SIGSTOP);
    waitstate(p, "T");
    check("stopped: SIGTERM waits", kill(p, SIGTERM), 0);
    msleep(50);
    check("stopped: still stopped", state(p), 'T');
    kill(p, SIGCONT);
    check("cont: then SIGTERM kills it", waitstate(p, "Z"), 'Z');
    check("killed: wait4", w4(p, WNOHANG, &st), p);
    check("killed by SIGTERM", WIFSIGNALED(st) && WTERMSIG(st) == SIGTERM, 1);
    check("killed: no core flag", WCOREDUMP(st), 0);
}

static void chld(void) {
    int st;
    // SIGCHLD を無視すると、子はゾンビにならずに消え、wait は ECHILD
    signal(SIGCHLD, SIG_IGN);
    pid_t a = fork();
    if (a == 0)
        _exit(1);
    check("SIG_IGN: kid disappears", waitstate(a, "X"), 'X');
    check("SIG_IGN: wait4 ECHILD", w4(-1, WNOHANG, &st), -ECHILD);
    signal(SIGCHLD, SIG_DFL);
    // SA_NOCLDWAIT も同じ
    struct sigaction sa;
    memset(&sa, 0, sizeof sa);
    sa.sa_handler = SIG_DFL;
    sa.sa_flags = SA_NOCLDWAIT;
    sigaction(SIGCHLD, &sa, NULL);
    pid_t b = fork();
    if (b == 0)
        _exit(2);
    check("SA_NOCLDWAIT: kid disappears", waitstate(b, "X"), 'X');
    check("SA_NOCLDWAIT: wait4 ECHILD", w4(-1, WNOHANG, &st), -ECHILD);
    signal(SIGCHLD, SIG_DFL);
    // 無視していても、動いている子がいれば WNOHANG は 0
    pid_t c = kid_pause();
    signal(SIGCHLD, SIG_IGN);
    check("SIG_IGN with a live kid: wait4 WNOHANG 0", w4(-1, WNOHANG, &st), 0);
    kill(c, SIGKILL);
    waitstate(c, "X");
    check("SIG_IGN: killed kid disappears too", w4(-1, WNOHANG, &st), -ECHILD);
    signal(SIGCHLD, SIG_DFL);
}

static void orphan(void) {
    // setsid した子のグループは孤児 (グループの外の同じセッションに親がいない): SIGTSTP の既定の動作 (止まる) はしない。
    // SIGSTOP は止まる
    pid_t p = fork();
    if (p == 0) {
        prctl(PR_SET_PDEATHSIG, SIGKILL);
        setsid();
        for (;;)
            pause();
    }
    msleep(100);
    waitstate(p, "S");
    check("orphaned pgrp: SIGTSTP is 0", kill(p, SIGTSTP), 0);
    msleep(100);
    check("orphaned pgrp: SIGTSTP does not stop", state(p), 'S');
    kill(p, SIGTTIN);
    msleep(100);
    check("orphaned pgrp: SIGTTIN does not stop", state(p), 'S');
    kill(p, SIGSTOP);
    check("orphaned pgrp: SIGSTOP stops", waitstate(p, "T"), 'T');
    reap(p);
    // 自分のグループを作った子 (setpgid) は孤児でない (親が同じセッションのちがうグループ): SIGTSTP で止まる。
    // このプロセスがセッションリーダーだと、子のグループの外の親 (このプロセス) が同じセッションにいるので孤児でない
    pid_t q = fork();
    if (q == 0) {
        prctl(PR_SET_PDEATHSIG, SIGKILL);
        setpgid(0, 0);
        for (;;)
            pause();
    }
    msleep(100);
    waitstate(q, "S");
    kill(q, SIGTSTP);
    check("own pgrp, parent outside: SIGTSTP stops", waitstate(q, "T"), 'T');
    reap(q);
}

static void pgids(void) {
    pid_t a = kid_pause(), b = kid_pause();
    check("setpgid: negative pgid EINVAL (before ESRCH)", setpgid(4000000, -1) < 0 ? errno : 0, EINVAL);
    check("setpgid: not my child ESRCH", setpgid(1, 0) < 0 ? errno : 0, ESRCH);
    check("setpgid: kid to its own group", setpgid(a, a), 0);
    check("setpgid: other kid joins it", setpgid(b, a), 0);
    check("getpgid: b is in a's group", getpgid(b), a);
    check("setpgid: to a group that does not exist EPERM", setpgid(b, 4000000) < 0 ? errno : 0, EPERM);
    check("setpgid: back to my group", setpgid(b, getpgrp()), 0);
    check("getpgid: bad pid ESRCH", getpgid(4000000) < 0 ? errno : 0, ESRCH);
    check("getsid: bad pid ESRCH", getsid(4000000) < 0 ? errno : 0, ESRCH);
    // setsid した子: ちがうセッションの子は動かせない (EPERM)
    pid_t s = fork();
    if (s == 0) {
        prctl(PR_SET_PDEATHSIG, SIGKILL);
        setsid();
        for (;;)
            pause();
    }
    msleep(100);
    waitstate(s, "S");
    check("setsid kid: getsid is itself", getsid(s), s);
    check("setsid kid: setpgid EPERM (other session / session leader)", setpgid(s, s) < 0 ? errno : 0, EPERM);
    check("setpgid: into a group of another session EPERM", setpgid(a, s) < 0 ? errno : 0, EPERM);
    reap(s);
    // exec した子は動かせない (EACCES)
    pid_t e = fork();
    if (e == 0) {
        execl("/proc/self/exe", "procwait", "--sleep", (char *)NULL);
        _exit(127);
    }
    msleep(300);
    check("execed kid: setpgid EACCES", setpgid(e, 0) < 0 ? errno : 0, EACCES);
    reap(e);
    reap(a);
    reap(b);
}

static void kills(void) {
    pid_t p = kid_pause();
    check("kill: bad signal to a live kid EINVAL", kill(p, 65) < 0 ? errno : 0, EINVAL);
    check("kill: bad signal to nobody ESRCH (lookup first)", kill(4000000, 65) < 0 ? errno : 0, ESRCH);
    check("kill: sig 0 to nobody ESRCH", kill(4000000, 0) < 0 ? errno : 0, ESRCH);
    check("tkill: tid 0 EINVAL", syscall(SYS_tkill, 0, SIGCONT) < 0 ? errno : 0, EINVAL);
    check("tkill: tid -1 EINVAL", syscall(SYS_tkill, -1, SIGCONT) < 0 ? errno : 0, EINVAL);
    check("tgkill: tgid 0 EINVAL", syscall(SYS_tgkill, 0, p, SIGCONT) < 0 ? errno : 0, EINVAL);
    check("tgkill: bad signal to nobody ESRCH", syscall(SYS_tgkill, 4000000, 4000000, 65) < 0 ? errno : 0, ESRCH);
    check("tgkill: bad signal EINVAL", syscall(SYS_tgkill, p, p, 65) < 0 ? errno : 0, EINVAL);
    check("tgkill: wrong tgid ESRCH", syscall(SYS_tgkill, getpid(), p, 0) < 0 ? errno : 0, ESRCH);
    check("wait4: unknown option EINVAL", syscall(SYS_wait4, -1, NULL, WNOHANG | 0x10, NULL) < 0 ? errno : 0, EINVAL);
    check("waitid: unknown option EINVAL", syscall(SYS_waitid, P_ALL, 0, NULL, WNOHANG | WEXITED | 0x10, NULL) < 0 ? errno : 0, EINVAL);
    check("waitid: no event bits EINVAL", syscall(SYS_waitid, P_ALL, 0, NULL, WNOHANG, NULL) < 0 ? errno : 0, EINVAL);
    check("waitid: P_PGID negative EINVAL", syscall(SYS_waitid, P_PGID, -5, NULL, WNOHANG | WEXITED, NULL) < 0 ? errno : 0, EINVAL);
    check("waitid: P_PID 0 EINVAL", syscall(SYS_waitid, P_PID, 0, NULL, WNOHANG | WEXITED, NULL) < 0 ? errno : 0, EINVAL);
    check("waitid: bad idtype EINVAL", syscall(SYS_waitid, 7, 0, NULL, WNOHANG | WEXITED, NULL) < 0 ? errno : 0, EINVAL);
    reap(p);
}

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "--sleep") == 0) {
        prctl(PR_SET_PDEATHSIG, SIGKILL);
        sleep(30);
        return 0;
    }
    // 自分のセッションの中で (孤児のグループの決まりが、動かすところによらないように): セッションを作り、その中で
    // 別のグループの子が動かす
    if (argc < 2 || strcmp(argv[1], "--inner") != 0) {
        pid_t p = fork();
        if (p == 0) {
            setsid();
            pid_t q = fork();
            if (q == 0) {
                setpgid(0, 0);
                execl("/proc/self/exe", "procwait", "--inner", (char *)NULL);
                _exit(127);
            }
            int st = 0;
            waitpid(q, &st, 0);
            _exit(WIFEXITED(st) ? WEXITSTATUS(st) : 99);
        }
        int st = 0;
        waitpid(p, &st, 0);
        return WIFEXITED(st) ? WEXITSTATUS(st) : 99;
    }
    zombies();
    order();
    stopcont();
    chld();
    orphan();
    pgids();
    kills();
    printf("procwait: %d checks, %d failed\n", checks, fails);
    fflush(stdout);
    return fails;
}

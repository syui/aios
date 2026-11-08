// audit の exec の名前 (Linux の do_execveat_common、bprm_open_interpreter / open_exec、binfmt_script、audit_bprm):
// execveat (dirfd と相対の名前、AT_EMPTY_PATH の fd、O_PATH の fd)、#! のスクリプト (インタプリタの名前と、引数に入る
// スクリプトの名前 /dev/fd/N/NAME)、動的なプログラムの ld.so (PT_INTERP の名前)、ない ld.so (ENOENT)、相対の名前の CWD。
// 動的なプログラムと ld.so は、このテストが書く小さな ELF (ld.so は exit_group(7) するだけ。本体の入口は exit_group(8))。
// 記録は組 READLOG で読み、子のシステムコールの記録とそのなかまだけ出す (inode= dev= pid= comm= exe= proctitle= は *)
// 出すだけのテスト: root で (test/ldiff.sh --root auditexecat)
//   aios の中で: sudo ./auditexecat      aarch64-linux-gnu-gcc -O2 -static -o auditexecat test/auditexecat.c
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/audit.h>
#include <linux/netlink.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static char buf[65536];
static int seqno = 1;
static long s0 = -1;
static int m = -1;
static pid_t main_pid, child_pid;
// いま見せている SYSCALL の記録の audit(...) (なかまの CWD、PATH、EXECVE、PROCTITLE、EOE も見せる)
static char shown[64];

static const char *en(int e) {
    switch (e) {
    case 0: return "ok";
    case EPERM: return "EPERM";
    case EINVAL: return "EINVAL";
    case ERANGE: return "ERANGE";
    case EACCES: return "EACCES";
    case ENOENT: return "ENOENT";
    case EEXIST: return "EEXIST";
    case ESRCH: return "ESRCH";
    default: {
        static char b[32];
        snprintf(b, sizeof b, "errno%d", e);
        return b;
    }
    }
}

// sessionid を S+N に (4294967295 はそのまま)
static const char *ses(unsigned long v) {
    static char b[4][32];
    static int k;
    char *o = b[k++ % 4];
    if (v == 4294967295UL) {
        snprintf(o, 32, "4294967295");
        return o;
    }
    if (s0 < 0)
        s0 = v;
    snprintf(o, 32, "S+%ld", (long)v - s0);
    return o;
}

static int asock(unsigned groups) {
    int s = socket(AF_NETLINK, SOCK_RAW | SOCK_CLOEXEC, NETLINK_AUDIT);
    struct sockaddr_nl a = {.nl_family = AF_NETLINK, .nl_groups = groups};
    if (s < 0 || bind(s, (struct sockaddr *)&a, sizeof a)) {
        printf("audit socket: %s\n", en(errno));
        exit(0);
    }
    return s;
}

static void req(int s, int type, int flags, const void *data, int len) {
    static struct {
        struct nlmsghdr h;
        char d[8192];
    } q;
    memset(&q, 0, sizeof q);
    q.h.nlmsg_len = NLMSG_LENGTH(len);
    q.h.nlmsg_type = type;
    q.h.nlmsg_flags = NLM_F_REQUEST | flags;
    q.h.nlmsg_seq = ++seqno;
    memcpy(q.d, data, len);
    struct sockaddr_nl k = {.nl_family = AF_NETLINK};
    sendto(s, &q, q.h.nlmsg_len, 0, (struct sockaddr *)&k, sizeof k);
}

// 記録の文字: audit(...) と pid= の数、tty= comm= exe= proctitle= を *、ses= と old-ses= を S+N に
static void mask_text(char *t) {
    char out[4096];
    int o = 0;
    for (int i = 0; t[i] && o < 4000;) {
        int start = i == 0 || t[i - 1] == ' ';
        if (!strncmp(t + i, "audit(", 6)) {
            o += sprintf(out + o, "audit(*)");
            while (t[i] && t[i] != ')')
                i++;
            if (t[i])
                i++;
            continue;
        }
        if (start && (!strncmp(t + i, "pid=", 4) || !strncmp(t + i, "ppid=", 5))) {
            int kl = t[i] == 'p' && t[i + 1] == 'p' ? 5 : 4;
            o += sprintf(out + o, "%.*s*", kl, t + i);
            i += kl;
            while (t[i] >= '0' && t[i] <= '9')
                i++;
            continue;
        }
        const char *tok[] = {"tty=", "comm=", "exe=", "proctitle=", "inode=", "dev="};
        int done = 0;
        for (int k = 0; k < 6 && start && !done; k++) {
            int kl = strlen(tok[k]);
            if (!strncmp(t + i, tok[k], kl)) {
                o += sprintf(out + o, "%s*", tok[k]);
                i += kl;
                while (t[i] && t[i] != ' ')
                    i++;
                done = 1;
            }
        }
        if (done)
            continue;
        if (start && (!strncmp(t + i, "ses=", 4) || !strncmp(t + i, "old-ses=", 8))) {
            int kl = t[i] == 's' ? 4 : 8;
            o += sprintf(out + o, "%.*s%s", kl, t + i, ses(strtoul(t + i + kl, NULL, 10)));
            i += kl;
            while (t[i] >= '0' && t[i] <= '9')
                i++;
            continue;
        }
        // 子の pid は child、このテストの pid は parent に
        int named = 0;
        // exit= の値だけ (syscall= の番号とまちがえない)
        int at_exit = i >= 4 && !strncmp(t + i - 4, "exit", 4);
        for (int k = 0; k < 2 && t[i] == '=' && at_exit && !named; k++) {
            pid_t who = k ? main_pid : child_pid;
            char c[16];
            int cl = snprintf(c, sizeof c, "%d", who);
            if (who > 0 && !strncmp(t + i + 1, c, cl) && !(t[i + 1 + cl] >= '0' && t[i + 1 + cl] <= '9')) {
                o += sprintf(out + o, "=%s", k ? "parent" : "child");
                i += 1 + cl;
                named = 1;
            }
        }
        if (named)
            continue;
        // 引数の a0= から a3= のうち、スタックなどの大きな番地 (9 けた以上で ffffffff で始まらないもの) は *
        if (start && t[i] == 'a' && t[i + 1] >= '0' && t[i + 1] <= '3' && t[i + 2] == '=') {
            int k = i + 3, n = 0;
            while ((t[k + n] >= '0' && t[k + n] <= '9') || (t[k + n] >= 'a' && t[k + n] <= 'f'))
                n++;
            if (n >= 9 && strncmp(t + k, "ffffffff", 8)) {
                o += sprintf(out + o, "%.3s*", t + i);
                i = k + n;
                continue;
            }
        }
        out[o++] = t[i++];
    }
    out[o] = 0;
    strcpy(t, out);
}

static void replies(int s, int ms, const char *what) {
    if (what)
        printf("%s:\n", what);
    for (;;) {
        struct pollfd p = {s, POLLIN, 0};
        if (poll(&p, 1, ms) <= 0)
            break;
        long n = recv(s, buf, sizeof buf, MSG_DONTWAIT);
        if (n <= 0)
            break;
        for (struct nlmsghdr *h = (void *)buf; NLMSG_OK(h, n); h = NLMSG_NEXT(h, n)) {
            int dl = h->nlmsg_len - NLMSG_HDRLEN;
            if (h->nlmsg_type == NLMSG_ERROR) {
                struct nlmsgerr *e = NLMSG_DATA(h);
                printf("  ERROR %s (for type %u)\n", en(-e->error), e->msg.nlmsg_type);
                continue;
            }
            char t[4096];
            snprintf(t, sizeof t, "%.*s", dl, (char *)NLMSG_DATA(h));
            char stamp[64] = "";
            sscanf(t, "%63[^ ]", stamp);
            if (h->nlmsg_type == AUDIT_SYSCALL) {
                char kid[32];
                snprintf(kid, sizeof kid, " pid=%d ", child_pid);
                if (child_pid <= 0 || !strstr(t, kid)) {
                    shown[0] = 0;
                    continue;
                }
                snprintf(shown, sizeof shown, "%s", stamp);
            } else if (h->nlmsg_type >= 1300 && h->nlmsg_type < 1400 && h->nlmsg_type != AUDIT_CONFIG_CHANGE) {
                if (strcmp(stamp, shown))
                    continue;
            }
            mask_text(t);
            printf("  record type=%u: %s\n", h->nlmsg_type, t);
        }
    }
}

static void records(const char *what) {
    replies(m, 300, what);
}

// ルールを組み立てる
static char rbuf[sizeof(struct audit_rule_data) + 4096];
static struct audit_rule_data *R = (void *)rbuf;

static void rule(unsigned flags, unsigned action) {
    memset(rbuf, 0, sizeof rbuf);
    R->flags = flags;
    R->action = action;
}

static void sys(int nr) {
    R->mask[nr / 32] |= 1u << (nr % 32);
}

static void field(unsigned type, unsigned op, unsigned val) {
    unsigned i = R->field_count++;
    R->fields[i] = type;
    R->fieldflags[i] = op;
    R->values[i] = val;
}

static void str(unsigned type, unsigned op, const char *s) {
    int len = strlen(s);
    memcpy(R->buf + R->buflen, s, len);
    R->buflen += len;
    field(type, op, len);
}

static void send_rule(int s, int type, const char *what) {
    req(s, type, NLM_F_ACK, R, sizeof *R + R->buflen);
    replies(s, 300, what);
}

static void add(int s, const char *what) {
    send_rule(s, AUDIT_ADD_RULE, what);
}


static void set_status(int s, unsigned mask, unsigned enabled) {
    struct audit_status a;
    memset(&a, 0, sizeof a);
    a.mask = mask;
    a.enabled = enabled;
    req(s, AUDIT_SET, NLM_F_ACK, &a, sizeof a);
    replies(s, 300, NULL);
}

static const int calls[] = {SYS_execve, SYS_execveat};

// 子のシステムコールのルール (ppid がこのテスト)
static void r_child(void) {
    rule(AUDIT_FILTER_EXIT, AUDIT_ALWAYS);
    for (unsigned i = 0; i < sizeof calls / sizeof calls[0]; i++)
        sys(calls[i]);
    field(AUDIT_PPID, AUDIT_EQUAL, main_pid);
    str(AUDIT_FILTERKEY, AUDIT_EQUAL, "x");
}

// path を写す (実行するプログラム)
static void copy(const char *from, const char *to) {
    int a = open(from, O_RDONLY), b = open(to, O_WRONLY | O_CREAT | O_TRUNC, 0755);
    long n;
    while ((n = read(a, buf, sizeof buf)) > 0)
        if (write(b, buf, n) != n)
            break;
    close(a);
    close(b);
    chmod(to, 0755);
}

static void put_file(const char *p, const void *d, size_t n) {
    int fd = open(p, O_WRONLY | O_CREAT | O_TRUNC, 0755);
    if (write(fd, d, n) != (long)n)
        printf("write %s: %s\n", p, en(errno));
    close(fd);
    chmod(p, 0755);
}

// 小さな ELF (aarch64、ET_DYN、0 番地から 1 つの PT_LOAD)。interp があれば PT_INTERP も。入口は exit_group(code)
static void put_elf(const char *p, const char *interp, int code) {
    unsigned char f[512];
    memset(f, 0, sizeof f);
    int nph = interp ? 2 : 1;
    unsigned long code_off = 64 + 56 * nph;
    unsigned long str_off = code_off + 12;
    unsigned long size = str_off + (interp ? strlen(interp) + 1 : 0);
    unsigned char id[16] = {0x7f, 'E', 'L', 'F', 2, 1, 1, 0};
    memcpy(f, id, 16);
    *(unsigned short *)(f + 16) = 3;   // ET_DYN
    *(unsigned short *)(f + 18) = 183; // EM_AARCH64
    *(unsigned *)(f + 20) = 1;
    *(unsigned long *)(f + 24) = code_off;
    *(unsigned long *)(f + 32) = 64;
    *(unsigned short *)(f + 52) = 64;
    *(unsigned short *)(f + 54) = 56;
    *(unsigned short *)(f + 56) = nph;
    unsigned char *ph = f + 64;
    if (interp) {
        *(unsigned *)(ph + 0) = 3; // PT_INTERP
        *(unsigned *)(ph + 4) = 4;
        *(unsigned long *)(ph + 8) = str_off;
        *(unsigned long *)(ph + 16) = str_off;
        *(unsigned long *)(ph + 24) = str_off;
        *(unsigned long *)(ph + 32) = strlen(interp) + 1;
        *(unsigned long *)(ph + 40) = strlen(interp) + 1;
        *(unsigned long *)(ph + 48) = 1;
        ph += 56;
    }
    *(unsigned *)(ph + 0) = 1; // PT_LOAD
    *(unsigned *)(ph + 4) = 5; // R X
    *(unsigned long *)(ph + 8) = 0;
    *(unsigned long *)(ph + 16) = 0;
    *(unsigned long *)(ph + 24) = 0;
    *(unsigned long *)(ph + 32) = size;
    *(unsigned long *)(ph + 40) = size;
    *(unsigned long *)(ph + 48) = 0x10000;
    unsigned ins[3] = {0xd2800000u | (unsigned)code << 5, 0xd2800bc8u, 0xd4000001u}; // mov x0, #code; mov x8, #94; svc #0
    memcpy(f + code_off, ins, sizeof ins);
    if (interp)
        strcpy((char *)f + str_off, interp);
    put_file(p, f, size);
}

static const char *files[] = {"prog", "interp", "dyn", "noint", "script", "dscript", "sub/prog"};

// /tmp/ax を空にして消す (シェルのない手本の Linux でも)
static void clean(void) {
    for (unsigned i = 0; i < sizeof files / sizeof files[0]; i++) {
        char p[64];
        snprintf(p, sizeof p, "/tmp/ax/%s", files[i]);
        unlink(p);
    }
    rmdir("/tmp/ax/sub");
    rmdir("/tmp/ax");
}

static char *const ev[] = {"A=1", NULL};

// 1 つの case: 子が fd を 3 からにして /tmp/ax で exec する。終わりかたと記録を出す
static void run(const char *what, int how) {
    child_pid = fork();
    if (child_pid == 0) {
        syscall(SYS_close_range, 3, ~0u, 0, 0);
        if (chdir("/tmp/ax"))
            _exit(2);
        char *av[] = {"prog", "--exec", NULL};
        int fd;
        switch (how) {
        case 0: syscall(SYS_execve, "/tmp/ax/dyn", av, ev, 0); break;
        case 1: syscall(SYS_execve, "dyn", av, ev, 0); break;
        case 2: syscall(SYS_execve, "/tmp/ax/noint", av, ev, 0); break;
        case 3: syscall(SYS_execve, "/tmp/ax/script", av, ev, 0); break;
        case 4: syscall(SYS_execve, "/tmp/ax/dscript", av, ev, 0); break;
        case 5:
            fd = open("/tmp/ax/sub", O_RDONLY | O_DIRECTORY);
            syscall(SYS_execveat, fd, "prog", av, ev, 0);
            break;
        case 6:
            fd = open("/tmp/ax/prog", O_RDONLY);
            syscall(SYS_execveat, fd, "", av, ev, AT_EMPTY_PATH);
            break;
        case 7:
            fd = open("/tmp/ax/dyn", O_PATH);
            syscall(SYS_execveat, fd, "", av, ev, AT_EMPTY_PATH);
            break;
        case 8:
            fd = open("/tmp/ax", O_RDONLY | O_DIRECTORY);
            syscall(SYS_execveat, fd, "script", av, ev, 0);
            break;
        case 9:
            fd = open("/tmp/ax", O_RDONLY | O_DIRECTORY);
            syscall(SYS_execveat, fd, "dscript", av, ev, 0);
            break;
        case 10:
            fd = open("/tmp/ax/script", O_RDONLY);
            syscall(SYS_execveat, fd, "", av, ev, AT_EMPTY_PATH);
            break;
        case 11: syscall(SYS_execveat, AT_FDCWD, "sub/prog", av, ev, 0); break;
        case 12:
            fd = open("/tmp/ax", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
            syscall(SYS_execveat, fd, "script", av, ev, 0);
            break;
        }
        _exit(100 + errno);
    }
    int st;
    waitpid(child_pid, &st, 0);
    printf("== %s: exit %d\n", what, WIFEXITED(st) ? WEXITSTATUS(st) : -WTERMSIG(st));
    records(NULL);
}

int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "--exec"))
        _exit(0);
    setvbuf(stdout, NULL, _IONBF, 0);
    main_pid = getpid();
    clean();
    mkdir("/tmp/ax", 0755);
    mkdir("/tmp/ax/sub", 0755);
    copy("/proc/self/exe", "/tmp/ax/prog");
    copy("/proc/self/exe", "/tmp/ax/sub/prog");
    put_elf("/tmp/ax/interp", NULL, 7);
    put_elf("/tmp/ax/dyn", "/tmp/ax/interp", 8);
    put_elf("/tmp/ax/noint", "/tmp/ax/missing", 9);
    const char *sc = "#!/tmp/ax/prog --exec\n";
    put_file("/tmp/ax/script", sc, strlen(sc));
    const char *dsc = "#! /tmp/ax/dyn\n";
    put_file("/tmp/ax/dscript", dsc, strlen(dsc));
    m = asock(1);
    int s = asock(0);
    set_status(s, AUDIT_STATUS_ENABLED, 1);
    r_child();
    add(s, "add child rule");
    records("records (add)");

    run("execve dynamic (PT_INTERP)", 0);
    run("execve dynamic, relative name", 1);
    run("execve dynamic, missing PT_INTERP", 2);
    run("execve #! script", 3);
    run("execve #! script with a dynamic interpreter", 4);
    run("execveat dirfd + name", 5);
    run("execveat fd AT_EMPTY_PATH", 6);
    run("execveat O_PATH fd of dynamic AT_EMPTY_PATH", 7);
    run("execveat dirfd + #! script", 8);
    run("execveat dirfd + #! script with a dynamic interpreter", 9);
    run("execveat #! script fd AT_EMPTY_PATH", 10);
    run("execveat AT_FDCWD + relative", 11);
    run("execveat O_CLOEXEC dirfd + #! script", 12);

    r_child();
    send_rule(s, AUDIT_DEL_RULE, "del child rule");
    set_status(s, AUDIT_STATUS_ENABLED, 0);
    records("records (end)");
    clean();
    return 0;
}

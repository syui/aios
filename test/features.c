// カーネルにある機能の一覧 (yes / no): システムコール (ENOSYS かどうか)、namespace を作れるか、ソケットの family、LSM、
// /proc と /sys のものなど。test/ldiff.sh が手本の Linux と aios の両方で動かし、「aios にはあるのに手本の Linux には
// ない」ものを知らせる (手本の Linux は tinyconfig に test/linux.config を足したものなので、テストが使う機能がないと、
// ENOSYS や EINVAL がテストの答えにかくれて、同じに見えることがある: CONFIG_USER_NS がなかったときの landlock の
// テストのように)。ふつうの人で動く。権限がなくて断られた (EPERM、EACCES) ものは「ある」とする
//   aios の中で: ./features      gcc -O2 -static -o features test/features.c
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

static void say(const char *name, int yes) {
    printf("%-28s %s\n", name, yes ? "yes" : "no");
}

// システムコールがあるか: ENOSYS でなければある (引数はみなまちがえて、何もしないように)
static void sys(const char *name, long nr, long a, long b, long c, long d) {
    errno = 0;
    long r = syscall(nr, a, b, c, d, 0, 0);
    say(name, !(r < 0 && errno == ENOSYS));
}

// 子で unshare(flags) (ユーザーの namespace といっしょに)。EINVAL (その種類がない) と ENOSYS だけが「ない」
static void ns(const char *name, int flags) {
    fflush(stdout);
    pid_t p = fork();
    if (p == 0) {
        int r = unshare(flags);
        _exit(r == 0 ? 0 : errno == EINVAL || errno == ENOSYS ? 1 : 0);
    }
    int st = 0;
    waitpid(p, &st, 0);
    say(name, WIFEXITED(st) && WEXITSTATUS(st) == 0);
}

static void fam(const char *name, int domain, int type, int proto) {
    int s = socket(domain, type | SOCK_CLOEXEC, proto);
    int e = errno;
    if (s >= 0)
        close(s);
    say(name, s >= 0 || !(e == EAFNOSUPPORT || e == EPROTONOSUPPORT || e == ENOSYS || e == ESOCKTNOSUPPORT || e == EINVAL));
}

static void path(const char *name, const char *p) {
    struct stat st;
    say(name, lstat(p, &st) == 0);
}

// ファイルの中に word があるか (/sys/kernel/security/lsm など)
static void has(const char *name, const char *p, const char *word) {
    char b[4096] = "";
    int fd = open(p, O_RDONLY | O_CLOEXEC);
    if (fd >= 0) {
        ssize_t n = read(fd, b, sizeof b - 1);
        if (n > 0)
            b[n] = 0;
        close(fd);
    }
    say(name, strstr(b, word) != NULL);
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    // システムコール (番号は arm64。どれも引数がまちがっていて、何もしない)
    sys("landlock", 444, 0, 0, 1 << 30, 0);
    sys("seccomp", SYS_seccomp, 99, 0, 0, 0);
    sys("io_uring", SYS_io_uring_setup, 0, 0, 0, 0);
    sys("bpf", SYS_bpf, -1, 0, 0, 0);
    sys("perf_event_open", SYS_perf_event_open, 0, 0, 0, 0);
    sys("fanotify", SYS_fanotify_init, ~0L, 0, 0, 0);
    sys("inotify", SYS_inotify_init1, ~0L, 0, 0, 0);
    sys("eventfd", SYS_eventfd2, 0, ~0L, 0, 0);
    sys("timerfd", SYS_timerfd_create, -1, 0, 0, 0);
    sys("signalfd", SYS_signalfd4, -1, 0, 0, ~0L);
    sys("epoll", SYS_epoll_create1, ~0L, 0, 0, 0);
    sys("memfd_create", SYS_memfd_create, 0, ~0L, 0, 0);
    sys("memfd_secret", 447, ~0L, 0, 0, 0);
    sys("userfaultfd", SYS_userfaultfd, ~0L, 0, 0, 0);
    sys("pidfd_open", SYS_pidfd_open, 0, ~0L, 0, 0);
    sys("pidfd_getfd", 438, -1, -1, ~0L, 0);
    sys("open_tree", SYS_open_tree, -1, 0, ~0L, 0);
    sys("fsopen", SYS_fsopen, 0, ~0L, 0, 0);
    sys("mount_setattr", SYS_mount_setattr, -1, 0, ~0L, 0);
    sys("name_to_handle_at", SYS_name_to_handle_at, -1, 0, 0, 0);
    sys("kcmp", SYS_kcmp, 0, 0, -1, 0);
    sys("process_vm_readv", SYS_process_vm_readv, 0, 0, 0, 0);
    sys("membarrier", SYS_membarrier, -1, 0, 0, 0);
    sys("rseq", SYS_rseq, 0, 0, ~0L, 0);
    sys("futex_waitv", 449, 0, 0, ~0L, 0);
    sys("mq_open", SYS_mq_open, 0, 0, 0, 0);
    // ない鍵を、作らずに (IPC_CREAT なし): ENOENT
    sys("sysv_msg", SYS_msgget, 0x7a7a7a7a, 0, 0, 0);
    sys("sysv_sem", SYS_semget, 0x7a7a7a7a, 0, 0, 0);
    sys("sysv_shm", SYS_shmget, 0x7a7a7a7a, 0, 0, 0);
    sys("keys", SYS_add_key, 0, 0, 0, 0);
    sys("io_setup", SYS_io_setup, 0, 0, 0, 0);
    sys("swapon", SYS_swapon, 0, ~0L, 0, 0);
    sys("acct", SYS_acct, 1, 0, 0, 0);
    sys("quotactl", SYS_quotactl, 0, 0, 0, 0);
    sys("fadvise", SYS_fadvise64, -1, 0, 0, 0);
    sys("madvise", SYS_madvise, 1, 0, -1, 0);
    sys("mlock", SYS_mlock, 1, 0, 0, 0);
    sys("mbind", SYS_mbind, 1, 0, -1, 0);
    sys("cachestat", 451, -1, 0, 0, ~0L);
    sys("map_shadow_stack", 453, 0, 0, ~0L, 0);
    sys("lsm_get_self_attr", 459, 0, 0, 0, ~0L);
    sys("mseal", 462, 1, 0, ~0L, 0);
    sys("file_getattr", 468, -1, 0, 0, 0);
    // namespace (ユーザーの namespace の中で)
    ns("ns_user", CLONE_NEWUSER);
    ns("ns_uts", CLONE_NEWUSER | CLONE_NEWUTS);
    ns("ns_ipc", CLONE_NEWUSER | CLONE_NEWIPC);
    ns("ns_net", CLONE_NEWUSER | CLONE_NEWNET);
    ns("ns_mnt", CLONE_NEWUSER | CLONE_NEWNS);
    ns("ns_pid", CLONE_NEWUSER | CLONE_NEWPID);
    ns("ns_cgroup", CLONE_NEWUSER | CLONE_NEWCGROUP);
    ns("ns_time", CLONE_NEWUSER | 0x80);
    // ソケット
    fam("af_unix", AF_UNIX, SOCK_STREAM, 0);
    fam("af_inet", AF_INET, SOCK_STREAM, 0);
    fam("af_inet6", AF_INET6, SOCK_STREAM, 0);
    fam("af_netlink_route", AF_NETLINK, SOCK_RAW, 0);
    fam("af_netlink_netfilter", AF_NETLINK, SOCK_RAW, 12);
    fam("af_netlink_audit", AF_NETLINK, SOCK_RAW, 9);
    fam("af_packet", AF_PACKET, SOCK_DGRAM, 0);
    fam("af_vsock", 40, SOCK_STREAM, 0);
    fam("ping_icmp", AF_INET, SOCK_DGRAM, 1);
    fam("udplite", AF_INET, SOCK_DGRAM, 136);
    fam("sctp", AF_INET, SOCK_STREAM, 132);
    fam("mptcp", AF_INET, SOCK_STREAM, 262);
    // LSM と、/proc と /sys
    has("lsm_landlock", "/sys/kernel/security/lsm", "landlock");
    has("lsm_apparmor", "/sys/kernel/security/lsm", "apparmor");
    has("lsm_yama", "/sys/kernel/security/lsm", "yama");
    path("proc_self_ns_user", "/proc/self/ns/user");
    path("proc_self_ns_pid", "/proc/self/ns/pid");
    path("proc_self_fdinfo", "/proc/self/fdinfo");
    path("proc_self_mountinfo", "/proc/self/mountinfo");
    path("proc_pressure", "/proc/pressure");
    path("proc_config_gz", "/proc/config.gz");
    path("sys_fs_cgroup", "/sys/fs/cgroup/cgroup.controllers");
    path("sys_kernel_btf", "/sys/kernel/btf/vmlinux");
    path("dev_shm", "/dev/shm");
    path("efivarfs", "/sys/firmware/efi/efivars");
    return 0;
}

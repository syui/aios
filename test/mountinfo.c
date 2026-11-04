// /proc/PID/mountinfo (Linux 7.4 の show_mountinfo)、statx の STATX_MNT_ID / STATX_MNT_ID_UNIQUE / STATX_ATTR_MOUNT_ROOT、
// ファイルシステムごとの st_dev。マウントの表は Linux と aios でちがうので、番号はじかに出さず、そろい方を出す:
// 行の形、親がいるか、st_dev と major:minor、statx と fdinfo の mnt_id、ユーザーとマウントの namespace の中で
// tmpfs・bind・ro・umount・名前の空白をしたときの行の見え方。ふつうの人で (root なら nobody になる) 動く
// 出すだけのテスト: test/ldiff.sh mountinfo @home /tmp
//   aios の中で: ./mountinfo $HOME /tmp      gcc -O2 -static -o mountinfo test/mountinfo.c
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef STATX_MNT_ID
#define STATX_MNT_ID 0x1000
#endif
#ifndef STATX_MNT_ID_UNIQUE
#define STATX_MNT_ID_UNIQUE 0x4000
#endif
#ifndef STATX_ATTR_MOUNT_ROOT
#define STATX_ATTR_MOUNT_ROOT 0x2000
#endif

struct ent {
    long id, parent;
    unsigned maj, min;
    char root[256], point[256], opts[128], optional[128], type[32], source[128], super[128];
};

static struct ent ents[256];
static int nents;

// /proc/PID/mountinfo を読む (pid 0 は self)。行の数 (読めなければ -errno)
static int load(pid_t pid) {
    char p[64];
    if (pid)
        snprintf(p, sizeof p, "/proc/%d/mountinfo", pid);
    else
        snprintf(p, sizeof p, "/proc/self/mountinfo");
    FILE *f = fopen(p, "r");
    if (!f)
        return -errno;
    char line[1024];
    nents = 0;
    while (fgets(line, sizeof line, f) && nents < 256) {
        struct ent *e = &ents[nents];
        memset(e, 0, sizeof *e);
        char *sep = strstr(line, " - ");
        if (!sep)
            continue;
        *sep = 0;
        int n = 0;
        if (sscanf(line, "%ld %ld %u:%u %255s %255s %127s %n", &e->id, &e->parent, &e->maj, &e->min, e->root, e->point, e->opts, &n) < 7)
            continue;
        snprintf(e->optional, sizeof e->optional, "%s", line + n);
        if (sscanf(sep + 3, "%31s %127s %127s", e->type, e->source, e->super) < 3)
            continue;
        nents++;
    }
    fclose(f);
    return nents;
}

static struct ent *by_point(const char *point) {
    struct ent *r = NULL;
    for (int i = 0; i < nents; i++)
        if (strcmp(ents[i].point, point) == 0)
            r = &ents[i];
    return r;
}

static struct ent *by_id(long id) {
    for (int i = 0; i < nents; i++)
        if (ents[i].id == id)
            return &ents[i];
    return NULL;
}

static long sx_mnt(const char *path, unsigned mask, unsigned *got, uint64_t *attr, unsigned *dmaj, unsigned *dmin, uint64_t *amask) {
    struct statx s;
    memset(&s, 0, sizeof s);
    if (syscall(SYS_statx, AT_FDCWD, path, 0, mask, &s) != 0)
        return -errno;
    if (got)
        *got = s.stx_mask;
    if (attr)
        *attr = s.stx_attributes;
    if (amask)
        *amask = s.stx_attributes_mask;
    if (dmaj)
        *dmaj = s.stx_dev_major;
    if (dmin)
        *dmin = s.stx_dev_minor;
    return (long)s.stx_mnt_id;
}

// path を含むマウント (statx の mnt_id の行) について、そろい方を出す
static void about(const char *label, const char *path) {
    char n[128];
    unsigned got = 0, dmaj = 0, dmin = 0;
    uint64_t attr = 0, amask = 0;
    long id = sx_mnt(path, STATX_BASIC_STATS, &got, &attr, &dmaj, &dmin, &amask);
    snprintf(n, sizeof n, "%s: statx mask has MNT_ID", label);
    printf("%-56s %s\n", n, id >= 0 && (got & STATX_MNT_ID) ? "yes" : "no");
    struct ent *e = id >= 0 ? by_id(id) : NULL;
    snprintf(n, sizeof n, "%s: a mountinfo line has that id", label);
    printf("%-56s %s\n", n, e ? "yes" : "no");
    if (!e)
        return;
    snprintf(n, sizeof n, "%s: fstype", label);
    printf("%-56s %s\n", n, e->type);
    struct stat st;
    stat(path, &st);
    snprintf(n, sizeof n, "%s: st_dev is the line's major:minor", label);
    printf("%-56s %s\n", n, major(st.st_dev) == e->maj && minor(st.st_dev) == e->min ? "yes" : "no");
    snprintf(n, sizeof n, "%s: stx_dev is st_dev", label);
    printf("%-56s %s\n", n, dmaj == major(st.st_dev) && dmin == minor(st.st_dev) ? "yes" : "no");
    snprintf(n, sizeof n, "%s: the parent is listed", label);
    printf("%-56s %s\n", n, by_id(e->parent) || e->parent == e->id ? "yes" : "no");
    snprintf(n, sizeof n, "%s: opts / super opts", label);
    printf("%-56s %.2s / %.2s\n", n, e->opts, e->super);
    snprintf(n, sizeof n, "%s: STATX_ATTR_MOUNT_ROOT (in the mask too)", label);
    printf("%-56s %s (%s)\n", n, attr & STATX_ATTR_MOUNT_ROOT ? "yes" : "no", amask & STATX_ATTR_MOUNT_ROOT ? "yes" : "no");
    // fdinfo の mnt_id
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    char p[64], b[2048] = "";
    snprintf(p, sizeof p, "/proc/self/fdinfo/%d", fd);
    int f = open(p, O_RDONLY);
    if (f >= 0) {
        ssize_t k = read(f, b, sizeof b - 1);
        if (k > 0)
            b[k] = 0;
        close(f);
    }
    char *m = strstr(b, "mnt_id:");
    snprintf(n, sizeof n, "%s: fdinfo mnt_id is the statx one", label);
    printf("%-56s %s\n", n, m && atol(m + 7) == id ? "yes" : "no");
    close(fd);
    unsigned g2 = 0;
    long u = sx_mnt(path, STATX_MNT_ID_UNIQUE, &g2, NULL, NULL, NULL, NULL);
    snprintf(n, sizeof n, "%s: MNT_ID_UNIQUE: mask, > 2^31", label);
    printf("%-56s %s %s\n", n, g2 & STATX_MNT_ID_UNIQUE ? "yes" : "no", (uint64_t)u > (1ULL << 31) ? "yes" : "no");
}

// ユーザーとマウントの namespace の中で、tmpfs、bind、ro、umount、名前の空白
static void t_ns(const char *tmp) {
    char base[256], a[300], b[300], sp[300], sub[320], inner[320], pm[300];
    snprintf(base, sizeof base, "%s/mi-%d", tmp, getpid());
    mkdir(base, 0755);
    snprintf(a, sizeof a, "%s/a", base);
    snprintf(b, sizeof b, "%s/b", base);
    snprintf(sp, sizeof sp, "%s/with space", base);
    mkdir(a, 0755);
    mkdir(b, 0755);
    mkdir(sp, 0755);
    // bind するもと (外の tmpfs の中に、前もって)
    snprintf(sub, sizeof sub, "%s/src", base);
    snprintf(inner, sizeof inner, "%s/src/inner", base);
    mkdir(sub, 0755);
    mkdir(inner, 0755);
    load(0);
    long outer_root = by_point("/") ? by_point("/")->id : -1;
    struct ent *otmp = NULL;
    {
        long id = sx_mnt(tmp, STATX_BASIC_STATS, NULL, NULL, NULL, NULL, NULL);
        otmp = by_id(id);
    }
    long outer_tmp = otmp ? otmp->id : -1;
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        if (unshare(CLONE_NEWUSER | CLONE_NEWNS) != 0) {
            printf("unshare(NEWUSER|NEWNS): %s\n", strerror(errno));
            _exit(0);
        }
        int n = load(0);
        printf("%-56s %s\n", "new mount ns: lines", n > 0 ? "some" : "none");
        struct ent *r = by_point("/");
        printf("%-56s %s\n", "new mount ns: / has another id", r && r->id != outer_root ? "yes" : "no");
        long tid = sx_mnt(tmp, STATX_BASIC_STATS, NULL, NULL, NULL, NULL, NULL);
        printf("%-56s %s\n", "new mount ns: the tmp mount has another id", tid != outer_tmp ? "yes" : "no");
        printf("%-56s %s\n", "mount tmpfs on a", mount("mysrc", a, "tmpfs", 0, NULL) == 0 ? "0" : strerror(errno));
        load(0);
        struct ent *e = by_point(a);
        printf("%-56s %s\n", "  a line for a", e ? "yes" : "no");
        if (e) {
            printf("%-56s %s %s %s %.2s / %.2s\n", "  root, fstype, source, opts / super", e->root, e->type, e->source, e->opts, e->super);
            printf("%-56s %s\n", "  its parent is the tmp mount", e->parent == tid ? "yes" : "no");
            printf("%-56s %s\n", "  optional fields", e->optional[0] ? e->optional : "(none)");
            struct stat st;
            stat(a, &st);
            printf("%-56s %s\n", "  st_dev is its major:minor", major(st.st_dev) == e->maj && minor(st.st_dev) == e->min ? "yes" : "no");
            printf("%-56s %s\n", "  major is 0 (anonymous)", e->maj == 0 ? "yes" : "no");
            struct stat ts;
            stat(tmp, &ts);
            printf("%-56s %s\n", "  another dev than tmp", ts.st_dev != st.st_dev ? "yes" : "no");
            uint64_t attr = 0;
            sx_mnt(a, STATX_BASIC_STATS, NULL, &attr, NULL, NULL, NULL);
            printf("%-56s %s\n", "  a is a mount root (statx)", attr & STATX_ATTR_MOUNT_ROOT ? "yes" : "no");
        }
        // bind: 外の tmpfs の中の src/inner を b に (root の欄はその tmpfs の中のパス)
        printf("%-56s %s\n", "bind src/inner on b", mount(inner, b, NULL, MS_BIND, NULL) == 0 ? "0" : strerror(errno));
        load(0);
        e = by_point(b);
        if (e) {
            struct ent *et = by_id(tid);
            char want[320];
            snprintf(want, sizeof want, "%s/src/inner", base + strlen(tmp));
            printf("%-56s %s\n", "  root is the path inside the tmp tmpfs", strcmp(e->root, want) == 0 ? "yes" : e->root);
            printf("%-56s %s\n", "  fstype and source are the tmp mount's", et && strcmp(e->type, et->type) == 0 && strcmp(e->source, et->source) == 0 ? "yes" : "no");
            printf("%-56s %s\n", "  same major:minor as the tmp mount", et && et->maj == e->maj && et->min == e->min ? "yes" : "no");
            printf("%-56s %s\n", "  another id, parent is the tmp mount", et && et->id != e->id && e->parent == tid ? "yes" : "no");
            struct stat s1, s2;
            stat(b, &s1);
            stat(inner, &s2);
            printf("%-56s %s\n", "  b and src/inner: same dev and ino", s1.st_dev == s2.st_dev && s1.st_ino == s2.st_ino ? "yes" : "no");
        } else {
            printf("%-56s %s\n", "  a line for b", "no");
        }
        // /proc/self/mounts の bind の行
        {
            FILE *f = fopen("/proc/self/mounts", "r");
            char line[1024], want[320];
            snprintf(want, sizeof want, " %s ", b);
            int found = 0;
            while (f && fgets(line, sizeof line, f))
                if (strstr(line, want)) {
                    char src[128], pt[256], ty[32];
                    sscanf(line, "%127s %255s %31s", src, pt, ty);
                    struct ent *et = by_id(tid);
                    printf("%-56s %s\n", "  /proc/self/mounts: source and fstype of the tmp mount", et && strcmp(src, et->source) == 0 && strcmp(ty, et->type) == 0 ? "yes" : "no");
                    found = 1;
                }
            if (f)
                fclose(f);
            if (!found)
                printf("%-56s %s\n", "  /proc/self/mounts", "no line");
        }
        // ro で
        printf("%-56s %s\n", "mount tmpfs ro on the spaced dir", mount("ro src", sp, "tmpfs", MS_RDONLY, NULL) == 0 ? "0" : strerror(errno));
        {
            FILE *f = fopen("/proc/self/mountinfo", "r");
            char line[1024];
            int found = 0;
            while (f && fgets(line, sizeof line, f))
                if (strstr(line, "with\\040space")) {
                    char *s = strstr(line, " - ");
                    char pt[300] = "", op[64] = "", src[64] = "";
                    long id, par;
                    unsigned ma, mi;
                    char rt[64];
                    sscanf(line, "%ld %ld %u:%u %63s %299s %63s", &id, &par, &ma, &mi, rt, pt, op);
                    if (s)
                        sscanf(s + 3, "%*s %63s", src);
                    const char *tail = strrchr(pt, '/');
                    printf("%-56s %s, opts %.2s, source %s\n", "  the line (escaped)", tail ? tail + 1 : pt, op, src);
                    found = 1;
                }
            if (f)
                fclose(f);
            if (!found)
                printf("%-56s %s\n", "  the line", "missing");
        }
        // umount
        printf("%-56s %s\n", "umount b", umount(b) == 0 ? "0" : strerror(errno));
        load(0);
        printf("%-56s %s\n", "  b's line is gone", by_point(b) ? "no" : "yes");
        printf("%-56s %s\n", "umount a", umount(a) == 0 ? "0" : strerror(errno));
        load(0);
        printf("%-56s %s\n", "  a's line is gone", by_point(a) ? "no" : "yes");
        fflush(stdout);
        // 親から見る (/proc/PID/mountinfo): 子の namespace のもの
        pause();
        _exit(0);
    }
    // 子が終わりまで書いて pause したら、親から子の mountinfo を見る
    usleep(300000);
    for (int i = 0; i < 50; i++) {
        snprintf(pm, sizeof pm, "/proc/%d/mountinfo", c);
        FILE *f = fopen(pm, "r");
        char line[1024];
        int sp_found = 0;
        while (f && fgets(line, sizeof line, f))
            if (strstr(line, "with\\040space"))
                sp_found = 1;
        if (f)
            fclose(f);
        if (sp_found) {
            printf("%-56s %s\n", "the parent sees the child's mount in /proc/PID", "yes");
            break;
        }
        if (i == 49)
            printf("%-56s %s\n", "the parent sees the child's mount in /proc/PID", "no");
        usleep(20000);
    }
    load(0);
    printf("%-56s %s\n", "the parent's own mountinfo has no such line", by_point(sp) ? "no" : "yes");
    kill(c, 9);
    waitpid(c, NULL, 0);
    rmdir(inner);
    rmdir(sub);
    rmdir(a);
    rmdir(b);
    rmdir(sp);
    rmdir(base);
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IONBF, 0);
    if (argc < 3) {
        fprintf(stderr, "usage: mountinfo EXT4-DIR TMPFS-DIR\n");
        return 2;
    }
    if (geteuid() == 0 && (setgid(65534) < 0 || setuid(65534) < 0))
        return 1;
    const char *home = argv[1], *tmp = argv[2];
    int n = load(0);
    printf("%-56s %s\n", "/proc/self/mountinfo", n > 0 ? "has lines" : n == 0 ? "empty" : strerror(-n));
    if (n <= 0)
        return 0;
    // どの行も形がそろっている (id がみなちがう、親は表にいるか自分)
    int ids_unique = 1, parents = 1, root_found = 0;
    for (int i = 0; i < nents; i++) {
        for (int j = i + 1; j < nents; j++)
            if (ents[i].id == ents[j].id)
                ids_unique = 0;
        if (!by_id(ents[i].parent) && ents[i].parent != ents[i].id && strcmp(ents[i].point, "/") != 0)
            parents = 0;
        if (strcmp(ents[i].point, "/") == 0)
            root_found = 1;
    }
    printf("%-56s %s\n", "ids are unique", ids_unique ? "yes" : "no");
    printf("%-56s %s\n", "every parent is listed (but the root's)", parents ? "yes" : "no");
    printf("%-56s %s\n", "a line for /", root_found ? "yes" : "no");
    struct ent *r = by_point("/");
    if (r)
        printf("%-56s %s %.2s\n", "/: root field, opts", r->root, r->opts);
    // ファイルシステムごとに dev がちがう
    struct stat sh, st, sp;
    stat(home, &sh);
    stat(tmp, &st);
    stat("/proc/self", &sp);
    printf("%-56s %s\n", "ext4, tmpfs and proc have different st_dev", sh.st_dev != st.st_dev && st.st_dev != sp.st_dev && sh.st_dev != sp.st_dev ? "yes" : "no");
    char f[300];
    snprintf(f, sizeof f, "%s/mi-file-%d", home, getpid());
    close(open(f, O_CREAT | O_WRONLY, 0644));
    about("ext4 file", f);
    unlink(f);
    snprintf(f, sizeof f, "%s/mi-file-%d", tmp, getpid());
    close(open(f, O_CREAT | O_WRONLY, 0644));
    about("tmpfs file", f);
    unlink(f);
    about("tmpfs dir (the mount point)", tmp);
    about("proc", "/proc/self/status");
    // statx の mask 0 でも MNT_ID はつく
    unsigned got = 0;
    sx_mnt(tmp, 0, &got, NULL, NULL, NULL, NULL);
    printf("%-56s %s\n", "statx mask 0: MNT_ID in the answer", got & STATX_MNT_ID ? "yes" : "no");
    // /proc/PID/mountinfo の許可
    struct stat ms;
    stat("/proc/self/mountinfo", &ms);
    printf("%-56s 0%o\n", "/proc/self/mountinfo mode", ms.st_mode);
    t_ns(tmp);
    return 0;
}

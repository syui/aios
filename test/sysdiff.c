// Linux と答えあわせするための、システムコールの並び。
// 種 (seed) から op の並びを作って動かし、答えを 1 行ずつ出す。並びは種だけで決まる (動かした答えによらない)
// ので、同じ種なら Linux でも aios でも同じシステムコールが同じ順に動く。2 つの出力の最初にちがう行が、
// ふるまいのちがい。aish の sysdiff (aish-vm) が、このマシンの Linux と VM の aios で動かしてくらべる。
// 手本: syzkaller (システムコールの並びを作る。fd は前の op が作ったものを使う)、gVisor の syscall テスト
// (Linux で動かしたものを答えにする)、Csmith と C-Reduce (答えくらべと、ちがいを小さくする)
//
//   sysdiff [-n OPS] DIR CASE...
//     CASE  SEED か SEED:I,J,... (I、J 番めの op だけ動かす。ちがいを小さくするとき) か FROM-TO (その種みな)
//           か @FILE (FILE に書いた CASE をみな。空白か改行で分ける。シリアルの端末に長い行を打たないため)
//     DIR   case ごとの場所をここに作り、終わったら消す (書けるところ。ext4 なら ext4 とくらべる)
//
// 出力はアーキテクチャによらない: フラグは名前で、stat は種類、許可、リンクの数、大きさ (ファイルとリンク
// だけ)、自分のものか。ino や時刻は出さない
//   sys SYSNAME RELEASE MACHINE      (はじめに 1 度。くらべない)
//   case CASE
//   I NAME(ARGS) = 答え [くわしく]
//   world                           (終わったときの木: 名前、stat、中身のハッシュ、リンク先)
//   end exit N | end signal N       (30 秒で終わらなければ SIGALRM)
// はじめの木は a (ファイル)、b/、b/c、d -> b。名前は a b c d と . ..、長すぎるもの。.. は 1 つのパスに 1 つまでで、リンク先には使わないので、case の
// 場所 (DIR/cN/w/r) から出ても 1 つ上 (w) まで。world は w から見せる
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef SYS_close_range
#define SYS_close_range 436
#endif
#ifndef SYS_openat2
#define SYS_openat2 437
#endif
#ifndef SYS_faccessat2
#define SYS_faccessat2 439
#endif
#ifndef SYS_fchmodat2
#define SYS_fchmodat2 452
#endif
#ifndef RENAME_NOREPLACE
#define RENAME_NOREPLACE 1
#endif
#ifndef RENAME_EXCHANGE
#define RENAME_EXCHANGE 2
#endif
#ifndef SEEK_DATA
#define SEEK_DATA 3
#endif
#ifndef SEEK_HOLE
#define SEEK_HOLE 4
#endif
// 64 ビットのカーネルが F_GETFL の答えに足す O_LARGEFILE (アーキテクチャで値がちがうので、くらべない)
#if defined(__x86_64__)
#define K_LARGEFILE 0100000
#elif defined(__aarch64__)
#define K_LARGEFILE 0400000
#else
#define K_LARGEFILE 0
#endif

#define MAXOPS 200
// fd の引数: 前の op の答え (op の番号 * 2 + どちらの端か) か、これら
#define REF_CWD (-100)
#define REF_BAD (-1)
#define REF_BIG (-2)

// ---- 種からの数 (splitmix64。どのマシンでも同じ並び) ----

static uint64_t rs;

static uint64_t rnd(void) {
    uint64_t z = (rs += 0x9e3779b97f4a7c15ULL);
    z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9ULL;
    z = (z ^ (z >> 27)) * 0x94d049bb133111ebULL;
    return z ^ (z >> 31);
}

static unsigned pick(unsigned n) {
    return (unsigned)(rnd() % n);
}

static int chance(unsigned pct) {
    return pick(100) < pct;
}

// ---- 出力 (1 行ずつ write。fork の前にためておかない) ----

static char line[16384];
static int ll;

static void say(const char *fmt, ...) {
    va_list ap;
    int room = (int)sizeof line - 1 - ll;
    int n = 0;
    va_start(ap, fmt);
    if (room > 0)
        n = vsnprintf(line + ll, room, fmt, ap);
    va_end(ap);
    if (n > 0)
        ll += n < room ? n : room - 1;
}

static void flush(void) {
    line[ll++] = '\n';
    for (int o = 0; o < ll;) {
        ssize_t w = write(1, line + o, ll - o);
        if (w <= 0)
            break;
        o += w;
    }
    ll = 0;
}

// 長い x の並び (長すぎる名前) は x*N に
static void sayq(const char *s) {
    size_t n = strlen(s);
    if (n > 40 && strspn(s, "x") == n) {
        say("\"x*%zu\"", n);
        return;
    }
    say("\"");
    for (; *s; s++)
        say(*s >= 32 && *s < 127 && *s != '"' && *s != '\\' ? "%c" : "\\x%02x", (unsigned char)*s);
    say("\"");
}

static const char *ename(int e) {
    static char b[16];
    switch (e) {
    case EPERM: return "EPERM";
    case ENOENT: return "ENOENT";
    case EINTR: return "EINTR";
    case EIO: return "EIO";
    case ENXIO: return "ENXIO";
    case EBADF: return "EBADF";
    case EAGAIN: return "EAGAIN";
    case ENOMEM: return "ENOMEM";
    case EACCES: return "EACCES";
    case EFAULT: return "EFAULT";
    case EBUSY: return "EBUSY";
    case EEXIST: return "EEXIST";
    case EXDEV: return "EXDEV";
    case ENODEV: return "ENODEV";
    case ENOTDIR: return "ENOTDIR";
    case EISDIR: return "EISDIR";
    case EINVAL: return "EINVAL";
    case ENFILE: return "ENFILE";
    case EMFILE: return "EMFILE";
    case ENOTTY: return "ENOTTY";
    case ETXTBSY: return "ETXTBSY";
    case EFBIG: return "EFBIG";
    case ENOSPC: return "ENOSPC";
    case ESPIPE: return "ESPIPE";
    case EROFS: return "EROFS";
    case EMLINK: return "EMLINK";
    case EPIPE: return "EPIPE";
    case ERANGE: return "ERANGE";
    case ENAMETOOLONG: return "ENAMETOOLONG";
    case ENOSYS: return "ENOSYS";
    case ENOTEMPTY: return "ENOTEMPTY";
    case ELOOP: return "ELOOP";
    case EOPNOTSUPP: return "EOPNOTSUPP";
    case EOVERFLOW: return "EOVERFLOW";
    }
    snprintf(b, sizeof b, "E%d", e);
    return b;
}

static void ret(long r) {
    if (r < 0)
        say(" = %s", ename(errno));
    else
        say(" = %ld", r);
}

static uint32_t fnv(const unsigned char *p, long n) {
    uint32_t h = 2166136261u;
    for (long i = 0; i < n; i++)
        h = (h ^ p[i]) * 16777619u;
    return h;
}

static void sayst(const struct stat *st) {
    mode_t m = st->st_mode;
    const char *t = S_ISREG(m) ? "reg" : S_ISDIR(m) ? "dir" : S_ISLNK(m) ? "lnk" : S_ISFIFO(m) ? "fifo" : S_ISSOCK(m) ? "sock" : S_ISCHR(m) ? "chr" : S_ISBLK(m) ? "blk" : "?";
    say(" %s %04o n%lu", t, (unsigned)(m & 07777), (unsigned long)st->st_nlink);
    if (S_ISREG(m) || S_ISLNK(m))
        say(" s%lld", (long long)st->st_size);
    say(st->st_uid == geteuid() && st->st_gid == getegid() ? " me" : " other");
}

// ---- 選ぶもの (どれも、値ではなく何番めかを選んで、名前で出す) ----

enum {
    OPEN, OPENAT2, CLOSE, WRITE, PWRITE, READ, PREAD, LSEEK, FTRUNCATE, TRUNCATE, FSTAT, STAT, MKDIR, UNLINK, RMDIR,
    RENAME, LINK, SYMLINK, READLINK, CHMOD, FCHMOD, ACCESS, GETDENTS, DUP, DUP3, FCNTL, PIPE, FSYNC, NKIND
};
static const unsigned weight[NKIND] = {14, 2, 5, 8, 4, 6, 3, 4, 3, 2, 4, 7, 6, 4, 3, 7, 4, 5, 3, 3, 2, 3, 4, 2, 2, 4, 2, 1};

static const char *names[] = {"a", "b", "c", "d"};

static const int accv[] = {O_RDONLY, O_WRONLY, O_RDWR, 3};
static const char *accn[] = {"O_RDONLY", "O_WRONLY", "O_RDWR", "O_ACCMODE"};
static const unsigned accw[] = {40, 25, 33, 2};

// open のフラグ (何番めのビットか)
#define NOF 11
static const int ofv[NOF] = {O_CREAT, O_EXCL, O_TRUNC, O_APPEND, O_DIRECTORY, O_NOFOLLOW, O_CLOEXEC, O_NONBLOCK, O_PATH, O_NOCTTY, O_TMPFILE};
static const char *ofn[NOF] = {"O_CREAT", "O_EXCL", "O_TRUNC", "O_APPEND", "O_DIRECTORY", "O_NOFOLLOW", "O_CLOEXEC", "O_NONBLOCK", "O_PATH", "O_NOCTTY", "O_TMPFILE"};
static const unsigned ofp[NOF] = {45, 15, 15, 15, 12, 10, 20, 8, 5, 2, 2};
#define B_CREAT 1
#define B_TMPFILE (1 << 10)

static const unsigned modes[] = {0644, 0600, 0755, 0444, 0000, 0700, 0777, 04755, 01755, 0200};
#define NMODES (sizeof modes / sizeof *modes)
static const long long lens[] = {0, 1, 7, 100, 4096, 5000, 12000};
static const long long rlens[] = {0, 1, 10, 4096, 16384};
static const long long offs[] = {0, 3, 4096, 4097, 100000, -1};
static const long long soffs[] = {0, 1, 5, 100, 4096, 10000, -1, -4097, 1LL << 40};
static const long long tlens[] = {0, 1, 5, 4096, 5000, 100000, -1};
static const int whv[] = {SEEK_SET, SEEK_CUR, SEEK_END, SEEK_DATA, SEEK_HOLE, 7};
static const char *whn[] = {"SEEK_SET", "SEEK_CUR", "SEEK_END", "SEEK_DATA", "SEEK_HOLE", "7"};
static const unsigned whw[] = {30, 25, 25, 8, 8, 4};
static const char *resn[] = {"RESOLVE_NO_XDEV", "RESOLVE_NO_MAGICLINKS", "RESOLVE_NO_SYMLINKS", "RESOLVE_BENEATH", "RESOLVE_IN_ROOT"};
static const int amv[] = {F_OK, R_OK, W_OK, X_OK, R_OK | W_OK};
static const char *amn[] = {"F_OK", "R_OK", "W_OK", "X_OK", "R_OK|W_OK"};
static const int afv[] = {0, AT_EACCESS, AT_SYMLINK_NOFOLLOW};
static const char *afn[] = {"0", "AT_EACCESS", "AT_SYMLINK_NOFOLLOW"};
static const int rnv[] = {0, RENAME_NOREPLACE, RENAME_EXCHANGE, RENAME_NOREPLACE | RENAME_EXCHANGE};
static const char *rnn[] = {"0", "RENAME_NOREPLACE", "RENAME_EXCHANGE", "RENAME_NOREPLACE|RENAME_EXCHANGE"};
static const unsigned rnw[] = {60, 23, 15, 2};
enum { C_GETFL, C_SETFL, C_GETFD, C_SETFD, C_DUPFD, C_DUPFD_CLOEXEC };
static const char *cmdn[] = {"F_GETFL", "F_SETFL", "F_GETFD", "F_SETFD", "F_DUPFD", "F_DUPFD_CLOEXEC"};
static const int dupmin[] = {0, 3, 5, 10, 100};

static unsigned pickw(const unsigned *w, int n) {
    unsigned t = 0;
    for (int i = 0; i < n; i++)
        t += w[i];
    unsigned x = pick(t);
    int i = 0;
    while (x >= w[i])
        x -= w[i++];
    return (unsigned)i;
}

struct op {
    int kind;
    char p1[300], p2[300];
    // fb: fd の引数を作った op がしくじっていたら、いま開いているいちばん新しい fd を使う (EBADF ばかりにならないように)
    int d1, d2, fd, fb;
    unsigned acc, of, mode;
    long long off, len;
    int x, y;
};

static struct op ops[MAXOPS];
static int res[MAXOPS][2];

// 作った fd の引数のもと (op の番号 * 2 + 端)、パイプか (F_SETFL でも O_NONBLOCK を消さない: 読み書きで止まらない
// ように)、開けられそうか (あるはずのパスを、ふつうのフラグで。そういうものを多く使う)
static int nref, refs[2 * MAXOPS], refpipe[2 * MAXOPS], reflikely[2 * MAXOPS];

static void addref(int r, int pipe, int likely) {
    refs[nref] = r;
    refpipe[nref] = pipe;
    reflikely[nref] = likely;
    nref++;
}

static int genfd2(int *pipe, int *likely) {
    *pipe = *likely = 0;
    if (nref == 0 || chance(4))
        return chance(50) ? REF_BAD : REF_BIG;
    int cand[2 * MAXOPS], n = 0;
    for (int i = 0; i < nref; i++)
        if (reflikely[i])
            cand[n++] = i;
    if (n == 0 || chance(20)) {
        n = 0;
        for (int i = 0; i < nref; i++)
            cand[n++] = i;
    }
    // 近いものほど選ばれやすく
    int i = cand[chance(60) ? n - 1 - (int)pick(n < 3 ? n : 3) : (int)pick(n)];
    *pipe = refpipe[i];
    *likely = reflikely[i];
    return refs[i];
}

static int genfd(int *pipe) {
    int likely;
    return genfd2(pipe, &likely);
}

static int gendir(void) {
    int pipe;
    return chance(88) ? REF_CWD : genfd(&pipe);
}

// あるはずのパス (はじめの木と、前の op が作ったもの。作れたかは動かすまでわからないが、
// syzkaller と同じく、作ったものを使うほうが中まで届く)
// 種類も覚える: f ファイル、d ディレクトリ、l リンク、? わからない
#define NKNOWN 48
static char known[NKNOWN][64], ktype[NKNOWN];
static int nknown;

static void know(const char *p, char t) {
    if (!p[0] || strlen(p) >= 64)
        return;
    for (int i = 0; i < nknown; i++)
        if (!strcmp(known[i], p)) {
            ktype[i] = t;
            return;
        }
    int i = nknown < NKNOWN ? nknown++ : (int)pick(NKNOWN);
    strcpy(known[i], p);
    ktype[i] = t;
}

static char kindof(const char *p) {
    for (int i = 0; i < nknown; i++)
        if (!strcmp(known[i], p))
            return ktype[i];
    return '?';
}

// パス。target ならリンク先 (.. と長すぎる名前は使わない)。あるはずのパスそのものなら、その種類 (なければ 0)。
// あるはずのディレクトリの下の名前も作る
static char genpath(char *p, int target) {
    if (nknown && chance(55)) {
        int k = (int)pick(nknown);
        strcpy(p, known[k]);
        if (chance(5))
            strcat(p, "/");
        else if (!target && ktype[k] != 'f' && chance(30)) {
            strcat(p, "/");
            strcat(p, names[pick(4)]);
        } else
            return ktype[k];
        return 0;
    }
    unsigned r = pick(100);
    if (r < 2) {
        p[0] = 0;
        return 0;
    }
    if (!target && r < 4) {
        memset(p, 'x', 256);
        p[256] = 0;
        return 0;
    }
    if (r < 7) {
        strcpy(p, ".");
        return 0;
    }
    int n = 1 + chance(45) + chance(20), dotdot = 0;
    p[0] = 0;
    for (int i = 0; i < n; i++) {
        if (i)
            strcat(p, chance(4) ? "//" : "/");
        if (i && !target && !dotdot && chance(8)) {
            strcat(p, "..");
            dotdot = 1;
        } else if (i && chance(4)) {
            strcat(p, ".");
        } else {
            strcat(p, names[pick(4)]);
        }
    }
    if (chance(6))
        strcat(p, "/");
    return 0;
}

static void gen(uint64_t seed, int n) {
    rs = seed;
    nref = 0;
    nknown = 0;
    know("a", 'f');
    know("b", 'd');
    know("b/c", 'f');
    know("d", 'd');
    know("d/c", 'f');
    for (int i = 0; i < n; i++) {
        struct op *o = &ops[i];
        int pipe = 0;
        memset(o, 0, sizeof *o);
        o->kind = (int)pickw(weight, NKIND);
        o->fb = chance(80);
        // fd がまだないうちに fd の op を選んだら、開ける op に
        if (nref == 0 && o->kind != OPENAT2 && o->kind != TRUNCATE && o->kind != STAT && o->kind != MKDIR && o->kind != UNLINK && o->kind != RMDIR &&
            o->kind != RENAME && o->kind != LINK && o->kind != SYMLINK && o->kind != READLINK && o->kind != CHMOD && o->kind != ACCESS)
            o->kind = chance(80) ? OPEN : PIPE;
        o->d1 = o->d2 = REF_CWD;
        o->fd = REF_BAD;
        switch (o->kind) {
        case OPEN:
        case OPENAT2: {
            o->d1 = gendir();
            char k = genpath(o->p1, 0);
            o->acc = pickw(accw, 4);
            for (int b = 0; b < NOF; b++)
                if (chance(ofp[b]))
                    o->of |= 1u << b;
            // ディレクトリはたいてい読むだけで (書けば EISDIR)
            if (k == 'd' && chance(75)) {
                o->acc = 0;
                o->of &= ~(B_CREAT | (1u << 2));
            }
            o->mode = (o->of & (B_CREAT | B_TMPFILE)) || chance(10) ? modes[pick(NMODES)] : 0;
            if (o->kind == OPENAT2)
                for (int b = 0; b < 5; b++)
                    if (chance(15))
                        o->x |= 1 << b;
            if ((o->of & B_CREAT) && !k)
                know(o->p1, 'f');
            int odd = (1 << 1) | (1 << 4) | (1 << 8) | B_TMPFILE;
            addref(2 * i, 0, o->d1 == REF_CWD && o->acc != 3 && !(o->of & odd) && (k || ((o->of & B_CREAT) && o->p1[0] && o->p1[0] != 'x')));
            break;
        }
        case CLOSE:
        case FSTAT:
        case GETDENTS:
            o->fd = genfd(&pipe);
            break;
        case FSYNC:
            o->fd = genfd(&pipe);
            o->x = chance(30);
            break;
        case WRITE:
        case PWRITE:
            o->fd = genfd(&pipe);
            o->len = lens[pick(7)];
            o->x = (int)pick(256);
            if (o->kind == PWRITE)
                o->off = offs[pick(6)];
            break;
        case READ:
        case PREAD:
            o->fd = genfd(&pipe);
            o->len = rlens[pick(5)];
            if (o->kind == PREAD)
                o->off = offs[pick(6)];
            break;
        case LSEEK:
            o->fd = genfd(&pipe);
            o->off = soffs[pick(9)];
            o->x = (int)pickw(whw, 6);
            break;
        case FTRUNCATE:
            o->fd = genfd(&pipe);
            o->len = tlens[pick(7)];
            break;
        case TRUNCATE:
            genpath(o->p1, 0);
            o->len = tlens[pick(7)];
            break;
        case STAT:
            // AT_EMPTY_PATH なら fd そのもの
            if (chance(6)) {
                o->d1 = genfd(&pipe);
                o->x = 2;
            } else {
                o->d1 = gendir();
                genpath(o->p1, 0);
                o->x = chance(40);
            }
            break;
        case MKDIR:
            o->d1 = gendir();
            genpath(o->p1, 0);
            o->mode = modes[pick(NMODES)];
            know(o->p1, 'd');
            break;
        case UNLINK:
        case RMDIR:
            o->d1 = gendir();
            genpath(o->p1, 0);
            break;
        case RENAME:
        case LINK:
            o->d1 = gendir();
            genpath(o->p1, 0);
            o->d2 = gendir();
            genpath(o->p2, 0);
            o->x = o->kind == RENAME ? (int)pickw(rnw, 4) : chance(20);
            know(o->p2, o->kind == RENAME ? kindof(o->p1) : 'f');
            break;
        case SYMLINK:
            genpath(o->p1, 1);
            o->d2 = gendir();
            genpath(o->p2, 0);
            know(o->p2, 'l');
            break;
        case READLINK:
            o->d1 = gendir();
            genpath(o->p1, 0);
            o->len = chance(80) ? 100 : chance(50) ? 1 : 0;
            break;
        case CHMOD:
            o->d1 = gendir();
            genpath(o->p1, 0);
            o->mode = modes[pick(NMODES)];
            o->y = chance(10);
            break;
        case FCHMOD:
            o->fd = genfd(&pipe);
            o->mode = modes[pick(NMODES)];
            break;
        case ACCESS:
            o->d1 = gendir();
            genpath(o->p1, 0);
            o->x = (int)pick(5);
            o->y = chance(30) ? (int)pick(3) : -1;
            break;
        case DUP: {
            int likely;
            o->fd = genfd2(&pipe, &likely);
            addref(2 * i, pipe, likely);
            break;
        }
        case DUP3: {
            int likely;
            o->fd = genfd2(&pipe, &likely);
            o->x = 3 + (int)pick(10);
            o->y = chance(60) ? 0 : chance(88) ? 1 : 2;
            addref(2 * i, pipe, likely && o->y != 2);
            break;
        }
        case FCNTL: {
            int likely;
            o->fd = genfd2(&pipe, &likely);
            o->x = (int)pick(6);
            if (o->x == C_SETFL)
                o->y = (chance(40) ? 1 : 0) | (pipe || chance(40) ? 2 : 0) | (chance(5) ? 4 : 0);
            else if (o->x == C_SETFD)
                o->y = chance(60);
            else if (o->x == C_DUPFD || o->x == C_DUPFD_CLOEXEC) {
                o->y = dupmin[pick(5)];
                addref(2 * i, pipe, likely);
            }
            break;
        }
        case PIPE:
            o->x = chance(30);
            addref(2 * i, 1, 1);
            addref(2 * i + 1, 1, 1);
            break;
        }
    }
}

// ---- 動かす ----

// 開いている fd と、それを作った op の番号 + 1 (fb のため。閉じたら 0)
static int alive[1024];

static void born(int i, long fd) {
    if (fd >= 0 && fd < 1024)
        alive[fd] = i + 1;
}

static int fdof(int ref) {
    if (ref == REF_CWD)
        return AT_FDCWD;
    if (ref == REF_BAD)
        return -1;
    if (ref == REF_BIG)
        return 999;
    return res[ref / 2][ref % 2];
}

static int fdarg(const struct op *o) {
    int fd = fdof(o->fd);
    if (fd < 0 && o->fd >= 0 && o->fb) {
        int best = 0;
        for (int f = 0; f < 1024; f++)
            if (alive[f] > best) {
                best = alive[f];
                fd = f;
            }
    }
    return fd;
}

static void saydir(int ref) {
    int f = fdof(ref);
    if (f == AT_FDCWD)
        say("cwd");
    else
        say("%d", f);
}

static int ofval(const struct op *o) {
    int v = accv[o->acc];
    for (int b = 0; b < NOF; b++)
        if (o->of & (1u << b))
            v |= ofv[b];
    return v;
}

static void sayof(const struct op *o) {
    say("%s", accn[o->acc]);
    for (int b = 0; b < NOF; b++)
        if (o->of & (1u << b))
            say("|%s", ofn[b]);
}

// F_GETFL の答えを名前に (O_SYNC は O_DSYNC をふくむので先に)
static void sayfl(int f) {
    static const struct {
        int v;
        const char *n;
    } fl[] = {
        {O_APPEND, "O_APPEND"}, {O_NONBLOCK, "O_NONBLOCK"}, {O_DIRECTORY, "O_DIRECTORY"}, {O_NOFOLLOW, "O_NOFOLLOW"},
        {O_PATH, "O_PATH"}, {O_DIRECT, "O_DIRECT"}, {O_NOATIME, "O_NOATIME"}, {O_SYNC, "O_SYNC"}, {O_DSYNC, "O_DSYNC"},
        {O_CLOEXEC, "O_CLOEXEC"}, {O_CREAT, "O_CREAT"}, {O_EXCL, "O_EXCL"}, {O_TRUNC, "O_TRUNC"}, {O_NOCTTY, "O_NOCTTY"},
        {O_TMPFILE & ~O_DIRECTORY, "__O_TMPFILE"},
    };
    // 読み書きの口は下の 2 ビット (musl の O_ACCMODE は O_PATH もふくむので使わない)
    f &= ~K_LARGEFILE;
    say(" %s", accn[f & 3]);
    f &= ~3;
    for (size_t i = 0; i < sizeof fl / sizeof *fl; i++)
        if ((f & fl[i].v) == fl[i].v) {
            say("|%s", fl[i].n);
            f &= ~fl[i].v;
        }
    if (f)
        say("|?");
}

static char dtype(unsigned char t) {
    switch (t) {
    case DT_REG: return 'f';
    case DT_DIR: return 'd';
    case DT_LNK: return 'l';
    case DT_FIFO: return 'p';
    case DT_SOCK: return 's';
    case DT_CHR: return 'c';
    case DT_BLK: return 'b';
    }
    return 'u';
}

static int cmpstr(const void *a, const void *b) {
    return strcmp(*(char *const *)a, *(char *const *)b);
}

// getdents64 を終わりまで。名前は並べなおす (並びはファイルシステムしだい)
static void saydents(int fd) {
    static char buf[8192];
    char *v[256];
    int n = 0;
    long r;
    while ((r = syscall(SYS_getdents64, fd, buf, sizeof buf)) > 0) {
        for (long o = 0; o < r;) {
            unsigned short reclen;
            memcpy(&reclen, buf + o + 16, sizeof reclen);
            const char *name = buf + o + 19;
            if (n < 256) {
                v[n] = malloc(strlen(name) + 3);
                sprintf(v[n], "%s:%c", name, dtype((unsigned char)buf[o + 18]));
                n++;
            }
            if (reclen == 0)
                break;
            o += reclen;
        }
    }
    int e = errno;
    if (r < 0 && n == 0) {
        say(" = %s", ename(e));
        return;
    }
    qsort(v, n, sizeof *v, cmpstr);
    say(" = %d", n);
    for (int i = 0; i < n; i++) {
        size_t l = strlen(v[i]);
        if (l > 40 && strspn(v[i], "x") == l - 2)
            say(" x*%zu%s", l - 2, v[i] + l - 2);
        else
            say(" %s", v[i]);
        free(v[i]);
    }
    if (r < 0)
        say(" then %s", ename(e));
}

static unsigned char wbuf[16384], rbuf[16384];

static void exec(int i, const struct op *o) {
    long r = 0;
    struct stat st;
    int fd = fdarg(o);
    say("%d ", i);
    switch (o->kind) {
    case OPEN:
        say("openat(");
        saydir(o->d1);
        say(", ");
        sayq(o->p1);
        say(", ");
        sayof(o);
        say(", %04o)", o->mode);
        r = syscall(SYS_openat, fdof(o->d1), o->p1, ofval(o), o->mode);
        ret(r);
        if (r >= 0)
            res[i][0] = (int)r;
        born(i, r);
        break;
    case OPENAT2: {
        struct {
            uint64_t flags, mode, resolve;
        } how = {(uint64_t)(unsigned)ofval(o), o->mode, (uint64_t)o->x};
        say("openat2(");
        saydir(o->d1);
        say(", ");
        sayq(o->p1);
        say(", ");
        sayof(o);
        say(", %04o, ", o->mode);
        if (!o->x)
            say("0");
        for (int b = 0, first = 1; b < 5; b++)
            if (o->x & (1 << b)) {
                say("%s%s", first ? "" : "|", resn[b]);
                first = 0;
            }
        say(")");
        r = syscall(SYS_openat2, fdof(o->d1), o->p1, &how, sizeof how);
        ret(r);
        if (r >= 0)
            res[i][0] = (int)r;
        born(i, r);
        break;
    }
    case CLOSE:
        say("close(%d)", fd);
        r = syscall(SYS_close, fd);
        ret(r);
        if (r == 0 && fd >= 0 && fd < 1024)
            alive[fd] = 0;
        break;
    case WRITE:
    case PWRITE:
        for (long long k = 0; k < o->len; k++)
            wbuf[k] = (unsigned char)(o->x + k * 7);
        if (o->kind == WRITE) {
            say("write(%d, %lld)", fd, o->len);
            r = syscall(SYS_write, fd, wbuf, (size_t)o->len);
        } else {
            say("pwrite64(%d, %lld, %lld)", fd, o->len, o->off);
            r = syscall(SYS_pwrite64, fd, wbuf, (size_t)o->len, (off_t)o->off);
        }
        ret(r);
        break;
    case READ:
    case PREAD:
        if (o->kind == READ) {
            say("read(%d, %lld)", fd, o->len);
            r = syscall(SYS_read, fd, rbuf, (size_t)o->len);
        } else {
            say("pread64(%d, %lld, %lld)", fd, o->len, o->off);
            r = syscall(SYS_pread64, fd, rbuf, (size_t)o->len, (off_t)o->off);
        }
        ret(r);
        if (r > 0)
            say(" h%08x", fnv(rbuf, r));
        break;
    case LSEEK:
        say("lseek(%d, %lld, %s)", fd, o->off, whn[o->x]);
        ret(syscall(SYS_lseek, fd, (off_t)o->off, whv[o->x]));
        break;
    case FTRUNCATE:
        say("ftruncate(%d, %lld)", fd, o->len);
        ret(syscall(SYS_ftruncate, fd, (off_t)o->len));
        break;
    case TRUNCATE:
        say("truncate(");
        sayq(o->p1);
        say(", %lld)", o->len);
        ret(syscall(SYS_truncate, o->p1, (off_t)o->len));
        break;
    case FSTAT:
        say("fstat(%d)", fd);
        r = syscall(SYS_fstat, fd, &st);
        ret(r);
        if (r == 0)
            sayst(&st);
        break;
    case STAT: {
        static const int sfv[] = {0, AT_SYMLINK_NOFOLLOW, AT_EMPTY_PATH};
        static const char *sfn[] = {"0", "AT_SYMLINK_NOFOLLOW", "AT_EMPTY_PATH"};
        say("newfstatat(");
        saydir(o->d1);
        say(", ");
        sayq(o->p1);
        say(", %s)", sfn[o->x]);
        r = syscall(SYS_newfstatat, fdof(o->d1), o->p1, &st, sfv[o->x]);
        ret(r);
        if (r == 0)
            sayst(&st);
        break;
    }
    case MKDIR:
        say("mkdirat(");
        saydir(o->d1);
        say(", ");
        sayq(o->p1);
        say(", %04o)", o->mode);
        ret(syscall(SYS_mkdirat, fdof(o->d1), o->p1, o->mode));
        break;
    case UNLINK:
    case RMDIR:
        say("unlinkat(");
        saydir(o->d1);
        say(", ");
        sayq(o->p1);
        say(", %s)", o->kind == RMDIR ? "AT_REMOVEDIR" : "0");
        ret(syscall(SYS_unlinkat, fdof(o->d1), o->p1, o->kind == RMDIR ? AT_REMOVEDIR : 0));
        break;
    case RENAME:
    case LINK:
        say(o->kind == RENAME ? "renameat2(" : "linkat(");
        saydir(o->d1);
        say(", ");
        sayq(o->p1);
        say(", ");
        saydir(o->d2);
        say(", ");
        sayq(o->p2);
        if (o->kind == RENAME) {
            say(", %s)", rnn[o->x]);
            r = syscall(SYS_renameat2, fdof(o->d1), o->p1, fdof(o->d2), o->p2, rnv[o->x]);
        } else {
            say(", %s)", o->x ? "AT_SYMLINK_FOLLOW" : "0");
            r = syscall(SYS_linkat, fdof(o->d1), o->p1, fdof(o->d2), o->p2, o->x ? AT_SYMLINK_FOLLOW : 0);
        }
        ret(r);
        break;
    case SYMLINK:
        say("symlinkat(");
        sayq(o->p1);
        say(", ");
        saydir(o->d2);
        say(", ");
        sayq(o->p2);
        say(")");
        ret(syscall(SYS_symlinkat, o->p1, fdof(o->d2), o->p2));
        break;
    case READLINK: {
        char t[128];
        say("readlinkat(");
        saydir(o->d1);
        say(", ");
        sayq(o->p1);
        say(", %lld)", o->len);
        r = syscall(SYS_readlinkat, fdof(o->d1), o->p1, t, (size_t)o->len);
        ret(r);
        if (r >= 0) {
            t[r] = 0;
            say(" ");
            sayq(t);
        }
        break;
    }
    case CHMOD:
        say(o->y ? "fchmodat2(" : "fchmodat(");
        saydir(o->d1);
        say(", ");
        sayq(o->p1);
        say(", %04o%s)", o->mode, o->y ? ", AT_SYMLINK_NOFOLLOW" : "");
        if (o->y)
            r = syscall(SYS_fchmodat2, fdof(o->d1), o->p1, o->mode, AT_SYMLINK_NOFOLLOW);
        else
            r = syscall(SYS_fchmodat, fdof(o->d1), o->p1, o->mode);
        ret(r);
        break;
    case FCHMOD:
        say("fchmod(%d, %04o)", fd, o->mode);
        ret(syscall(SYS_fchmod, fd, o->mode));
        break;
    case ACCESS:
        say(o->y < 0 ? "faccessat(" : "faccessat2(");
        saydir(o->d1);
        say(", ");
        sayq(o->p1);
        say(", %s", amn[o->x]);
        if (o->y < 0) {
            say(")");
            r = syscall(SYS_faccessat, fdof(o->d1), o->p1, amv[o->x]);
        } else {
            say(", %s)", afn[o->y]);
            r = syscall(SYS_faccessat2, fdof(o->d1), o->p1, amv[o->x], afv[o->y]);
        }
        ret(r);
        break;
    case GETDENTS:
        say("getdents64(%d)", fd);
        saydents(fd);
        break;
    case DUP:
        say("dup(%d)", fd);
        r = syscall(SYS_dup, fd);
        ret(r);
        if (r >= 0)
            res[i][0] = (int)r;
        born(i, r);
        break;
    case DUP3: {
        static const int dfv[] = {0, O_CLOEXEC, 1};
        static const char *dfn[] = {"0", "O_CLOEXEC", "1"};
        say("dup3(%d, %d, %s)", fd, o->x, dfn[o->y]);
        r = syscall(SYS_dup3, fd, o->x, dfv[o->y]);
        ret(r);
        if (r >= 0)
            res[i][0] = (int)r;
        born(i, r);
        break;
    }
    case FCNTL:
        say("fcntl(%d, %s", fd, cmdn[o->x]);
        switch (o->x) {
        case C_GETFL:
            say(")");
            r = syscall(SYS_fcntl, fd, F_GETFL);
            if (r < 0)
                ret(r);
            else {
                say(" =");
                sayfl((int)r);
            }
            break;
        case C_SETFL: {
            int v = 0;
            char s[64] = "";
            if (o->y & 1) {
                v |= O_APPEND;
                strcat(s, "|O_APPEND");
            }
            if (o->y & 2) {
                v |= O_NONBLOCK;
                strcat(s, "|O_NONBLOCK");
            }
            if (o->y & 4) {
                v |= O_NOATIME;
                strcat(s, "|O_NOATIME");
            }
            say(", %s)", s[0] ? s + 1 : "0");
            ret(syscall(SYS_fcntl, fd, F_SETFL, v));
            break;
        }
        case C_GETFD:
            say(")");
            r = syscall(SYS_fcntl, fd, F_GETFD);
            if (r < 0)
                ret(r);
            else
                say(" = %s", r & FD_CLOEXEC ? "FD_CLOEXEC" : "0");
            break;
        case C_SETFD:
            say(", %s)", o->y ? "FD_CLOEXEC" : "0");
            ret(syscall(SYS_fcntl, fd, F_SETFD, o->y ? FD_CLOEXEC : 0));
            break;
        default:
            say(", %d)", o->y);
            r = syscall(SYS_fcntl, fd, o->x == C_DUPFD ? F_DUPFD : F_DUPFD_CLOEXEC, o->y);
            ret(r);
            if (r >= 0)
                res[i][0] = (int)r;
            born(i, r);
        }
        break;
    case PIPE: {
        int p[2];
        say("pipe2(O_NONBLOCK%s)", o->x ? "|O_CLOEXEC" : "");
        r = syscall(SYS_pipe2, p, O_NONBLOCK | (o->x ? O_CLOEXEC : 0));
        ret(r);
        if (r == 0) {
            res[i][0] = p[0];
            res[i][1] = p[1];
            born(i, p[0]);
            born(i, p[1]);
            say(" [%d,%d]", p[0], p[1]);
        }
        break;
    }
    case FSYNC:
        say("%s(%d)", o->x ? "fdatasync" : "fsync", fd);
        ret(syscall(o->x ? SYS_fdatasync : SYS_fsync, fd));
        break;
    }
    flush();
}

// ---- 終わったときの木 ----

static void entry(const char *p, int depth);

static void listdir(const char *p, int depth) {
    DIR *d = opendir(p);
    if (!d) {
        say("  ");
        sayq(p);
        say(" list %s", ename(errno));
        flush();
        return;
    }
    char *v[512];
    int n = 0;
    struct dirent *e;
    while ((e = readdir(d)) && n < 512)
        if (strcmp(e->d_name, ".") && strcmp(e->d_name, ".."))
            v[n++] = strdup(e->d_name);
    closedir(d);
    qsort(v, n, sizeof *v, cmpstr);
    for (int i = 0; i < n; i++) {
        char q[4096];
        snprintf(q, sizeof q, "%s/%s", p, v[i]);
        entry(q, depth);
        free(v[i]);
    }
}

static void entry(const char *p, int depth) {
    struct stat st;
    // 表示は ./ をとって
    const char *show = strncmp(p, "./", 2) ? p : p + 2;
    say("  ");
    sayq(show);
    if (lstat(p, &st) < 0) {
        say(" %s", ename(errno));
        flush();
        return;
    }
    sayst(&st);
    if (S_ISREG(st.st_mode)) {
        // 中身のハッシュは頭とおしりの 64 KiB (lseek で 1 TiB 先に書いた穴だらけのものもある)
        int fd = open(p, O_RDONLY | O_NOFOLLOW | O_NONBLOCK);
        if (fd < 0)
            say(" h%s", ename(errno));
        else {
            uint32_t h = 2166136261u;
            long long size = st.st_size;
            for (int part = 0; part < 2; part++) {
                long long from = part ? (size > 65536 ? size - 65536 : 65536) : 0, to = part ? size : (size < 65536 ? size : 65536);
                while (from < to) {
                    long want = to - from < (long long)sizeof rbuf ? (long)(to - from) : (long)sizeof rbuf;
                    long n = pread(fd, rbuf, want, from);
                    if (n <= 0)
                        break;
                    for (long k = 0; k < n; k++)
                        h = (h ^ rbuf[k]) * 16777619u;
                    from += n;
                }
            }
            say(" h%08x", h);
            close(fd);
        }
    }
    if (S_ISLNK(st.st_mode)) {
        char t[512];
        ssize_t n = readlink(p, t, sizeof t - 1);
        if (n >= 0) {
            t[n] = 0;
            say(" -> ");
            sayq(t);
        }
    }
    flush();
    if (S_ISDIR(st.st_mode) && depth < 6)
        listdir(p, depth + 1);
}

// 消す (許可のないディレクトリも、開けてから)
static void rmrf(const char *p) {
    struct stat st;
    if (lstat(p, &st) < 0)
        return;
    if (!S_ISDIR(st.st_mode)) {
        unlink(p);
        return;
    }
    chmod(p, 0700);
    DIR *d = opendir(p);
    if (d) {
        char *v[512];
        int n = 0;
        struct dirent *e;
        while ((e = readdir(d)) && n < 512)
            if (strcmp(e->d_name, ".") && strcmp(e->d_name, ".."))
                v[n++] = strdup(e->d_name);
        closedir(d);
        for (int i = 0; i < n; i++) {
            char q[4096];
            snprintf(q, sizeof q, "%s/%s", p, v[i]);
            rmrf(q);
            free(v[i]);
        }
    }
    rmdir(p);
}

// はじめの木 (どの case も同じ): a (6 バイト)、b/、b/c (5000 バイト、0600)、d -> b
static void setup(void) {
    int fd = open("a", O_WRONLY | O_CREAT | O_EXCL, 0644);
    if (fd >= 0) {
        if (write(fd, "hello\n", 6) != 6)
            _exit(2);
        close(fd);
    }
    if (mkdir("b", 0755) != 0)
        _exit(2);
    fd = open("b/c", O_WRONLY | O_CREAT | O_EXCL, 0600);
    if (fd >= 0) {
        memset(wbuf, 'z', 5000);
        if (write(fd, wbuf, 5000) != 5000)
            _exit(2);
        close(fd);
    }
    if (symlink("b", "d") != 0)
        _exit(2);
}

static void child(const char *w, int n, const unsigned char *keep) {
    signal(SIGPIPE, SIG_IGN);
    umask(022);
    // 受けついだ fd を閉じる (どちらも 3 から使う)
    if (syscall(SYS_close_range, 3, ~0U, 0) != 0)
        for (int f = 3; f < 256; f++)
            close(f);
    if (chdir(w) != 0 || chdir("r") != 0)
        _exit(2);
    setup();
    alarm(30);
    for (int i = 0; i < n; i++) {
        res[i][0] = res[i][1] = -1;
        if (!keep || keep[i])
            exec(i, &ops[i]);
    }
    alarm(0);
    if (chdir(w) != 0)
        _exit(2);
    say("world");
    flush();
    listdir(".", 0);
    _exit(0);
}

// 1 つの case を動かす (場所が作れなければ 1)
static int runcase(const char *base, int c, const char *arg, int n) {
    char *end;
    uint64_t seed = strtoull(arg, &end, 10);
    unsigned char keep[MAXOPS], *kp = NULL;
    if (*end == ':') {
        memset(keep, 0, sizeof keep);
        kp = keep;
        for (char *s = end + 1; *s;) {
            long k = strtol(s, &end, 10);
            if (end == s)
                break;
            if (k >= 0 && k < MAXOPS)
                keep[k] = 1;
            s = *end == ',' ? end + 1 : end;
        }
    }
    gen(seed, n);
    char cd[4200], w[4300], r[4400];
    snprintf(cd, sizeof cd, "%s/c%d", base, c);
    snprintf(w, sizeof w, "%s/w", cd);
    snprintf(r, sizeof r, "%s/r", w);
    rmrf(cd);
    if (mkdir(cd, 0755) || mkdir(w, 0755) || mkdir(r, 0755) || chmod(w, 0755) || chmod(r, 0755)) {
        perror(r);
        return 1;
    }
    say("case %s", arg);
    flush();
    pid_t pid = fork();
    if (pid == 0)
        child(w, n, kp);
    int st = 0;
    if (pid < 0 || waitpid(pid, &st, 0) < 0)
        say("end fork %s", ename(errno));
    else if (WIFSIGNALED(st))
        say("end signal %d", WTERMSIG(st));
    else
        say("end exit %d", WEXITSTATUS(st));
    flush();
    rmrf(cd);
    return 0;
}

// CASE か FROM-TO
static int runarg(const char *base, int *c, const char *arg, int n) {
    char *end;
    uint64_t from = strtoull(arg, &end, 10);
    if (*end != '-')
        return runcase(base, (*c)++, arg, n);
    uint64_t to = strtoull(end + 1, NULL, 10);
    for (uint64_t x = from; x <= to; x++) {
        char one[32];
        snprintf(one, sizeof one, "%llu", (unsigned long long)x);
        if (runcase(base, (*c)++, one, n))
            return 1;
    }
    return 0;
}

int main(int argc, char **argv) {
    int n = 30, a = 1;
    if (a + 1 < argc && !strcmp(argv[a], "-n")) {
        n = atoi(argv[a + 1]);
        a += 2;
    }
    if (n < 1 || n > MAXOPS || argc - a < 2) {
        fprintf(stderr, "usage: sysdiff [-n OPS] DIR SEED[:I,J,...]|FROM-TO...\n");
        return 2;
    }
    char base[4096];
    if (!realpath(argv[a], base)) {
        perror(argv[a]);
        return 2;
    }
    a++;
    struct utsname u;
    if (uname(&u) == 0) {
        say("sys %s %s %s", u.sysname, u.release, u.machine);
        flush();
    }
    int c = 0;
    for (; a < argc; a++) {
        if (argv[a][0] != '@') {
            if (runarg(base, &c, argv[a], n))
                return 2;
            continue;
        }
        FILE *f = fopen(argv[a] + 1, "r");
        if (!f) {
            perror(argv[a] + 1);
            return 2;
        }
        char tok[1024];
        while (fscanf(f, "%1023s", tok) == 1)
            if (runarg(base, &c, tok, n))
                return 2;
        fclose(f);
    }
    return 0;
}

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
//   sysdiff -l   aios がえらんだちがい (chosen) の一覧: chosen NAME わけ
//
// aios がえらんだちがい: Linux に合わせるのは最初の手本で、Linux よりよい道をえらんだところは chosen に名前と
// わけを書く (doc/aios.md の「aios がえらんだちがい」)。Linux で動かすときは、そこだけ aios のふるまいをまねて
// 答えにする (手本は、Linux の答えに aios がえらんだものを足したもの)。それを通った op の行の終わりには
// [chosen NAME] をつける (どちらでも同じ条件で)。aish の sysdiff は、それをまちがいのちがいと分けて数える
//
// 出力はアーキテクチャによらない: フラグは名前で、stat は種類、許可、リンクの数、大きさ (ファイルとリンク
// だけ)、自分のものか。ino や時刻は出さない (utimensat が入れる決まった時刻だけ、t で出す)
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
#include <sys/vfs.h>
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
#ifndef SYS_statx
#if defined(__x86_64__)
#define SYS_statx 332
#else
#define SYS_statx 291
#endif
#endif
#ifndef SYS_copy_file_range
#if defined(__x86_64__)
#define SYS_copy_file_range 326
#else
#define SYS_copy_file_range 285
#endif
#endif
#define FL_KEEP_SIZE 0x01
#define FL_PUNCH_HOLE 0x02
#define FL_ZERO_RANGE 0x10
#define K_TMPFS_MAGIC 0x01021994
#define K_AT_STATX_FORCE_SYNC 0x2000
#define K_AT_STATX_DONT_SYNC 0x4000
#define K_STATX_BASIC_STATS 0x7ffU
#define K_STATX__RESERVED 0x80000000U
// struct statx (libc によってはないので、カーネルの形を自分で)
struct kstatx {
    uint32_t mask, blksize;
    uint64_t attributes;
    uint32_t nlink, uid, gid;
    uint16_t mode, pad1;
    uint64_t ino, size, blocks, attributes_mask;
    struct {
        int64_t sec;
        uint32_t nsec;
        int32_t pad;
    } atime, btime, ctime, mtime;
    uint32_t rdev_major, rdev_minor, dev_major, dev_minor;
    uint64_t spare[14];
};
// utimensat が入れる時刻 (stat はこれだけ t で出す。ほかの時刻はくらべない)
#define T1 1000000000LL
#define T2 1500000000LL
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

// aios がえらんだちがい (上の説明と doc/aios.md)。ここに足したら、exec で Linux のときにまねて [chosen NAME] をつける
static const struct {
    const char *name, *why;
} chosen[] = {
    {"pwrite-append", "O_APPEND で開いた fd でも pwrite64 は off に書く (POSIX)。Linux は off を見ずに終わりに書く (pwrite(2) の BUGS)"},
    {"seek-unwritten", "SEEK_DATA / SEEK_HOLE は、fallocate でとってまだ書いていないところ (未初期化の extent) をいつも穴とする。Linux の ext4 は、そこがページキャッシュにあれば (読んだあと) データとする"},
    {"utimensat-omit", "utimensat の時刻がどちらも UTIME_OMIT でも、フラグと fd とパスはしらべる (ないファイルは ENOENT。POSIX)。Linux はパスも見ずに 0"},
    {"copy-append", "O_APPEND で開いたファイルへも copy_file_range と sendfile で写せる (位置をわたせばそこへ、なければ終わりに足す)。Linux は EBADF と EINVAL"},
    {"zero-range-tmpfs", "tmpfs でも fallocate の ZERO_RANGE ができる (ext4 と同じ)。Linux の tmpfs は EOPNOTSUPP"},
    {"sendfile-self", "同じファイルの中の sendfile は copy_file_range と同じ: 読みはじめたときの終わりまでを写し、重なれば EINVAL。Linux は重なっても写し、同じ回に書いたものが読みにまざる"},
};

// seek-unwritten: この case で、未初期化のブロックをとる fallocate (mode 0、KEEP_SIZE、ZERO_RANGE) がうまくいったか
static int fallocated;

// aios の中で動いているか (uname の sysname)。でなければ chosen のふるまいをまねる
static int on_aios;

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
    if (st->st_mtime == T1 || st->st_mtime == T2)
        say(" t%lld", (long long)st->st_mtime);
}

// statx の答え: sayst と同じ形に、基本の印 (STATX_BASIC_STATS がみなあるか)
static void saystx(const struct kstatx *x) {
    struct stat st;
    memset(&st, 0, sizeof st);
    st.st_mode = x->mode;
    st.st_nlink = x->nlink;
    st.st_size = (off_t)x->size;
    st.st_uid = x->uid;
    st.st_gid = x->gid;
    st.st_mtime = (time_t)x->mtime.sec;
    sayst(&st);
    if ((x->mask & K_STATX_BASIC_STATS) == K_STATX_BASIC_STATS)
        say(" basic");
    else
        say(" mask%x", x->mask & K_STATX_BASIC_STATS);
}

// ---- 選ぶもの (どれも、値ではなく何番めかを選んで、名前で出す) ----

enum {
    OPEN, OPENAT2, CLOSE, WRITE, PWRITE, READ, PREAD, LSEEK, FTRUNCATE, TRUNCATE, FSTAT, STAT, MKDIR, UNLINK, RMDIR,
    RENAME, LINK, SYMLINK, READLINK, CHMOD, FCHMOD, ACCESS, GETDENTS, DUP, DUP3, FCNTL, PIPE, FSYNC,
    CHOWN, FCHOWN, UTIMENS, UMASK, FALLOC, COPYRANGE, SENDFILE, STATX, NKIND
};
static const unsigned weight[NKIND] = {14, 2, 5, 8, 4, 6, 3, 4, 3, 2, 4, 7, 6, 4, 3, 7, 4, 5, 3, 3, 2, 3, 4, 2, 2, 4, 2, 1,
                                       3, 2, 4, 1, 3, 3, 2, 3};

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
// fchownat / fchown: uid と gid を -1、自分、root (0) から
static const char *idn[] = {"-1", "me", "0"};

static long idval(int k, int gid) {
    return k == 0 ? -1 : k == 1 ? (long)(gid ? getegid() : geteuid()) : 0;
}
// utimensat の時刻 (atime, mtime)。ns が UTIME_NOW / UTIME_OMIT でなければ秒。BADNS は ns がまちがったもの
#define BADNS 1000000000L
static const long long utv[][4] = {
    {-1, 0, 0, 0},                          // NULL (いま)
    {0, UTIME_NOW, 0, UTIME_NOW},           // {NOW, NOW}
    {0, UTIME_OMIT, 0, UTIME_OMIT},         // {OMIT, OMIT} (Linux はパスも見ずに 0)
    {0, UTIME_OMIT, T1, 0},                 // {OMIT, T1}
    {T2, 0, 0, UTIME_NOW},                  // {T2, NOW}
    {T1, 0, T2, 0},                         // {T1, T2}
    {0, UTIME_NOW, 0, UTIME_OMIT},          // {NOW, OMIT}
    {T1, BADNS, T1, 0},                     // ns がまちがい (EINVAL)
};
static const char *utn[] = {"NULL", "{NOW, NOW}", "{OMIT, OMIT}", "{OMIT, 1000000000}", "{1500000000, NOW}", "{1000000000, 1500000000}", "{NOW, OMIT}", "{1000000000.1000000000, 1000000000}"};
static const unsigned utw[] = {18, 14, 10, 14, 14, 14, 10, 6};
static const int umv[] = {022, 077, 0, 027, 0777};
// fallocate の mode
static const int fmv[] = {0, FL_KEEP_SIZE, FL_PUNCH_HOLE | FL_KEEP_SIZE, FL_PUNCH_HOLE, FL_ZERO_RANGE, FL_ZERO_RANGE | FL_KEEP_SIZE, 0x40000000};
static const char *fmn[] = {"0", "KEEP_SIZE", "PUNCH_HOLE|KEEP_SIZE", "PUNCH_HOLE", "ZERO_RANGE", "ZERO_RANGE|KEEP_SIZE", "0x40000000"};
static const unsigned fmw[] = {25, 15, 25, 5, 15, 10, 5};
static const long long foffs[] = {0, 1, 4096, 5000, -1};
static const long long flens[] = {0, 1, 100, 4096, 10000, 1 << 20};
// copy_file_range と sendfile の位置と長さ
static const long long coffs[] = {0, 3, 4096, 100000, -1};
static const long long clens[] = {0, 1, 7, 100, 5000, 1 << 20};
// statx の flags と mask
static const int sxf[] = {0, AT_SYMLINK_NOFOLLOW, K_AT_STATX_FORCE_SYNC, K_AT_STATX_DONT_SYNC, K_AT_STATX_FORCE_SYNC | K_AT_STATX_DONT_SYNC, 0x8000};
static const char *sxfn[] = {"0", "AT_SYMLINK_NOFOLLOW", "AT_STATX_FORCE_SYNC", "AT_STATX_DONT_SYNC", "AT_STATX_FORCE_SYNC|AT_STATX_DONT_SYNC", "0x8000"};
static const unsigned sxfw[] = {50, 20, 10, 10, 5, 5};
static const unsigned sxm[] = {K_STATX_BASIC_STATS, 3, 0, K_STATX__RESERVED};
static const char *sxmn[] = {"STATX_BASIC_STATS", "STATX_TYPE|STATX_MODE", "0", "STATX__RESERVED"};
static const unsigned sxmw[] = {60, 20, 15, 5};
// getdents64 のバッファ (小さいものは何度も読んで、続きから読めるか)
static const long long dbufs[] = {8192, 64, 32, 24};
static const unsigned dbufw[] = {70, 12, 12, 6};

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
    fallocated = 0;
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
            o->kind != RENAME && o->kind != LINK && o->kind != SYMLINK && o->kind != READLINK && o->kind != CHMOD && o->kind != ACCESS &&
            o->kind != CHOWN && o->kind != UTIMENS && o->kind != UMASK && o->kind != STATX)
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
            o->fd = genfd(&pipe);
            break;
        case GETDENTS:
            o->fd = genfd(&pipe);
            o->len = dbufs[pickw(dbufw, 4)];
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
            // linkat の AT_EMPTY_PATH: fd そのもの (O_TMPFILE のファイルに名前をつける)
            if (o->kind == LINK && chance(15)) {
                o->d1 = genfd(&pipe);
                o->x = 2;
            } else {
                o->d1 = gendir();
                genpath(o->p1, 0);
                o->x = o->kind == RENAME ? (int)pickw(rnw, 4) : chance(20);
            }
            o->d2 = gendir();
            genpath(o->p2, 0);
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
        case CHOWN:
            // AT_EMPTY_PATH なら fd そのもの
            if (chance(8)) {
                o->d1 = genfd(&pipe);
                o->y = 2;
            } else {
                o->d1 = gendir();
                genpath(o->p1, 0);
                o->y = chance(20);
            }
            o->x = (int)pick(3) | (int)pick(3) << 2;
            break;
        case FCHOWN:
            o->fd = genfd(&pipe);
            o->x = (int)pick(3) | (int)pick(3) << 2;
            break;
        case UTIMENS:
            // y: 0 パス、1 パスで AT_SYMLINK_NOFOLLOW、2 fd と "" で AT_EMPTY_PATH、3 fd と NULL (futimens)
            if (chance(12)) {
                o->d1 = genfd(&pipe);
                o->y = chance(50) ? 3 : 2;
            } else {
                o->d1 = gendir();
                genpath(o->p1, 0);
                o->y = chance(20);
            }
            o->x = (int)pickw(utw, 8);
            break;
        case UMASK:
            o->x = (int)pick(5);
            break;
        case FALLOC:
            o->fd = genfd(&pipe);
            o->x = (int)pickw(fmw, 7);
            o->off = foffs[pick(5)];
            o->len = flens[pick(6)];
            break;
        case COPYRANGE:
        case SENDFILE:
            // fd が読むほう、d2 が書くほう。x: 1 読む位置をわたす、2 書く位置をわたす (sendfile は 1 だけ)
            o->fd = genfd(&pipe);
            o->d2 = genfd(&pipe);
            o->x = (int)pick(o->kind == SENDFILE ? 2 : 4);
            o->off = coffs[pick(5)];
            o->y = (int)pick(5);
            o->len = clens[pick(6)];
            o->mode = o->kind == COPYRANGE && chance(5);
            break;
        case STATX:
            if (chance(8)) {
                o->d1 = genfd(&pipe);
                o->y = -1;
            } else {
                o->d1 = gendir();
                genpath(o->p1, 0);
                o->y = (int)pickw(sxfw, 6);
            }
            o->x = (int)pickw(sxmw, 4);
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

// ref の fd。fb なら、それを作った op がしくじっていたとき、いま開いているいちばん新しい fd (avoid でないもの)
static int fdref(int ref, int fb, int avoid) {
    int fd = fdof(ref);
    if (fd < 0 && ref >= 0 && fb) {
        int best = 0;
        for (int f = 0; f < 1024; f++)
            if (alive[f] > best && f != avoid) {
                best = alive[f];
                fd = f;
            }
    }
    return fd;
}

static int fdarg(const struct op *o) {
    return fdref(o->fd, o->fb, -1);
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

// getdents64 を終わりまで (バッファは size バイト)。名前は並べなおす (並びはファイルシステムしだい)
static void saydents(int fd, long long size) {
    static char buf[8192];
    char *v[256];
    int n = 0;
    long r;
    while ((r = syscall(SYS_getdents64, fd, buf, (size_t)size)) > 0) {
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
            // chosen pwrite-append: O_APPEND の fd でも off に書く。Linux では O_APPEND をはずして書き、もどす
            int fl = (int)syscall(SYS_fcntl, fd, F_GETFL);
            int app = fl >= 0 && (fl & O_APPEND);
            if (app && !on_aios)
                syscall(SYS_fcntl, fd, F_SETFL, fl & ~O_APPEND);
            r = syscall(SYS_pwrite64, fd, wbuf, (size_t)o->len, (off_t)o->off);
            int e = errno;
            if (app && !on_aios)
                syscall(SYS_fcntl, fd, F_SETFL, fl);
            errno = e;
            ret(r);
            if (app && r > 0)
                say(" [chosen pwrite-append]");
            break;
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
    case LSEEK: {
        say("lseek(%d, %lld, %s)", fd, o->off, whn[o->x]);
        // chosen seek-unwritten: fallocate でとってまだ書いていないところは、いつも穴。Linux の ext4 は、そこが
        // ページキャッシュにあれば (読んだあと) データとするので、Linux ではキャッシュから追い出してから見る
        int sk = fallocated && (whv[o->x] == SEEK_DATA || whv[o->x] == SEEK_HOLE);
        if (sk && !on_aios)
            posix_fadvise(fd, 0, 0, POSIX_FADV_DONTNEED);
        ret(syscall(SYS_lseek, fd, (off_t)o->off, whv[o->x]));
        if (sk)
            say(" [chosen seek-unwritten]");
        break;
    }
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
            static const int lfv[] = {0, AT_SYMLINK_FOLLOW, AT_EMPTY_PATH};
            static const char *lfn[] = {"0", "AT_SYMLINK_FOLLOW", "AT_EMPTY_PATH"};
            say(", %s)", lfn[o->x]);
            r = syscall(SYS_linkat, fdof(o->d1), o->p1, fdof(o->d2), o->p2, lfv[o->x]);
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
        if (o->len == 8192)
            say("getdents64(%d)", fd);
        else
            say("getdents64(%d, %lld)", fd, o->len);
        saydents(fd, o->len);
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
    case CHOWN: {
        static const int cfv[] = {0, AT_SYMLINK_NOFOLLOW, AT_EMPTY_PATH};
        static const char *cfn[] = {"0", "AT_SYMLINK_NOFOLLOW", "AT_EMPTY_PATH"};
        say("fchownat(");
        saydir(o->d1);
        say(", ");
        sayq(o->p1);
        say(", %s, %s, %s)", idn[o->x & 3], idn[o->x >> 2], cfn[o->y]);
        ret(syscall(SYS_fchownat, fdof(o->d1), o->p1, idval(o->x & 3, 0), idval(o->x >> 2, 1), cfv[o->y]));
        break;
    }
    case FCHOWN:
        say("fchown(%d, %s, %s)", fd, idn[o->x & 3], idn[o->x >> 2]);
        ret(syscall(SYS_fchown, fd, idval(o->x & 3, 0), idval(o->x >> 2, 1)));
        break;
    case UTIMENS: {
        static const int ufv[] = {0, AT_SYMLINK_NOFOLLOW, AT_EMPTY_PATH, 0};
        static const char *ufn[] = {"0", "AT_SYMLINK_NOFOLLOW", "AT_EMPTY_PATH", "0"};
        struct timespec ts[2] = {{(time_t)utv[o->x][0], (long)utv[o->x][1]}, {(time_t)utv[o->x][2], (long)utv[o->x][3]}};
        say("utimensat(");
        saydir(o->d1);
        say(", ");
        if (o->y == 3)
            say("NULL");
        else
            sayq(o->p1);
        say(", %s, %s)", utn[o->x], ufn[o->y]);
        // chosen utimensat-omit: どちらも OMIT でも、fd とパスはしらべて、変えずに 0。Linux はパスも見ずに 0 なので、
        // Linux では同じたどりかたで答えを出す: パスは fstatat (同じフラグ)、"" と AT_EMPTY_PATH は開いている fd か、
        // NULL (futimens) は開いている O_PATH でない fd か
        int omit = utv[o->x][1] == UTIME_OMIT && utv[o->x][3] == UTIME_OMIT;
        if (omit && !on_aios) {
            long long sb[32];
            int f = fdof(o->d1);
            if (o->y == 3) {
                int fl = (int)syscall(SYS_fcntl, f, F_GETFL);
                r = fl < 0 ? -1 : (fl & O_PATH) ? (errno = EBADF, -1) : 0;
            } else if (o->y == 2)
                r = syscall(SYS_fcntl, f, F_GETFD) < 0 ? -1 : 0;
            else
                r = syscall(SYS_newfstatat, f, o->p1, sb, ufv[o->y]);
        } else
            r = syscall(SYS_utimensat, fdof(o->d1), o->y == 3 ? NULL : o->p1, utv[o->x][0] < 0 ? NULL : ts, ufv[o->y]);
        ret(r);
        if (omit)
            say(" [chosen utimensat-omit]");
        break;
    }
    case UMASK:
        say("umask(%04o) = %04o", umv[o->x], (unsigned)syscall(SYS_umask, umv[o->x]));
        break;
    case FALLOC: {
        say("fallocate(%d, %s, %lld, %lld)", fd, fmn[o->x], o->off, o->len);
        int m = fmv[o->x];
        // chosen zero-range-tmpfs: tmpfs のファイルでも ZERO_RANGE ができる。Linux の tmpfs は EOPNOTSUPP なので、
        // Linux ではそのとき、穴をあけて (PUNCH_HOLE|KEEP_SIZE) から場所をとる (mode 0 か KEEP_SIZE)
        struct stat st;
        struct statfs sf;
        int zt = (m & FL_ZERO_RANGE) && fstat(fd, &st) == 0 && S_ISREG(st.st_mode) && fstatfs(fd, &sf) == 0 && sf.f_type == K_TMPFS_MAGIC;
        r = syscall(SYS_fallocate, fd, m, (off_t)o->off, (off_t)o->len);
        if (zt && !on_aios && r < 0 && errno == EOPNOTSUPP) {
            r = syscall(SYS_fallocate, fd, FL_PUNCH_HOLE | FL_KEEP_SIZE, (off_t)o->off, (off_t)o->len);
            if (r == 0)
                r = syscall(SYS_fallocate, fd, m & FL_KEEP_SIZE, (off_t)o->off, (off_t)o->len);
        }
        ret(r);
        if (zt)
            say(" [chosen zero-range-tmpfs]");
        // 穴をあけるほかは、未初期化のブロックをとる
        if (r == 0 && !(m & FL_PUNCH_HOLE))
            fallocated = 1;
        break;
    }
    case COPYRANGE:
    case SENDFILE: {
        long long in = o->off, out = coffs[o->y];
        // 書くほうも、作った op がしくじっていたら開いているもの (読むほうとはちがう fd)
        int to = fdref(o->d2, o->fb, fd);
        // chosen copy-append: O_APPEND で開いたファイルへも写す (位置をわたせばそこへ、なければ終わりに足す)。Linux
        // では O_APPEND をはずし、書く位置がなければ終わりで写して、書くほうの位置を終わり + 写した分にしてもどす
        int fl = (int)syscall(SYS_fcntl, to, F_GETFL);
        struct stat ost;
        int app = fl >= 0 && (fl & O_APPEND) && fstat(to, &ost) == 0 && S_ISREG(ost.st_mode);
        int im = app && !on_aios, e = 0;
        // chosen sendfile-self: sendfile で、読むほうと書くほうが同じふつうのファイル
        struct stat ist, xst;
        int self = o->kind == SENDFILE && fstat(fd, &ist) == 0 && S_ISREG(ist.st_mode) && fstat(to, &xst) == 0 && ist.st_dev == xst.st_dev &&
                   ist.st_ino == xst.st_ino;
        if (im)
            syscall(SYS_fcntl, to, F_SETFL, fl & ~O_APPEND);
        if (o->kind == COPYRANGE) {
            say("copy_file_range(%d, ", fd);
            if (o->x & 1)
                say("%lld", in);
            else
                say("NULL");
            say(", %d, ", to);
            if (o->x & 2)
                say("%lld", out);
            else
                say("NULL");
            say(", %lld, %u)", o->len, o->mode);
            long long end = im ? (long long)ost.st_size : 0;
            r = syscall(SYS_copy_file_range, fd, (o->x & 1) ? &in : NULL, to, (o->x & 2) ? &out : im ? &end : NULL, (size_t)o->len, o->mode);
            e = errno;
            if (im && r > 0 && !(o->x & 2))
                syscall(SYS_lseek, to, (off_t)end, SEEK_SET);
        } else {
            say("sendfile(%d, %d, ", to, fd);
            if (o->x & 1)
                say("%lld", in);
            else
                say("NULL");
            say(", %lld)", o->len);
            // chosen sendfile-self: 同じファイルの中は copy_file_range と同じ (読みはじめたときの終わりまでを写し、重なれば
            // EINVAL)。Linux では、長さ 0 の sendfile でほかのしらべをしてから、長さを終わりまでに縮め、重なりを見て写す。
            // Linux で copy-append: sendfile は書く位置をわたせないので、書くほうの位置を終わりにし、読む位置はわたして
            // 写す (同じ開いたファイルなら位置もひとつ)。読むほうに位置がなければ (パイプ)、O_APPEND によらず EINVAL
            int sim = im || (self && !on_aios);
            long long p = !sim ? 0 : (o->x & 1) ? in : (long long)syscall(SYS_lseek, fd, 0, SEEK_CUR);
            if (sim && p >= 0) {
                long long back = syscall(SYS_lseek, to, 0, SEEK_CUR);
                if (im)
                    syscall(SYS_lseek, to, (off_t)ost.st_size, SEEK_SET);
                if (self) {
                    long long q = p;
                    r = syscall(SYS_sendfile, to, fd, &q, (size_t)0);
                    if (r == 0) {
                        long long at = syscall(SYS_lseek, to, 0, SEEK_CUR), size = ist.st_size;
                        long long n = p >= size ? 0 : o->len < size - p ? o->len : size - p;
                        if (n > 0 && at < p + n && p < at + n) {
                            r = -1;
                            errno = EINVAL;
                        } else
                            r = syscall(SYS_sendfile, to, fd, &p, (size_t)n);
                    }
                } else
                    r = syscall(SYS_sendfile, to, fd, &p, (size_t)o->len);
                e = errno;
                if (r <= 0)
                    syscall(SYS_lseek, to, (off_t)back, SEEK_SET);
                else if (o->x & 1)
                    in = p;
                else
                    syscall(SYS_lseek, fd, (off_t)p, SEEK_SET);
            } else {
                r = syscall(SYS_sendfile, to, fd, (o->x & 1) ? &in : NULL, (size_t)o->len);
                e = errno;
            }
        }
        if (im)
            syscall(SYS_fcntl, to, F_SETFL, fl);
        errno = e;
        ret(r);
        if (r >= 0 && (o->x & 1))
            say(" in %lld", in);
        if (r >= 0 && (o->x & 2))
            say(" out %lld", out);
        if (app)
            say(" [chosen copy-append]");
        if (self)
            say(" [chosen sendfile-self]");
        break;
    }
    case STATX: {
        struct kstatx x;
        memset(&x, 0, sizeof x);
        int fl = o->y < 0 ? AT_EMPTY_PATH : sxf[o->y];
        say("statx(");
        saydir(o->d1);
        say(", ");
        sayq(o->p1);
        say(", %s, %s)", o->y < 0 ? "AT_EMPTY_PATH" : sxfn[o->y], sxmn[o->x]);
        r = syscall(SYS_statx, fdof(o->d1), o->p1, fl, sxm[o->x], &x);
        ret(r);
        if (r == 0)
            saystx(&x);
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
    if (argc == 2 && !strcmp(argv[1], "-l")) {
        for (size_t i = 0; i < sizeof chosen / sizeof *chosen; i++)
            printf("chosen %s %s\n", chosen[i].name, chosen[i].why);
        return 0;
    }
    if (a + 1 < argc && !strcmp(argv[a], "-n")) {
        n = atoi(argv[a + 1]);
        a += 2;
    }
    if (n < 1 || n > MAXOPS || argc - a < 2) {
        fprintf(stderr, "usage: sysdiff [-n OPS] DIR SEED[:I,J,...]|FROM-TO|@FILE...\n       sysdiff -l\n");
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
        on_aios = !strcmp(u.sysname, "aios");
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

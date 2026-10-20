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
//                と、手本の Linux のまちがい (linux_bugs) の一覧: linuxbug NAME わけ
//
// aios がえらんだちがい: Linux に合わせるのは最初の手本で、Linux よりよい道をえらんだところは chosen に名前と
// わけを書く (doc/aios.md の「aios がえらんだちがい」)。Linux で動かすときは、そこだけ aios のふるまいをまねて
// 答えにする (手本は、Linux の答えに aios がえらんだものを足したもの)。それを通った op の行の終わりには
// [chosen NAME] をつける (どちらでも同じ条件で)。aish の sysdiff は、それをまちがいのちがいと分けて数える。
// 手本の Linux のまちがい (linux_bugs) も同じしくみ: Linux の側でただして答えにし、[linuxbug NAME] をつける
// (Linux が直ったら消す。doc/aios.md の「手本の Linux のまちがい」)
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
#include <limits.h>
#include <setjmp.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/file.h>
#include <poll.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/resource.h>
#include <sys/types.h>
#include <sys/uio.h>
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
#ifndef SYS_preadv2
#if defined(__x86_64__)
#define SYS_preadv2 327
#define SYS_pwritev2 328
#else
#define SYS_preadv2 286
#define SYS_pwritev2 287
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
    case ESRCH: return "ESRCH";
    case EINTR: return "EINTR";
    case ECHILD: return "ECHILD";
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
    {"splice-snapshot", "ファイルからパイプへの splice / sendfile は、そのときの中身をパイプに写す。あとでファイルを書いたり切りつめたりしても、パイプの中身は変わらない。Linux はページキャッシュのページそのものを入れるので、読む前に変わればそれが読める"},
    {"pipe-fault", "パイプの readv / writev で途中の入れ物にさわれないとき、さわれる分をみな読み書きしてその数を返す (なにもできなければ EFAULT)。Linux はパイプの箱 (4 KiB) ごとに写し、途中でさわれなくなった箱は捨てる (7 バイトの writev は EFAULT、12000 バイトは 4096。readv は写した分も数えず箱に残す)"},
    {"pipe-nowait", "パイプの preadv2 / pwritev2 の RWF_NOWAIT は、そのパイプを splice や tee に使ったあとも効く (待たない)。Linux は splice に使った (しくじっても) パイプの FMODE_NOWAIT を消すので、それからは EOPNOTSUPP"},
};

// 手本の Linux のまちがい (直るまで、Linux の側でただして答えにする。見つけた版と、どう直るべきかも)
static const struct {
    const char *name, *why;
} linux_bugs[] = {
    {"zero-range-eof", "ext4 の fallocate ZERO_RANGE で、範囲がファイルの終わりをこえ、終わりのブロックがもうディスクにあると、そのブロックの中 (ファイルの終わりまで) が 0 にならない (7.2.11 と 7.3: ext4_alloc_file_blocks の ext4_block_zero_eof が、落としたばかりのページを読みもどす。6.18 は 0 になる)"},
    {"efault-extend", "ファイルの終わりより先への書きこみがさわれない入れ物で EFAULT になっても、ファイルがその位置まで伸びる (ext4 も tmpfs も。6.18、7.2.11、7.3: generic_perform_write が write_begin のあとに写し、0 バイトでも write_end (ext4_da_do_write_end、shmem_write_end) が i_size を pos にする)。なにも書けなかったのだから伸びないのが正しい"},
    {"tmpfs-extend", "tmpfs で、写し (mmap) でファイルの終わりの先 (同じページの中) に書いたものが、ファイルを伸ばすと (ftruncate、fallocate、終わりより先への write) そのまま中身になる (7.2.11 と 7.3: shmem_setattr と shmem_write_end はページの終わりの先を 0 にしない。ext4 は pagecache_isize_extended で 0 にする)。POSIX は、伸ばしたところは 0 と読めると決めている"},
};

// seek-unwritten: この case で、未初期化のブロックをとる fallocate (mode 0、KEEP_SIZE、ZERO_RANGE) がうまくいったか
static int fallocated;

// aios の中で動いているか (uname の sysname)。でなければ chosen のふるまいをまねる
static int on_aios;
// -r: 手本の Linux のまちがいをたださない (Linux がもう直ったかを見る: -r ありとなしの答えをくらべる)
static int raw;

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
    CHOWN, FCHOWN, UTIMENS, UMASK, FALLOC, COPYRANGE, SENDFILE, STATX, SPLICE, TEE, READV, WRITEV, PREADV2, PWRITEV2, MMAP, MUNMAP, MSYNC, MREAD, MWRITE, FLOCK, RLOCK, FORK, KILL, WAIT, CHLDACT, SETPGID, GETPGID, NKIND};
static const unsigned weight[NKIND] = {14, 2, 5, 8, 4, 6, 3, 4, 3, 2, 4, 7, 6, 4, 3, 7, 4, 5, 3, 3, 2, 3, 4, 2, 2, 4, 4, 1,
                                       3, 2, 4, 1, 3, 3, 2, 3, 4, 2, 3, 3, 3, 3, 4, 1, 2, 4, 4, 5, 8, 4, 5, 5, 1, 2, 2};

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
// readv / writev / preadv2 / pwritev2 の iovec の形 (y): 全部で len バイトを、1 つ、2 つ、3 つ (まん中は長さ 0)、4 つ
// (はじめが長さ 0)、0 こ、1025 こ (EINVAL)、2 つめがさわれない (bad: EFAULT。1 つめの分だけ)
static const char *const shapes[] = {"1", "2", "3z", "z4", "0", "big", "bad"};
#define NSHAPE 7
static const unsigned shapew[NSHAPE] = {6, 5, 3, 2, 1, 1, 2};
// preadv2 / pwritev2 の flags (RWF_*。bad は知らないビット)
static const struct {
    unsigned v;
    const char *n;
} rwfs[] = {
    {0, "0"}, {1, "HIPRI"}, {2, "DSYNC"}, {4, "SYNC"}, {8, "NOWAIT"}, {0x10, "APPEND"}, {0x20, "NOAPPEND"}, {0x40, "ATOMIC"},
    {0x80, "DONTCACHE"}, {0x100, "NOSIGNAL"}, {0x200, "bad"}, {0x30, "APPEND|NOAPPEND"}, {0x0a, "DSYNC|NOWAIT"}, {0x88, "NOWAIT|DONTCACHE"},
};
#define NRWF 14
static const unsigned rwfw[NRWF] = {8, 2, 3, 2, 8, 6, 4, 2, 4, 2, 2, 2, 2, 2};
// mmap: 長さ、ファイルの位置 (3 はそろっていない: EINVAL)、prot、msync の flags。mread / mwrite の写しの中の位置
// (-1 はいちばん終わりのバイト) と長さ (写しの終わりで切る)
static const long long mlens[] = {0, 1, 4096, 8192, 12000, 100000};
static const unsigned mlensw[6] = {1, 2, 4, 4, 3, 3};
static const long long moffs[] = {0, 4096, 8192, 3};
static const unsigned moffsw[4] = {7, 2, 1, 1};
static const int mprotv[] = {PROT_READ, PROT_READ | PROT_WRITE, PROT_NONE, PROT_WRITE};
static const char *const mprotn[] = {"PROT_READ", "PROT_READ|PROT_WRITE", "PROT_NONE", "PROT_WRITE"};
static const unsigned mprotw[4] = {5, 6, 1, 2};
static const int msv[] = {MS_ASYNC, MS_SYNC, MS_INVALIDATE, MS_SYNC | MS_ASYNC, 0};
static const char *const msn[] = {"MS_ASYNC", "MS_SYNC", "MS_INVALIDATE", "MS_SYNC|MS_ASYNC", "0"};
static const long long maoffs[] = {0, 3, 4096, 4097, 8191, -1};
static const long long malens[] = {1, 10, 100, 4096};
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
// flock(2): LOCK_SH / LOCK_EX / LOCK_UN (いつも LOCK_NB: 1 つのプロセスの中では、待つと止まる) と、だめな op
static const int flockv[] = {LOCK_SH, LOCK_EX, LOCK_UN, LOCK_SH | LOCK_EX, 0};
static const char *const flockn[] = {"LOCK_SH|LOCK_NB", "LOCK_EX|LOCK_NB", "LOCK_UN|LOCK_NB", "LOCK_SH|LOCK_EX|LOCK_NB", "LOCK_NB"};
static const unsigned flockw[5] = {5, 5, 3, 1, 1};
// fcntl のレコードロック (POSIX と OFD。待つ F_SETLKW は 1 つのプロセスの中では止まるので使わない): cmd、種類、
// whence、始まりと長さ (負の長さは始まりの手前)、l_pid (OFD では 0 でなければ EINVAL)
#ifndef F_OFD_GETLK
#define F_OFD_GETLK 36
#define F_OFD_SETLK 37
#endif
static const int rlcmdv[] = {F_SETLK, F_GETLK, F_OFD_SETLK, F_OFD_GETLK};
static const char *const rlcmdn[] = {"F_SETLK", "F_GETLK", "F_OFD_SETLK", "F_OFD_GETLK"};
static const unsigned rlcmdw[4] = {5, 3, 5, 3};
static const int rltv[] = {F_RDLCK, F_WRLCK, F_UNLCK, 3};
static const char *const rltn[] = {"F_RDLCK", "F_WRLCK", "F_UNLCK", "3"};
static const unsigned rltw[4] = {5, 5, 3, 1};
static const int rlwv[] = {SEEK_SET, SEEK_CUR, SEEK_END, 7};
static const char *const rlwn[] = {"SEEK_SET", "SEEK_CUR", "SEEK_END", "7"};
static const unsigned rlww[4] = {8, 2, 2, 1};
// プロセス: 子のふるまい (すぐ終わる、自分にシグナルを送って死ぬ、止まる、待つ)、終わりの番号、送るシグナル、
// 待つときのシグナルの扱い (既定、無視、ハンドラ、ブロック)、グループ (そのまま、setpgid(0, 0)、setsid())
static const char *const kbehn[] = {"exit", "raise", "stop", "pause"};
static const unsigned kbehw[4] = {2, 2, 2, 5};
static const int kcodes[] = {0, 1, 3, 255};
static const int ksigs[] = {0, SIGTERM, SIGKILL, SIGSTOP, SIGCONT, SIGUSR1, SIGINT, SIGTSTP, SIGCHLD, SIGHUP, SIGABRT, 65};
static const unsigned ksigw[12] = {2, 5, 3, 5, 7, 4, 3, 4, 1, 2, 1, 1};
// 待つ子が扱いを決めるシグナル (2 ビットずつ: 0 既定、1 無視、2 ハンドラ、3 ブロック)
static const int kdisp[] = {SIGTERM, SIGUSR1, SIGINT, SIGTSTP};
static const char *const kdispn[] = {"default", "ignore", "handle", "block"};
static const char *const kgrpn[] = {"", ", setpgid", ", setsid"};
// wait4 の options (いつも WNOHANG) と waitid の options (いつも WNOHANG)
static const int w4opts[] = {0, WUNTRACED, WCONTINUED, WUNTRACED | WCONTINUED, 0x10, __WALL};
static const char *const w4optn[] = {"WNOHANG", "WNOHANG|WUNTRACED", "WNOHANG|WCONTINUED", "WNOHANG|WUNTRACED|WCONTINUED", "WNOHANG|0x10", "WNOHANG|__WALL"};
static const unsigned w4optw[6] = {3, 4, 4, 5, 1, 1};
static const int wiopts[] = {WEXITED, WSTOPPED, WCONTINUED, WEXITED | WSTOPPED | WCONTINUED, WEXITED | WNOWAIT, WSTOPPED | WCONTINUED | WNOWAIT, 0};
static const char *const wioptn[] = {"WNOHANG|WEXITED", "WNOHANG|WSTOPPED", "WNOHANG|WCONTINUED", "WNOHANG|WEXITED|WSTOPPED|WCONTINUED", "WNOHANG|WEXITED|WNOWAIT", "WNOHANG|WSTOPPED|WCONTINUED|WNOWAIT", "WNOHANG"};
static const unsigned wioptw[7] = {4, 3, 4, 5, 2, 3, 1};
// 待つ相手: その子、だれでも (-1 / P_ALL)、自分のグループ (0 / P_PGID 0)、その子のグループ、いない番号、waitid のだめな idtype
static const unsigned wtgtw[6] = {8, 4, 2, 2, 1, 1};
static const long long rlstarts[] = {0, 0, 0, 0, 1, 5, 100, 4096, 10000, -1};
static const long long rllens[] = {0, 0, 0, 1, 10, 100, 4096, -1, -10};
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
// splice の長さと flags (SPLICE_F_ALL のほかのビットは EINVAL)
static const long long splens[] = {0, 1, 7, 100, 4096, 5000, 65536, 1 << 20};
static const unsigned splw[] = {3, 10, 10, 15, 12, 15, 15, 20};
static const unsigned spfv[] = {0, SPLICE_F_NONBLOCK, SPLICE_F_MOVE | SPLICE_F_MORE, 0x10};
static const char *spfn[] = {"0", "SPLICE_F_NONBLOCK", "SPLICE_F_MOVE|SPLICE_F_MORE", "0x10"};
static const unsigned spfw[] = {40, 30, 25, 5};
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
    // preadv2 / pwritev2: rwfs の番号
    unsigned rwf;
    long long off, len;
    int x, y;
};

static struct op ops[MAXOPS];
static int res[MAXOPS][2];

// 作った fd の引数のもと (op の番号 * 2 + 端)、パイプか (1 読む口、2 書く口。F_SETFL でも O_NONBLOCK を消さない:
// 読み書きで止まらないように)、開けられそうか (あるはずのパスを、ふつうのフラグで。そういうものを多く使う)、
// 何で開いたか (1 読む、2 書く。O_PATH は 0)
static int nref, refs[2 * MAXOPS], refpipe[2 * MAXOPS], reflikely[2 * MAXOPS], refacc[2 * MAXOPS], refdir[2 * MAXOPS], reforig[2 * MAXOPS];
// genfd2 が選んだものの refacc と refdir (ディレクトリを開いたもの。dup はそれを引きつぐ)。lastfile は genfile が
// ファイルらしいものを選べたか
static int lastacc, lastdir, lastfile;
// genfd2 が選んだものを開いた op の番号 (dup はそれを引きつぐ。パイプは -1): 同じファイルを別に開いたもの (ロックの取り合い) を探すのに
static int lastorig;
// gensame が、同じファイルを別に開いたものを選べたか。lastlock は前のロックの op が使った fd を開いた op (取り合いになるよう、
// 次はちがう open のものを多めに)
static int lastsame, lastlock = -1;
// fork した子 (FORK の op の番号) と、その子が自分のグループを作るか (1 setpgid、2 setsid)
static int nkref, krefs[MAXOPS], kgrp[MAXOPS];

// 子の ref (FORK の op の番号)。なければ -1 (いない番号で)。近いものほど選ばれやすく
static int genkid(void) {
    if (nkref == 0 || chance(5))
        return -1;
    return krefs[chance(60) ? nkref - 1 - (int)pick(nkref < 3 ? nkref : 3) : (int)pick(nkref)];
}

// 写した領域 (mmap の op の番号) と、できていそうか (fd が読めるふつうのファイルで、長さと位置がよい)。
// exec では mres / mlen にその場所と長さ (外したら NULL)
static int nmref, mrefs[MAXOPS], mlikely[MAXOPS];

// 写しの ref。なければ -1 (できていそうなものを多めに、近いものほど選ばれやすく)
static int genmap(void) {
    if (nmref == 0)
        return -1;
    int cand[MAXOPS], n = 0;
    for (int i = 0; i < nmref; i++)
        if (mlikely[i])
            cand[n++] = mrefs[i];
    if (n == 0 || chance(15)) {
        n = 0;
        for (int i = 0; i < nmref; i++)
            cand[n++] = mrefs[i];
    }
    return cand[chance(60) ? n - 1 - (int)pick(n < 3 ? n : 3) : (int)pick(n)];
}

static void addref(int r, int pipe, int likely, int acc) {
    refs[nref] = r;
    refpipe[nref] = pipe;
    reflikely[nref] = likely;
    refacc[nref] = acc;
    refdir[nref] = lastdir;
    reforig[nref] = lastorig;
    nref++;
}

static int genfd2(int *pipe, int *likely) {
    *pipe = *likely = lastacc = lastdir = 0;
    lastorig = -1;
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
    lastacc = refacc[i];
    lastdir = refdir[i];
    lastorig = reforig[i];
    return refs[i];
}

static int genfd(int *pipe) {
    int likely;
    return genfd2(pipe, &likely);
}

// パイプの end の口 (1 読む、2 書く) の fd。なければ (と 1 割は) ほかの fd。avoid (ほかの口の ref) と同じ pipe2 の
// ものでないほうがあれば、たいていそちら (同じパイプどうしの splice / tee は EINVAL)
static int genpipe2(int end, int avoid) {
    int cand[2 * MAXOPS], n = 0, pipe;
    for (int i = 0; i < nref; i++)
        if (refpipe[i] == end && (avoid < 0 || refs[i] / 2 != avoid / 2 || chance(15)))
            cand[n++] = i;
    if (n == 0 || chance(10))
        return genfd(&pipe);
    return refs[cand[chance(60) ? n - 1 - (int)pick(n < 3 ? n : 3) : (int)pick(n)]];
}

static int genpipe(int end) {
    return genpipe2(end, -1);
}

// パイプでない、開けられそうなものを want (1 読む、2 書く) で開いた fd。なければ (と 1 割は) ほかの fd
static int genfile(int want) {
    int cand[2 * MAXOPS], n = 0, pipe;
    for (int i = 0; i < nref; i++)
        if (!refpipe[i] && reflikely[i] && (refacc[i] & want))
            cand[n++] = i;
    lastfile = 0;
    if (n == 0 || chance(10))
        return genfd(&pipe);
    int j = cand[chance(60) ? n - 1 - (int)pick(n < 3 ? n : 3) : (int)pick(n)];
    lastacc = refacc[j];
    lastdir = refdir[j];
    lastorig = reforig[j];
    lastfile = !lastdir;
    return refs[j];
}

// 同じファイル (同じパスを開いた、別の open) がほかにもある fd (ロックの取り合いが起きるところ)。want (1 読む、2 書く) で
// 開いたものを多めに。なければ genfile
static int gensame(int want) {
    int cand[2 * MAXOPS], n = 0;
    lastsame = 0;
    for (int i = 0; i < nref; i++) {
        if (refpipe[i] || !reflikely[i] || reforig[i] < 0 || refdir[i])
            continue;
        const struct op *a = &ops[reforig[i]];
        int other = 0;
        for (int j = 0; j < nref && !other; j++)
            if (j != i && !refpipe[j] && reflikely[j] && reforig[j] >= 0 && reforig[j] != reforig[i] && strcmp(ops[reforig[j]].p1, a->p1) == 0)
                other = 1;
        if (other && ((refacc[i] & want) || chance(20)))
            cand[n++] = i;
    }
    if (n == 0 || chance(10))
        return genfile(want);
    // 前のロックとちがう open のものを多めに
    int alt[2 * MAXOPS], na = 0;
    for (int k = 0; k < n; k++)
        if (reforig[cand[k]] != lastlock)
            alt[na++] = cand[k];
    if (na > 0 && chance(75)) {
        memcpy(cand, alt, na * sizeof alt[0]);
        n = na;
    }
    int j = cand[chance(60) ? n - 1 - (int)pick(n < 3 ? n : 3) : (int)pick(n)];
    lastacc = refacc[j];
    lastdir = refdir[j];
    lastorig = reforig[j];
    lastfile = 1;
    lastsame = 1;
    return refs[j];
}

// 同じファイルを別に開いたものがまだなければ、ロックの op のかわりに、開いているファイルをもう一度開く op にする
// (取り合いが起きるように)。開いているファイルがなければ 0
static int reopen_same(struct op *o, int i) {
    int cand[2 * MAXOPS], n = 0;
    for (int k = 0; k < nref; k++)
        if (!refpipe[k] && reflikely[k] && reforig[k] >= 0 && !refdir[k])
            cand[n++] = k;
    if (n == 0)
        return 0;
    int k = cand[chance(60) ? n - 1 - (int)pick(n < 3 ? n : 3) : (int)pick(n)];
    memset(o, 0, sizeof *o);
    o->kind = OPEN;
    o->d1 = o->d2 = REF_CWD;
    o->fd = REF_BAD;
    strcpy(o->p1, ops[reforig[k]].p1);
    o->acc = chance(50) ? 2 : chance(50) ? 0 : 1;
    static const int accbits[] = {1, 2, 3, 0};
    lastdir = 0;
    lastorig = i;
    addref(2 * i, 0, 1, accbits[o->acc]);
    return 1;
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
    nmref = 0;
    nkref = 0;
    lastlock = -1;
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
        // splice と tee はパイプがないとたいてい EINVAL なので、まだなければたいていパイプを作る
        if ((o->kind == SPLICE || o->kind == TEE) && chance(70)) {
            int any = 0;
            for (int k = 0; k < nref; k++)
                any |= refpipe[k];
            if (!any)
                o->kind = PIPE;
        }
        // 子を使う op は、まだ子がいなければたいてい fork に
        if ((o->kind == KILL || o->kind == WAIT || o->kind == SETPGID || o->kind == GETPGID) && nkref == 0 && chance(80))
            o->kind = FORK;
        // 写しを使う op は、まだ写しがなければたいてい mmap に
        if ((o->kind == MUNMAP || o->kind == MSYNC || o->kind == MREAD || o->kind == MWRITE) && nmref == 0 && chance(80))
            o->kind = MMAP;
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
            static const int accbits[] = {1, 2, 3, 0};
            lastdir = k == 'd';
            lastorig = i;
            addref(2 * i, 0, o->d1 == REF_CWD && o->acc != 3 && !(o->of & odd) && (k || ((o->of & B_CREAT) && o->p1[0] && o->p1[0] != 'x')),
                   (o->of & (1u << 8)) ? 0 : accbits[o->acc]);
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
            addref(2 * i, pipe, likely, lastacc);
            break;
        }
        case DUP3: {
            int likely;
            o->fd = genfd2(&pipe, &likely);
            o->x = 3 + (int)pick(10);
            o->y = chance(60) ? 0 : chance(88) ? 1 : 2;
            addref(2 * i, pipe, likely && o->y != 2, lastacc);
            break;
        }
        case FCNTL: {
            int likely;
            o->fd = genfd2(&pipe, &likely);
            o->x = (int)pick(6);
            // O_NONBLOCK はいつも残す: fd の番号は使いまわされるので、ファイルだと思った fd がパイプのこともあり、
            // 待つパイプ (read、splice、tee) は終わらない
            if (o->x == C_SETFL)
                o->y = (chance(40) ? 1 : 0) | 2 | (chance(5) ? 4 : 0);
            else if (o->x == C_SETFD)
                o->y = chance(60);
            else if (o->x == C_DUPFD || o->x == C_DUPFD_CLOEXEC) {
                o->y = dupmin[pick(5)];
                addref(2 * i, pipe, likely, lastacc);
            }
            break;
        }
        case PIPE:
            o->x = chance(30);
            lastdir = 0;
            lastorig = -1;
            addref(2 * i, 1, 1, 1);
            addref(2 * i + 1, 2, 1, 2);
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
        case SPLICE: {
            // fd が読むほう、d2 が書くほう: パイプ → ファイル、ファイル → パイプ、パイプ → パイプ、なんでも。
            // x: 1 読む位置をわたす、2 書く位置をわたす (ふつうはパイプでない側だけ。パイプの側は ESPIPE)
            int k = (int)pick(10);
            if (k < 4) {
                o->fd = genpipe(1);
                o->d2 = genfile(2);
                o->x = (chance(50) ? 2 : 0) | (chance(8) ? 1 : 0);
            } else if (k < 7) {
                o->fd = genfile(1);
                o->d2 = genpipe(2);
                o->x = (chance(50) ? 1 : 0) | (chance(8) ? 2 : 0);
            } else if (k < 9) {
                o->fd = genpipe(1);
                o->d2 = genpipe2(2, o->fd);
                o->x = chance(8) ? (int)pick(4) : 0;
            } else {
                o->fd = genfd(&pipe);
                o->d2 = genfd(&pipe);
                o->x = (int)pick(4);
            }
            o->off = coffs[pick(5)];
            o->y = (int)pick(5);
            o->len = splens[pickw(splw, 8)];
            o->mode = pickw(spfw, 4);
            break;
        }
        case TEE:
            // fd (読む口) から d2 (書く口) へ、読み減らさずに。たいていパイプどうし
            o->fd = chance(85) ? genpipe(1) : genfd(&pipe);
            o->d2 = chance(85) ? genpipe2(2, o->fd) : genfd(&pipe);
            o->len = splens[pickw(splw, 8)];
            o->mode = pickw(spfw, 4);
            break;
        case FORK:
            // 子のふるまい (x)、終わりの番号か自分に送るシグナル (y)、待つ子のシグナルの扱い (mode)、グループ (acc)
            o->x = (int)pickw(kbehw, 4);
            o->y = o->x == 0 ? (int)pick(4) : (int)(1 + pick(3));
            for (int b = 0; b < 4; b++)
                o->mode |= (unsigned)(chance(40) ? 0 : 1 + pick(3)) << (2 * b);
            o->acc = o->x >= 2 && chance(45) ? (chance(50) ? 1u : 2u) : 0u;
            kgrp[i] = (int)o->acc;
            krefs[nkref++] = i;
            break;
        case KILL:
            // その子 (x 0) か、その子のグループ (x 1: 自分のグループを作った子だけ) に、シグナル y。止まる子には
            // SIGCONT を、待つ子には止めるシグナルを多めに (止まって、動いて、の知らせが出るように)
            o->d1 = genkid();
            o->x = o->d1 >= 0 && kgrp[o->d1] && chance(40);
            o->y = (int)pickw(ksigw, 12);
            if (o->d1 >= 0 && ops[o->d1].x == 2 && chance(45))
                o->y = 4;
            else if (o->d1 >= 0 && ops[o->d1].x == 3 && chance(25))
                o->y = chance(50) ? 3 : 7;
            break;
        case WAIT:
            // wait4 (x 0) か waitid (x 1)。相手 (y) と options (mode)
            o->x = chance(50);
            o->d1 = genkid();
            o->y = (int)pickw(wtgtw, 6);
            if (o->y == 3 && (o->d1 < 0 || !kgrp[o->d1]))
                o->y = 0;
            if (o->y == 5 && !o->x)
                o->y = 4;
            o->mode = o->x ? pickw(wioptw, 7) : pickw(w4optw, 6);
            // 止まる子と待つ子は、止まった・動いたの知らせも見る
            if (o->d1 >= 0 && ops[o->d1].x >= 2 && chance(40))
                o->mode = o->x ? (chance(50) ? 3u : 5u) : 3u;
            break;
        case CHLDACT:
            // 自分の SIGCHLD の扱い: x 0 既定、1 無視、2 ハンドラ。y: SA_NOCLDSTOP (1) と SA_NOCLDWAIT (2)
            o->x = (int)pick(3);
            o->y = chance(50) ? 0 : (int)(1 + pick(3));
            break;
        case SETPGID:
            // setpgid(子か自分 (d1 -2), pgid): x 0 は 0、1 はその子の番号、2 はほかの子 (d2) の番号、3 は自分のグループ、4 は -1
            o->d1 = chance(12) ? -2 : genkid();
            o->x = (int)pick(5);
            o->d2 = genkid();
            break;
        case GETPGID:
            // getpgid (x 0) か getsid (x 1) を、子か自分 (d1 -2) に
            o->d1 = chance(15) ? -2 : genkid();
            o->x = chance(40);
            break;
        case FLOCK:
            // たいてい、同じファイルをほかにも開いているもの (取り合い)。なければ、そのファイルをもう一度開く op に
            o->x = (int)pickw(flockw, 5);
            if (chance(70)) {
                o->fd = gensame(3);
                if (!lastsame && chance(60) && reopen_same(o, i))
                    break;
            } else
                o->fd = genfd(&pipe);
            lastlock = lastorig;
            break;
        case RLOCK:
            o->x = (int)pickw(rlcmdw, 4);
            o->y = (int)pickw(rltw, 4);
            // 種類に合った口 (F_RDLCK は読める fd、F_WRLCK は書ける fd) で、たいてい同じファイルをほかにも開いているもの
            if (chance(75)) {
                o->fd = gensame(o->y == 1 ? 2 : 1);
                if (!lastsame && chance(60) && reopen_same(o, i))
                    break;
            } else
                o->fd = genfd(&pipe);
            lastlock = lastorig;
            o->acc = pickw(rlww, 4);
            o->off = rlstarts[pick(10)];
            o->len = rllens[pick(9)];
            o->rwf = chance(5);
            break;
        case MMAP: {
            // fd のファイルを写す (たいてい読めるふつうのファイル。len と off は表から。prot と MAP_SHARED / MAP_PRIVATE)。
            // できたら写しの ref
            int file = chance(85);
            o->fd = file ? genfile(1) : genfd(&pipe);
            o->len = mlens[pickw(mlensw, 6)];
            o->off = moffs[pickw(moffsw, 4)];
            o->x = (int)pickw(mprotw, 4);
            o->y = chance(50);
            mlikely[nmref] = file && lastfile && o->len > 0 && o->off % 4096 == 0;
            mrefs[nmref++] = i;
            break;
        }
        case MUNMAP:
            o->d1 = genmap();
            o->x = chance(30);
            break;
        case MSYNC:
            o->d1 = genmap();
            o->x = (int)pick(5);
            break;
        case MREAD:
        case MWRITE:
            // 写しの中の位置から len バイトを読む / 書く (さわれなければ SIGSEGV、ファイルの終わりの先のページなら SIGBUS)
            o->d1 = genmap();
            o->off = maoffs[pick(6)];
            o->len = malens[pick(4)];
            o->x = (int)pick(256);
            break;
        case READV:
        case WRITEV:
        case PREADV2:
        case PWRITEV2: {
            // iovec の形 (y) と全部の長さ。p*v2 は位置 (-1 ならいまの位置) と flags。NOWAIT で読むときは
            // cold (mode 1: キャッシュから外してから。EAGAIN) か warm (まず読んでおいてから。読める)
            int rd = o->kind == READV || o->kind == PREADV2;
            o->fd = genfd(&pipe);
            o->len = rd ? rlens[pick(5)] : lens[pick(7)];
            o->x = (int)pick(256);
            o->y = (int)pickw(shapew, NSHAPE);
            if (o->kind == PREADV2 || o->kind == PWRITEV2) {
                o->off = chance(25) ? -1 : offs[pick(6)];
                unsigned k = pickw(rwfw, NRWF);
                // 書くときの DONTCACHE は、書き出しが終わってから捨てる (いつかは Linux の中の時間しだい) ので使わない
                while (o->kind == PWRITEV2 && (rwfs[k].v & 0x80))
                    k = pickw(rwfw, NRWF);
                o->rwf = k;
                o->mode = rd && (rwfs[k].v & 8) && chance(50);
            }
            break;
        }
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

// chosen splice-snapshot: Linux の splice / sendfile は、ファイルからパイプへページキャッシュのページそのものを入れる
// ので、読む前にファイルが変わればパイプの中身も変わる。aios はそのときの中身を写す。Linux では、そのときの中身の写し
// (名前のないファイル。case の場所 (w の上) に作るので world には出ない) から入れてまねる。写しは穴をとばして作る
// (SEEK_DATA / SEEK_HOLE。fd の位置はもどす)。写しの fd か、作れなければ -1
static int snapshot(int fd) {
    static unsigned char sbuf[65536];
    struct stat st;
    if (fstat(fd, &st) != 0 || !S_ISREG(st.st_mode))
        return -1;
    int s = open("../..", O_TMPFILE | O_RDWR, 0600);
    if (s < 0) {
        char p[64];
        snprintf(p, sizeof p, "../../.snap%d", (int)getpid());
        s = open(p, O_RDWR | O_CREAT | O_TRUNC, 0600);
        if (s < 0)
            return -1;
        unlink(p);
    }
    long long save = syscall(SYS_lseek, fd, 0, SEEK_CUR), size = st.st_size;
    for (long long off = 0; off < size;) {
        long long d = syscall(SYS_lseek, fd, (off_t)off, SEEK_DATA);
        if (d < 0)
            break;
        long long h = syscall(SYS_lseek, fd, (off_t)d, SEEK_HOLE);
        if (h < 0 || h > size)
            h = size;
        for (long long at = d; at < h;) {
            long n = pread(fd, sbuf, (size_t)(h - at < (long long)sizeof sbuf ? h - at : (long long)sizeof sbuf), at);
            if (n <= 0)
                break;
            if (pwrite(s, sbuf, n, at) != n) {
                close(s);
                return -1;
            }
            at += n;
        }
        off = h;
    }
    if (ftruncate(s, size) != 0) {
        close(s);
        return -1;
    }
    if (save >= 0)
        syscall(SYS_lseek, fd, (off_t)save, SEEK_SET);
    return s;
}

// splice-snapshot: ふつうのファイル (読める口) からパイプへ、長さ 0 でなく、flags がまちがっていないとき。fd と to の
// 種類は両方で同じに見る (印も同じ条件で)
static int file_to_pipe(int fd, int to, long long len, unsigned flags) {
    struct stat ist, xst;
    return len > 0 && !(flags & ~0x0fu) && fstat(fd, &ist) == 0 && S_ISREG(ist.st_mode) && fstat(to, &xst) == 0 && S_ISFIFO(xst.st_mode);
}

// Linux で、読める口 (O_PATH でない) か。写しから入れるのは、そのときだけ (でなければ本物が EBADF)
static int readable_fd(int fd) {
    int fl = (int)syscall(SYS_fcntl, fd, F_GETFL);
    return fl >= 0 && !(fl & O_PATH) && !(fl & 1);
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

// mmap の写し (op の番号ごと): 場所と長さ (外したら NULL)。mread / mwrite のシグナルは sigsetjmp で受けて答えにする
static void *mres[MAXOPS];
static size_t mlen[MAXOPS];
static sigjmp_buf jb;
static volatile sig_atomic_t jsig, injmp;

static void onsig(int s) {
    if (!injmp) {
        signal(s, SIG_DFL);
        raise(s);
        return;
    }
    jsig = s;
    siglongjmp(jb, 1);
}

// プロセス: FORK の op ごとの子の番号 (fork できなければ 0)、同期のパイプの読む口 (800 から)、回収したか。
// me は case のプロセス (op を動かすもの)、sess は case のセッションのリーダー (その親)
static pid_t kpid[MAXOPS], me_pid, sess_pid;
static int ksync[MAXOPS], kgone[MAXOPS];
// 子の中で: ハンドラが書く同期のパイプの書く口
static volatile int kfd = -1;

static void khandler(int s) {
    unsigned char c = (unsigned char)s;
    ssize_t r = write(kfd, &c, 1);
    (void)r;
}

static void chldhandler(int s) {
    (void)s;
}

static const char *signame(int s) {
    static char b[16];
    static const char *const n[] = {"0", "SIGHUP", "SIGINT", "SIGQUIT", "SIGILL", "SIGTRAP", "SIGABRT", "SIGBUS", "SIGFPE", "SIGKILL", "SIGUSR1", "SIGSEGV", "SIGUSR2", "SIGPIPE", "SIGALRM", "SIGTERM", "SIGSTKFLT", "SIGCHLD", "SIGCONT", "SIGSTOP", "SIGTSTP", "SIGTTIN", "SIGTTOU"};
    if (s >= 0 && s < (int)(sizeof n / sizeof *n))
        return n[s];
    snprintf(b, sizeof b, "%d", s);
    return b;
}

// 番号の名前: 子なら kN、case のプロセスなら me、そのセッションのリーダーなら sess、0 は 0、ほかは other
static const char *pidname(long v) {
    static char b[16];
    if (v == 0)
        return "0";
    if (v == me_pid)
        return "me";
    if (v == sess_pid)
        return "sess";
    for (int i = 0; i < MAXOPS; i++)
        if (kpid[i] && kpid[i] == v) {
            snprintf(b, sizeof b, "k%d", i);
            return b;
        }
    return "other";
}

// 子 (ref) の番号。いない (fork しなかった、できなかった) ならいない番号 (Linux の pid_max より大きい)
static pid_t kidpid(int ref) {
    return ref >= 0 && kpid[ref] ? kpid[ref] : 4000000;
}

// /proc/PID/stat の状態 (R、S、T、Z ...)。なければ X
static char kstate(pid_t pid) {
    char p[64], b[512];
    snprintf(p, sizeof p, "/proc/%d/stat", (int)pid);
    int fd = open(p, O_RDONLY | O_CLOEXEC);
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

// 子が落ちつくまで (眠っている、止まっている、ゾンビ、もういない が 3 ms あけて 3 回つづく。3 秒まで) 待つ。その状態
static char settle(pid_t pid) {
    char last = 0;
    int same = 0;
    for (int k = 0; k < 1000; k++) {
        char st = kstate(pid);
        if (st == 'S' || st == 'T' || st == 't' || st == 'Z' || st == 'X') {
            same = st == last ? same + 1 : 0;
            if (same >= 2)
                return st;
        } else
            same = 0;
        last = st;
        usleep(3000);
    }
    return last;
}

// 状態の名前と、子のハンドラが受けとったシグナル (同期のパイプにたまったもの)
static void saykid(int ref, char st) {
    say(" (%s", st == 'S' ? "sleeping" : st == 'T' || st == 't' ? "stopped" : st == 'Z' ? "zombie" : st == 'X' ? "gone" : "?");
    if (ref >= 0 && ksync[ref] >= 0) {
        unsigned char c[64];
        ssize_t n = read(ksync[ref], c, sizeof c);
        for (ssize_t k = 0; k < n; k++)
            if (c[k] != 'r')
                say("%s%s", k ? "," : ", handled ", signame(c[k]));
    }
    say(")");
}

// 子の中: ふるまいのとおりにして、終わらない (待つ子と止まる子はシグナルを待ちつづける)
static void kidmain(const struct op *o, int wfd) {
    prctl(PR_SET_PDEATHSIG, SIGKILL);
    int nul = open("/dev/null", O_RDWR);
    if (nul >= 0) {
        dup2(nul, 1);
        dup2(nul, 2);
        if (nul > 2)
            close(nul);
    }
    kfd = wfd;
    if (o->acc == 1)
        setpgid(0, 0);
    else if (o->acc == 2)
        setsid();
    switch (o->x) {
    case 0:
        _exit(kcodes[o->y]);
    case 1: {
        static const int rs[] = {0, SIGTERM, SIGKILL, SIGABRT};
        signal(rs[o->y], SIG_DFL);
        raise(rs[o->y]);
        _exit(99);
    }
    case 2:
        raise(SIGSTOP);
        for (;;)
            pause();
    default: {
        sigset_t blk;
        sigemptyset(&blk);
        for (int b = 0; b < 4; b++) {
            unsigned d = (o->mode >> (2 * b)) & 3;
            if (d == 1)
                signal(kdisp[b], SIG_IGN);
            else if (d == 2) {
                struct sigaction sa;
                memset(&sa, 0, sizeof sa);
                sa.sa_handler = khandler;
                sigaction(kdisp[b], &sa, NULL);
            } else if (d == 3)
                sigaddset(&blk, kdisp[b]);
        }
        sigprocmask(SIG_BLOCK, &blk, NULL);
        unsigned char r = 'r';
        if (write(wfd, &r, 1) != 1)
            _exit(98);
        for (;;)
            pause();
    }
    }
}

// case の終わり: のこっている子をみな SIGKILL で終わらせて回収する
static void reapkids(void) {
    signal(SIGCHLD, SIG_DFL);
    for (int i = 0; i < MAXOPS; i++)
        if (kpid[i] && !kgone[i]) {
            kill(kpid[i], SIGKILL);
            waitpid(kpid[i], NULL, 0);
            kgone[i] = 1;
        }
}

// wait4 の status
static void saywst(int st) {
    if (WIFEXITED(st))
        say(" exit %d", WEXITSTATUS(st));
    else if (WIFSIGNALED(st))
        say(" signal %s%s", signame(WTERMSIG(st)), WCOREDUMP(st) ? " core" : "");
    else if (WIFSTOPPED(st))
        say(" stopped %s", signame(WSTOPSIG(st)));
    else if (WIFCONTINUED(st))
        say(" continued");
    else
        say(" status %#x", st);
}

// 回収した (終わった子を待って、WNOWAIT でなかった) 子に印
static void markgone(pid_t pid) {
    for (int i = 0; i < MAXOPS; i++)
        if (kpid[i] == pid)
            kgone[i] = 1;
}

// readv などの iovec: 全部で len バイトを形 (shapes) に分けて buf に並べる (bad の 2 つめはさわれないページ badpage)
static void *badpage;
static int mkiov(struct iovec *iov, int shape, unsigned char *buf, long long len) {
    long long a = len / 2, b = len - a, t = len / 3;
    switch (shape) {
    case 1:
        iov[0] = (struct iovec){buf, (size_t)a};
        iov[1] = (struct iovec){buf + a, (size_t)b};
        return 2;
    case 2:
        iov[0] = (struct iovec){buf, (size_t)a};
        iov[1] = (struct iovec){buf + a, 0};
        iov[2] = (struct iovec){buf + a, (size_t)b};
        return 3;
    case 3:
        iov[0] = (struct iovec){buf, 0};
        iov[1] = (struct iovec){buf, (size_t)t};
        iov[2] = (struct iovec){buf + t, (size_t)t};
        iov[3] = (struct iovec){buf + 2 * t, (size_t)(len - 2 * t)};
        return 4;
    case 4:
        return 0;
    case 5:
        for (int i = 0; i < 1025; i++)
            iov[i] = (struct iovec){buf, 0};
        iov[0].iov_len = (size_t)len;
        return 1025;
    case 6:
        iov[0] = (struct iovec){buf, (size_t)a};
        iov[1] = (struct iovec){badpage, (size_t)(b ? b : 1)};
        return 2;
    default:
        iov[0] = (struct iovec){buf, (size_t)len};
        return 1;
    }
}

// chosen pipe-nowait: Linux は splice に使ったパイプの口 (fd が両方あれば、しくじっても) の FMODE_NOWAIT を消す。
// その口 (パイプの inode と、読む口か書く口か: dup でも同じ口) を両方の側で覚えて、そこへの NOWAIT には印をつけ、
// Linux では NOWAIT なしで呼ぶ (このロボットのパイプは O_NONBLOCK なので、答えは同じ)
static struct {
    ino_t ino;
    int mode;
} spliced[2 * MAXOPS];
static int nspliced;

static int pipe_key(int fd, ino_t *ino, int *mode) {
    struct stat st;
    int fl = (int)syscall(SYS_fcntl, fd, F_GETFL);
    if (fl < 0 || fstat(fd, &st) != 0 || !S_ISFIFO(st.st_mode))
        return 0;
    *ino = st.st_ino;
    *mode = fl & O_ACCMODE;
    return 1;
}

static int is_spliced(int fd) {
    ino_t ino;
    int mode;
    if (!pipe_key(fd, &ino, &mode))
        return 0;
    for (int i = 0; i < nspliced; i++)
        if (spliced[i].ino == ino && spliced[i].mode == mode)
            return 1;
    return 0;
}

static void mark_spliced(int fd) {
    ino_t ino;
    int mode;
    if (is_spliced(fd) || !pipe_key(fd, &ino, &mode) || nspliced >= 2 * MAXOPS)
        return;
    spliced[nspliced].ino = ino;
    spliced[nspliced].mode = mode;
    nspliced++;
}

// linuxbug tmpfs-extend: この op がファイル (fd) に書きはじめる位置 (書かないものは LLONG_MAX)。chosen の pwrite-append と
// copy-append で、Linux もアイオスと同じ位置に書く
static long long wstart_of(const struct op *o, int fd, long long size) {
    int fl = (int)syscall(SYS_fcntl, fd, F_GETFL);
    int app = fl >= 0 && (fl & O_APPEND);
    long long cur = app ? size : (long long)lseek(fd, 0, SEEK_CUR);
    switch (o->kind) {
    case WRITE:
    case WRITEV:
        return cur;
    case PWRITE:
        return o->off;
    case PWRITEV2:
        if (o->off < 0)
            return cur;
        return (rwfs[o->rwf].v & 0x10) ? size : o->off;
    // sendfile は書く口の位置に (O_APPEND なら終わりに)、copy_file_range と splice は位置をわたせばそこに
    case SENDFILE:
        return cur;
    case COPYRANGE:
    case SPLICE:
        return (o->x & 2) && coffs[o->y] >= 0 ? coffs[o->y] : cur;
    default:
        return LLONG_MAX;
    }
}

static void exec(int i, const struct op *o) {
    long r = 0;
    struct stat st;
    int fd = fdarg(o);
    say("%d ", i);
    // linuxbug tmpfs-extend: Linux の tmpfs は、写し (mmap) でファイルの終わりの先 (同じページの中) に書いたものが、
    // ファイルを伸ばすと (ftruncate、fallocate、終わりより先への write) そのまま中身になる (ext4 は 0 にする)。
    // Linux では、伸ばした op のあとで、前の終わりからそのページの終わり (op が書いたところの手前) までを 0 にする。
    // 印は両方で (tmpfs のふつうのファイルが伸び、前の終わりがページの途中で、そこに op が書いていなければ)
    // (truncate はパスなので、書けるように開いて見る: 伸ばせたなら書ける。パイプは O_NONBLOCK で ENXIO)
    int xfd = (o->kind == SENDFILE || o->kind == COPYRANGE || o->kind == SPLICE) ? fdref(o->d2, o->fb, fd)
              : o->kind == TRUNCATE                                                ? open(o->p1, O_WRONLY | O_NONBLOCK)
                                                                                   : fd;
    struct stat xs0;
    struct statfs xsf;
    int xt = xfd >= 0 && fstat(xfd, &xs0) == 0 && S_ISREG(xs0.st_mode) && fstatfs(xfd, &xsf) == 0 && xsf.f_type == K_TMPFS_MAGIC;
    long long wstart = xt ? wstart_of(o, xfd, (long long)xs0.st_size) : 0;
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
        int reg = fstat(fd, &st) == 0 && S_ISREG(st.st_mode);
        int zt = (m & FL_ZERO_RANGE) && reg && fstatfs(fd, &sf) == 0 && sf.f_type == K_TMPFS_MAGIC;
        // linuxbug zero-range-eof: ext4 の ZERO_RANGE が終わりをこえると、終わりのブロックの中が 0 にならないことがある。
        // Linux ではそのあと、ファイルの終わりまでを穴にして 0 にする
        int zb = (m & FL_ZERO_RANGE) && !(m & FL_KEEP_SIZE) && reg && !zt && o->off >= 0 && o->off < st.st_size && o->off + o->len > st.st_size;
        r = syscall(SYS_fallocate, fd, m, (off_t)o->off, (off_t)o->len);
        if (zt && !on_aios && r < 0 && errno == EOPNOTSUPP) {
            r = syscall(SYS_fallocate, fd, FL_PUNCH_HOLE | FL_KEEP_SIZE, (off_t)o->off, (off_t)o->len);
            if (r == 0)
                r = syscall(SYS_fallocate, fd, m & FL_KEEP_SIZE, (off_t)o->off, (off_t)o->len);
        }
        if (zb && !on_aios && !raw && r == 0)
            syscall(SYS_fallocate, fd, FL_PUNCH_HOLE | FL_KEEP_SIZE, (off_t)o->off, (off_t)(st.st_size - o->off));
        ret(r);
        if (zt)
            say(" [chosen zero-range-tmpfs]");
        if (zb)
            say(" [linuxbug zero-range-eof]");
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
        // chosen splice-snapshot: sendfile で、ふつうのファイルからパイプへ
        int f2p = o->kind == SENDFILE && file_to_pipe(fd, to, o->len, 0), snap = -1;
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
            } else if (f2p && !on_aios && readable_fd(fd) && (snap = snapshot(fd)) >= 0) {
                // chosen splice-snapshot: Linux では、そのときの写しから入れる (位置は fd のものを使い、もどす)
                long long pos = (o->x & 1) ? in : (long long)syscall(SYS_lseek, fd, 0, SEEK_CUR);
                r = syscall(SYS_sendfile, to, snap, &pos, (size_t)o->len);
                e = errno;
                if (r >= 0 && (o->x & 1))
                    in = pos;
                else if (r >= 0)
                    syscall(SYS_lseek, fd, (off_t)pos, SEEK_SET);
                close(snap);
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
        if (f2p && r > 0)
            say(" [chosen splice-snapshot]");
        break;
    }
    case SPLICE: {
        long long in = o->off, out = coffs[o->y];
        int to = fdref(o->d2, o->fb, fd);
        say("splice(%d, ", fd);
        if (o->x & 1)
            say("%lld", in);
        else
            say("NULL");
        say(", %d, ", to);
        if (o->x & 2)
            say("%lld", out);
        else
            say("NULL");
        say(", %lld, %s)", o->len, spfn[o->mode]);
        // chosen pipe-nowait: Linux の __do_splice の順で、パイプの口を覚える (fd が両方あって、読む口に位置があれば
        // そこで ESPIPE、なければ消して、書く口も同じ)
        {
            int fi = (int)syscall(SYS_fcntl, fd, F_GETFL), fo = (int)syscall(SYS_fcntl, to, F_GETFL);
            if (fi >= 0 && !(fi & O_PATH) && fo >= 0 && !(fo & O_PATH)) {
                ino_t k;
                int m, stop = 0;
                if (pipe_key(fd, &k, &m)) {
                    if (o->x & 1)
                        stop = 1;
                    else
                        mark_spliced(fd);
                }
                if (!stop && pipe_key(to, &k, &m)) {
                    if (!(o->x & 2))
                        mark_spliced(to);
                }
            }
        }
        // chosen copy-append: O_APPEND で開いたファイルへも書く (位置をわたせばそこへ、なければ終わりに足す)。Linux
        // では O_APPEND をはずし、書く位置がなければ終わりをわたして書いて、書くほうの位置を終わり + 書いた分にしてもどす
        int fl = (int)syscall(SYS_fcntl, to, F_GETFL);
        struct stat ost;
        int app = fl >= 0 && (fl & O_APPEND) && fstat(to, &ost) == 0 && S_ISREG(ost.st_mode);
        int im = app && !on_aios;
        long long end = im ? (long long)ost.st_size : 0;
        if (im)
            syscall(SYS_fcntl, to, F_SETFL, fl & ~O_APPEND);
        // chosen splice-snapshot: ふつうのファイル → パイプ (書く位置なし) は、Linux ではそのときの写しから入れる
        int f2p = !(o->x & 2) && file_to_pipe(fd, to, o->len, spfv[o->mode]), snap = -1, e;
        if (f2p && !on_aios && readable_fd(fd) && (snap = snapshot(fd)) >= 0) {
            long long pos = (o->x & 1) ? in : (long long)syscall(SYS_lseek, fd, 0, SEEK_CUR);
            r = syscall(SYS_splice, snap, &pos, to, NULL, (size_t)o->len, spfv[o->mode]);
            e = errno;
            if (r >= 0 && (o->x & 1))
                in = pos;
            else if (r >= 0)
                syscall(SYS_lseek, fd, (off_t)pos, SEEK_SET);
            close(snap);
        } else {
            r = syscall(SYS_splice, fd, (o->x & 1) ? &in : NULL, to, (o->x & 2) ? &out : im ? &end : NULL, (size_t)o->len, spfv[o->mode]);
            e = errno;
        }
        if (im && r > 0 && !(o->x & 2))
            syscall(SYS_lseek, to, (off_t)end, SEEK_SET);
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
        if (f2p && r > 0)
            say(" [chosen splice-snapshot]");
        break;
    }
    case TEE: {
        int to = fdref(o->d2, o->fb, fd);
        say("tee(%d, %d, %lld, %s)", fd, to, o->len, spfn[o->mode]);
        ret(syscall(SYS_tee, fd, to, (size_t)o->len, spfv[o->mode]));
        break;
    }
    case READV:
    case WRITEV:
    case PREADV2:
    case PWRITEV2: {
        static struct iovec iov[1025];
        int rd = o->kind == READV || o->kind == PREADV2;
        int v2 = o->kind == PREADV2 || o->kind == PWRITEV2;
        unsigned fl = v2 ? rwfs[o->rwf].v : 0;
        if (!rd)
            for (long long k = 0; k < o->len; k++)
                wbuf[k] = (unsigned char)(o->x + k * 7);
        int cnt = mkiov(iov, o->y, rd ? rbuf : wbuf, o->len);
        say("%s(%d, %lld [%s]", rd ? (v2 ? "preadv2" : "readv") : (v2 ? "pwritev2" : "writev"), fd, o->len, shapes[o->y]);
        if (v2)
            say(", %lld, %s%s", o->off, rwfs[o->rwf].n, o->mode ? " cold" : "");
        say(")");
        // NOWAIT で読む: cold なら、まず書き残しをディスクへ (fsync。DONTNEED は書いていないページを外さない)、読むところの
        // ページをまるごとキャッシュから外して (fadvise DONTNEED。aios も本当に外す)、EAGAIN を見る。warm なら、まずふつうに
        // 読んでおいて (どちらもキャッシュにある) 読めるのを見る。Linux は EAGAIN のときに先読みを出すので、前のがまだなら
        // 終わるのを待ってから外す
        long long at = -1;
        int cold = 0;
        if (rd && v2 && (fl & 8)) {
            at = o->off >= 0 ? o->off : (long long)lseek(fd, 0, SEEK_CUR);
            if (at >= 0) {
                if (o->mode) {
                    long long from = at & ~4095LL, to = (at + (o->len ? o->len : 1) + 4095) & ~4095LL;
                    syscall(SYS_fsync, fd);
                    if (!on_aios)
                        usleep(10000);
                    posix_fadvise(fd, (off_t)from, (off_t)(to - from), POSIX_FADV_DONTNEED);
                    cold = 1;
                } else {
                    syscall(SYS_pread64, fd, rbuf, (size_t)o->len, (off_t)at);
                }
            }
        }
        // chosen pipe-fault: パイプの bad (2 つめがさわれない) は、さわれる 1 つめの分をみな読み書きする。Linux は箱ごとに
        // 写して途中でさわれなくなった箱を捨てる (数えない) ので、Linux では 1 つめだけを渡す
        int wf = 0;
        if (o->y == 6 && iov[0].iov_len > 0) {
            struct stat st;
            wf = fstat(fd, &st) == 0 && S_ISFIFO(st.st_mode);
            if (wf && !on_aios)
                cnt = 1;
        }
        // chosen pipe-nowait: splice に使ったパイプの口への NOWAIT は、Linux では NOWAIT なしで (上の spliced)
        int pn = v2 && (fl & 8) && is_spliced(fd);
        if (pn && !on_aios)
            fl &= ~8u;
        // chosen pwrite-append (pwritev2 も): O_APPEND の fd でも off に書く。Linux では O_APPEND をはずして書き、もどす
        // (RWF_APPEND / RWF_NOAPPEND があれば、どちらも同じ答えなのでそのまま)
        int fl0 = -1, app = 0;
        if (o->kind == PWRITEV2 && o->off >= 0 && !(fl & 0x30)) {
            fl0 = (int)syscall(SYS_fcntl, fd, F_GETFL);
            app = fl0 >= 0 && (fl0 & O_APPEND);
            if (app && !on_aios)
                syscall(SYS_fcntl, fd, F_SETFL, fl0 & ~O_APPEND);
        }
        // linuxbug efault-extend: さわれない入れ物の書きこみ (EFAULT) で Linux はファイルを位置まで伸ばす (終わりより先に
        // 書くとき)。Linux では伸びたぶんを切りもどす。印は両方で (書く位置が終わりより先で EFAULT なら)
        struct stat st0;
        int ef = !rd && o->y == 6 && fstat(fd, &st0) == 0 && S_ISREG(st0.st_mode);
        long long wpos = -1;
        if (ef) {
            int wfl = (int)syscall(SYS_fcntl, fd, F_GETFL);
            wpos = o->off >= 0 ? o->off : (wfl >= 0 && (wfl & O_APPEND)) ? (long long)st0.st_size : (long long)lseek(fd, 0, SEEK_CUR);
        }
        if (o->kind == READV)
            r = syscall(SYS_readv, fd, iov, cnt);
        else if (o->kind == WRITEV)
            r = syscall(SYS_writev, fd, iov, cnt);
        else if (o->kind == PREADV2)
            r = syscall(SYS_preadv2, fd, iov, cnt, (long)o->off, 0L, fl);
        else
            r = syscall(SYS_pwritev2, fd, iov, cnt, (long)o->off, 0L, fl);
        int e = errno;
        if (app && !on_aios)
            syscall(SYS_fcntl, fd, F_SETFL, fl0);
        int fixed = ef && r < 0 && e == EFAULT && wpos > (long long)st0.st_size;
        if (fixed && !on_aios && !raw) {
            struct stat st1;
            if (fstat(fd, &st1) == 0 && st1.st_size > st0.st_size)
                syscall(SYS_ftruncate, fd, (off_t)st0.st_size);
        }
        // cold: Linux の 1 回めの答えは、先読みが間に合えば中身、でなければ EAGAIN (時間しだい) なので、EAGAIN にそろえる
        // (読んでしまった位置ももどす)。aios は EAGAIN と決まっている (そのときそのページを読んでおく)。
        // そのあと 2 回めを読む: どちらも読める (then)
        if (cold && !on_aios && r > 0) {
            r = -1;
            e = EAGAIN;
            if (o->off < 0)
                lseek(fd, (off_t)at, SEEK_SET);
        }
        errno = e;
        ret(r);
        if (fixed)
            say(" [linuxbug efault-extend]");
        if (pn)
            say(" [chosen pipe-nowait]");
        if (app && r > 0)
            say(" [chosen pwrite-append]");
        if (wf && r > 0)
            say(" [chosen pipe-fault]");
        if (rd && r > 0)
            say(" h%08x", fnv(rbuf, r));
        if (cold) {
            // Linux では先読みのディスクが終わるのを待つ (10 ms ずつ、EAGAIN のあいだ 1 秒まで: DONTCACHE のときなどは
            // 終わるまで EAGAIN のまま)。aios は 1 回めに読んでおくので、2 回めはいつも読める
            long r2 = syscall(SYS_preadv2, fd, iov, cnt, (long)o->off, 0L, fl);
            for (int k = 0; !on_aios && r2 < 0 && errno == EAGAIN && k < 100; k++) {
                usleep(10000);
                r2 = syscall(SYS_preadv2, fd, iov, cnt, (long)o->off, 0L, fl);
            }
            say(" then");
            ret(r2);
            if (r2 > 0)
                say(" h%08x", fnv(rbuf, r2));
        }
        break;
    }
    case FORK: {
        say("fork(%s", kbehn[o->x]);
        if (o->x == 0)
            say(" %d", kcodes[o->y]);
        else if (o->x == 1) {
            static const int rs[] = {0, SIGTERM, SIGKILL, SIGABRT};
            say(" %s", signame(rs[o->y]));
        } else if (o->x == 3)
            for (int b = 0; b < 4; b++)
                say("%s%s:%s", b ? " " : " ", signame(kdisp[b]) + 3, kdispn[(o->mode >> (2 * b)) & 3]);
        say("%s)", kgrpn[o->acc]);
        // 同期のパイプ (子のハンドラが受けとったシグナルを書く)。fd は 800 から (op の fd の番号を変えない)
        int sp[2], rd = -1, wr = -1;
        if (pipe2(sp, O_CLOEXEC) == 0) {
            rd = (int)syscall(SYS_fcntl, sp[0], F_DUPFD_CLOEXEC, 800);
            wr = (int)syscall(SYS_fcntl, sp[1], F_DUPFD_CLOEXEC, 800);
            close(sp[0]);
            close(sp[1]);
        }
        pid_t pid = fork();
        if (pid == 0) {
            if (rd >= 0)
                close(rd);
            kidmain(o, wr);
        }
        if (wr >= 0)
            close(wr);
        if (pid < 0) {
            ret(-1);
            if (rd >= 0)
                close(rd);
            break;
        }
        kpid[i] = pid;
        ksync[i] = rd;
        say(" = k%d", i);
        // 待つ子は、扱いを決めおえた印 (r) を待つ
        if (o->x == 3 && rd >= 0) {
            struct pollfd pf = {rd, POLLIN, 0};
            if (poll(&pf, 1, 3000) == 1) {
                unsigned char c;
                if (read(rd, &c, 1) != 1)
                    say(" (no ready)");
            }
        }
        if (rd >= 0)
            fcntl(rd, F_SETFL, O_NONBLOCK);
        saykid(i, settle(pid));
        break;
    }
    case KILL: {
        pid_t pid = kidpid(o->d1);
        int sig = ksigs[o->y];
        say("kill(%s%s, %s)", o->x ? "-" : "", o->d1 >= 0 && kpid[o->d1] ? pidname(pid) : "none", signame(sig));
        ret(syscall(SYS_kill, o->x ? -(long)pid : (long)pid, sig));
        if (o->d1 >= 0 && kpid[o->d1] && !kgone[o->d1])
            saykid(o->d1, settle(pid));
        break;
    }
    case WAIT: {
        pid_t kp = kidpid(o->d1);
        long id = o->y == 0 ? kp : o->y == 1 ? -1 : o->y == 2 ? 0 : o->y == 3 ? -(long)kp : 4000000;
        const char *idn = o->y == 0 ? (o->d1 >= 0 && kpid[o->d1] ? pidname(kp) : "none") : o->y == 1 ? "-1" : o->y == 2 ? "0" : o->y == 3 ? pidname(kp) : "bad";
        if (!o->x) {
            int st = 0x5a5a5a5a;
            say("wait4(%s%s, %s)", o->y == 3 ? "-" : "", idn, w4optn[o->mode]);
            r = syscall(SYS_wait4, id, &st, WNOHANG | w4opts[o->mode], NULL);
            if (r < 0)
                ret(r);
            else if (r == 0)
                say(" = 0");
            else {
                say(" = %s", pidname(r));
                saywst(st);
                if (WIFEXITED(st) || WIFSIGNALED(st))
                    markgone((pid_t)r);
            }
        } else {
            // waitid: P_PID、P_ALL、P_PGID (0 は自分のグループ)、P_PGID その子のグループ、P_PID いない番号、だめな idtype 7
            int idtype = o->y == 0 || o->y == 4 ? P_PID : o->y == 1 ? P_ALL : o->y == 5 ? 7 : P_PGID;
            long wid = o->y == 0 || o->y == 4 ? id : o->y == 1 ? 0 : o->y == 2 ? 0 : o->y == 3 ? kp : 0;
            siginfo_t si;
            memset(&si, 0x55, sizeof si);
            say("waitid(%s, %s%s, %s)", idtype == P_PID ? "P_PID" : idtype == P_ALL ? "P_ALL" : idtype == P_PGID ? "P_PGID" : "7", o->y == 3 ? "" : "", o->y == 1 || o->y == 5 ? "0" : idn, wioptn[o->mode]);
            r = syscall(SYS_waitid, idtype, wid, &si, WNOHANG | wiopts[o->mode], NULL);
            ret(r);
            if (r == 0) {
                if (si.si_pid == 0)
                    say(" {signo %d, code %d, status %d}", si.si_signo, si.si_code, si.si_status);
                else {
                    const char *cn = si.si_code == CLD_EXITED ? "CLD_EXITED" : si.si_code == CLD_KILLED ? "CLD_KILLED" : si.si_code == CLD_DUMPED ? "CLD_DUMPED" : si.si_code == CLD_STOPPED ? "CLD_STOPPED" : si.si_code == CLD_CONTINUED ? "CLD_CONTINUED" : "?";
                    say(" {%s, %s, %s, ", signame(si.si_signo), cn, pidname(si.si_pid));
                    say("uid %s, ", si.si_uid == getuid() ? "me" : "other");
                    if (si.si_code == CLD_EXITED)
                        say("status %d}", si.si_status);
                    else
                        say("status %s}", signame(si.si_status));
                    if ((si.si_code == CLD_EXITED || si.si_code == CLD_KILLED || si.si_code == CLD_DUMPED) && !(wiopts[o->mode] & WNOWAIT))
                        markgone(si.si_pid);
                }
            }
        }
        break;
    }
    case CHLDACT: {
        static const char *const an[] = {"SIG_DFL", "SIG_IGN", "handler"};
        static const char *const fn[] = {"0", "SA_NOCLDSTOP", "SA_NOCLDWAIT", "SA_NOCLDSTOP|SA_NOCLDWAIT"};
        struct sigaction sa;
        memset(&sa, 0, sizeof sa);
        sa.sa_handler = o->x == 0 ? SIG_DFL : o->x == 1 ? SIG_IGN : chldhandler;
        sa.sa_flags = SA_RESTART | ((o->y & 1) ? SA_NOCLDSTOP : 0) | ((o->y & 2) ? SA_NOCLDWAIT : 0);
        say("sigaction(SIGCHLD, %s, %s)", an[o->x], fn[o->y]);
        ret(sigaction(SIGCHLD, &sa, NULL));
        break;
    }
    case SETPGID: {
        pid_t who = o->d1 == -2 ? 0 : kidpid(o->d1);
        long pg = o->x == 0 ? 0 : o->x == 1 ? (long)who : o->x == 2 ? (long)kidpid(o->d2) : o->x == 3 ? (long)getpgrp() : -1;
        if (o->x == 1 && o->d1 == -2)
            pg = me_pid;
        say("setpgid(%s, ", o->d1 == -2 ? "0" : o->d1 >= 0 && kpid[o->d1] ? pidname(who) : "none");
        if (o->x == 2 && !(o->d2 >= 0 && kpid[o->d2]))
            say("none)");
        else
            say("%s)", o->x == 4 ? "-1" : o->x == 3 ? (pg == me_pid ? "me" : "mygroup") : pidname(pg));
        ret(syscall(SYS_setpgid, (long)who, pg));
        break;
    }
    case GETPGID: {
        pid_t who = o->d1 == -2 ? 0 : kidpid(o->d1);
        say("%s(%s)", o->x ? "getsid" : "getpgid", o->d1 == -2 ? "0" : o->d1 >= 0 && kpid[o->d1] ? pidname(who) : "none");
        r = syscall(o->x ? SYS_getsid : SYS_getpgid, (long)who);
        if (r < 0)
            ret(r);
        else
            say(" = %s", pidname(r));
        break;
    }
    case FLOCK:
        say("flock(%d, %s)", fd, flockn[o->x]);
        ret(syscall(SYS_flock, fd, flockv[o->x] | LOCK_NB));
        break;
    case RLOCK: {
        struct flock l;
        memset(&l, 0, sizeof l);
        l.l_type = (short)rltv[o->y];
        l.l_whence = (short)rlwv[o->acc];
        l.l_start = (off_t)o->off;
        l.l_len = (off_t)o->len;
        l.l_pid = o->rwf ? 1234 : 0;
        say("fcntl(%d, %s, {%s, %s, %lld, %lld, pid %d})", fd, rlcmdn[o->x], rltn[o->y], rlwn[o->acc], o->off, o->len, (int)l.l_pid);
        r = syscall(SYS_fcntl, fd, rlcmdv[o->x], &l);
        ret(r);
        // GETLK の答え: ぶつかるロックがなければ F_UNLCK だけ、あればその種類と場所と持ち主 (自分のプロセスなら self、
        // OFD のロックは -1。ほかのプロセスは other: pid は Linux と aios でちがう)
        if (r == 0 && (o->x == 1 || o->x == 3)) {
            const char *t = l.l_type == F_RDLCK ? "F_RDLCK" : l.l_type == F_WRLCK ? "F_WRLCK" : l.l_type == F_UNLCK ? "F_UNLCK" : "?";
            if (l.l_type == F_UNLCK)
                say(" {F_UNLCK}");
            else
                say(" {%s, %s, %lld, %lld, %s}", t, l.l_whence == SEEK_SET ? "SEEK_SET" : l.l_whence == SEEK_CUR ? "SEEK_CUR" : l.l_whence == SEEK_END ? "SEEK_END" : "?", (long long)l.l_start, (long long)l.l_len,
                    l.l_pid == -1 ? "-1" : l.l_pid == getpid() ? "self" : "other");
        }
        break;
    }
    case MMAP: {
        say("mmap(%d, %lld, %s, %s, %lld)", fd, o->len, mprotn[o->x], o->y ? "MAP_SHARED" : "MAP_PRIVATE", o->off);
        void *p = mmap(NULL, (size_t)o->len, mprotv[o->x], o->y ? MAP_SHARED : MAP_PRIVATE, fd, (off_t)o->off);
        if (p == MAP_FAILED) {
            ret(-1);
        } else {
            say(" = m%d", i);
            mres[i] = p;
            mlen[i] = (size_t)o->len;
        }
        break;
    }
    case MUNMAP: {
        int m = o->d1;
        say("munmap(m%d%s)", m, o->x ? ", first page" : "");
        if (m < 0 || !mres[m]) {
            say(" = (no mapping)");
            break;
        }
        size_t n = o->x && mlen[m] > 4096 ? 4096 : mlen[m];
        r = munmap(mres[m], n);
        ret(r);
        if (r == 0) {
            if (n == mlen[m]) {
                mres[m] = NULL;
            } else {
                mres[m] = (char *)mres[m] + n;
                mlen[m] -= n;
            }
        }
        break;
    }
    case MSYNC: {
        int m = o->d1;
        say("msync(m%d, %s)", m, msn[o->x]);
        if (m < 0 || !mres[m]) {
            say(" = (no mapping)");
            break;
        }
        ret(msync(mres[m], mlen[m], msv[o->x]));
        break;
    }
    case MREAD:
    case MWRITE: {
        int m = o->d1, wr = o->kind == MWRITE;
        say("%s(m%d, %lld, %lld)", wr ? "mwrite" : "mread", m, o->off, o->len);
        if (m < 0 || !mres[m]) {
            say(" = (no mapping)");
            break;
        }
        long long n = (long long)mlen[m], off = o->off < 0 ? (n > 0 ? n - 1 : 0) : o->off;
        if (off >= n) {
            say(" = (beyond the mapping)");
            break;
        }
        long long len = o->len < n - off ? o->len : n - off;
        volatile unsigned char *p = (volatile unsigned char *)mres[m] + off;
        injmp = 1;
        if (sigsetjmp(jb, 1) == 0) {
            if (wr)
                for (long long k = 0; k < len; k++)
                    p[k] = (unsigned char)(o->x + k * 7);
            else
                for (long long k = 0; k < len; k++)
                    rbuf[k] = p[k];
            injmp = 0;
            say(" = %lld", len);
            if (!wr)
                say(" h%08x", fnv(rbuf, len));
        } else {
            injmp = 0;
            say(" = %s", jsig == SIGBUS ? "SIGBUS" : jsig == SIGSEGV ? "SIGSEGV" : "signal");
        }
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
    if (xt) {
        struct stat xs1;
        long long end0 = (long long)xs0.st_size, pe = (end0 / 4096 + 1) * 4096;
        if (fstat(xfd, &xs1) == 0 && (long long)xs1.st_size > end0 && end0 % 4096 != 0) {
            long long gend = (long long)xs1.st_size < pe ? (long long)xs1.st_size : pe;
            if (wstart < gend)
                gend = wstart;
            if (gend > end0) {
                say(" [linuxbug tmpfs-extend]");
                if (!on_aios && !raw) {
                    static const unsigned char zeros[4096];
                    // 伸ばせた fd は書ける (O_APPEND は外して書き、もどす)
                    int fl = (int)syscall(SYS_fcntl, xfd, F_GETFL);
                    if (fl >= 0 && (fl & O_APPEND))
                        syscall(SYS_fcntl, xfd, F_SETFL, fl & ~O_APPEND);
                    if (pwrite(xfd, zeros, (size_t)(gend - end0), (off_t)end0) < 0)
                        say(" (zeroing failed: %s)", ename(errno));
                    if (fl >= 0 && (fl & O_APPEND))
                        syscall(SYS_fcntl, xfd, F_SETFL, fl);
                }
            }
        }
    }
    if (o->kind == TRUNCATE && xfd >= 0)
        close(xfd);
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
    // さわれないページ (iovec の bad)
    badpage = mmap(NULL, 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (badpage == MAP_FAILED)
        badpage = (void *)16;
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
    // 自分のセッションを作り、op はその中の別のグループの子が動かす: 孤児のプロセスグループの決まり (止めるシグナルを
    // 捨てるか) が、ロボットを動かすところ (端末、init) によらず同じになるように。子の終わりかたをそのまま伝える
    setsid();
    pid_t inner = fork();
    if (inner > 0) {
        int st = 0;
        while (waitpid(inner, &st, 0) < 0 && errno == EINTR)
            ;
        if (WIFSIGNALED(st)) {
            signal(WTERMSIG(st), SIG_DFL);
            raise(WTERMSIG(st));
        }
        _exit(WIFEXITED(st) ? WEXITSTATUS(st) : 2);
    }
    if (inner < 0)
        _exit(2);
    setpgid(0, 0);
    me_pid = getpid();
    sess_pid = getppid();
    memset(kpid, 0, sizeof kpid);
    memset(kgone, 0, sizeof kgone);
    for (int i = 0; i < MAXOPS; i++)
        ksync[i] = -1;
    // コアは作らない (作るかどうかで wait の status の印が変わる。作ると world に出る)
    struct rlimit nocore = {0, 0};
    setrlimit(RLIMIT_CORE, &nocore);
    signal(SIGPIPE, SIG_IGN);
    signal(SIGBUS, onsig);
    signal(SIGSEGV, onsig);
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
    reapkids();
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
        for (size_t i = 0; i < sizeof linux_bugs / sizeof *linux_bugs; i++)
            printf("linuxbug %s %s\n", linux_bugs[i].name, linux_bugs[i].why);
        return 0;
    }
    if (a < argc && !strcmp(argv[a], "-r")) {
        raw = 1;
        a++;
    }
    if (a + 1 < argc && !strcmp(argv[a], "-n")) {
        n = atoi(argv[a + 1]);
        a += 2;
    }
    if (n < 1 || n > MAXOPS || argc - a < 2) {
        fprintf(stderr, "usage: sysdiff [-r] [-n OPS] DIR SEED[:I,J,...]|FROM-TO|@FILE...\n       sysdiff -l\n");
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

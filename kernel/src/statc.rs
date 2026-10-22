// 大きなロックなしの stat と statx (Linux の RCU-walk と seqcount の考え)。ふつうの道の newfstatat は大きなロック (smp.rs) の中で
// パスをたどるので、いくつもの CPU で stat を呼ぶと 1 つずつしか動かない (bench/smp.md: 4 つで 1 つのときの 0.81 倍、
// Linux は 2.11 倍)。Linux は RCU-walk で、ロックを取らずにパスをたどり、とちゅうで変わっていれば (d_seq) ロックを取って
// たどりなおす。ここも同じ考えで、答えを覚えておく:
//   ふつうの道 (大きなロックの中) の newfstatat か statx が答えたら learn が、その答え (file::Stat) と、それが正しいあいだの印
//   (通ったディレクトリの版、inode の中身の版、だれでもは通れないディレクトリの許し (gates)、全体の版) を、ロックの外から
//   読める表 (TABLE) に書く。syscall::fast の newfstatat と statx は lookup で、表にあって印がみないまと同じで、呼んだ人が
//   gates を通れれば、ロックを取らずにそれを struct stat か struct statx にして返す (C の stat は newfstatat、Rust の
//   std::fs::metadata は statx)。ちがえば (と、表にないもの、書いている途中のもの) ふつうの道へ。
//   「ない」(ENOENT) も覚える (シェルの PATH さがし、ライブラリや locale のファイルさがし: いちばん多い stat)。答えるときは
//   ふつうの道と同じわけ (missing_cached) を /proc/ai/why に残す (why::fast_missing。取るのは輪のロックだけ)。
// 版の箱 (DIRS、INOS) は id のハッシュで分けた AtomicU64: vfs が名前やディレクトリの属性を変えるたびに DIRS を、ext4 が
// inode を書くたびに INOS を増やす (箱が同じほかのものが変わっても、古いとみなすだけで、まちがった答えは出さない)。
// マウントやルート、vfs の版の表を空にしたときは EPOCH (みな古い)。
// 表の 1 つは AtomicU64 の並び (seqlock: 書く側は大きなロックを持って、seq を奇数にしてから書いて偶数に。読む側は前後で
// seq を見て、ちがえばふつうの道へ)。Rc にも RefCell にもヒープにもさわらない。
// どのプロセスでも使えるわけではない: はじめのマウントの namespace で、chroot していなくて、fd の表 (cwd) を自分だけで
// 使っているもの (スレッドがいれば、ほかのスレッドが chdir するかもしれない)。ある答えは ext4 のファイルだけ (tmpfs や /proc
// は、中身が変わっても印が変わらない)。「ない」はパスの覚えにあるもの (名前が変われば、どのファイルシステムでも vfs が
// ディレクトリの版を進める)。パスは 192 バイトまで、.. のあるもの、/ で終わるものはふつうの道へ
use crate::file::Stat;
use crate::proc::Proc;
use core::sync::atomic::{AtomicU64, Ordering::{Acquire, Relaxed, Release}};

/// 表の数 (1 つ 512 バイト: 512 KiB。使うのは 1 つ 54 語)
const N: usize = 1024;
/// 版の箱の数
const BOXES: usize = 4096;
/// パスの長さの上限 (先頭の / なし、cwd をつけたあと)
pub const PATH: usize = 192;
/// 覚える、通ったディレクトリと gates の数の上限
const DIRS_MAX: usize = 12;
const GATES_MAX: usize = 4;

// 1 つの並び (AtomicU64 の番号)
const SEQ: usize = 0;
/// パスの長さ | follow << 16 | ディレクトリの数 << 24 | gates の数 << 32 | mg << 40 (statx の時刻の出しかた。sysfile の mgtime)
/// | missing << 41 (答えは ENOENT。inode の版と STAT は見ない)
const SHAPE: usize = 1;
const EPOCH_W: usize = 2;
/// inode の箱の番号と、その版
const INO_BOX: usize = 3;
const INO_GEN: usize = 4;
/// ディレクトリ: 箱の番号 << 48 | 版 (48 ビット)
const DIR0: usize = 5;
/// gates: uid << 32 | gid と、mode を 4 つ (16 ビットずつ)
const GATE0: usize = DIR0 + DIRS_MAX;
const GATE_MODES: usize = GATE0 + GATES_MAX;
const PATH0: usize = GATE_MODES + 1;
/// file::Stat: ino、mode | nlink << 32、uid | gid << 32、rdev、size、blocks、mtime、ctime
const STAT0: usize = PATH0 + PATH / 8;
const WORDS: usize = STAT0 + 8;

#[repr(align(64))]
struct Slot([AtomicU64; 64]);

const Z: AtomicU64 = AtomicU64::new(0);
static TABLE: [Slot; N] = [const { Slot([Z; 64]) }; N];
static DIRS: [AtomicU64; BOXES] = [Z; BOXES];
static INOS: [AtomicU64; BOXES] = [Z; BOXES];
static EPOCH: AtomicU64 = AtomicU64::new(1);
/// 答えた数と、ふつうの道へ行った数 (/proc/bkl)
pub static HITS: AtomicU64 = AtomicU64::new(0);
pub static MISSES: AtomicU64 = AtomicU64::new(0);

const _: () = assert!(WORDS <= 64);

fn box_of(id: (usize, u64)) -> usize {
    ((id.1 ^ (id.0 as u64).rotate_left(40)).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 52) as usize % BOXES
}

/// ディレクトリ dir の中の名前か、dir の属性が変わった (vfs::name_changed、dir_changed)
pub fn dir_bump(dir: (usize, u64)) {
    DIRS[box_of(dir)].fetch_add(1, Release);
}

/// inode id の中身 (大きさ、時刻、許可、リンクの数 ...) が変わった (extfs の write_inode)
pub fn ino_bump(id: (usize, u64)) {
    INOS[box_of(id)].fetch_add(1, Release);
}

/// id の inode の箱の版 (inode を書くたびに増える: ino_bump)。vm::write_fast の許しがまだ新しいか
pub fn ino_ver(id: (usize, u64)) -> u64 {
    INOS[box_of(id)].load(Acquire)
}

/// 覚えをみな古くする (マウント、ルート、vfs の版の表を空にしたとき)
pub fn epoch_bump() {
    EPOCH.fetch_add(1, Release);
}

/// cwd と path をつないで畳む (先頭の / なし、vfs の normal_into と同じ形)。.. のあるもの、. だけ、/ か /. で終わるもの、
/// 長すぎるものは None (ふつうの道へ)
fn normalize(cwd: &[u8], path: &[u8], out: &mut [u8; PATH]) -> Option<usize> {
    if path == b"." || (path.len() > 1 && (path.ends_with(b"/") || path.ends_with(b"/."))) {
        return None;
    }
    let base: &[u8] = if path.first() == Some(&b'/') { b"" } else { cwd };
    let mut n = 0;
    for part in [base, path] {
        for c in part.split(|&b| b == b'/') {
            match c {
                b"" | b"." => {}
                b".." => return None,
                c => {
                    let need = c.len() + (n != 0) as usize;
                    if n + need > PATH {
                        return None;
                    }
                    if n != 0 {
                        out[n] = b'/';
                        n += 1;
                    }
                    out[n..n + c.len()].copy_from_slice(c);
                    n += c.len();
                }
            }
        }
    }
    Some(n)
}

fn hash(p: &[u8], follow: bool) -> usize {
    let h = p.iter().fold(0xcbf2_9ce4_8422_2325u64 ^ follow as u64, |h, &b| (h ^ b as u64).wrapping_mul(0x0100_0000_01b3));
    (h >> 20) as usize % N
}

fn words(p: &[u8]) -> impl Iterator<Item = u64> + '_ {
    p.chunks(8).map(|c| {
        let mut w = [0u8; 8];
        w[..c.len()].copy_from_slice(c);
        u64::from_le_bytes(w)
    })
}

/// このプロセスで使えるか (はじめのマウントの namespace、chroot なし、fd の表を自分だけで)。cwd を返す。
/// ユーザーの namespace の中も使わない (struct stat の uid と gid は、中の番号に読みかえて書くので)
fn usable(p: &Proc) -> Option<&str> {
    if p.cred.ns.mnt.is_some() || p.cred.ns.user.is_some() {
        return None;
    }
    let files = p.files.as_ref()?;
    if !files.private() {
        return None;
    }
    let f = files.get();
    if !f.root.is_empty() {
        return None;
    }
    Some(f.cwd.as_str())
}

/// ふつうの道の newfstatat か statx が答えた (大きなロックの中): 表に書く。mg は sysfile の mgtime
pub fn learn(path: &str, follow: bool, ino: &crate::vfs::InodeRef, st: &Stat, mg: bool) {
    if ino.fast_stat() {
        write(path, follow, Some((ino, st, mg)));
    }
}

/// ふつうの道の newfstatat か statx が ENOENT と答えた (大きなロックの中)
pub fn learn_missing(path: &str, follow: bool) {
    write(path, follow, None);
}

fn write(path: &str, follow: bool, found: Option<(&crate::vfs::InodeRef, &Stat, bool)>) {
    let p = crate::proc::current();
    let Some(cwd) = usable(p) else { return };
    let mut norm = [0u8; PATH];
    let Some(len) = normalize(cwd.as_bytes(), path.as_bytes(), &mut norm) else { return };
    let key = &norm[..len];
    // 通ったところと gates は、パスの覚え (vfs の PATHS) から。いま正しくて、同じ答え (同じ inode か、ない) のものだけ
    let Some((dirs, gates)) = crate::vfs::fast_stat_trail(core::str::from_utf8(key).ok().unwrap_or(""), follow, found.map(|f| f.0)) else { return };
    if dirs.len() > DIRS_MAX || gates.len() > GATES_MAX {
        return;
    }
    let mg = found.is_some_and(|f| f.2);
    let s = &TABLE[hash(key, follow)].0;
    let seq = s[SEQ].load(Relaxed);
    s[SEQ].store(seq | 1, Relaxed);
    core::sync::atomic::fence(Release);
    s[SHAPE].store(len as u64 | (follow as u64) << 16 | (dirs.len() as u64) << 24 | (gates.len() as u64) << 32 | (mg as u64) << 40 | (found.is_none() as u64) << 41, Relaxed);
    s[EPOCH_W].store(EPOCH.load(Acquire), Relaxed);
    if let Some((ino, _, _)) = found {
        let ib = box_of(ino.id());
        s[INO_BOX].store(ib as u64, Relaxed);
        s[INO_GEN].store(INOS[ib].load(Acquire), Relaxed);
    }
    for (i, d) in dirs.iter().enumerate() {
        let b = box_of(*d);
        s[DIR0 + i].store((b as u64) << 48 | (DIRS[b].load(Acquire) & 0xffff_ffff_ffff), Relaxed);
    }
    let mut modes = 0u64;
    for (i, (mode, uid, gid)) in gates.iter().enumerate() {
        s[GATE0 + i].store((*uid as u64) << 32 | *gid as u64, Relaxed);
        modes |= ((*mode & 0xffff) as u64) << (16 * i);
    }
    s[GATE_MODES].store(modes, Relaxed);
    for (i, w) in words(key).enumerate() {
        s[PATH0 + i].store(w, Relaxed);
    }
    if let Some((_, st, _)) = found {
        let w = [st.ino, st.mode as u64 | (st.nlink as u64) << 32, st.uid as u64 | (st.gid as u64) << 32, st.rdev, st.size, st.blocks, st.mtime, st.ctime];
        for (i, w) in w.iter().enumerate() {
            s[STAT0 + i].store(*w, Relaxed);
        }
    }
    s[SEQ].store((seq | 1) + 1, Release);
}

/// ユーザーの path (NUL で終わる文字) を buf に写す (写っているページだけ。なければ None)
fn user_path(p: &Proc, va: usize, buf: &mut [u8; 256]) -> Option<usize> {
    let mut n = 0;
    while n < buf.len() {
        // ページの終わりまでずつ (先のページが写っていなくても、文字がこのページで終われば読める)
        let end = ((va + n) | 0xfff) + 1;
        let take = (end - (va + n)).min(buf.len() - n);
        if !p.pt().copy_in_nofault(va + n, &mut buf[n..n + take]) {
            return None;
        }
        if let Some(z) = buf[n..n + take].iter().position(|&b| b == 0) {
            return Some(n + z);
        }
        n += take;
    }
    None
}

const AT_FDCWD: i64 = -100;
const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
const AT_NO_AUTOMOUNT: u64 = 0x800;

/// 表の答え: あるもの (と mg) か、ない
enum Answer {
    Found(Stat, bool),
    Missing,
}

const NEWFSTATAT: u64 = 79;
const STATX: u64 = 291;
const ENOENT: i64 = 2;

/// syscall::fast の newfstatat: 表にあって、いまも正しければ答える (0 か -ENOENT)。でなければ None (ふつうの道へ)
pub fn lookup(p: &Proc, dirfd: i64, pathp: usize, st: usize, flags: u64) -> Option<i64> {
    let mut norm = [0u8; PATH];
    let r = if flags & !(AT_SYMLINK_NOFOLLOW | AT_NO_AUTOMOUNT) != 0 {
        None
    } else {
        find(p, dirfd, pathp, flags & AT_SYMLINK_NOFOLLOW == 0, &mut norm).and_then(|(a, len)| match a {
            Answer::Found(s, _) => p.pt().copy_out_nofault(st, &s.to_bytes()).then_some(0),
            Answer::Missing => Some(missing(p, NEWFSTATAT, &norm[..len])),
        })
    };
    count(r)
}

/// 「ない」と答える: ふつうの道と同じわけを /proc/ai/why に
fn missing(p: &Proc, nr: u64, key: &[u8]) -> i64 {
    crate::why::fast_missing(p, nr, core::str::from_utf8(key).unwrap_or("?"));
    -ENOENT
}

/// syscall::fast の statx。ふつうの道 (sysfile::statx) が EINVAL にするもの (mask の予約のビット、SYNC の 2 つとも、
/// 知らないフラグ) と AT_EMPTY_PATH、NULL のパスは、ふつうの道へ
pub fn lookup_statx(p: &Proc, dirfd: i64, pathp: usize, flags: u64, mask: u32, buf: usize) -> Option<i64> {
    const AT_STATX_SYNC_TYPE: u64 = 0x6000;
    let r = if mask & 0x8000_0000 != 0 || flags & AT_STATX_SYNC_TYPE == AT_STATX_SYNC_TYPE || flags & !(AT_SYMLINK_NOFOLLOW | AT_NO_AUTOMOUNT | AT_STATX_SYNC_TYPE) != 0 || pathp == 0 {
        None
    } else {
        let mut norm = [0u8; PATH];
        find(p, dirfd, pathp, flags & AT_SYMLINK_NOFOLLOW == 0, &mut norm).and_then(|(a, len)| match a {
            Answer::Found(s, mg) => p.pt().copy_out_nofault(buf, &s.to_statx(mask, mg)).then_some(0),
            Answer::Missing => Some(missing(p, STATX, &norm[..len])),
        })
    };
    count(r)
}

/// 大きなロックなしの open (file::fast_open) が引く答え: リンクをたどった stat か、ない
pub enum Open {
    Found(Stat),
    Missing,
}

/// file::fast_open: path (dirfd からの、リンクをたどる) が表にあって、いまも正しく、呼んだ人が途中を通れれば、その答えと
/// norm に畳んだパスの長さ。でなければ None (ふつうの道へ)。stat の数 (HITS / MISSES) には入れない
pub fn open_lookup(p: &Proc, dirfd: i64, pathp: usize, norm: &mut [u8; PATH]) -> Option<(Open, usize)> {
    find(p, dirfd, pathp, true, norm).map(|(a, len)| {
        let a = match a {
            Answer::Found(s, _) => Open::Found(s),
            Answer::Missing => Open::Missing,
        };
        (a, len)
    })
}

/// 全体の版 (マウントやルートが変わると増える: epoch_bump)。file の棚 (Shelf) が、しまったときの版とくらべる
pub fn epoch() -> u64 {
    EPOCH.load(Acquire)
}

fn count(r: Option<i64>) -> Option<i64> {
    if r.is_some() { &HITS } else { &MISSES }.fetch_add(1, Relaxed);
    r
}

/// 表から答えを探す: (答え, norm に畳んだパスの長さ)。表にないか、古いか、通れないかもしれなければ None
fn find(p: &Proc, dirfd: i64, pathp: usize, follow: bool, norm: &mut [u8; PATH]) -> Option<(Answer, usize)> {
    // しくじらせる決まり (fail.rs) があれば、それを通すためにふつうの道へ
    if crate::fail::ON.load(Relaxed) {
        return None;
    }
    let mut raw = [0u8; 256];
    let rlen = user_path(p, pathp, &mut raw)?;
    let path = &raw[..rlen];
    // 開いているディレクトリからのものは、ふつうの道 (fd を見るのにロックがいる)。絶対のパスは dirfd を見ない
    if path.is_empty() || (dirfd != AT_FDCWD && path[0] != b'/') {
        return None;
    }
    // /dev/stdin や /proc/self/fd/N (sysfile の fd_link) は、ふつうの道
    if path.starts_with(b"/dev/") || path.starts_with(b"/proc/") {
        return None;
    }
    let cwd = usable(p)?;
    let len = normalize(cwd.as_bytes(), path, norm)?;
    let key = &norm[..len];
    let s = &TABLE[hash(key, follow)].0;
    let seq = s[SEQ].load(Acquire);
    if seq & 1 != 0 || seq == 0 {
        return None;
    }
    let shape = s[SHAPE].load(Relaxed);
    let (slen, sfollow, ndirs, ngates) = ((shape & 0xffff) as usize, shape >> 16 & 1 != 0, (shape >> 24 & 0xff) as usize, (shape >> 32 & 0xff) as usize);
    if slen != len || sfollow != follow || ndirs > DIRS_MAX || ngates > GATES_MAX {
        return None;
    }
    if s[EPOCH_W].load(Relaxed) != EPOCH.load(Acquire) {
        return None;
    }
    let is_missing = shape >> 41 & 1 != 0;
    let ib = s[INO_BOX].load(Relaxed) as usize % BOXES;
    if !is_missing && s[INO_GEN].load(Relaxed) != INOS[ib].load(Acquire) {
        return None;
    }
    for i in 0..ndirs {
        let w = s[DIR0 + i].load(Relaxed);
        if w & 0xffff_ffff_ffff != DIRS[(w >> 48) as usize % BOXES].load(Acquire) & 0xffff_ffff_ffff {
            return None;
        }
    }
    if !words(key).enumerate().all(|(i, w)| s[PATH0 + i].load(Relaxed) == w) {
        return None;
    }
    // だれでもは通れないディレクトリを、呼んだ人が通れるか (覚えたのはほかの人かもしれない)
    let modes = s[GATE_MODES].load(Relaxed);
    for i in 0..ngates {
        let g = s[GATE0 + i].load(Relaxed);
        let m = crate::vfs::Meta { mode: (modes >> (16 * i) & 0xffff) as u32, uid: (g >> 32) as u32, gid: g as u32, ..crate::vfs::Meta::default() };
        if !p.cred.may(&m, crate::cred::X, false) {
            return None;
        }
    }
    let mut w = [0u64; 8];
    for (i, w) in w.iter_mut().enumerate() {
        *w = s[STAT0 + i].load(Relaxed);
    }
    // 読んでいるあいだに書かれていなければ、正しい
    core::sync::atomic::fence(Acquire);
    if s[SEQ].load(Relaxed) != seq {
        return None;
    }
    if is_missing {
        return Some((Answer::Missing, len));
    }
    let st = Stat { ino: w[0], mode: w[1] as u32, nlink: (w[1] >> 32) as u32, uid: w[2] as u32, gid: (w[2] >> 32) as u32, rdev: w[3], size: w[4], blocks: w[5], mtime: w[6], ctime: w[7] };
    Some((Answer::Found(st, shape >> 40 & 1 != 0), len))
}

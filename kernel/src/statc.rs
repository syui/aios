// 大きなロックなしの stat と statx (Linux の RCU-walk と seqcount の考え)。ふつうの道の newfstatat は大きなロック (smp.rs) の中で
// パスをたどるので、いくつもの CPU で stat を呼ぶと 1 つずつしか動かない (bench/smp.md: 4 つで 1 つのときの 0.81 倍、
// Linux は 2.11 倍)。Linux は RCU-walk で、ロックを取らずにパスをたどり、とちゅうで変わっていれば (d_seq) ロックを取って
// たどりなおす。ここも同じ考えで、答えを覚えておく:
//   ふつうの道 (大きなロックの中) の newfstatat か statx が答えたら learn が、その答え (file::Stat) と、それが正しいあいだの印
//   (通ったところ (ディレクトリと、そこで引いた名前) の版、inode の中身の版、だれでもは通れないディレクトリの許し (gates)、
//   全体の版) を、ロックの外から
//   読める表 (TABLE) に書く。syscall::fast の newfstatat と statx は lookup で、表にあって印がみないまと同じで、呼んだ人が
//   gates を通れれば、ロックを取らずにそれを struct stat か struct statx にして返す (C の stat は newfstatat、Rust の
//   std::fs::metadata は statx)。ちがえば (と、表にないもの、書いている途中のもの) ふつうの道へ。
//   「ない」(ENOENT) も覚える (シェルの PATH さがし、ライブラリや locale のファイルさがし: いちばん多い stat)。答えるときは
//   ふつうの道と同じわけ (missing_cached) を /proc/ai/why に残す (why::fast_missing。取るのは輪のロックだけ)。
//   大きなロックなしの O_CREAT (fastdir.rs) が作ったファイルは learn_new が、親の答えに 1 つ足して書く。
// 版の箱 (NAMES、INOS) はハッシュで分けた AtomicU64: vfs が (ディレクトリ, 名前) を変える (作る・消す・名前を変える) たびに
// その NAMES を、ext4 が inode を書くたびに INOS を増やす (箱が同じほかのものが変わっても、古いとみなすだけで、まちがった
// 答えは出さない)。通ったところは名前ごと (Linux の dentry ごとの d_seq): ディレクトリに名前を足しても、ほかの名前の答えは
// 古くならない。マウントやルート、vfs の版の表を空にしたとき、ディレクトリの属性 (chmod、chown) を変えたときは EPOCH (みな古い)。
// 表の 1 つは AtomicU64 の並び (seqlock: 書く側は seq を CAS で奇数にしてから書いて偶数に (取れなければ、ほかの人が書いている
// ので書かない: 覚えなので、なくてもよい)。読む側は前後で
// seq を見て、ちがえばふつうの道へ)。Rc にも RefCell にもヒープにもさわらない。
// どのプロセスでも使えるわけではない: はじめのマウントの namespace で、chroot していなくて、fd の表 (cwd) を自分だけで
// 使っているもの (スレッドがいれば、ほかのスレッドが chdir するかもしれない)。ある答えは ext4 のファイルだけ (tmpfs や /proc
// は、中身が変わっても印が変わらない)。「ない」はパスの覚えにあるもの (名前が変われば、どのファイルシステムでも vfs が
// その名前の版を進める)。パスは 192 バイトまで、.. のあるもの、/ で終わるものはふつうの道へ
use crate::file::Stat;
use crate::proc::Proc;
use core::sync::atomic::{AtomicU64, Ordering::{Acquire, Relaxed, Release}};

/// 表の数 (1 つ 512 バイト: 4 MiB。64 語をみな使う)。WAYS こずつの組に分け、パスのハッシュで組を選び、組の中の
/// どれにでも入れる (前は 1024 で、1 つの場所に 1 つだけ: 1000 のファイルを順に stat すると、同じ場所になったものが
/// 追い出しあって、2 回目も 4 割しか当たらなかった。Linux の dcache は、ぶつかっても追い出さない)。4096 のときは、
/// 1000 のファイルとないもの 1000 (fsbench) で 3000 ほどになると、組の 4 つからあふれたものが 16% ほど追い出され、
/// それを開くとふつうの道 (50 us) へ行っていた
const N: usize = 8192;
const WAYS: usize = 4;
const SETS: usize = N / WAYS;
/// inode の版の箱の数
const BOXES: usize = 4096;
/// (ディレクトリ, 名前) の版の箱の数 (番号は 16 ビット: DIR0 の語の上。512 KiB)
const NAME_BOXES: usize = 65536;
/// パスの長さの上限 (先頭の / なし、cwd をつけたあと)
pub const PATH: usize = 192;
/// 覚える、通ったディレクトリと gates の数の上限。ディレクトリは 1 つの 64 語に入るだけ (12 のときは、/home/USER の下の
/// 10 段のパス (fsbench の deep-stat) がいつも入らず、そのたびに表を引いて外れ、ふつうの道で覚えようとして断っていた)
const DIRS_MAX: usize = 22;
const GATES_MAX: usize = 4;

// 1 つの並び (AtomicU64 の番号)
const SEQ: usize = 0;
/// パスの長さ | follow << 16 | ディレクトリの数 << 24 | gates の数 << 32 | mg << 40 (statx の時刻の出しかた。sysfile の mgtime)
/// | missing << 41 (答えは ENOENT。inode の版と STAT は見ない) | plain << 42 (リンクをたどらずに着いた: たどったパスが
/// パスそのもの。大きなロックなしの open が新しく開くときの、ファイルのパスにできる)
const SHAPE: usize = 1;
const EPOCH_W: usize = 2;
/// inode の箱の番号と、その版
const INO_BOX: usize = 3;
const INO_GEN: usize = 4;
/// 通ったところ ((ディレクトリ, 名前) ごと): 箱の番号 (name_box) << 48 | 版 (48 ビット)
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
/// 組ごとの、次に追い出す番 (書く人 (大きなロックの中の learn と、ロックなしで作ったファイルの learn_new) がいっしょに
/// 進めても、追い出すものが同じになるだけ)
static NEXT: [core::sync::atomic::AtomicU8; SETS] = [const { core::sync::atomic::AtomicU8::new(0) }; SETS];
static NAMES: [AtomicU64; NAME_BOXES] = [Z; NAME_BOXES];
static INOS: [AtomicU64; BOXES] = [Z; BOXES];
static EPOCH: AtomicU64 = AtomicU64::new(1);

/// 答えた数と、ふつうの道へ行った数 (/proc/bkl)
pub static HITS: AtomicU64 = AtomicU64::new(0);
pub static MISSES: AtomicU64 = AtomicU64::new(0);

const _: () = assert!(WORDS == 64);

fn box_of(id: (usize, u64)) -> usize {
    ((id.1 ^ (id.0 as u64).rotate_left(40)).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 52) as usize % BOXES
}

/// (dir, name) の箱 (name は vfs::name_hash)
pub fn name_box(dir: (usize, u64), name: u64) -> usize {
    ((dir.1 ^ (dir.0 as u64).rotate_left(40) ^ name.rotate_left(21)).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 48) as usize % NAME_BOXES
}

/// ディレクトリ dir の中の名前 name (vfs::name_hash) が変わった (vfs::name_changed)
pub fn name_bump(dir: (usize, u64), name: u64) {
    NAMES[name_box(dir, name)].fetch_add(1, Release);
}

/// inode id の中身 (大きさ、時刻、許可、リンクの数 ...) が変わった (extfs の write_inode)
pub fn ino_bump(id: (usize, u64)) {
    INOS[box_of(id)].fetch_add(1, Release);
}

/// id の inode の箱の版 (inode を書くたびに増える: ino_bump)。vm::write_fast の許しがまだ新しいか
pub fn ino_ver(id: (usize, u64)) -> u64 {
    INOS[box_of(id)].load(Acquire)
}

/// 箱 b (name_box) の版 (vfs::fast_stat_trail が、パスの覚えがいまも正しいかを確かめる前に読む)
pub fn box_ver(b: usize) -> u64 {
    NAMES[b % NAME_BOXES].load(Acquire)
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
    // 終わりでよく混ぜる (FNV-1a のままビットを取ると、最後の 1 文字だけちがうパス: w0 w1 w2、file1 file2 ... がみな同じ
    // 場所になり、たがいに追い出しあっていた)
    let h = (h ^ (h >> 31)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    (h ^ (h >> 29)) as usize % SETS
}

/// 組の中で key (と follow) のものが入っている場所 (seq で守らずに見るだけ: 答えるまえに check が確かめなおす)
fn slot_of(set: usize, key: &[u8], follow: bool) -> Option<&'static [AtomicU64; 64]> {
    (0..WAYS).map(|w| &TABLE[set * WAYS + w].0).find(|s| {
        let shape = s[SHAPE].load(Relaxed);
        s[SEQ].load(Relaxed) != 0 && (shape & 0xffff) as usize == key.len() && (shape >> 16 & 1 != 0) == follow && words(key).enumerate().all(|(i, w)| s[PATH0 + i].load(Relaxed) == w)
    })
}

/// 書く場所: 同じ key のものがあればそこ、なければ空きか古い (全体の版がちがう) もの、なければ組の中で順に追い出す
fn slot_for(set: usize, key: &[u8], follow: bool) -> &'static [AtomicU64; 64] {
    if let Some(s) = slot_of(set, key, follow) {
        return s;
    }
    let epoch = EPOCH.load(Relaxed);
    if let Some(s) = (0..WAYS).map(|w| &TABLE[set * WAYS + w].0).find(|s| s[SEQ].load(Relaxed) == 0 || s[EPOCH_W].load(Relaxed) != epoch) {
        return s;
    }
    let w = NEXT[set].load(Relaxed) as usize % WAYS;
    NEXT[set].store(((w + 1) % WAYS) as u8, Relaxed);
    &TABLE[set * WAYS + w].0
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
pub fn learn(path: &str, follow: bool, ino: &crate::vfs::InodeRef, mg: bool) {
    if ino.fast_stat() {
        write(path, follow, Some((ino, mg)));
    }
}

/// O_CREAT の openat と unlinkat のふつうの道 (大きなロックの中): 親のディレクトリ (dir、パス path) を覚える。次からは
/// 大きなロックなしの作る・消す (fastdir.rs) がこれで親を引く。statx もこの答えを使うので、mg は statx が覚えさせるときと同じ
pub fn learn_dir(path: &str, dir: &crate::vfs::InodeRef, mg: bool) {
    learn(path, true, dir, mg);
}

/// ふつうの道の newfstatat か statx が ENOENT と答えた (大きなロックの中)
pub fn learn_missing(path: &str, follow: bool) {
    write(path, follow, None);
}

fn write(path: &str, follow: bool, found: Option<(&crate::vfs::InodeRef, bool)>) {
    let p = crate::proc::current();
    let Some(cwd) = usable(p) else { return };
    let mut norm = [0u8; PATH];
    let Some(len) = normalize(cwd.as_bytes(), path.as_bytes(), &mut norm) else { return };
    let key = &norm[..len];
    // 通ったディレクトリは名前の数より少なくならない (/ も入る)。入りきらないものは、パスの覚えを引く前にやめる
    if key.iter().filter(|&&b| b == b'/').count() + 1 > DIRS_MAX {
        return;
    }
    // 通ったところと gates は、パスの覚え (vfs の PATHS) から。いま正しくて、同じ答え (同じ inode か、ない) のものだけ
    let Some((dirs, gates, plain)) = crate::vfs::fast_stat_trail(core::str::from_utf8(key).ok().unwrap_or(""), follow, found.map(|f| f.0)) else { return };
    if dirs.len() > DIRS_MAX || gates.len() > GATES_MAX {
        return;
    }
    let mg = found.is_some_and(|f| f.1);
    // inode の版を読んでから中身を (大きなロックなしの作る・消すが、そのあいだに inode を書けば、版が古いので答えない)
    let epoch = EPOCH.load(Acquire);
    let ino_ver = found.map(|(ino, _)| {
        let ib = box_of(ino.id());
        (ib, INOS[ib].load(Acquire), Stat::of_inode(ino))
    });
    // ディレクトリの版は、パスの覚えを確かめる前に読んだもの (vfs::fast_stat_trail)
    let mut dw = [0u64; DIRS_MAX];
    for (i, (b, sv)) in dirs.iter().enumerate() {
        dw[i] = (*b as u64) << 48 | (sv & 0xffff_ffff_ffff);
    }
    let mut gw = [0u64; GATES_MAX];
    let mut modes = 0u64;
    for (i, (mode, uid, gid)) in gates.iter().enumerate() {
        gw[i] = (*uid as u64) << 32 | *gid as u64;
        modes |= ((*mode & 0xffff) as u64) << (16 * i);
    }
    put(key, follow, &dw[..dirs.len()], &gw[..gates.len()], modes, epoch, mg, plain, ino_ver.as_ref());
}

/// 表に書く: dirs は (箱 << 48 | 版)、gates は (uid << 32 | gid) で、その mode は modes に 16 ビットずつ。
/// found は (inode の箱, その版, 答え)。None なら「ない」。plain はリンクをたどらずに着いたもの (SHAPE)
#[allow(clippy::too_many_arguments)]
fn put(key: &[u8], follow: bool, dirs: &[u64], gates: &[u64], modes: u64, epoch: u64, mg: bool, plain: bool, found: Option<&(usize, u64, Stat)>) {
    let s = slot_for(hash(key, follow), key, follow);
    // 書く人は 1 人ずつ (ほかの人が書いていれば書かない)
    let seq = s[SEQ].load(Relaxed);
    if seq & 1 != 0 || s[SEQ].compare_exchange(seq, seq | 1, Acquire, Relaxed).is_err() {
        return;
    }
    core::sync::atomic::fence(Release);
    s[SHAPE].store(key.len() as u64 | (follow as u64) << 16 | (dirs.len() as u64) << 24 | (gates.len() as u64) << 32 | (mg as u64) << 40 | (found.is_none() as u64) << 41 | (plain as u64) << 42, Relaxed);
    s[EPOCH_W].store(epoch, Relaxed);
    if let Some((ib, v, _)) = found {
        s[INO_BOX].store(*ib as u64, Relaxed);
        s[INO_GEN].store(*v, Relaxed);
    }
    for (i, w) in dirs.iter().enumerate() {
        s[DIR0 + i].store(*w, Relaxed);
    }
    for (i, w) in gates.iter().enumerate() {
        s[GATE0 + i].store(*w, Relaxed);
    }
    s[GATE_MODES].store(modes, Relaxed);
    for (i, w) in words(key).enumerate() {
        s[PATH0 + i].store(w, Relaxed);
    }
    if let Some((_, _, st)) = found {
        let w = [st.ino, st.mode as u64 | (st.nlink as u64) << 32, st.uid as u64 | (st.gid as u64) << 32, st.rdev, st.size, st.blocks, st.mtime, st.ctime];
        for (i, w) in w.iter().enumerate() {
            s[STAT0 + i].store(*w, Relaxed);
        }
    }
    s[SEQ].store((seq | 1) + 1, Release);
}

/// 大きなロックなしの O_CREAT (fastdir.rs) が、表にある親のディレクトリ parent (畳んだパス) に作ったファイル name (ino) を
/// 書く: 親の答えの印に、親で name を引いたこと ((dir, name) の箱と、作ったあとの版) を足したもの (Linux の d_instantiate:
/// 作ったばかりのファイルの stat も、たどらずに答える。前はふつうの道で作ったものだけがパスの覚えに入り、ロックなしで
/// 作ったものは、はじめの stat がふつうの道でたどりなおしていた)。before は作る前の (dir, name) の箱の版 (box_ver): いま
/// ちょうど 1 つ進んでいれば (進めたのが自分の name_changed だけなら) 書く。ほかの人が同じ箱を進めていれば (そのファイルを
/// 消したかもしれない) 書かない。id、st、ver は作ったファイルの inode と、その stat と inode の版 (ExtFs::fast_create が、
/// まだだれも引けないあいだに取ったもの)
pub fn learn_new(parent: &[u8], name: &str, dir: (usize, u64), before: u64, id: (usize, u64), st: Stat, ver: u64) {
    let len = parent.len() + (!parent.is_empty()) as usize + name.len();
    if len > PATH {
        return;
    }
    let mut buf = [0u8; PATH];
    let mut n = 0;
    for part in [parent, if parent.is_empty() { b"" } else { b"/" }, name.as_bytes()] {
        buf[n..n + part.len()].copy_from_slice(part);
        n += part.len();
    }
    let key = &buf[..len];
    let epoch = EPOCH.load(Acquire);
    let ino_ver = (box_of(id), ver, st);
    let dbox = name_box(dir, crate::vfs::name_hash(name));
    if NAMES[dbox].load(Acquire) != before.wrapping_add(1) {
        return;
    }
    // 親の答え (check と同じに、seq のあいだに読む)。いまも正しいディレクトリのものだけ
    let Some(s) = slot_of(hash(parent, true), parent, true) else { return };
    let seq = s[SEQ].load(Acquire);
    let shape = s[SHAPE].load(Relaxed);
    let (ndirs, ngates) = ((shape >> 24 & 0xff) as usize, (shape >> 32 & 0xff) as usize);
    if seq & 1 != 0 || shape >> 41 & 1 != 0 || ndirs + 1 > DIRS_MAX || ngates > GATES_MAX || s[EPOCH_W].load(Relaxed) != epoch {
        return;
    }
    let mut dw = [0u64; DIRS_MAX];
    for (i, w) in dw.iter_mut().enumerate().take(ndirs) {
        *w = s[DIR0 + i].load(Relaxed);
        if *w & 0xffff_ffff_ffff != NAMES[(*w >> 48) as usize].load(Acquire) & 0xffff_ffff_ffff {
            return;
        }
    }
    let mut gw = [0u64; GATES_MAX];
    for (i, w) in gw.iter_mut().enumerate().take(ngates) {
        *w = s[GATE0 + i].load(Relaxed);
    }
    let mut modes = s[GATE_MODES].load(Relaxed);
    let (pmode, owner) = (s[STAT0 + 1].load(Relaxed) as u32, s[STAT0 + 2].load(Relaxed));
    core::sync::atomic::fence(Acquire);
    if s[SEQ].load(Relaxed) != seq || pmode & 0o170000 != 0o040000 {
        return;
    }
    // 親もだれでもは通れなければ gates に (vfs の walk と同じ)。owner は uid | gid << 32、gates は uid << 32 | gid
    let mut ngates = ngates;
    if pmode & 0o111 != 0o111 {
        if ngates == GATES_MAX {
            return;
        }
        gw[ngates] = (owner & 0xffff_ffff) << 32 | owner >> 32;
        modes |= ((pmode & 0xffff) as u64) << (16 * ngates);
        ngates += 1;
    }
    dw[ndirs] = (dbox as u64) << 48 | (before.wrapping_add(1) & 0xffff_ffff_ffff);
    // 親がリンクをたどらずに着いたものなら、作ったファイルも
    put(key, true, &dw[..ndirs + 1], &gw[..ngates], modes, epoch, true, shape >> 42 & 1 != 0, Some(&ino_ver));
}

/// ユーザーの path (NUL で終わる文字) を buf に写す (写っているページだけ。なければ None)
pub(crate) fn user_path(p: &Proc, va: usize, buf: &mut [u8; 256]) -> Option<usize> {
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
    /// stat、mg、plain (SHAPE)、fresh (inode の中身の版がいまのもの: stat がいまの inode のもの)
    Found(Stat, bool, bool, bool),
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
        find(p, dirfd, pathp, flags & AT_SYMLINK_NOFOLLOW == 0, true, &mut norm).and_then(|(a, len)| match a {
            Answer::Found(s, ..) => p.pt().copy_out_nofault(st, &s.to_bytes()).then_some(0),
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
        find(p, dirfd, pathp, flags & AT_SYMLINK_NOFOLLOW == 0, true, &mut norm).and_then(|(a, len)| match a {
            Answer::Found(s, mg, ..) => p.pt().copy_out_nofault(buf, &s.to_statx(mask, mg)).then_some(0),
            Answer::Missing => Some(missing(p, STATX, &norm[..len])),
        })
    };
    count(r)
}

/// 大きなロックなしの open (file::fast_open) が引く答え: リンクをたどった stat と、リンクをたどらずに着いたか、stat が
/// いまの inode のものか (書いたあとは inode の版が進んで古い。名前と inode の番号は正しい) か、ない
pub enum Open {
    Found(Stat, bool, bool),
    Missing,
}

/// file::fast_open: path (dirfd からの、リンクをたどる) が表にあって、通ったところがいまも同じで、呼んだ人が途中を
/// 通れれば、その答えと norm に畳んだパスの長さ。でなければ None (ふつうの道へ)。stat の数 (HITS / MISSES) には入れない
pub fn open_lookup(p: &Proc, dirfd: i64, pathp: usize, norm: &mut [u8; PATH]) -> Option<(Open, usize)> {
    find(p, dirfd, pathp, true, false, norm).map(|(a, len)| {
        let a = match a {
            Answer::Found(s, _, plain, fresh) => Open::Found(s, plain, fresh),
            Answer::Missing => Open::Missing,
        };
        (a, len)
    })
}

/// 大きなロックなしの open (file::fast_open) が、表の答えがいまも正しいと確かめてから、ふつうの道へ渡したもの
/// (CPU ごとに、スレッドとパスのアドレス)。ふつうの道の openat は、これを同じ答えでもういちど覚えない: 棚にない
/// ファイルを次々に開くと、開くたびに表を書きなおしていた (TCG で 7 us)。ほかの CPU へ移ったり、ちがうものが残って
/// いたりしても、覚えるか覚えないかだけ (答えは変わらない)
static SEEN: [(core::sync::atomic::AtomicUsize, core::sync::atomic::AtomicUsize); crate::smp::MAXCPU] = [const { (core::sync::atomic::AtomicUsize::new(0), core::sync::atomic::AtomicUsize::new(0)) }; crate::smp::MAXCPU];

/// fast_open が、スレッド tid のパス pathp の答えを確かめてから、ふつうの道へ
pub fn seen_set(tid: u32, pathp: usize) {
    let s = &SEEN[crate::smp::id() % crate::smp::MAXCPU];
    s.0.store(tid as usize, Relaxed);
    s.1.store(pathp, Relaxed);
}

/// ふつうの道の openat: いまのシステムコールの前に、fast_open が同じものを確かめたか (印は消す)
pub fn seen_take(tid: u32, pathp: usize) -> bool {
    let s = &SEEN[crate::smp::id() % crate::smp::MAXCPU];
    let same = pathp != 0 && s.0.load(Relaxed) == tid as usize && s.1.load(Relaxed) == pathp;
    s.1.store(0, Relaxed);
    same
}

/// 全体の版 (マウントやルートが変わると増える: epoch_bump)。file の棚 (Shelf) が、しまったときの版とくらべる
pub fn epoch() -> u64 {
    EPOCH.load(Acquire)
}

fn count(r: Option<i64>) -> Option<i64> {
    if r.is_some() { &HITS } else { &MISSES }.fetch_add(1, Relaxed);
    r
}

/// 表から答えを探す: (答え, norm に畳んだパスの長さ)。表にないか、古いか、通れないかもしれなければ None。ino なら
/// inode の中身の版も (でなければ、古いかは答えの fresh に)
fn find(p: &Proc, dirfd: i64, pathp: usize, follow: bool, ino: bool, norm: &mut [u8; PATH]) -> Option<(Answer, usize)> {
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
    check(p, &norm[..len], follow, ino).map(|a| (a, len))
}

/// 大きなロックなしの作る・消す (fastdir.rs): ディレクトリ dpath (dirfd からの、リンクをたどる。空なら cwd) が表にあって、
/// 通ったところがいまも同じで、呼んだ人が途中を通れれば、その inode の番号と、norm に畳んだパスの長さ。ディレクトリの
/// inode の中身の版は見ない (そのディレクトリに作るたびに進む): 許しや、いまもディレクトリかは、呼ぶ側が ext4 のロックの
/// 中で inode から読む
pub fn dir_lookup(p: &Proc, dirfd: i64, dpath: &[u8], norm: &mut [u8; PATH]) -> Option<(u64, usize)> {
    if crate::fail::ON.load(Relaxed) || (dirfd != AT_FDCWD && dpath.first() != Some(&b'/')) {
        return None;
    }
    if dpath.starts_with(b"/dev/") || dpath.starts_with(b"/proc/") || dpath == b"/dev" || dpath == b"/proc" {
        return None;
    }
    let cwd = usable(p)?;
    let len = normalize(cwd.as_bytes(), dpath, norm)?;
    match check(p, &norm[..len], true, false)? {
        Answer::Found(st, ..) if st.mode & 0o170000 == 0o040000 => Some((st.ino, len)),
        _ => None,
    }
}

/// 大きなロックなしの open (fastdir::fast_open): 表で引いたファイルの親 (畳んだパス key) のディレクトリが、表にあって
/// いまも正しく、呼んだ人が途中を通れれば、その inode の番号 (許しや、いまもディレクトリかは、呼ぶ側が ext4 のロックの
/// 中で inode から読む)
pub fn dir_of(p: &Proc, key: &[u8]) -> Option<u64> {
    match check(p, key, true, false)? {
        Answer::Found(st, ..) if st.mode & 0o170000 == 0o040000 => Some(st.ino),
        _ => None,
    }
}

/// key (畳んだパス) の答えが表にあって、いまも正しく、呼んだ人が途中を通れれば、その答え。ino なら inode の中身の版も見る
/// (でなければ、同じかを答えの fresh に)
fn check(p: &Proc, key: &[u8], follow: bool, ino: bool) -> Option<Answer> {
    let len = key.len();
    let s = slot_of(hash(key, follow), key, follow)?;
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
    let fresh = is_missing || s[INO_GEN].load(Relaxed) == INOS[ib].load(Acquire);
    if ino && !fresh {
        return None;
    }
    for i in 0..ndirs {
        let w = s[DIR0 + i].load(Relaxed);
        if w & 0xffff_ffff_ffff != NAMES[(w >> 48) as usize].load(Acquire) & 0xffff_ffff_ffff {
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
        return Some(Answer::Missing);
    }
    let st = Stat { ino: w[0], mode: w[1] as u32, nlink: (w[1] >> 32) as u32, uid: w[2] as u32, gid: (w[2] >> 32) as u32, rdev: w[3], size: w[4], blocks: w[5], mtime: w[6], ctime: w[7] };
    Some(Answer::Found(st, shape >> 40 & 1 != 0, shape >> 42 & 1 != 0, fresh))
}

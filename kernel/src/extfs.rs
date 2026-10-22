// ext2 / ext4。書きかえたブロックは印をつけておき、操作の終わりにまとめて書き出す
//
// 読み書きできるもの: 直接/間接ブロック (ext2)、extents・64bit・flex_bg・
// metadata_csum (crc32c)・未初期化グループ (ext4)、filetype、sparse_super、large_file
// ext4 のジャーナル (jbd2) を使う: メタデータはジャーナルに書いてから本当の場所へ
// (data=ordered: ファイルの中身は先に直接)。マウントのときに残っていれば再生する (jbd2.rs)。
// htree で索引のついたディレクトリは、索引を保ったまま名前を足す (htree.rs)。
use crate::kalloc;
use crate::memlayout::PGSIZE;
use crate::tmpfs::statfs_bytes;
use crate::vfs::*;
use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;
use core::cell::{Cell, RefCell};
use core::mem::ManuallyDrop;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

// ---- ext4 のロック ----
// 大きなロック (smp.rs) のほかに、ext4 の中 (ExtFs の RefCell、ブロックのキャッシュ、ExtInode の数と Rc<ExtFs> の数) を守る。
// 大きなロックの中の道も、ext4 に入るところ (ExtInode の Inode の関数、ExtInode の Drop、外から呼ぶ ExtFs の関数) でいつも取る。
// 大きなロックなしの作る・消す (fastdir.rs) はこれだけを取る: ext4 の中は 1 つずつ、そのほか (パスをたどる、fd を置く、
// ユーザーとのやりとり) はいくつもの CPU で同時に (Linux はディレクトリごとの i_rwsem とグループごとのロック)。
// 持ち主はスレッド (proc の番号。スケジューラの中は CPU) で、同じスレッドなら重ねて取れる。眠るとき (proc::sched) は手放し、
// 起きたらまた取る (大きなロックと同じく、眠っているあいだにほかが ext4 を変えてよい: with_block の wgen がそれを見る)。
// 取る順は 大きなロック → ext4 のロック → fd の表ごとのスピンロック
static FS_OWNER: AtomicUsize = AtomicUsize::new(0);
/// 重ねて取った数 (持ち主だけが書く)
static FS_DEPTH: AtomicUsize = AtomicUsize::new(0);
/// 待った回数と時間 (ns) (/proc/bkl)
pub static FS_WAIT: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

fn fs_me() -> usize {
    crate::proc::cur_index().unwrap_or(crate::proc::NPROC + crate::smp::id()) + 1
}

/// ext4 のロックを持っているしるし (落とすと放す)
pub struct FsGuard(());

pub fn fs_lock() -> FsGuard {
    let me = fs_me();
    if FS_OWNER.load(Ordering::Relaxed) == me {
        FS_DEPTH.store(FS_DEPTH.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
    } else {
        fs_acquire(me);
        FS_DEPTH.store(1, Ordering::Relaxed);
    }
    FsGuard(())
}

fn fs_acquire(me: usize) {
    if FS_OWNER.compare_exchange(0, me, Ordering::Acquire, Ordering::Relaxed).is_ok() {
        return;
    }
    let t0 = crate::timer::uptime_ns();
    loop {
        // 放す側が sev で起こす (大きなロックと同じ。回りつづけると TCG では持っている CPU まで遅くなる)
        unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
        if FS_OWNER.load(Ordering::Relaxed) == 0 && FS_OWNER.compare_exchange(0, me, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            break;
        }
    }
    FS_WAIT[0].fetch_add(1, Ordering::Relaxed);
    FS_WAIT[1].fetch_add(crate::timer::uptime_ns() - t0, Ordering::Relaxed);
}

fn fs_release() {
    FS_DEPTH.store(0, Ordering::Relaxed);
    FS_OWNER.store(0, Ordering::Release);
    unsafe { core::arch::asm!("dsb ish", "sev", options(nostack)) };
}

impl Drop for FsGuard {
    fn drop(&mut self) {
        let d = FS_DEPTH.load(Ordering::Relaxed);
        if d > 1 {
            FS_DEPTH.store(d - 1, Ordering::Relaxed);
        } else {
            fs_release();
        }
    }
}

/// 眠る前 (proc::sched): いまのスレッドが ext4 のロックを持っていれば手放し、重ねた数を返す (0 なら持っていない)
pub fn fs_unlock_for_sleep() -> usize {
    if FS_OWNER.load(Ordering::Relaxed) != fs_me() {
        return 0;
    }
    let d = FS_DEPTH.load(Ordering::Relaxed);
    fs_release();
    d
}

/// 起きたあと: fs_unlock_for_sleep が手放したものを取りなおす
pub fn fs_relock_after_sleep(d: usize) {
    if d > 0 {
        fs_acquire(fs_me());
        FS_DEPTH.store(d, Ordering::Relaxed);
    }
}

/// システムコールや例外から戻るとき: ext4 のロックを持ったままなら、取ったところと放すところの組がこわれている
/// (そのままでは ext4 に入るものがみな止まる)
pub fn fs_check_released() {
    if FS_OWNER.load(Ordering::Relaxed) == fs_me() {
        panic!("extfs: the ext4 lock is still held on the way out (depth {})", FS_DEPTH.load(Ordering::Relaxed));
    }
}

/// ルートの ext4 (ExtFs::root が 1 度だけ入れる。それからは変えない)
static mut ROOT_FS: Option<Rc<ExtFs>> = None;

#[path = "htree.rs"]
mod htree;
#[path = "jbd2.rs"]
mod jbd2;

const MAGIC: u16 = 0xef53;
const ROOT_INO: u32 = 2;
/// 覚えておくブロックの数の上限 (RAM の 1/8 まで)
const CACHE_BLOCKS: usize = 16384;
/// ファイルを読むとき、まとめて読むブロックの数 (128 KiB)
const READ_AHEAD: usize = 32;
/// noio (RWF_NOWAIT) でキャッシュになかったときの答え
const NOIO_EAGAIN: i64 = 11;

fn cache_limit(bsize: usize) -> usize {
    CACHE_BLOCKS.min(crate::memlayout::ram_size() / 8 / bsize).max(64)
}
const EROFS: i64 = 30;
const EFBIG: i64 = 27;

// 機能の印
const COMPAT_SPARSE_SUPER2: u32 = 0x200;
const INCOMPAT_FILETYPE: u32 = 0x2;
const INCOMPAT_RECOVER: u32 = 0x4;
const INCOMPAT_EXTENTS: u32 = 0x40;
const INCOMPAT_64BIT: u32 = 0x80;
const INCOMPAT_FLEX_BG: u32 = 0x200;
const INCOMPAT_CSUM_SEED: u32 = 0x2000;
const INCOMPAT_LARGEDIR: u32 = 0x4000;
const RO_SPARSE_SUPER: u32 = 0x1;
const RO_LARGE_FILE: u32 = 0x2;
const RO_HUGE_FILE: u32 = 0x8;
const RO_GDT_CSUM: u32 = 0x10;
const RO_DIR_NLINK: u32 = 0x20;
const RO_EXTRA_ISIZE: u32 = 0x40;
const RO_METADATA_CSUM: u32 = 0x400;
const INCOMPAT_READ: u32 = INCOMPAT_FILETYPE | INCOMPAT_RECOVER | INCOMPAT_EXTENTS | INCOMPAT_64BIT | INCOMPAT_FLEX_BG | INCOMPAT_CSUM_SEED | INCOMPAT_LARGEDIR;
const RO_WRITE: u32 = RO_SPARSE_SUPER | RO_LARGE_FILE | RO_HUGE_FILE | RO_DIR_NLINK | RO_EXTRA_ISIZE | RO_METADATA_CSUM;

// inode の印
const INDEX_FL: u32 = 0x1000;
const HUGE_FILE_FL: u32 = 0x40000;
const EXTENTS_FL: u32 = 0x80000;
const INLINE_DATA_FL: u32 = 0x1000_0000;
const IMMUTABLE_FL: u32 = 0x10;
const APPEND_FL: u32 = 0x20;

// グループの印
const BG_INODE_UNINIT: u16 = 0x1;
const BG_BLOCK_UNINIT: u16 = 0x2;

const EXT_MAGIC: u16 = 0xf30a;
const EXT_INIT_MAX_LEN: u32 = 32768;
/// ディレクトリブロックの末尾にある、チェックサム用の偽のエントリ
const DIR_TAIL: usize = 12;
const DIR_TAIL_FT: u8 = 0xde;

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn put16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}

fn now_secs() -> u32 {
    (crate::timer::epoch_ns() / 1_000_000_000) as u32
}

// ---- crc32c (Castagnoli)。Linux の crc32c と同じく、最後の反転はしない ----

const fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0x82f6_3b78 } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}

static CRC_TABLE: [u32; 256] = crc_table();

/// CRC32C (ext4 と jbd2 のチェックサム。はじめと終わりで反転はしない)。CPU に CRC32 の命令があれば
/// (ARMv8.1 からは必ず。ラズパイ 3/4 の A53/A72、QEMU、Apple の CPU にもある) 8 バイトずつ、なければ表で 1 バイトずつ
/// (表では 1 バイトに命令が 5 つほど: QEMU の TCG で 4 KiB のブロックに 100 us ほど)
fn crc32c(crc: u32, data: &[u8]) -> u32 {
    if hw_crc() {
        unsafe { crc32c_hw(crc, data) }
    } else {
        crc32c_table(crc, data)
    }
}

fn crc32c_table(mut crc: u32, data: &[u8]) -> u32 {
    for &b in data {
        crc = CRC_TABLE[((crc ^ b as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    crc
}

#[target_feature(enable = "crc")]
unsafe fn crc32c_hw(mut crc: u32, data: &[u8]) -> u32 {
    use core::arch::aarch64::{__crc32cb, __crc32cd};
    // 8 バイトにそろったところは u64 で (リトルエンディアンなので、バイトの順に読むのと同じ)
    let (head, body, tail) = unsafe { data.align_to::<u64>() };
    for &b in head {
        crc = __crc32cb(crc, b);
    }
    for &w in body {
        crc = __crc32cd(crc, w);
    }
    for &b in tail {
        crc = __crc32cb(crc, b);
    }
    crc
}

/// CRC32 の命令を使うか (0: まだ見ていない、1: 使わない、2: 使う)。ID_AA64ISAR0_EL1 の CRC32 (19:16) を見て、
/// 表と同じ答えになるかも 1 度だけ確かめる (ずれたら ext4 のチェックサムがみなこわれるので)
static HW_CRC: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

fn hw_crc() -> bool {
    use core::sync::atomic::Ordering::Relaxed;
    match HW_CRC.load(Relaxed) {
        2 => true,
        1 => false,
        _ => {
            let isar0: u64;
            unsafe { core::arch::asm!("mrs {}, id_aa64isar0_el1", out(reg) isar0, options(nomem, nostack, preserves_flags)) };
            // そろっていないところ、u64、のこりのどれも通るように、長さ 61 でずらして
            let probe: [u8; 64] = core::array::from_fn(|i| (i as u8).wrapping_mul(37).wrapping_add(11));
            let has = (isar0 >> 16) & 0xf != 0;
            let ok = has && unsafe { crc32c_hw(!0, &probe[1..62]) } == crc32c_table(!0, &probe[1..62]);
            HW_CRC.store(if ok { 2 } else { 1 }, Relaxed);
            match (has, ok) {
                (true, true) => println!("extfs: crc32c with the CPU's CRC32 instructions"),
                (true, false) => println!("extfs: crc32c by table (the CPU's CRC32 instructions gave a different answer)"),
                _ => println!("extfs: crc32c by table (no CRC32 instructions)"),
            }
            ok
        }
    }
}

/// ブロックの読み書き。1 ブロックを 1 ページに入れて覚えておく
impl Cache {
    /// 書いていないもの (メタデータもファイルの中身も) でなく、だれも写していない (参照がこの表だけ: ページキャッシュや
    /// mmap の写しが見ているページは、捨てると別のページを読みこんで食いちがう) いちばん古いブロックを 1 つ捨てる。
    /// 捨てられないものは後ろへ回す (また古くなったら見る)。64 こ見て捨てられなければあきらめる (表は上限をこえて
    /// ふくらむ: 写されているページはページキャッシュ (vm.rs) のもので、そちらが手放せば捨てられる)。
    /// order には forget したものも残っているので、map にないものはとばす
    fn evict_one(&mut self) -> bool {
        let mut tries = 0;
        while let Some(x) = self.order.pop_front() {
            let Some(&p) = self.map.get(&x) else { continue };
            if kalloc::refs(p) > 1 || self.dirty.contains(&x) || self.data.contains(&x) {
                self.order.push_back(x);
                tries += 1;
                if tries >= 64 {
                    return false;
                }
                continue;
            }
            self.map.remove(&x);
            kalloc::put(p);
            self.evictions += 1;
            return true;
        }
        false
    }

    /// ブロック b を覚えから外す (放したブロックなど)。order からはここでは外さない: order は覚えているブロック
    /// みな (16384 まで) なので、外すたびになめると大きなファイルを消すのがブロックの数 × 16384 になる
    /// (16 MiB で 6700 万回)。残ったものが覚えている数をこえたら、まとめて作りなおす (ならして 1 つあたり O(1))
    fn forget(&mut self, b: u64) {
        let Some(p) = self.map.remove(&b) else { return };
        // ページは参照で持つ (写しているところがまだ見ているかもしれない)
        kalloc::put(p);
        if self.order.len() > 2 * self.map.len() + 64 {
            // 外したあとにまた入ったものは 2 度ある: 古いほうを残す
            let mut seen = BTreeSet::new();
            let map = &self.map;
            self.order.retain(|x| map.contains_key(x) && seen.insert(*x));
        }
    }
}

struct Cache {
    map: BTreeMap<u64, *mut u8>,
    order: VecDeque<u64>,
    /// まだディスクに書いていないメタデータのブロック (ジャーナルを通す)
    dirty: BTreeSet<u64>,
    /// まだディスクに書いていないファイルの中身のブロック (直接書く)
    data: BTreeSet<u64>,
    sb_dirty: bool,
    /// チェックサムを直して書き出すグループ
    gd_dirty: BTreeSet<u32>,
    /// ビットマップを書きかえたグループ (チェックサムを直す)
    bb_dirty: BTreeSet<u32>,
    ib_dirty: BTreeSet<u32>,
    /// チェックサムをまだ計算していないディレクトリのブロック → (ディレクトリの inode, generation)。
    /// 書きかえるたびに 4 KiB の crc32c を計算せず、ジャーナルやディスクへ写す直前 (settle_csum) に 1 回だけ。
    /// Linux はブロックを書きかえるたびに計算する (ext4_handle_dirty_dirblock)
    dir_csum: BTreeMap<u64, (u32, u32)>,
    /// /proc/ai/fs の数: 引いたときにあった、なかった (ディスクから読んだ)、まとめて読んだ、捨てた、書き出した回数、
    /// 書いたブロック (メタデータ、中身)、ジャーナルのトランザクション
    hits: u64,
    misses: u64,
    readahead: u64,
    evictions: u64,
    flushes: u64,
    written_meta: u64,
    written_data: u64,
    commits: u64,
}

pub struct ExtFs {
    fs: usize,
    bsize: usize,
    inode_size: usize,
    ipg: u32,
    bpg: u32,
    first_data_block: u64,
    groups: u32,
    desc_size: usize,
    /// metadata_csum
    csum: bool,
    csum_seed: u32,
    /// 新しいファイルを extents で作る
    extents: bool,
    /// ファイルの大きさの上限 (Linux の ext4_max_size: extent の論理ブロックは 32 ビット。huge_file がなければ
    /// i_blocks (512 バイト単位、32 ビット) でも決まる)
    max_bytes: u64,
    /// 書けない (未対応の機能、回復の要るジャーナル)
    ro: bool,
    sparse_super: bool,
    sb: RefCell<Vec<u8>>,
    gdt: RefCell<Vec<u8>>,
    cache: RefCell<Cache>,
    journal: RefCell<Option<jbd2::Journal>>,
    /// 最後に書き出した時刻 (ticks)
    last_flush: core::cell::Cell<u64>,
    /// inode ごとの、いま生きている ExtInode の数 (開いているファイル、mmap、カレントディレクトリ ...)
    users: RefCell<BTreeMap<u32, usize>>,
    /// リンクが 0 になったが、まだ使われているので残してある inode (最後の利用者がいなくなったら片付ける)
    orphans: RefCell<BTreeSet<u32>>,
    /// O_TMPFILE で O_EXCL なしに作った、まだ名前のない inode (link で一度だけ名前をつけられる)
    linkable: RefCell<BTreeSet<u32>>,
    /// スワップに使っているファイル (書きかえさせない)
    swapfiles: RefCell<BTreeSet<u32>>,
    /// 名前のキャッシュ: (ディレクトリ, 名前) → inode (あったものだけ)。ディレクトリのブロックを書きかえたら
    /// (modify_dir_block) そのディレクトリの分を、ディレクトリの inode を空けたらその分を消す
    names: RefCell<Names>,
    /// 書く操作 (write_op、消したファイルの片づけ) のたびに増える。ディスクを眠って待った読み (with_block) は、
    /// 起きたときにこれが変わっていたら、読んだものを捨てて vfs::ERETRY (読みを頭からやりなおす)。
    /// 眠っているあいだに書かれたら、持っている inode やエクステントの写しはもう古いかもしれない
    wgen: core::cell::Cell<u64>,
    /// いま書く操作の中 (その中の読みは眠らない)
    writing: core::cell::Cell<usize>,
    /// 名前を消すたびに増える (探しているあいだにディスクを待って眠り、そのあいだに消されたら覚えない)
    names_gen: core::cell::Cell<u64>,
    /// meta() の答え (inode → Meta)。stat や open の 1 回で、たどるディレクトリとファイルの meta が何度も
    /// 引かれ、そのたびに inode を読んで解くのは高い (TCG で 1 回 7 us)。inode を書く (write_inode) ときに捨てる
    metas: RefCell<BTreeMap<u32, Meta>>,
    /// metas のどれかを捨てるたび (write_inode) に進む。ExtInode が自分の meta を覚えているのは、これが同じあいだ
    meta_ver: Cell<u64>,
}

/// meta() の答えの覚えの大きさ (こえたら全部捨てる)
const METAS_MAX: usize = 8192;

/// 名前のキャッシュの大きさ (こえたら全部捨てて作りなおす)
const NAMES_MAX: usize = 65536; // 1 つ 100 バイトくらいで 6 MiB ほど

/// 名前のキャッシュ: ディレクトリごとに 名前 → inode (引くときに文字列を作らずにすむように)
struct Names {
    dirs: BTreeMap<u32, BTreeMap<String, u32>>,
    len: usize,
}

impl Names {
    fn get(&self, dir: u32, name: &str) -> Option<u32> {
        self.dirs.get(&dir)?.get(name).copied()
    }

    /// 入れる (いっぱいなら先に全部捨てる)
    fn insert(&mut self, dir: u32, name: String, ino: u32) {
        if self.len >= NAMES_MAX {
            self.dirs.clear();
            self.len = 0;
        }
        if self.dirs.entry(dir).or_default().insert(name, ino).is_none() {
            self.len += 1;
        }
    }

    fn forget(&mut self, dir: u32) {
        if let Some(m) = self.dirs.remove(&dir) {
            self.len -= m.len();
        }
    }
}

/// ディレクトリの項目を足すすき間: (ブロック, 位置, rec_len, そこの項目が使っている長さ)
type Slot = (u64, usize, usize, usize);
/// ディレクトリの項目の場所: (ブロック, 位置, rec_len, 同じブロックの前の項目 (位置, rec_len))
type Entry = (u64, usize, usize, Option<(usize, usize)>);

/// ディスク上の inode (inode_size バイトまるごと)
#[derive(Clone)]
struct Raw(Vec<u8>);

impl Raw {
    fn mode(&self) -> u32 {
        u16_at(&self.0, 0) as u32
    }
    fn size(&self) -> u64 {
        let lo = u32_at(&self.0, 4) as u64;
        if self.mode() & S_IFMT == S_IFREG { lo | (u32_at(&self.0, 108) as u64) << 32 } else { lo }
    }
    fn set_size(&mut self, s: u64) {
        put32(&mut self.0, 4, s as u32);
        if self.mode() & S_IFMT == S_IFREG {
            put32(&mut self.0, 108, (s >> 32) as u32);
        }
    }
    fn links(&self) -> u16 {
        u16_at(&self.0, 26)
    }
    fn set_links(&mut self, n: u16) {
        put16(&mut self.0, 26, n);
    }
    /// 512 バイト単位の使用量
    fn blocks512(&self, bsize: usize) -> u64 {
        let v = u32_at(&self.0, 28) as u64 | (u16_at(&self.0, 0x74) as u64) << 32;
        if self.flags() & HUGE_FILE_FL != 0 { v * (bsize / 512) as u64 } else { v }
    }
    fn add_blocks512(&mut self, delta: i64, bsize: usize) {
        let v = (self.blocks512(bsize) as i64 + delta).max(0) as u64;
        put32(&mut self.0, 28, v as u32);
        put16(&mut self.0, 0x74, (v >> 32) as u16);
        let f = self.flags() & !HUGE_FILE_FL;
        put32(&mut self.0, 32, f);
    }
    fn flags(&self) -> u32 {
        u32_at(&self.0, 32)
    }
    fn set_flags(&mut self, f: u32) {
        put32(&mut self.0, 32, f);
    }
    fn block(&self, i: usize) -> u32 {
        u32_at(&self.0, 40 + i * 4)
    }
    fn set_block(&mut self, i: usize, v: u32) {
        put32(&mut self.0, 40 + i * 4, v);
    }
    /// i_block (60 バイト): extents の根
    fn iblock(&mut self) -> &mut [u8] {
        &mut self.0[40..100]
    }
    fn gen_no(&self) -> u32 {
        u32_at(&self.0, 0x64)
    }
    fn gid(&self) -> u32 {
        u16_at(&self.0, 24) as u32 | (u16_at(&self.0, 122) as u32) << 16
    }
    /// 許しを決めるもの (mode、uid、gid) だけの Meta (大きなロックなしの作る・消すが、ext4 のロックの中で見る)
    fn perm_meta(&self) -> Meta {
        let uid = u16_at(&self.0, 2) as u32 | (u16_at(&self.0, 120) as u32) << 16;
        Meta { mode: self.mode(), uid, gid: self.gid(), ..Meta::default() }
    }
    /// 作ったものの持ち主 (Linux の inode_init_owner): 作るプロセスの euid と egid。setgid のディレクトリの下なら
    /// そのグループで、ディレクトリなら setgid も受けつぐ。sysfile の own_new は、もう同じなら書きなおさない
    fn init_owner(&mut self, parent: &Raw) {
        let c = crate::cred::current();
        let sgid = parent.mode() & crate::cred::S_ISGID != 0;
        let (uid, gid) = (c.euid, if sgid { parent.gid() } else { c.egid });
        put16(&mut self.0, 2, uid as u16);
        put16(&mut self.0, 120, (uid >> 16) as u16);
        put16(&mut self.0, 24, gid as u16);
        put16(&mut self.0, 122, (gid >> 16) as u16);
        let mode = self.mode();
        if sgid && mode & S_IFMT == S_IFDIR {
            put16(&mut self.0, 0, (mode | crate::cred::S_ISGID) as u16);
        }
    }
    fn touch(&mut self) {
        let t = now_secs();
        put32(&mut self.0, 12, t); // ctime
        put32(&mut self.0, 16, t); // mtime
    }
}

/// ビットマップ d の start..n で最初の 0 のビット。64 ビットずつ見る (Linux の find_next_zero_bit。1 ビットずつだと、
/// 使っている inode が多いグループで作るたびに何千回もまわる)
fn first_zero(d: &[u8], start: u32, n: u32) -> Option<u32> {
    let mut bit = start;
    while bit < n {
        if bit % 64 == 0 && bit + 64 <= n {
            let i = (bit / 8) as usize;
            let w = u64::from_le_bytes(d[i..i + 8].try_into().unwrap());
            if w == u64::MAX {
                bit += 64;
                continue;
            }
            return Some(bit + (!w).trailing_zeros());
        }
        if d[(bit / 8) as usize] & (1 << (bit % 8)) == 0 {
            return Some(bit);
        }
        bit += 1;
    }
    None
}

fn ftype(mode: u32) -> u8 {
    match mode & S_IFMT {
        S_IFREG => 1,
        S_IFDIR => 2,
        S_IFCHR => 3,
        S_IFIFO => 5,
        S_IFSOCK => 6,
        S_IFLNK => 7,
        _ => 0,
    }
}

fn rec_len_for(name_len: usize) -> usize {
    (8 + name_len + 3) & !3
}

fn is_fast_symlink(r: &Raw) -> bool {
    r.mode() & S_IFMT == S_IFLNK && r.size() < 60 && r.flags() & EXTENTS_FL == 0 && u32_at(&r.0, 28) == 0
}

/// 3, 5, 7 のべき乗か
fn is_power_of(mut n: u32, b: u32) -> bool {
    while n > 1 && n % b == 0 {
        n /= b;
    }
    n == 1
}

impl ExtFs {
    /// ディスクの先頭にある ext2/ext4 を開く
    pub fn mount() -> Result<Rc<ExtFs>, &'static str> {
        let mut sb = vec![0u8; 1024];
        crate::block::read(2, &mut sb).map_err(|_| "read error")?;
        if u16_at(&sb, 56) != MAGIC {
            return Err("not ext2/ext4");
        }
        let incompat = u32_at(&sb, 96);
        let ro_compat = u32_at(&sb, 100);
        if incompat & !INCOMPAT_READ != 0 {
            return Err("unsupported ext4 features (meta_bg, inline_data, encrypt ...)");
        }
        let bsize = 1024usize << u32_at(&sb, 24);
        if bsize > PGSIZE {
            return Err("block size too large");
        }
        let rev = u32_at(&sb, 76);
        let inode_size = if rev == 0 { 128 } else { u16_at(&sb, 88) as usize };
        let bit64 = incompat & INCOMPAT_64BIT != 0;
        let desc_size = if bit64 { u16_at(&sb, 0xfe) as usize } else { 32 };
        let blocks = u32_at(&sb, 4) as u64 | if bit64 { (u32_at(&sb, 0x150) as u64) << 32 } else { 0 };
        let first_data_block = u32_at(&sb, 20) as u64;
        let bpg = u32_at(&sb, 32);
        let ipg = u32_at(&sb, 40);
        let groups = (blocks - first_data_block).div_ceil(bpg as u64) as u32;
        let csum = ro_compat & RO_METADATA_CSUM != 0;
        let csum_seed = if incompat & INCOMPAT_CSUM_SEED != 0 { u32_at(&sb, 0x270) } else { crc32c(!0, &sb[0x68..0x78]) };
        let mut ro = false;
        if ro_compat & !RO_WRITE != 0 || (ro_compat & RO_GDT_CSUM != 0 && !csum) {
            println!("extfs: unsupported features for writing ({:#x}), read-only", ro_compat & !RO_WRITE);
            ro = true;
        }
        let fs = Rc::new(ExtFs {
            fs: new_fs_id(),
            bsize,
            inode_size,
            ipg,
            bpg,
            first_data_block,
            groups,
            desc_size,
            csum,
            csum_seed,
            extents: incompat & INCOMPAT_EXTENTS != 0,
            max_bytes: {
                let bits = bsize.trailing_zeros();
                let res = (u32::MAX as u64) << bits;
                if ro_compat & RO_HUGE_FILE != 0 { res } else { res.min(((u32::MAX as u64) >> (bits - 9)) << bits) }
            },
            ro,
            sparse_super: ro_compat & RO_SPARSE_SUPER != 0 && u32_at(&sb, 92) & COMPAT_SPARSE_SUPER2 == 0,
            sb: RefCell::new(sb),
            gdt: RefCell::new(Vec::new()),
            cache: RefCell::new(Cache {
                map: BTreeMap::new(),
                order: VecDeque::new(),
                dirty: BTreeSet::new(),
                data: BTreeSet::new(),
                sb_dirty: false,
                gd_dirty: BTreeSet::new(),
                bb_dirty: BTreeSet::new(),
                ib_dirty: BTreeSet::new(),
                dir_csum: BTreeMap::new(),
                hits: 0,
                misses: 0,
                readahead: 0,
                evictions: 0,
                flushes: 0,
                written_meta: 0,
                written_data: 0,
                commits: 0,
            }),
            journal: RefCell::new(None),
            last_flush: core::cell::Cell::new(0),
            users: RefCell::new(BTreeMap::new()),
            orphans: RefCell::new(BTreeSet::new()),
            linkable: RefCell::new(BTreeSet::new()),
            swapfiles: RefCell::new(BTreeSet::new()),
            names: RefCell::new(Names { dirs: BTreeMap::new(), len: 0 }),
            wgen: core::cell::Cell::new(0),
            writing: core::cell::Cell::new(0),
            names_gen: core::cell::Cell::new(0),
            metas: RefCell::new(BTreeMap::new()),
            meta_ver: Cell::new(0),
        });
        let gdt_len = groups as usize * desc_size;
        let mut gdt = vec![0u8; gdt_len.div_ceil(bsize) * bsize];
        for (i, chunk) in gdt.chunks_mut(bsize).enumerate() {
            chunk.copy_from_slice(&fs.read_block(first_data_block + 1 + i as u64).map_err(|_| "read error")?);
        }
        *fs.gdt.borrow_mut() = gdt;
        if !fs.ro {
            match fs.journal_open() {
                Ok(j) => {
                    let has = j.is_some();
                    *fs.journal.borrow_mut() = j;
                    // 再生で書きかわったかもしれないので、覚えていたものを捨てて読みなおす
                    fs.drop_cache();
                    for (i, chunk) in fs.gdt.borrow_mut().chunks_mut(bsize).enumerate() {
                        chunk.copy_from_slice(&fs.read_block(first_data_block + 1 + i as u64).map_err(|_| "read error")?);
                    }
                    if has {
                        println!("extfs: journal on");
                    }
                    if let Err(e) = fs.process_orphans() {
                        println!("extfs: orphan list: error {}", e);
                    }
                }
                Err(e) => {
                    println!("extfs: journal: {}", e);
                    return Err(e);
                }
            }
        } else if incompat & INCOMPAT_RECOVER != 0 {
            println!("extfs: journal needs recovery");
        }
        let kind = if fs.extents { "ext4" } else { "ext2" };
        println!("extfs: {} {} MiB, {} groups{}{}", kind, blocks * bsize as u64 / (1024 * 1024), groups, if csum { ", metadata_csum" } else { "" }, if fs.ro { ", read-only" } else { "" });
        Ok(fs)
    }

    pub fn root(self: &Rc<Self>) -> InodeRef {
        let _e = fs_lock();
        // 大きなロックなしの作る・消す (fastdir.rs) が使う ext4 (ルートのもの。いまマウントできる ext4 はこれだけ)
        unsafe {
            if (*(&raw const ROOT_FS)).is_none() {
                ROOT_FS = Some(self.clone());
            }
        }
        ExtInode::make(self, ROOT_INO)
    }

    /// vfs の id の 1 つめ (このファイルシステムの番号)
    pub fn fs_id(&self) -> usize {
        self.fs
    }

    /// ルートの ext4 (fastdir.rs。ext4 のロックを持って使う)
    pub fn root_fs() -> Option<&'static Rc<ExtFs>> {
        unsafe { (*(&raw const ROOT_FS)).as_ref() }
    }

    /// /proc/ai/fs の、このファイルシステムの中の様子 (1 つの JSON のオブジェクト)
    pub fn stats_json(&self) -> alloc::string::String {
        let _e = fs_lock();
        let c = self.cache.borrow();
        let j = self.journal.borrow();
        let journal = match j.as_ref() {
            Some(j) => alloc::format!("{{\"capacity\":{},\"seq\":{}}}", j.capacity(), j.seq()),
            None => alloc::string::String::from("null"),
        };
        let sb = self.sb.borrow();
        let since = crate::timer::ticks().saturating_sub(self.last_flush.get()) / crate::timer::HZ;
        alloc::format!(
            "{{\"block_size\":{},\"ro\":{},\"cache\":{{\"blocks\":{},\"limit\":{},\"dirty_meta\":{},\"dirty_data\":{},\"hits\":{},\"misses\":{},\"readahead\":{},\"evictions\":{},\"flushes\":{},\"written_meta\":{},\"written_data\":{},\"commits\":{},\"last_flush_s\":{}}},\"journal\":{},\"metas\":{{\"entries\":{},\"max\":{}}},\"names\":{{\"entries\":{},\"max\":{}}},\"inodes_in_use\":{},\"orphans\":{},\"blocks\":{{\"total\":{},\"free\":{}}},\"inodes\":{{\"total\":{},\"free\":{}}}}}",
            self.bsize,
            self.ro,
            c.map.len(),
            cache_limit(self.bsize),
            c.dirty.len(),
            c.data.len(),
            c.hits,
            c.misses,
            c.readahead,
            c.evictions,
            c.flushes,
            c.written_meta,
            c.written_data,
            c.commits,
            since,
            journal,
            self.metas.borrow().len(),
            METAS_MAX,
            self.names.borrow().len,
            NAMES_MAX,
            self.users.borrow().len(),
            self.orphans.borrow().len(),
            self.blocks_count(),
            self.sb_u64(12, 0x158),
            u32_at(&sb, 0),
            u32_at(&sb, 16)
        )
    }

    pub fn kind(&self) -> &'static str {
        if self.extents { "ext4" } else { "ext2" }
    }

    fn check_rw(&self) -> Result<(), i64> {
        if self.ro { Err(-EROFS) } else { Ok(()) }
    }

    // ---- ブロック ----

    fn with_block<T>(&self, b: u64, f: impl FnOnce(&mut [u8]) -> T) -> Result<T, i64> {
        let mut c = self.cache.borrow_mut();
        let page = match c.map.get(&b).copied() {
            Some(p) => {
                c.hits += 1;
                p
            }
            // ディスクを読まない区間 (proc::noio: preadv2 の RWF_NOWAIT): キャッシュになければ EAGAIN
            None if crate::proc::noio_on() => return Err(-NOIO_EAGAIN),
            // 読むだけの道 (proc::io_sleepable) で、書く操作の中でなければ、ディスクを眠って待つ
            None if self.writing.get() == 0 && crate::proc::io_sleep_ok() => {
                drop(c);
                return self.with_block_sleep(b, f);
            }
            None => {
                c.misses += 1;
                if c.map.len() >= cache_limit(self.bsize) {
                    c.evict_one();
                }
                // ページがなければ、書き終わったブロックを捨てて作る (ディスクから全部読むので 0 にしない)
                let p = match kalloc::alloc_dirty() {
                    Some(p) => p,
                    None if c.evict_one() => kalloc::alloc_dirty().ok_or(-ENOSPC)?,
                    None => return Err(-ENOSPC),
                };
                let buf = unsafe { core::slice::from_raw_parts_mut(p, self.bsize) };
                if crate::block::read(b * (self.bsize / crate::block::SECTOR) as u64, buf).is_err() {
                    kalloc::free(p);
                    return Err(-EIO);
                }
                c.map.insert(b, p);
                c.order.push_back(b);
                p
            }
        };
        drop(c);
        Ok(f(unsafe { core::slice::from_raw_parts_mut(page, self.bsize) }))
    }

    /// with_block の、ディスクを眠って待つ道 (キャッシュになかった)。眠るあいだはキャッシュを借りない。
    /// 起きたら: 書く操作があった (wgen が変わった) なら捨てて ERETRY、ほかの人が先に入れていればそちらを使う
    fn with_block_sleep<T>(&self, b: u64, f: impl FnOnce(&mut [u8]) -> T) -> Result<T, i64> {
        self.cache.borrow_mut().misses += 1;
        let g0 = self.wgen.get();
        let p = match kalloc::alloc_dirty() {
            Some(p) => p,
            None => {
                self.cache.borrow_mut().evict_one();
                kalloc::alloc_dirty().ok_or(-ENOSPC)?
            }
        };
        let buf = unsafe { core::slice::from_raw_parts_mut(p, self.bsize) };
        if crate::block::read_wait(b * (self.bsize / crate::block::SECTOR) as u64, buf).is_err() {
            kalloc::free(p);
            return Err(-EIO);
        }
        if self.wgen.get() != g0 {
            kalloc::free(p);
            return Err(-crate::vfs::ERETRY);
        }
        let mut c = self.cache.borrow_mut();
        let page = match c.map.get(&b) {
            Some(&q) => {
                kalloc::free(p);
                q
            }
            None => {
                if c.map.len() >= cache_limit(self.bsize) {
                    c.evict_one();
                }
                c.map.insert(b, p);
                c.order.push_back(b);
                p
            }
        };
        drop(c);
        Ok(f(unsafe { core::slice::from_raw_parts_mut(page, self.bsize) }))
    }

    /// ディスクから読まずに、b のページを用意する (中身はゼロ。すでにキャッシュにあればそのまま)。
    /// 新しく作ったブロックや、ブロックまるごと書きかえるときに (読んでもすぐ上書きするので)
    fn with_block_new<T>(&self, b: u64, f: impl FnOnce(&mut [u8]) -> T) -> Result<T, i64> {
        self.with_block_new_adopt(b, None, f)
    }

    /// with_block_new で、ファイル ino のブロック lb としてとるとき (adopt): そこを写しているところが穴の 0 のページを
    /// 持っていれば (vm::adopt_hole)、そのページをこのブロックのページにする (写している人にそのまま映る)
    fn with_block_new_adopt<T>(&self, b: u64, adopt: Option<(u32, u64)>, f: impl FnOnce(&mut [u8]) -> T) -> Result<T, i64> {
        let adopted = match adopt {
            Some((ino, lb)) if self.bsize == PGSIZE => crate::vm::adopt_hole((self.fs, ino as u64), lb as usize),
            _ => None,
        };
        let mut c = self.cache.borrow_mut();
        let have = c.map.get(&b).copied();
        let page = match (have, adopted) {
            (have, Some(p)) => {
                c.map.insert(b, p);
                match have {
                    Some(old) => kalloc::put(old),
                    None => c.order.push_back(b),
                }
                p
            }
            (Some(p), None) => p,
            (None, None) => {
                if c.map.len() >= cache_limit(self.bsize) {
                    c.evict_one();
                }
                let p = match kalloc::alloc() {
                    Some(p) => p,
                    None if c.evict_one() => kalloc::alloc().ok_or(-ENOSPC)?,
                    None => return Err(-ENOSPC),
                };
                unsafe { core::ptr::write_bytes(p, 0, self.bsize) };
                c.map.insert(b, p);
                c.order.push_back(b);
                p
            }
        };
        drop(c);
        Ok(f(unsafe { core::slice::from_raw_parts_mut(page, self.bsize) }))
    }

    /// ブロック b のページそのもの (参照をひとつ足して。使い終えたら kalloc::put)。なければディスクから読む
    fn frame(&self, b: u64) -> Result<*mut u8, i64> {
        let p = self.with_block(b, |d| d.as_mut_ptr())?;
        kalloc::get(p);
        Ok(p)
    }

    /// ファイル ino を size から upto まで伸ばす: 終わりのブロックの、size からそのブロックの終わり (upto まで) を 0 に
    /// (キャッシュにあるときだけ: 写しで終わりの先に書いたものを消す。Linux の pagecache_isize_extended。キャッシュに
    /// なければ、ディスクの上はもう 0: 切りつめと書き出しが 0 にしている)
    fn zero_gap(&self, ino: u32, r: &mut Raw, size: usize, upto: usize) {
        let bs = self.bsize;
        if size % bs == 0 || upto <= size {
            return;
        }
        let Ok(b) = self.map(ino, r, (size / bs) as u64, false) else { return };
        if b == 0 {
            return;
        }
        let end = upto.min((size / bs + 1) * bs) - (size / bs) * bs;
        if let Some(&p) = self.cache.borrow().map.get(&b) {
            unsafe { core::ptr::write_bytes(p.add(size % bs), 0, end - size % bs) };
        }
    }

    /// 先読み: ディスクの b から n ブロックを 1 回で読み、キャッシュにないものを覚える
    fn read_ahead(&self, b: u64, n: usize) -> Result<(), i64> {
        let spb = (self.bsize / crate::block::SECTOR) as u64;
        // ディスクから全部読む (0 にしない)
        let mut buf: Vec<u8> = Vec::with_capacity(n * self.bsize);
        unsafe { buf.set_len(n * self.bsize) };
        // 読むだけの道なら眠って待つ (with_block_sleep と同じ: そのあいだに書かれたら捨ててやりなおす)
        if self.writing.get() == 0 && crate::proc::io_sleep_ok() {
            let g0 = self.wgen.get();
            crate::block::read_wait(b * spb, &mut buf)?;
            if self.wgen.get() != g0 {
                return Err(-crate::vfs::ERETRY);
            }
        } else {
            crate::block::read(b * spb, &mut buf)?;
        }
        let mut c = self.cache.borrow_mut();
        for k in 0..n {
            let blk = b + k as u64;
            if c.map.contains_key(&blk) {
                continue;
            }
            // 上限をこえていたら 1 つ捨てる (捨てられなくても先読みは続ける: 写されているページで上限をこえているとき)
            if c.map.len() >= cache_limit(self.bsize) {
                c.evict_one();
            }
            let Some(p) = kalloc::alloc_dirty() else { break };
            unsafe { core::ptr::copy_nonoverlapping(buf[k * self.bsize..].as_ptr(), p, self.bsize) };
            c.map.insert(blk, p);
            c.order.push_back(blk);
            c.readahead += 1;
        }
        Ok(())
    }

    fn cached(&self, b: u64) -> bool {
        self.cache.borrow().map.contains_key(&b)
    }

    fn read_block(&self, b: u64) -> Result<Vec<u8>, i64> {
        self.with_block(b, |d| d.to_vec())
    }

    /// メタデータのブロックを書きかえる (ディスクへは flush でジャーナルを通して)
    fn modify_block<T>(&self, b: u64, f: impl FnOnce(&mut [u8]) -> T) -> Result<T, i64> {
        let r = self.with_block(b, f)?;
        let mut c = self.cache.borrow_mut();
        c.data.remove(&b);
        c.dirty.insert(b);
        // ディレクトリのブロックでなくなった (htree の索引になったなど): あとで計算しない
        if !c.dir_csum.is_empty() {
            c.dir_csum.remove(&b);
        }
        Ok(r)
    }

    /// ブロック b のディレクトリのチェックサムがまだなら、いま計算する (ジャーナルやディスクへ写す前に)
    pub(super) fn settle_csum(&self, b: u64) -> Result<(), i64> {
        let e = {
            let mut c = self.cache.borrow_mut();
            if c.dir_csum.is_empty() { None } else { c.dir_csum.remove(&b) }
        };
        if let Some((dir, generation)) = e {
            self.with_block(b, |d| self.set_dir_csum(dir, generation, d))?;
        }
        Ok(())
    }

    /// ファイルの中身のブロックを書きかえる (flush で、ジャーナルより先に直接)。
    /// fresh なら、ディスクから読まずにゼロから (新しいブロックか、まるごと書きかえるとき)
    fn modify_data_block<T>(&self, b: u64, f: impl FnOnce(&mut [u8]) -> T) -> Result<T, i64> {
        self.modify_data_block_as(b, false, f)
    }

    fn modify_data_block_as<T>(&self, b: u64, fresh: bool, f: impl FnOnce(&mut [u8]) -> T) -> Result<T, i64> {
        self.modify_data_block_adopt(b, fresh, None, f)
    }

    /// modify_data_block_as で、fresh なら ino のブロック lb として (写しの穴のページを引きとる: with_block_new_adopt)
    fn modify_data_block_adopt<T>(&self, b: u64, fresh: bool, adopt: Option<(u32, u64)>, f: impl FnOnce(&mut [u8]) -> T) -> Result<T, i64> {
        let r = if fresh { self.with_block_new_adopt(b, adopt, f)? } else { self.with_block(b, f)? };
        let mut c = self.cache.borrow_mut();
        c.dirty.remove(&b);
        c.data.insert(b);
        Ok(r)
    }

    /// 覚えているブロックをぜんぶ捨てる (書いていないものはないこと)
    fn drop_cache(&self) {
        let mut c = self.cache.borrow_mut();
        for (_, p) in core::mem::take(&mut c.map) {
            kalloc::put(p);
        }
        c.order.clear();
    }

    /// スーパーブロックの入ったブロックと、その中の場所
    fn sb_block(&self) -> (u64, usize) {
        if self.bsize == 1024 { (1, 0) } else { (0, 1024) }
    }

    /// キャッシュにあるスーパーブロックの入ったブロックを data に合わせる
    fn sync_sb_cache(&self, data: &[u8]) {
        let (b, off) = self.sb_block();
        if let Some(&p) = self.cache.borrow().map.get(&b) {
            unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), p.add(off), 1024) };
        }
    }

    // ---- スーパーブロックとグループディスクリプタ ----

    fn sb_u64(&self, lo: usize, hi: usize) -> u64 {
        let sb = self.sb.borrow();
        u32_at(&sb, lo) as u64 | if self.desc_size >= 64 { (u32_at(&sb, hi) as u64) << 32 } else { 0 }
    }

    fn blocks_count(&self) -> u64 {
        self.sb_u64(4, 0x150)
    }

    fn sb_add_free_blocks(&self, delta: i64) {
        let v = (self.sb_u64(12, 0x158) as i64 + delta) as u64;
        let mut sb = self.sb.borrow_mut();
        put32(&mut sb, 12, v as u32);
        if self.desc_size >= 64 {
            put32(&mut sb, 0x158, (v >> 32) as u32);
        }
        drop(sb);
        self.cache.borrow_mut().sb_dirty = true;
    }

    fn sb_add_free_inodes(&self, delta: i64) {
        let mut sb = self.sb.borrow_mut();
        let v = (u32_at(&sb, 16) as i64 + delta) as u32;
        put32(&mut sb, 16, v);
        drop(sb);
        self.cache.borrow_mut().sb_dirty = true;
    }

    // ---- orphan リスト ----
    // 消したがまだ使われている inode は、superblock の s_last_orphan から i_dtime でつないでおく
    // (ext4 と同じ)。いきなり電源が切れても、次のマウント (や e2fsck) で片付けられる

    fn last_orphan(&self) -> u32 {
        u32_at(&self.sb.borrow(), 0xe8)
    }

    fn set_last_orphan(&self, ino: u32) {
        put32(&mut self.sb.borrow_mut(), 0xe8, ino);
        self.cache.borrow_mut().sb_dirty = true;
    }

    /// リストの頭に足す (r は呼ぶ側が書く)
    fn orphan_add(&self, ino: u32, r: &mut Raw) {
        put32(&mut r.0, 20, self.last_orphan());
        self.set_last_orphan(ino);
    }

    /// リストから外す
    fn orphan_del(&self, ino: u32) -> Result<(), i64> {
        let next = u32_at(&self.read_inode(ino)?.0, 20);
        if self.last_orphan() == ino {
            self.set_last_orphan(next);
            return Ok(());
        }
        let mut cur = self.last_orphan();
        for _ in 0..self.ipg.saturating_mul(self.groups) {
            if cur == 0 {
                break;
            }
            let mut r = self.read_inode(cur)?;
            let d = u32_at(&r.0, 20);
            if d == ino {
                put32(&mut r.0, 20, next);
                return self.write_inode(cur, &r);
            }
            cur = d;
        }
        Ok(())
    }

    /// マウントのとき: 前に残った orphan を片付ける
    fn process_orphans(&self) -> Result<(), i64> {
        let mut cur = self.last_orphan();
        if cur == 0 {
            return Ok(());
        }
        let mut n = 0;
        while cur != 0 && n < 1_000_000 {
            if cur < ROOT_INO || cur > self.ipg.saturating_mul(self.groups) {
                break;
            }
            let mut r = self.read_inode(cur)?;
            let next = u32_at(&r.0, 20);
            if r.links() == 0 {
                self.release(cur, &mut r)?;
            } else {
                put32(&mut r.0, 20, 0);
                self.write_inode(cur, &r)?;
            }
            cur = next;
            n += 1;
        }
        self.set_last_orphan(0);
        self.flush()?;
        println!("extfs: freed {} orphan inode(s)", n);
        Ok(())
    }

    fn gd_off(&self, g: u32) -> usize {
        g as usize * self.desc_size
    }

    /// lo (と 64bit なら hi) の 32 ビットの組
    fn gd_blk(&self, g: u32, lo: usize, hi: usize) -> u64 {
        let d = self.gdt.borrow();
        let o = self.gd_off(g);
        u32_at(&d, o + lo) as u64 | if self.desc_size >= 64 { (u32_at(&d, o + hi) as u64) << 32 } else { 0 }
    }

    /// lo (と 64bit なら hi) の 16 ビットの組
    fn gd_cnt(&self, g: u32, lo: usize, hi: usize) -> u32 {
        let d = self.gdt.borrow();
        let o = self.gd_off(g);
        u16_at(&d, o + lo) as u32 | if self.desc_size >= 64 { (u16_at(&d, o + hi) as u32) << 16 } else { 0 }
    }

    fn gd_set_cnt(&self, g: u32, lo: usize, hi: usize, v: u32) {
        let mut d = self.gdt.borrow_mut();
        let o = self.gd_off(g);
        put16(&mut d, o + lo, v as u16);
        if self.desc_size >= 64 {
            put16(&mut d, o + hi, (v >> 16) as u16);
        }
        drop(d);
        self.cache.borrow_mut().gd_dirty.insert(g);
    }

    fn gd_add_cnt(&self, g: u32, lo: usize, hi: usize, delta: i32) {
        let v = (self.gd_cnt(g, lo, hi) as i64 + delta as i64).max(0) as u32;
        self.gd_set_cnt(g, lo, hi, v);
    }

    fn block_bitmap(&self, g: u32) -> u64 {
        self.gd_blk(g, 0x0, 0x20)
    }
    fn inode_bitmap(&self, g: u32) -> u64 {
        self.gd_blk(g, 0x4, 0x24)
    }
    fn inode_table(&self, g: u32) -> u64 {
        self.gd_blk(g, 0x8, 0x28)
    }
    fn gd_flags(&self, g: u32) -> u16 {
        u16_at(&self.gdt.borrow(), self.gd_off(g) + 0x12)
    }
    fn gd_clear_flag(&self, g: u32, f: u16) {
        let mut d = self.gdt.borrow_mut();
        let o = self.gd_off(g) + 0x12;
        let v = u16_at(&d, o) & !f;
        put16(&mut d, o, v);
        drop(d);
        self.cache.borrow_mut().gd_dirty.insert(g);
    }

    fn group_start(&self, g: u32) -> u64 {
        self.first_data_block + g as u64 * self.bpg as u64
    }

    /// このグループにある (または最後のグループで足りない) ブロック数
    fn blocks_in_group(&self, g: u32) -> u32 {
        (self.blocks_count() - self.group_start(g)).min(self.bpg as u64) as u32
    }

    fn has_super(&self, g: u32) -> bool {
        !self.sparse_super || g <= 1 || is_power_of(g, 3) || is_power_of(g, 5) || is_power_of(g, 7)
    }

    /// 未初期化 (BLOCK_UNINIT) のグループのブロックビットマップを作る
    fn init_block_bitmap(&self, g: u32) -> Result<(), i64> {
        let start = self.group_start(g);
        let n = self.blocks_in_group(g);
        let mut used: Vec<u64> = vec![];
        if self.has_super(g) {
            let gdt_blocks = (self.groups as usize * self.desc_size).div_ceil(self.bsize) as u64;
            let reserved = u16_at(&self.sb.borrow(), 0xce) as u64;
            used.extend(start..start + 1 + gdt_blocks + reserved);
        }
        let itb = (self.ipg as usize * self.inode_size).div_ceil(self.bsize) as u64;
        for h in 0..self.groups {
            used.push(self.block_bitmap(h));
            used.push(self.inode_bitmap(h));
            let t = self.inode_table(h);
            used.extend(t..t + itb);
        }
        let bm = self.block_bitmap(g);
        let bsize = self.bsize;
        self.modify_block(bm, |d| {
            d.fill(0);
            for b in used.into_iter().filter(|&b| b >= start && b < start + n as u64) {
                let bit = (b - start) as usize;
                d[bit / 8] |= 1 << (bit % 8);
            }
            // グループに無いブロックの分は使用中にしておく
            for bit in n as usize..bsize * 8 {
                d[bit / 8] |= 1 << (bit % 8);
            }
        })?;
        self.gd_clear_flag(g, BG_BLOCK_UNINIT);
        self.cache.borrow_mut().bb_dirty.insert(g);
        Ok(())
    }

    /// 未初期化 (INODE_UNINIT) のグループの inode ビットマップを作る
    fn init_inode_bitmap(&self, g: u32) -> Result<(), i64> {
        let (bm, ipg, bsize) = (self.inode_bitmap(g), self.ipg as usize, self.bsize);
        self.modify_block(bm, |d| {
            d.fill(0);
            for bit in ipg..bsize * 8 {
                d[bit / 8] |= 1 << (bit % 8);
            }
        })?;
        self.gd_clear_flag(g, BG_INODE_UNINIT);
        self.cache.borrow_mut().ib_dirty.insert(g);
        Ok(())
    }

    /// ビットマップ bm の中で、start から n 個のうち空いているビットを 1 つ取る
    fn take_in(&self, bm: u64, start: u32, n: u32) -> Result<Option<u32>, i64> {
        self.modify_block(bm, |d| {
            let bit = first_zero(d, start, n)?;
            d[(bit / 8) as usize] |= 1 << (bit % 8);
            Some(bit)
        })
    }

    /// goal のそば (同じグループの goal 以降、なければ他のグループ) にブロックを 1 つ取る
    fn alloc_block(&self, goal: u64) -> Result<u64, i64> {
        let goal = goal.clamp(self.first_data_block, self.blocks_count() - 1);
        let gg = ((goal - self.first_data_block) / self.bpg as u64) as u32;
        for k in 0..=self.groups {
            let g = (gg + k) % self.groups;
            if self.gd_cnt(g, 0xc, 0x2c) == 0 {
                continue;
            }
            if self.gd_flags(g) & BG_BLOCK_UNINIT != 0 {
                self.init_block_bitmap(g)?;
            }
            let n = self.blocks_in_group(g);
            let from = if k == 0 { (goal - self.group_start(g)) as u32 } else { 0 };
            let mut found = self.take_in(self.block_bitmap(g), from, n)?;
            if found.is_none() && from > 0 {
                found = self.take_in(self.block_bitmap(g), 0, from)?;
            }
            if let Some(bit) = found {
                self.gd_add_cnt(g, 0xc, 0x2c, -1);
                self.sb_add_free_blocks(-1);
                self.cache.borrow_mut().bb_dirty.insert(g);
                let b = self.group_start(g) + bit as u64;
                // 新しいブロックはゼロから (ディスクの古い中身は読まない)。ファイルの中身に使うなら
                // write_data が中身のブロックに付けかえる
                self.with_block_new(b, |d| d.fill(0))?;
                let mut c = self.cache.borrow_mut();
                c.data.remove(&b);
                c.dirty.insert(b);
                return Ok(b);
            }
        }
        Err(-ENOSPC)
    }

    fn free_block(&self, b: u64) -> Result<(), i64> {
        let rel = b - self.first_data_block;
        let (g, bit) = ((rel / self.bpg as u64) as u32, (rel % self.bpg as u64) as usize);
        let bm = self.block_bitmap(g);
        self.modify_block(bm, |d| d[bit / 8] &= !(1 << (bit % 8)))?;
        self.gd_add_cnt(g, 0xc, 0x2c, 1);
        self.sb_add_free_blocks(1);
        let mut c = self.cache.borrow_mut();
        c.bb_dirty.insert(g);
        // 捨てたブロックは書き出さない (data に残すと、flush がディスクの古い中身を読んで書き戻す)
        c.dirty.remove(&b);
        c.data.remove(&b);
        c.dir_csum.remove(&b);
        c.forget(b);
        Ok(())
    }

    fn alloc_inode(&self, goal_group: u32, dir: bool) -> Result<u32, i64> {
        let first_ino = u32_at(&self.sb.borrow(), 84).max(11);
        for k in 0..self.groups {
            let g = (goal_group + k) % self.groups;
            if self.gd_cnt(g, 0xe, 0x2e) == 0 {
                continue;
            }
            if self.gd_flags(g) & BG_INODE_UNINIT != 0 {
                self.init_inode_bitmap(g)?;
            }
            // 予約された inode (最初のグループの 1..first_ino) は飛ばす
            let from = if g == 0 { first_ino - 1 } else { 0 };
            let Some(bit) = self.take_in(self.inode_bitmap(g), from, self.ipg)? else { continue };
            return Ok(self.inode_taken(g, bit, dir));
        }
        Err(-ENOSPC)
    }

    /// 大きなロックなしの作る道 (fast_create): alloc_inode が選ぶ (グループ, ビット) を、何も書きかえずに。初期化して
    /// いないグループか、キャッシュにないビットマップに当たれば None (ふつうの道へ)
    fn pick_inode(&self, goal_group: u32) -> Option<(u32, u32)> {
        let first_ino = u32_at(&self.sb.borrow(), 84).max(11);
        for k in 0..self.groups {
            let g = (goal_group + k) % self.groups;
            if self.gd_cnt(g, 0xe, 0x2e) == 0 {
                continue;
            }
            let bm = self.inode_bitmap(g);
            if self.gd_flags(g) & BG_INODE_UNINIT != 0 || !self.cached(bm) {
                return None;
            }
            let from = if g == 0 { first_ino - 1 } else { 0 };
            match self.with_block(bm, |d| first_zero(d, from, self.ipg)) {
                Ok(Some(bit)) => return Some((g, bit)),
                Ok(None) => continue,
                Err(_) => return None,
            }
        }
        None
    }

    /// グループ g のビット bit の inode を取った (ビットマップはもう立てた): 数を直して番号を返す
    fn inode_taken(&self, g: u32, bit: u32, dir: bool) -> u32 {
        self.gd_add_cnt(g, 0xe, 0x2e, -1);
        if dir {
            self.gd_add_cnt(g, 0x10, 0x30, 1);
        }
        // まだ使われていない inode 表の残り (itable_unused) を減らす
        let unused = self.gd_cnt(g, 0x1c, 0x32);
        if self.ipg - unused <= bit {
            self.gd_set_cnt(g, 0x1c, 0x32, self.ipg - bit - 1);
        }
        self.sb_add_free_inodes(-1);
        self.cache.borrow_mut().ib_dirty.insert(g);
        let ino = g * self.ipg + bit + 1;
        // 前にこの番号だったファイルのページがページキャッシュに残っていたら捨てる (大きなロックなしの read は、
        // 表にあるページと大きさをいまのものとして読む)
        crate::vm::file_changed((self.fs, ino as u64));
        ino
    }

    fn free_inode(&self, ino: u32, dir: bool) -> Result<(), i64> {
        if dir {
            self.forget_names(ino);
        }
        // ページキャッシュの中身も (番号はまた使われる)
        crate::vm::file_changed((self.fs, ino as u64));
        let (g, bit) = ((ino - 1) / self.ipg, ((ino - 1) % self.ipg) as usize);
        let bm = self.inode_bitmap(g);
        self.modify_block(bm, |d| d[bit / 8] &= !(1 << (bit % 8)))?;
        self.gd_add_cnt(g, 0xe, 0x2e, 1);
        if dir {
            self.gd_add_cnt(g, 0x10, 0x30, -1);
        }
        self.sb_add_free_inodes(1);
        self.cache.borrow_mut().ib_dirty.insert(g);
        Ok(())
    }

    // ---- チェックサム (metadata_csum) ----

    fn inode_seed(&self, ino: u32, generation: u32) -> u32 {
        crc32c(crc32c(self.csum_seed, &ino.to_le_bytes()), &generation.to_le_bytes())
    }

    fn set_inode_csum(&self, ino: u32, r: &mut Raw) {
        if !self.csum {
            return;
        }
        let has_hi = self.inode_size > 128 && u16_at(&r.0, 0x80) >= 4;
        put16(&mut r.0, 0x7c, 0);
        if has_hi {
            put16(&mut r.0, 0x82, 0);
        }
        let c = crc32c(self.inode_seed(ino, r.gen_no()), &r.0[..self.inode_size]);
        put16(&mut r.0, 0x7c, c as u16);
        if has_hi {
            put16(&mut r.0, 0x82, (c >> 16) as u16);
        }
    }

    /// extent ブロックの末尾 (12 + 12 * max の位置) のチェックサム
    fn set_extent_csum(&self, ino: u32, generation: u32, d: &mut [u8]) {
        if !self.csum {
            return;
        }
        let off = 12 + 12 * u16_at(d, 4) as usize;
        if off + 4 <= d.len() {
            let c = crc32c(self.inode_seed(ino, generation), &d[..off]);
            put32(d, off, c);
        }
    }

    /// ディレクトリブロックの末尾の偽エントリのチェックサム
    fn set_dir_csum(&self, ino: u32, generation: u32, d: &mut [u8]) {
        if !self.csum {
            return;
        }
        let t = d.len() - DIR_TAIL;
        if u32_at(d, t) == 0 && u16_at(d, t + 4) as usize == DIR_TAIL && d[t + 7] == DIR_TAIL_FT {
            let c = crc32c(self.inode_seed(ino, generation), &d[..t]);
            put32(d, t + 8, c);
        }
    }

    fn gd_csum(&self, g: u32) {
        if !self.csum {
            return;
        }
        let mut d = self.gdt.borrow_mut();
        let o = self.gd_off(g);
        put16(&mut d, o + 0x1e, 0);
        let mut c = crc32c(self.csum_seed, &g.to_le_bytes());
        c = crc32c(c, &d[o..o + self.desc_size]);
        put16(&mut d, o + 0x1e, c as u16);
    }

    /// 書きかえたものをディスクへ。となりあうブロックは 1 回の要求にまとめる
    /// たまっていれば (メタデータはジャーナルの 1/4 か 512 ブロック、中身はキャッシュの 1/4)、
    /// または 5 秒たっていれば書き出す。
    /// ほかは sync、暇なとき (vfs::idle_sync)、再起動のときにまとめて 1 つのトランザクションで
    /// メタデータ (ジャーナルを通す) と中身 (直接書く) は別に数える。中身はキャッシュの 1/4 までためる
    fn maybe_flush(&self) -> Result<(), i64> {
        self.take_fast_writes();
        let limit = match self.journal.borrow().as_ref() {
            Some(j) => (j.capacity() / 4).min(512),
            None => 512,
        };
        let data_limit = (cache_limit(self.bsize) / 4).max(512);
        let (meta, data) = {
            let c = self.cache.borrow();
            (c.dirty.len(), c.data.len())
        };
        if meta >= limit || data >= data_limit || crate::timer::ticks() >= self.last_flush.get() + 5 * crate::timer::HZ {
            return self.flush();
        }
        Ok(())
    }

    /// 大きなロックなしの pwrite (vm::write_fast) が書いたページに、中身のブロックとして書き出す印 (data) をつける。
    /// いまもそのブロックのキャッシュのページそのものであるものだけ (そのあと切りつめた、捨てたものは、もうファイルのもの
    /// ではない)。Linux の、書いたページにつける PG_dirty にあたる
    fn take_fast_writes(&self) {
        for (ino, index, page) in crate::vm::take_written(self.fs) {
            if let Ok(mut r) = self.read_inode(ino as u32)
                && let Ok(b) = self.map(ino as u32, &mut r, index as u64, false)
                && b != 0
            {
                let mut c = self.cache.borrow_mut();
                if c.map.get(&b) == Some(&page) && !c.dirty.contains(&b) {
                    c.data.insert(b);
                }
            }
            kalloc::put(page);
        }
    }

    pub fn flush(&self) -> Result<(), i64> {
        // 書きかえたものをまとめて書き出す (中の block:write と jbd2:commit は引かれる)
        let _p = crate::prof::span("extfs:flush");
        self.take_fast_writes();
        self.last_flush.set(crate::timer::ticks());
        self.cache.borrow_mut().flushes += 1;
        // 写し (MAP_SHARED) で書いているページに、書き出す印をつけなおす (写しは書けるままなので、前に書き出したあとも書かれる)
        crate::vm::sync_shared();
        let (bb, ib) = {
            let mut c = self.cache.borrow_mut();
            (core::mem::take(&mut c.bb_dirty), core::mem::take(&mut c.ib_dirty))
        };
        // ビットマップのチェックサムをグループディスクリプタへ
        if self.csum {
            for g in bb {
                let n = self.bpg as usize / 8;
                let c = self.with_block(self.block_bitmap(g), |d| crc32c(self.csum_seed, &d[..n]))?;
                self.gd_set_cnt(g, 0x18, 0x38, c);
            }
            for g in ib {
                let n = self.ipg as usize / 8;
                let c = self.with_block(self.inode_bitmap(g), |d| crc32c(self.csum_seed, &d[..n]))?;
                self.gd_set_cnt(g, 0x1a, 0x3a, c);
            }
        }
        let (sb_dirty, gd_dirty) = {
            let mut c = self.cache.borrow_mut();
            (core::mem::take(&mut c.sb_dirty), core::mem::take(&mut c.gd_dirty))
        };
        let mut gdt_blocks = BTreeSet::new();
        for g in gd_dirty {
            self.gd_csum(g);
            gdt_blocks.insert(self.gd_off(g) / self.bsize);
        }
        for blk in gdt_blocks {
            let data = self.gdt.borrow()[blk * self.bsize..(blk + 1) * self.bsize].to_vec();
            self.modify_block(self.first_data_block + 1 + blk as u64, |d| d.copy_from_slice(&data))?;
        }
        if sb_dirty {
            let mut sb = self.sb.borrow_mut();
            if self.csum {
                let c = crc32c(!0, &sb[..0x3fc]);
                put32(&mut sb, 0x3fc, c);
            }
            let data = sb.clone();
            drop(sb);
            // スーパーブロックもメタデータとして、入っているブロックごとジャーナルを通す
            let (b, off) = self.sb_block();
            self.modify_block(b, |d| d[off..off + 1024].copy_from_slice(&data))?;
        }
        // ファイルの中身を先に (data=ordered)。印 (data / dirty) は書き終えてから外す: 書いているあいだに
        // キャッシュがいっぱいになると evict_one が印のないページを捨てるので、先に外すと、まだ書いていない
        // ページが捨てられて、ディスクの古い中身 (消したファイルのものなど) が書かれてしまう
        let data: Vec<u64> = self.cache.borrow().data.iter().copied().collect();
        self.write_blocks(&data)?;
        {
            let mut c = self.cache.borrow_mut();
            for b in &data {
                c.data.remove(b);
            }
            c.written_data += data.len() as u64;
        }
        let meta: Vec<u64> = self.cache.borrow().dirty.iter().copied().collect();
        if meta.is_empty() {
            return Ok(());
        }
        self.flush_meta(&meta)?;
        let mut c = self.cache.borrow_mut();
        for b in &meta {
            c.dirty.remove(b);
        }
        c.written_meta += meta.len() as u64;
        Ok(())
    }

    fn flush_meta(&self, meta: &[u64]) -> Result<(), i64> {
        let mut j = self.journal.borrow_mut();
        if let Some(jr) = j.as_mut() {
            if self.journal_commit(jr, meta)? {
                self.cache.borrow_mut().commits += 1;
                self.write_blocks(meta)?;
                return self.journal_done(jr);
            }
        }
        // ジャーナルがない (ext2 など) か、1 つのトランザクションに入りきらない
        self.write_blocks(meta)
    }

    /// ブロックをディスクの本当の場所へ。となりあうものは 1 回の要求にまとめる
    fn write_blocks(&self, dirty: &[u64]) -> Result<(), i64> {
        // 1 回に 1 MiB まで (4 KiB のブロックで)
        const RUN: usize = 256;
        let spb = (self.bsize / crate::block::SECTOR) as u64;
        let mut i = 0;
        while i < dirty.len() {
            let mut j = i + 1;
            while j < dirty.len() && j - i < RUN && dirty[j] == dirty[j - 1] + 1 {
                j += 1;
            }
            // ひとまとめに書くバッファ。ヒープに大きなかたまりが無ければ (混んでいる、割れている)、1 ブロックずつ
            // キャッシュのページから直接書く (前は取れずに panic した: memory allocation of 880640 bytes failed)
            let mut buf = Vec::new();
            if j - i > 1 && buf.try_reserve_exact((j - i) * self.bsize).is_ok() {
                buf.resize((j - i) * self.bsize, 0);
                for (k, &b) in dirty[i..j].iter().enumerate() {
                    self.settle_csum(b)?;
                    self.with_block(b, |d| buf[k * self.bsize..(k + 1) * self.bsize].copy_from_slice(d))?;
                }
                crate::block::write(dirty[i] * spb, &buf)?;
            } else {
                // キャッシュを借りたままディスクを待たない (眠るあいだにほかが同じキャッシュを使う)
                let mut one = vec![0u8; self.bsize];
                for &b in &dirty[i..j] {
                    self.settle_csum(b)?;
                    self.with_block(b, |d| one.copy_from_slice(d))?;
                    crate::block::write(b * spb, &one)?;
                }
            }
            i = j;
        }
        Ok(())
    }

    // ---- inode ----

    fn inode_loc(&self, ino: u32) -> (u64, usize) {
        let (g, idx) = ((ino - 1) / self.ipg, (ino - 1) % self.ipg);
        let byte = idx as usize * self.inode_size;
        (self.inode_table(g) + (byte / self.bsize) as u64, byte % self.bsize)
    }

    fn read_inode(&self, ino: u32) -> Result<Raw, i64> {
        let (b, off) = self.inode_loc(ino);
        let isz = self.inode_size;
        self.with_block(b, |d| Raw(d[off..off + isz].to_vec()))
    }

    fn write_inode(&self, ino: u32, r: &Raw) -> Result<(), i64> {
        // meta() の覚えを捨てる (書いたあとに読まれたら、新しいものを覚えなおす)。ExtInode の覚えも、大きなロックなしの
        // stat の覚え (statc.rs) も
        self.metas.borrow_mut().remove(&ino);
        self.meta_ver.set(self.meta_ver.get() + 1);
        crate::statc::ino_bump((self.fs, ino as u64));
        let mut r = r.clone();
        self.set_inode_csum(ino, &mut r);
        let (b, off) = self.inode_loc(ino);
        self.modify_block(b, |d| d[off..off + r.0.len()].copy_from_slice(&r.0))
    }

    fn group_of(&self, ino: u32) -> u32 {
        (ino - 1) / self.ipg
    }

    /// ファイルの lb 番目のブロックの場所。alloc なら無いところを作る (0 は穴)。未初期化のブロック (fallocate で
    /// とったもの) は 0 として読むので 0、alloc なら初期化ずみにして (中身は 0) その場所
    fn map(&self, ino: u32, r: &mut Raw, lb: u64, alloc: bool) -> Result<u64, i64> {
        if r.flags() & EXTENTS_FL != 0 {
            let (p, _, uninit) = self.ext_lookup(r, lb)?;
            if uninit {
                if !alloc {
                    return Ok(0);
                }
                self.ext_convert(ino, r, lb, p)?;
                return Ok(p);
            }
            if p != 0 || !alloc {
                return Ok(p);
            }
            // ひとつ前のブロックの隣を狙うとつながりやすい
            let goal = match lb.checked_sub(1).map(|l| self.ext_find(r, l)) {
                Some(Ok(prev)) if prev != 0 => prev + 1,
                _ => self.group_start(self.group_of(ino)),
            };
            let b = self.alloc_block(goal)?;
            let per = (self.bsize / 512) as i64;
            r.add_blocks512(per, self.bsize);
            if let Err(e) = self.ext_insert(ino, r, lb as u32, b) {
                self.free_block(b)?;
                r.add_blocks512(-per, self.bsize);
                return Err(e);
            }
            return Ok(b);
        }
        self.bmap(ino, r, lb, alloc)
    }

    // ---- 直接/間接ブロック (ext2) ----

    fn bmap(&self, ino: u32, r: &mut Raw, fb: u64, alloc: bool) -> Result<u64, i64> {
        let n = (self.bsize / 4) as u64;
        let goal = self.group_start(self.group_of(ino));
        let bsize = self.bsize;
        let fresh = |r: &mut Raw| -> Result<u32, i64> {
            let b = self.alloc_block(goal)?;
            r.add_blocks512((bsize / 512) as i64, bsize);
            Ok(b as u32)
        };
        // (i_block の位置, 間接の段数, その中での番号)
        let (slot, level, mut idx) = if fb < 12 {
            (fb as usize, 0, 0)
        } else if fb < 12 + n {
            (12, 1, fb - 12)
        } else if fb < 12 + n + n * n {
            (13, 2, fb - 12 - n)
        } else if fb < 12 + n + n * n + n * n * n {
            (14, 3, fb - 12 - n - n * n)
        } else {
            return Err(-EFBIG);
        };
        let mut b = r.block(slot);
        if b == 0 {
            if !alloc {
                return Ok(0);
            }
            b = fresh(r)?;
            r.set_block(slot, b);
        }
        for l in (0..level).rev() {
            let span = n.pow(l);
            let i = (idx / span) as usize;
            idx %= span;
            let mut next = self.with_block(b as u64, |d| u32_at(d, i * 4))?;
            if next == 0 {
                if !alloc {
                    return Ok(0);
                }
                next = fresh(r)?;
                self.modify_block(b as u64, |d| put32(d, i * 4, next))?;
            }
            b = next;
        }
        Ok(b as u64)
    }

    /// keep 番目以降のブロックを外す
    fn trunc_blocks(&self, ino: u32, r: &mut Raw, keep: u64) -> Result<(), i64> {
        if r.flags() & EXTENTS_FL != 0 {
            return self.ext_truncate(ino, r, keep);
        }
        let n = (self.bsize / 4) as u64;
        let per = (self.bsize / 512) as i64;
        for i in 0..12 {
            let b = r.block(i);
            if b != 0 && i as u64 >= keep {
                self.free_block(b as u64)?;
                r.set_block(i, 0);
                r.add_blocks512(-per, self.bsize);
            }
        }
        let mut first = 12u64;
        for (slot, level) in [(12usize, 1u32), (13, 2), (14, 3)] {
            let b = r.block(slot);
            if self.trunc_level(r, b, level, first, keep)? {
                r.set_block(slot, 0);
            }
            first += n.pow(level);
        }
        Ok(())
    }

    /// blk (level 段の間接ブロック、first から先を受け持つ) を keep に合わせて切る。全部外したら true
    fn trunc_level(&self, r: &mut Raw, blk: u32, level: u32, first: u64, keep: u64) -> Result<bool, i64> {
        let n = (self.bsize / 4) as u64;
        if blk == 0 {
            return Ok(true);
        }
        if first + n.pow(level) <= keep {
            return Ok(false);
        }
        if level > 0 {
            let child_span = n.pow(level - 1);
            let ptrs = self.with_block(blk as u64, |d| (0..n as usize).map(|i| u32_at(d, i * 4)).collect::<Vec<_>>())?;
            for (i, c) in ptrs.into_iter().enumerate() {
                if c != 0 && self.trunc_level(r, c, level - 1, first + i as u64 * child_span, keep)? {
                    self.modify_block(blk as u64, |d| put32(d, i * 4, 0))?;
                }
            }
        }
        if first >= keep {
            self.free_block(blk as u64)?;
            r.add_blocks512(-((self.bsize / 512) as i64), self.bsize);
            return Ok(true);
        }
        Ok(false)
    }

    // ---- extents (ext4) ----
    //
    // ノード: ヘッダ (magic u16, entries u16, max u16, depth u16, generation u32) に続いて 12 バイトの要素。
    // 葉: (block u32, len u16, start_hi u16, start_lo u32)。len > 32768 は未初期化 (0 として読む)
    // 索引: (block u32, leaf_lo u32, leaf_hi u16, unused u16)

    fn ext_leaf_start(e: &[u8]) -> u64 {
        u32_at(e, 8) as u64 | (u16_at(e, 6) as u64) << 32
    }

    fn ext_index_child(e: &[u8]) -> u64 {
        u32_at(e, 4) as u64 | (u16_at(e, 8) as u64) << 32
    }

    /// lb の物理ブロック (無い・未初期化なら 0)
    fn ext_find(&self, r: &Raw, lb: u64) -> Result<u64, i64> {
        self.ext_run(r, lb).map(|(p, _)| p)
    }

    /// lb の物理ブロックと、そこから同じ extent でつづくブロックの数 (lb を含む)。無い・未初期化なら (0, 1)
    fn ext_run(&self, r: &Raw, lb: u64) -> Result<(u64, u64), i64> {
        let (p, n, uninit) = self.ext_lookup(r, lb)?;
        Ok(if uninit { (0, 1) } else { (p, n) })
    }

    /// ファイルの lb 番目のブロックの場所 (未初期化でも)。穴なら 0 (スワップのファイル: 未初期化のブロックも、
    /// ディスクの上の場所があればよい。Linux の iomap_swapfile_activate と同じ)
    fn map_any(&self, ino: u32, r: &mut Raw, lb: u64) -> Result<u64, i64> {
        if r.flags() & EXTENTS_FL != 0 {
            return self.ext_lookup(r, lb).map(|(p, _, _)| p);
        }
        self.map(ino, r, lb, false)
    }

    /// lb の (物理ブロック, そこから同じ extent でつづくブロックの数 (lb を含む), 未初期化か)。無ければ (0, 1, false)。
    /// 木の節はブロックキャッシュの中でそのまま読む (写さない)
    fn ext_lookup(&self, r: &Raw, lb: u64) -> Result<(u64, u64, bool), i64> {
        enum Step {
            Found(u64, u64, bool),
            Child(u64),
        }
        fn step(node: &[u8], lb: u64) -> Result<Step, i64> {
            if u16_at(node, 0) != EXT_MAGIC {
                return Err(-EIO);
            }
            let n = (u16_at(node, 2) as usize).min((node.len() - 12) / 12);
            if u16_at(node, 6) == 0 {
                for i in 0..n {
                    let e = &node[12 + i * 12..24 + i * 12];
                    let (b, len) = (u32_at(e, 0) as u64, u16_at(e, 4) as u32);
                    let (len, uninit) = if len > EXT_INIT_MAX_LEN { (len - EXT_INIT_MAX_LEN, true) } else { (len, false) };
                    if lb >= b && lb < b + len as u64 {
                        let rest = b + len as u64 - lb;
                        return Ok(Step::Found(ExtFs::ext_leaf_start(e) + (lb - b), rest, uninit));
                    }
                }
                return Ok(Step::Found(0, 1, false));
            }
            let pick = (0..n).rev().find(|&i| u32_at(node, 12 + i * 12) as u64 <= lb);
            let Some(i) = pick else { return Ok(Step::Found(0, 1, false)) };
            Ok(Step::Child(ExtFs::ext_index_child(&node[12 + i * 12..24 + i * 12])))
        }
        let mut next = step(&r.0[40..100], lb)?;
        for _ in 0..8 {
            match next {
                Step::Found(p, n, u) => return Ok((p, n, u)),
                Step::Child(c) => next = self.with_block(c, |d| step(d, lb))??,
            }
        }
        Err(-EIO)
    }

    /// node (木の根か節) の下で、lb から先で最初にデータのある範囲 (論理ブロックの先頭, 数)。先頭は lb より前に
    /// しない。未初期化の extent は穴 (0 として読むので)。なければ None (SEEK_DATA / SEEK_HOLE)
    fn ext_next_data(&self, node: &[u8], lb: u64, depth: u32) -> Result<Option<(u64, u64)>, i64> {
        if u16_at(node, 0) != EXT_MAGIC || depth > 8 {
            return Err(-EIO);
        }
        let n = (u16_at(node, 2) as usize).min((node.len() - 12) / 12);
        let e = |i: usize| &node[12 + i * 12..24 + i * 12];
        if u16_at(node, 6) == 0 {
            for i in 0..n {
                let (b, len) = (u32_at(e(i), 0) as u64, u16_at(e(i), 4) as u32);
                if len > EXT_INIT_MAX_LEN {
                    continue;
                }
                let end = b + len as u64;
                if end > lb {
                    return Ok(Some((b.max(lb), end - b.max(lb))));
                }
            }
            return Ok(None);
        }
        // 索引: i 番目の子は、次の子の始まりの前まで
        for i in 0..n {
            if i + 1 < n && u32_at(e(i + 1), 0) as u64 <= lb {
                continue;
            }
            let child = self.with_block(ExtFs::ext_index_child(e(i)), |d| d.to_vec())?;
            if let Some(x) = self.ext_next_data(&child, lb, depth + 1)? {
                return Ok(Some(x));
            }
        }
        Ok(None)
    }

    fn ext_init_root(r: &mut Raw) {
        let ib = r.iblock();
        ib.fill(0);
        put16(ib, 0, EXT_MAGIC);
        put16(ib, 4, 4);
        r.set_flags(r.flags() | EXTENTS_FL);
    }

    /// 葉 (node) で lb → pb (raw は長さ。未初期化なら EXT_INIT_MAX_LEN を足したもの) を、前の同じ種類の extent に
    /// つなげられるならつなげる (長さは初期化ずみ 32768、未初期化 32767 まで)
    fn leaf_merge(node: &mut [u8], lb: u32, pb: u64, raw: u32) -> bool {
        let split = |raw: u32| if raw > EXT_INIT_MAX_LEN { (raw - EXT_INIT_MAX_LEN, true) } else { (raw, false) };
        let (cnt, uninit) = split(raw);
        let max = if uninit { EXT_INIT_MAX_LEN - 1 } else { EXT_INIT_MAX_LEN };
        let n = u16_at(node, 2) as usize;
        for i in 0..n {
            let e = 12 + i * 12;
            let b = u32_at(node, e) as u64;
            let (len, u) = split(u16_at(node, e + 4) as u32);
            if u == uninit && len + cnt <= max && b + len as u64 == lb as u64 && ExtFs::ext_leaf_start(&node[e..e + 12]) + len as u64 == pb {
                put16(node, e + 4, (len + cnt + if uninit { EXT_INIT_MAX_LEN } else { 0 }) as u16);
                return true;
            }
        }
        false
    }

    /// 12 バイトの要素をキー (先頭の u32) の順に差し込む。入らなければ false
    fn node_put(node: &mut [u8], ent: &[u8; 12]) -> bool {
        let n = u16_at(node, 2) as usize;
        if n >= u16_at(node, 4) as usize {
            return false;
        }
        let key = u32_at(ent, 0);
        let at = (0..n).find(|&i| u32_at(node, 12 + i * 12) > key).unwrap_or(n);
        node.copy_within(12 + at * 12..12 + n * 12, 24 + at * 12);
        node[12 + at * 12..24 + at * 12].copy_from_slice(ent);
        put16(node, 2, (n + 1) as u16);
        true
    }

    /// raw_len は長さ (未初期化なら EXT_INIT_MAX_LEN を足したもの)
    fn leaf_ent(lb: u32, pb: u64, raw_len: u32) -> [u8; 12] {
        let mut e = [0u8; 12];
        put32(&mut e, 0, lb);
        put16(&mut e, 4, raw_len as u16);
        put16(&mut e, 6, (pb >> 32) as u16);
        put32(&mut e, 8, pb as u32);
        e
    }

    fn index_ent(key: u32, child: u64) -> [u8; 12] {
        let mut e = [0u8; 12];
        put32(&mut e, 0, key);
        put32(&mut e, 4, child as u32);
        put16(&mut e, 8, (child >> 32) as u16);
        e
    }

    /// 新しい (空の) ノードブロック
    fn new_node(&self, ino: u32, r: &mut Raw, depth: u16) -> Result<(u64, Vec<u8>), i64> {
        let b = self.alloc_block(self.group_start(self.group_of(ino)))?;
        r.add_blocks512((self.bsize / 512) as i64, self.bsize);
        let mut d = vec![0u8; self.bsize];
        put16(&mut d, 0, EXT_MAGIC);
        put16(&mut d, 4, ((self.bsize - 12) / 12 - if self.csum { 1 } else { 0 }) as u16);
        put16(&mut d, 6, depth);
        Ok((b, d))
    }

    fn write_node(&self, ino: u32, generation: u32, b: u64, node: &[u8]) -> Result<(), i64> {
        self.modify_block(b, |d| {
            d[..node.len()].copy_from_slice(node);
            self.set_extent_csum(ino, generation, d);
        })
    }

    /// node に ent を入れる。あふれたら半分を新しいブロックへ分けて (キー, ブロック) を返す
    fn put_or_split(&self, ino: u32, r: &mut Raw, node: &mut [u8], ent: &[u8; 12]) -> Result<Option<(u32, u64)>, i64> {
        if ExtFs::node_put(node, ent) {
            return Ok(None);
        }
        let n = u16_at(node, 2) as usize;
        let key = u32_at(ent, 0);
        // 後ろに足すだけなら今のノードはいっぱいのままにして、右に新しいノード
        let keep = if key > u32_at(node, 12 + (n - 1) * 12) { n } else { n / 2 };
        let (nb, mut right) = self.new_node(ino, r, u16_at(node, 6))?;
        let moved = n - keep;
        right[12..12 + moved * 12].copy_from_slice(&node[12 + keep * 12..12 + n * 12]);
        put16(&mut right, 2, moved as u16);
        put16(node, 2, keep as u16);
        if moved == 0 || key >= u32_at(&right, 12) {
            ExtFs::node_put(&mut right, ent);
        } else {
            ExtFs::node_put(node, ent);
        }
        let split = u32_at(&right, 12);
        self.write_node(ino, r.gen_no(), nb, &right)?;
        Ok(Some((split, nb)))
    }

    /// node (深さ depth) の下に lb → pb (raw は長さ。未初期化なら EXT_INIT_MAX_LEN を足したもの。その範囲はまだ
    /// どこにもないこと) を入れる。node が分かれたら (キー, 新しいブロック)
    fn insert_rec(&self, ino: u32, r: &mut Raw, node: &mut [u8], lb: u32, pb: u64, raw: u32) -> Result<Option<(u32, u64)>, i64> {
        if u16_at(node, 0) != EXT_MAGIC {
            return Err(-EIO);
        }
        if u16_at(node, 6) == 0 {
            if ExtFs::leaf_merge(node, lb, pb, raw) {
                return Ok(None);
            }
            return self.put_or_split(ino, r, node, &ExtFs::leaf_ent(lb, pb, raw));
        }
        let n = u16_at(node, 2) as usize;
        let i = (0..n).rev().find(|&i| u32_at(node, 12 + i * 12) <= lb).unwrap_or(0);
        if u32_at(node, 12 + i * 12) > lb {
            put32(node, 12 + i * 12, lb); // 一番左より前に入るので、キーを下げる
        }
        let child = ExtFs::ext_index_child(&node[12 + i * 12..24 + i * 12]);
        let mut cnode = self.read_block(child)?;
        let split = self.insert_rec(ino, r, &mut cnode, lb, pb, raw)?;
        self.write_node(ino, r.gen_no(), child, &cnode)?;
        match split {
            None => Ok(None),
            Some((key, b)) => self.put_or_split(ino, r, node, &ExtFs::index_ent(key, b)),
        }
    }

    /// lb → pb を extents に足す
    fn ext_insert(&self, ino: u32, r: &mut Raw, lb: u32, pb: u64) -> Result<(), i64> {
        self.ext_insert_run(ino, r, lb, pb, 1)
    }

    /// lb → pb から raw (長さ。未初期化なら EXT_INIT_MAX_LEN を足したもの) を extents に足す。根があふれたら中身を
    /// 新しいブロックへ下ろして 1 段深くする
    fn ext_insert_run(&self, ino: u32, r: &mut Raw, lb: u32, pb: u64, raw: u32) -> Result<(), i64> {
        let mut root = r.0[40..100].to_vec();
        let split = self.insert_rec(ino, r, &mut root, lb, pb, raw)?;
        if let Some((key, b)) = split {
            let depth = u16_at(&root, 6);
            if depth >= 4 {
                return Err(-EFBIG);
            }
            let (cb, mut child) = self.new_node(ino, r, depth)?;
            let n = u16_at(&root, 2) as usize;
            child[12..12 + n * 12].copy_from_slice(&root[12..12 + n * 12]);
            put16(&mut child, 2, n as u16);
            self.write_node(ino, r.gen_no(), cb, &child)?;
            root[12..].fill(0);
            put16(&mut root, 2, 0);
            put16(&mut root, 6, depth + 1);
            ExtFs::node_put(&mut root, &ExtFs::index_ent(u32_at(&child, 12), cb));
            ExtFs::node_put(&mut root, &ExtFs::index_ent(key, b));
        }
        r.0[40..100].copy_from_slice(&root);
        Ok(())
    }

    /// 葉/索引のノードを keep に合わせて切る。空になったら true
    fn ext_trunc_node(&self, ino: u32, generation: u32, r: &mut Raw, node: &mut Vec<u8>, keep: u64) -> Result<bool, i64> {
        let n = u16_at(node, 2) as usize;
        let depth = u16_at(node, 6);
        let per = (self.bsize / 512) as i64;
        let mut out = 0;
        for i in 0..n {
            let e = 12 + i * 12;
            let ent: Vec<u8> = node[e..e + 12].to_vec();
            let b = u32_at(&ent, 0) as u64;
            let mut keep_ent = true;
            if depth == 0 {
                let raw_len = u16_at(&ent, 4) as u32;
                let (len, uninit) = if raw_len > EXT_INIT_MAX_LEN { (raw_len - EXT_INIT_MAX_LEN, true) } else { (raw_len, false) };
                let start = ExtFs::ext_leaf_start(&ent);
                let new_len = keep.saturating_sub(b).min(len as u64) as u32;
                for k in new_len..len {
                    self.free_block(start + k as u64)?;
                    r.add_blocks512(-per, self.bsize);
                }
                if new_len == 0 {
                    keep_ent = false;
                } else if new_len != len {
                    let l = if uninit { new_len + EXT_INIT_MAX_LEN } else { new_len };
                    put16(node, e + 4, l as u16);
                }
            } else {
                let child = ExtFs::ext_index_child(&ent);
                let mut cnode = self.read_block(child)?;
                if self.ext_trunc_node(ino, generation, r, &mut cnode, keep)? {
                    self.free_block(child)?;
                    r.add_blocks512(-per, self.bsize);
                    keep_ent = false;
                } else {
                    self.write_node(ino, generation, child, &cnode)?;
                }
            }
            if keep_ent {
                let cur: Vec<u8> = node[e..e + 12].to_vec();
                node[12 + out * 12..24 + out * 12].copy_from_slice(&cur);
                out += 1;
            }
        }
        put16(node, 2, out as u16);
        Ok(out == 0)
    }

    /// 葉/索引のノード (node) から論理ブロック [from, to) を外す (free ならブロックも返す)。extent のまん中を外すと
    /// 2 つに分かれる: ノードに入りきらなければ、いちばん後ろの extent を again へ (論理ブロック, 物理ブロック, raw の
    /// 長さ) 出して、あとで入れなおす。索引のキーは子の最初に合わせる (e2fsck が見る)。空になったら true
    #[allow(clippy::too_many_arguments)]
    fn ext_remove_node(&self, ino: u32, generation: u32, r: &mut Raw, node: &mut [u8], from: u64, to: u64, free: bool, again: &mut Vec<(u32, u64, u32)>) -> Result<bool, i64> {
        if u16_at(node, 0) != EXT_MAGIC {
            return Err(-EIO);
        }
        let cap = (node.len() - 12) / 12;
        let n = (u16_at(node, 2) as usize).min(cap);
        let max = (u16_at(node, 4) as usize).min(cap);
        let depth = u16_at(node, 6);
        let per = (self.bsize / 512) as i64;
        let mut out: Vec<[u8; 12]> = Vec::with_capacity(n + 1);
        for i in 0..n {
            let e: [u8; 12] = node[12 + i * 12..24 + i * 12].try_into().unwrap();
            let b = u32_at(&e, 0) as u64;
            if depth == 0 {
                let raw = u16_at(&e, 4) as u32;
                let (len, uninit) = if raw > EXT_INIT_MAX_LEN { (raw - EXT_INIT_MAX_LEN, true) } else { (raw, false) };
                let end = b + len as u64;
                if end <= from || b >= to {
                    out.push(e);
                    continue;
                }
                let start = ExtFs::ext_leaf_start(&e);
                let (cs, ce) = (b.max(from), end.min(to));
                if free {
                    for k in cs..ce {
                        self.free_block(start + (k - b))?;
                        r.add_blocks512(-per, self.bsize);
                    }
                }
                let piece = |lb: u64, len: u64| {
                    let raw = len as u32 + if uninit { EXT_INIT_MAX_LEN } else { 0 };
                    ExtFs::leaf_ent(lb as u32, start + (lb - b), raw)
                };
                if cs > b {
                    out.push(piece(b, cs - b));
                }
                if ce < end {
                    out.push(piece(ce, end - ce));
                }
            } else {
                // i 番目の子は、次の子の始まりの前まで
                let next = if i + 1 < n { u32_at(node, 24 + i * 12) as u64 } else { u64::MAX };
                if next <= from || b >= to {
                    out.push(e);
                    continue;
                }
                let child = ExtFs::ext_index_child(&e);
                let mut cnode = self.read_block(child)?;
                if self.ext_remove_node(ino, generation, r, &mut cnode, from, to, free, again)? {
                    self.free_block(child)?;
                    r.add_blocks512(-per, self.bsize);
                } else {
                    self.write_node(ino, generation, child, &cnode)?;
                    let mut e = e;
                    put32(&mut e, 0, u32_at(&cnode, 12));
                    out.push(e);
                }
            }
        }
        while out.len() > max {
            let x = out.pop().unwrap();
            again.push((u32_at(&x, 0), ExtFs::ext_leaf_start(&x), u16_at(&x, 4) as u32));
        }
        node[12..12 + n.max(out.len()) * 12].fill(0);
        for (i, x) in out.iter().enumerate() {
            node[12 + i * 12..24 + i * 12].copy_from_slice(x);
        }
        put16(node, 2, out.len() as u16);
        Ok(out.is_empty())
    }

    /// 論理ブロック [from, to) を extent の木から外す (free ならブロックも返す)
    fn ext_remove(&self, ino: u32, r: &mut Raw, from: u64, to: u64, free: bool) -> Result<(), i64> {
        let generation = r.gen_no();
        let mut root = r.0[40..100].to_vec();
        let mut again = vec![];
        let empty = self.ext_remove_node(ino, generation, r, &mut root, from, to, free, &mut again)?;
        r.0[40..100].copy_from_slice(&root);
        if empty {
            ExtFs::ext_init_root(r);
        }
        for (lb, pb, raw) in again {
            self.ext_insert_run(ino, r, lb, pb, raw)?;
        }
        Ok(())
    }

    /// 未初期化のブロック lb (場所 pb) を初期化ずみにする (中身は 0): そこへ書くとき。Linux の
    /// ext4_ext_convert_to_initialized と同じく、未初期化の extent は前・このブロック・後ろに分かれる
    fn ext_convert(&self, ino: u32, r: &mut Raw, lb: u64, pb: u64) -> Result<(), i64> {
        self.ext_remove(ino, r, lb, lb + 1, false)?;
        if let Err(e) = self.ext_insert_run(ino, r, lb as u32, pb, 1) {
            // 入れられなかった (ノードのブロックがとれない): このブロックは穴にもどす
            self.free_block(pb)?;
            r.add_blocks512(-((self.bsize / 512) as i64), self.bsize);
            return Err(e);
        }
        // ディスクの上の中身は古いもの: 0 にしてから書く (写しの穴のページがあればそれをこのブロックのページに)
        self.modify_data_block_adopt(pb, true, Some((ino, lb)), |d| d.fill(0))
    }

    /// 論理ブロック [from, to) の穴に、未初期化のブロックをとる (fallocate。Linux の EXT4_GET_BLOCKS_CREATE_UNWRIT_EXT:
    /// 0 として読み、SEEK_DATA では穴。書くときに初期化ずみになる)。もうあるものはそのまま。とれなくなったら
    /// ENOSPC (とった分は残す。Linux と同じ)
    fn ext_prealloc(&self, ino: u32, r: &mut Raw, from: u64, to: u64) -> Result<(), i64> {
        let per = (self.bsize / 512) as i64;
        // まだ木に入れていない、つながったもの (論理, 物理, 数)
        let mut run: Option<(u64, u64, u32)> = None;
        let flush = |r: &mut Raw, run: &mut Option<(u64, u64, u32)>| -> Result<(), i64> {
            match run.take() {
                Some((l, b, k)) => self.ext_insert_run(ino, r, l as u32, b, k + EXT_INIT_MAX_LEN),
                None => Ok(()),
            }
        };
        let mut lb = from;
        while lb < to {
            let (p, n, _) = self.ext_lookup(r, lb)?;
            if p != 0 {
                flush(r, &mut run)?;
                lb += n;
                continue;
            }
            // つづけてとると、ディスクの上でもつながりやすい
            let goal = match run {
                Some((_, b, k)) => b + k as u64,
                None => match lb.checked_sub(1).map(|l| self.ext_lookup(r, l)) {
                    Some(Ok((p, _, _))) if p != 0 => p + 1,
                    _ => self.group_start(self.group_of(ino)),
                },
            };
            let b = match self.alloc_block(goal) {
                Ok(b) => b,
                Err(e) => {
                    flush(r, &mut run)?;
                    return Err(e);
                }
            };
            r.add_blocks512(per, self.bsize);
            match &mut run {
                Some((_, rb, k)) if *rb + *k as u64 == b && *k < EXT_INIT_MAX_LEN - 1 => *k += 1,
                _ => {
                    flush(r, &mut run)?;
                    run = Some((lb, b, 1));
                }
            }
            lb += 1;
        }
        flush(r, &mut run)
    }

    /// ファイル (extent のもの) の [from, to) バイトを 0 にする (fallocate の PUNCH_HOLE と ZERO_RANGE): まるごと入る
    /// ブロックは外して穴に、はしのブロックは、あれば中の分を 0 に (Linux の ext4_zero_partial_blocks)
    fn punch(&self, ino: u32, r: &mut Raw, from: u64, to: u64) -> Result<(), i64> {
        if from >= to {
            return Ok(());
        }
        let bs = self.bsize as u64;
        let zero = |r: &mut Raw, lb: u64, a: u64, b: u64| -> Result<(), i64> {
            let p = self.map(ino, r, lb, false)?;
            if p != 0 {
                self.modify_data_block(p, |d| d[a as usize..b as usize].fill(0))?;
            }
            Ok(())
        };
        if from / bs == (to - 1) / bs && (from % bs != 0 || to % bs != 0) {
            zero(r, from / bs, from % bs, (to - 1) % bs + 1)?;
        } else {
            if from % bs != 0 {
                zero(r, from / bs, from % bs, bs)?;
            }
            if to % bs != 0 {
                zero(r, to / bs, 0, to % bs)?;
            }
        }
        let (first, last) = (from.div_ceil(bs), to / bs);
        if first < last {
            self.ext_remove(ino, r, first, last, true)?;
        }
        Ok(())
    }

    fn ext_truncate(&self, ino: u32, r: &mut Raw, keep: u64) -> Result<(), i64> {
        let generation = r.gen_no();
        let mut root = r.0[40..100].to_vec();
        let empty = self.ext_trunc_node(ino, generation, r, &mut root, keep)?;
        r.0[40..100].copy_from_slice(&root);
        if empty {
            ExtFs::ext_init_root(r);
        }
        Ok(())
    }

    // ---- 中身の読み書き ----

    fn read_data(&self, ino: u32, off: usize, buf: &mut [u8]) -> Result<usize, i64> {
        self.read_data_raw(ino, self.read_inode(ino)?, off, buf)
    }

    /// 読んである inode (r) で
    fn read_data_raw(&self, ino: u32, mut r: Raw, off: usize, buf: &mut [u8]) -> Result<usize, i64> {
        if r.flags() & INLINE_DATA_FL != 0 {
            return Err(-EIO);
        }
        let size = r.size() as usize;
        let len = buf.len().min(size.saturating_sub(off));
        let mut done = 0;
        let extents = r.flags() & EXTENTS_FL != 0;
        let dontcache = crate::proc::dontcache_on();
        // いまの extent (ファイルのブロック fb から n 個が、ディスクの pb から並んでいる)。ブロックごとに木を引かない
        let mut run: (u64, u64, u64) = (0, 0, 0);
        while done < len {
            let pos = off + done;
            let (fb, bo) = ((pos / self.bsize) as u64, pos % self.bsize);
            let n = (self.bsize - bo).min(len - done);
            if !(fb >= run.0 && fb < run.0 + run.2) {
                let next = if extents {
                    self.ext_run(&r, fb).map(|(p, k)| (fb, p, if p == 0 { 1 } else { k }))
                } else {
                    self.map(ino, &mut r, fb, false).map(|p| (fb, p, 1))
                };
                run = match next {
                    Ok(x) => x,
                    // noio でメタデータがキャッシュになかった: ここまでの分 (なければ EAGAIN)
                    Err(e) if e == -NOIO_EAGAIN && done > 0 => return Ok(done),
                    Err(e) => return Err(e),
                };
            }
            let b = if run.1 == 0 { 0 } else { run.1 + (fb - run.0) };
            if b == 0 {
                // 穴 (と未初期化のブロック): ディスクを読まない区間 (RWF_NOWAIT) なら、Linux と同じくキャッシュにないので
                // ここまで (なければ EAGAIN)。写しの表の 0 のページで読めるものは、その前に vm::file_page が読んでいる
                if crate::proc::noio_on() {
                    return if done > 0 { Ok(done) } else { Err(-NOIO_EAGAIN) };
                }
                buf[done..done + n].fill(0);
            } else {
                let mut fresh = 0;
                if !self.cached(b) {
                    // ディスクを読まない区間 (RWF_NOWAIT): ここまでの分 (なければ EAGAIN)
                    if crate::proc::noio_on() {
                        return if done > 0 { Ok(done) } else { Err(-NOIO_EAGAIN) };
                    }
                    // ディスクの上でつながっている後ろのブロックも 1 回で読む (最大 READ_AHEAD)
                    let last = (size.div_ceil(self.bsize) as u64).saturating_sub(fb);
                    let mut k = 1;
                    while (k as u64) < last.min(READ_AHEAD as u64)
                        && self.map(ino, &mut r, fb + k as u64, false)? == b + k as u64
                        && !self.cached(b + k as u64)
                    {
                        k += 1;
                    }
                    self.read_ahead(b, k)?;
                    fresh = k as u64;
                }
                self.with_block(b, |d| buf[done..done + n].copy_from_slice(&d[bo..bo + n]))?;
                // RWF_DONTCACHE: いま読んだブロックはキャッシュに残さない (前からあったものは残る: Linux の dropbehind)
                if dontcache && fresh > 0 {
                    let mut c = self.cache.borrow_mut();
                    for j in 0..fresh {
                        if !c.data.contains(&(b + j)) && !c.dirty.contains(&(b + j)) && c.map.get(&(b + j)).is_some_and(|&p| kalloc::refs(p) <= 1) {
                            c.forget(b + j);
                        }
                    }
                }
            }
            done += n;
        }
        Ok(len)
    }

    /// pages があれば、書いたブロックの (ファイルのブロックの番号, ブロック, ページ) を入れる (参照をひとつ足して:
    /// ページキャッシュに入れる。vm::file_written)
    fn write_data(&self, ino: u32, r: &mut Raw, off: usize, buf: &[u8], mut pages: Option<&mut Vec<(u64, u64, *mut u8)>>) -> Result<usize, i64> {
        let mut done = 0;
        // 終わりの先から書く: 終わりとのあいだ (終わりのブロックの中) を 0 に (写しで書いたものがあれば消える)
        if off > r.size() as usize && !buf.is_empty() {
            self.zero_gap(ino, r, r.size() as usize, off);
        }
        while done < buf.len() {
            let pos = off + done;
            let (fb, bo) = ((pos / self.bsize) as u64, pos % self.bsize);
            let n = (self.bsize - bo).min(buf.len() - done);
            // まだないブロック (これから作る) か、まるごと書きかえるなら、ディスクから読まない
            let fresh = (bo == 0 && n == self.bsize) || self.map(ino, r, fb, false)? == 0;
            let b = match self.map(ino, r, fb, true) {
                Ok(b) => b,
                Err(e) if done > 0 => {
                    let _ = e;
                    break;
                }
                Err(e) => return Err(e),
            };
            let p = self.modify_data_block_adopt(b, fresh, Some((ino, fb)), |d| {
                d[bo..bo + n].copy_from_slice(&buf[done..done + n]);
                d.as_mut_ptr()
            })?;
            if let Some(v) = pages.as_deref_mut() {
                kalloc::get(p);
                v.push((fb, b, p));
            }
            done += n;
        }
        if (off + done) as u64 > r.size() {
            r.set_size((off + done) as u64);
        }
        r.touch();
        Ok(done)
    }

    // ---- ディレクトリ ----

    /// (ブロック番号, ブロック内の位置, inode, rec_len, name, filetype) を順に。末尾の偽エントリは除く
    fn dir_entries(&self, ino: u32) -> Result<Vec<(u64, usize, u32, usize, String, u8)>, i64> {
        let mut r = self.read_inode(ino)?;
        let nblocks = r.size().div_ceil(self.bsize as u64);
        let mut out = vec![];
        for fb in 0..nblocks {
            let b = self.map(ino, &mut r, fb, false)?;
            if b == 0 {
                continue;
            }
            self.with_block(b, |d| {
                let mut o = 0;
                while o + 8 <= self.bsize {
                    let (i, rl, nl, ft) = (u32_at(d, o), u16_at(d, o + 4) as usize, d[o + 6] as usize, d[o + 7]);
                    if rl < 8 || o + rl > self.bsize {
                        break;
                    }
                    let tail = i == 0 && rl == DIR_TAIL && nl == 0 && ft == DIR_TAIL_FT && o + rl == self.bsize;
                    if !tail {
                        let name = String::from_utf8_lossy(&d[o + 8..o + 8 + nl.min(rl - 8)]).to_string();
                        out.push((b, o, i, rl, name, ft));
                    }
                    o += rl;
                }
            })?;
        }
        Ok(out)
    }

    /// dir (その inode r) の項目を順に (末尾の偽エントリは除く) f(ブロック番号, ブロック, 位置, 同じブロックの前の項目の
    /// 位置) に。f が Some を返したらそこで終わる。dir_entries とちがい、名前の文字列を作らない (作る・消すたびに引く)
    fn scan_dir<T>(&self, ino: u32, mut r: Raw, mut f: impl FnMut(u64, &[u8], usize, Option<usize>) -> Option<T>) -> Result<Option<T>, i64> {
        let nblocks = r.size().div_ceil(self.bsize as u64);
        for fb in 0..nblocks {
            let b = self.map(ino, &mut r, fb, false)?;
            if b == 0 {
                continue;
            }
            let hit = self.with_block(b, |d| {
                let (mut o, mut prev) = (0, None);
                while o + 8 <= self.bsize {
                    let (i, rl, nl, ft) = (u32_at(d, o), u16_at(d, o + 4) as usize, d[o + 6] as usize, d[o + 7]);
                    if rl < 8 || o + rl > self.bsize {
                        break;
                    }
                    let tail = i == 0 && rl == DIR_TAIL && nl == 0 && ft == DIR_TAIL_FT && o + rl == self.bsize;
                    if !tail && let Some(t) = f(b, d, o, prev) {
                        return Some(t);
                    }
                    prev = Some(o);
                    o += rl;
                }
                None
            })?;
            if hit.is_some() {
                return Ok(hit);
            }
        }
        Ok(None)
    }

    /// ディレクトリの項目の種類 (ft) から mode の種類。書かれていなければ inode を読む
    fn mode_of_ft(&self, ino: u32, ft: u8) -> u32 {
        match ft {
            1 => S_IFREG,
            2 => S_IFDIR,
            3 => S_IFCHR,
            5 => S_IFIFO,
            6 => S_IFSOCK,
            7 => S_IFLNK,
            _ => self.read_inode(ino).map_or(0, |r| r.mode()),
        }
    }

    /// dir の名前のキャッシュを捨てる
    fn forget_names(&self, dir: u32) {
        self.names_gen.set(self.names_gen.get() + 1);
        self.names.borrow_mut().forget(dir);
    }

    fn find(&self, dir: u32, name: &str) -> Result<u32, i64> {
        if let Some(ino) = self.names.borrow().get(dir, name) {
            return Ok(ino);
        }
        let generation = self.names_gen.get();
        let ino = self.find_uncached(dir, name)?;
        if self.names_gen.get() == generation {
            self.names.borrow_mut().insert(dir, String::from(name), ino);
        }
        Ok(ino)
    }

    fn find_uncached(&self, dir: u32, name: &str) -> Result<u32, i64> {
        // 索引があれば 1 つの葉だけ (決められなければ全部)
        let mut r = self.read_inode(dir)?;
        if r.flags() & INDEX_FL != 0 && name != "." && name != ".." {
            match self.dx_find(dir, name) {
                Ok(Some(ino)) => return Ok(ino),
                Ok(None) => return Err(-ENOENT),
                Err(_) => {}
            }
        }
        // 索引がない: ブロックを順に、その場でくらべる (名前の文字列は作らない)
        let nblocks = r.size().div_ceil(self.bsize as u64);
        for fb in 0..nblocks {
            let b = self.map(dir, &mut r, fb, false)?;
            if b != 0 && let Some(ino) = self.block_find(b, name.as_bytes())? {
                return Ok(ino);
            }
        }
        Err(-ENOENT)
    }

    /// ディレクトリのブロック b の中の name の inode
    pub(super) fn block_find(&self, b: u64, name: &[u8]) -> Result<Option<u32>, i64> {
        self.with_block(b, |d| {
            let mut o = 0;
            while o + 8 <= self.bsize {
                let (i, rl, nl) = (u32_at(d, o), u16_at(d, o + 4) as usize, d[o + 6] as usize);
                if rl < 8 || o + rl > self.bsize {
                    break;
                }
                if i != 0 && nl == name.len() && nl <= rl - 8 && &d[o + 8..o + 8 + nl] == name {
                    return Some(i);
                }
                o += rl;
            }
            None
        })
    }

    /// ディレクトリのブロックを書きかえる。チェックサムは写すときに (settle_csum)
    fn modify_dir_block(&self, dir: u32, generation: u32, b: u64, f: impl FnOnce(&mut [u8])) -> Result<(), i64> {
        self.forget_names(dir);
        self.modify_block(b, f)?;
        if self.csum {
            self.cache.borrow_mut().dir_csum.insert(b, (dir, generation));
        }
        Ok(())
    }

    fn dir_writable(&self, dir: u32) -> Result<Raw, i64> {
        self.check_rw()?;
        self.read_inode(dir)
    }

    /// 空のディレクトリブロック (metadata_csum なら末尾に偽エントリ)
    fn init_dir_block(&self, d: &mut [u8]) {
        d.fill(0);
        let end = if self.csum { self.bsize - DIR_TAIL } else { self.bsize };
        put16(d, 4, end as u16);
        if self.csum {
            let t = self.bsize - DIR_TAIL;
            put16(d, t + 4, DIR_TAIL as u16);
            d[t + 7] = DIR_TAIL_FT;
        }
    }

    fn add_entry(&self, dir: u32, name: &str, ino: u32, mode: u32) -> Result<(), i64> {
        if name.len() > 255 {
            return Err(-ENAMETOOLONG);
        }
        let r = self.dir_writable(dir)?;
        // 消したディレクトリ (開いたまま rmdir されたもの) には作れない (Linux の IS_DEADDIR)
        if r.links() == 0 {
            return Err(-ENOENT);
        }
        self.add_entry_in(dir, r, name, ino, mode, None)
    }

    /// 作るところ (create_node) が、名前を足す前に 1 度だけなめる (Linux の ext4_add_entry がすき間を探すのと同じ 1 回):
    /// name がもうあれば EEXIST、なければ入るすき間 (ブロック, 位置, rec_len, 使っている長さ)。入るところがなければ None
    /// (add_entry_in がブロックを足す)。索引つきなら dx_find で確かめるだけ (すき間は dx_add_entry が葉で探す)
    fn new_slot(&self, dir: u32, r: &Raw, name: &str) -> Result<Option<Slot>, i64> {
        if r.flags() & INDEX_FL != 0 {
            return if self.find(dir, name).is_ok() { Err(-EEXIST) } else { Ok(None) };
        }
        let need = rec_len_for(name.len());
        let mut fit = None;
        let dup = self.scan_dir(dir, r.clone(), |b, d, o, _| {
            let (i, rl, nl) = (u32_at(d, o), u16_at(d, o + 4) as usize, d[o + 6] as usize);
            if i != 0 && nl <= rl - 8 && &d[o + 8..o + 8 + nl] == name.as_bytes() {
                return Some(());
            }
            let used = if i == 0 { 0 } else { rec_len_for(nl.min(rl - 8)) };
            if fit.is_none() && rl >= used + need {
                fit = Some((b, o, rl, used));
            }
            None
        })?;
        if dup.is_some() { Err(-EEXIST) } else { Ok(fit) }
    }

    /// dir (inode r) に name を足す。slot は new_slot で見つけたすき間 (Some(None) はすき間がない)。None ならここで探す
    fn add_entry_in(&self, dir: u32, mut r: Raw, name: &str, ino: u32, mode: u32, slot: Option<Option<Slot>>) -> Result<(), i64> {
        if name.len() > 255 {
            return Err(-ENAMETOOLONG);
        }
        if r.flags() & INDEX_FL != 0 {
            self.dx_add_entry(dir, &mut r, name, ino, mode)?;
            return self.dir_changed(dir);
        }
        let generation = r.gen_no();
        let need = rec_len_for(name.len());
        let write_entry = |d: &mut [u8], o: usize, rl: usize| {
            put32(d, o, ino);
            put16(d, o + 4, rl as u16);
            d[o + 6] = name.len() as u8;
            d[o + 7] = ftype(mode);
            d[o + 8..o + 8 + name.len()].copy_from_slice(name.as_bytes());
        };
        // 既存のすき間を探す
        let fit = match slot {
            Some(s) => s,
            None => self.scan_dir(dir, r.clone(), |b, d, o, _| {
                let (i, rl, nl) = (u32_at(d, o), u16_at(d, o + 4) as usize, d[o + 6] as usize);
                let used = if i == 0 { 0 } else { rec_len_for(nl.min(rl - 8)) };
                (rl >= used + need).then_some((b, o, rl, used))
            })?,
        };
        if let Some((b, o, rl, used)) = fit {
            self.modify_dir_block(dir, generation, b, |d| {
                if used == 0 {
                    write_entry(d, o, rl);
                } else {
                    put16(d, o + 4, used as u16);
                    write_entry(d, o + used, rl - used);
                }
            })?;
            r.touch();
            return self.write_inode(dir, &r);
        }
        // 1 ブロックに入りきらなくなったら索引つきに
        if self.make_indexed(dir, &mut r)? {
            self.dx_add_entry(dir, &mut r, name, ino, mode)?;
            return self.dir_changed(dir);
        }
        // 新しいブロックを足す
        let fb = r.size() / self.bsize as u64;
        let b = self.map(dir, &mut r, fb, true)?;
        let end = if self.csum { self.bsize - DIR_TAIL } else { self.bsize };
        self.modify_dir_block(dir, generation, b, |d| {
            self.init_dir_block(d);
            write_entry(d, 0, end);
        })?;
        r.set_size(r.size() + self.bsize as u64);
        r.touch();
        self.write_inode(dir, &r)
    }

    /// 項目 name を、ino (種類 mode) を指すように書きかえる。名前もハッシュも変わらないので、その場で
    /// (RENAME_EXCHANGE。set_dotdot と同じ)
    fn set_entry(&self, dir: u32, name: &str, ino: u32, mode: u32) -> Result<(), i64> {
        let r = self.dir_writable(dir)?;
        let generation = r.gen_no();
        let (b, o) = self.dir_entries(dir)?.into_iter().find(|e| e.2 != 0 && e.4 == name).map(|e| (e.0, e.1)).ok_or(-ENOENT)?;
        self.modify_dir_block(dir, generation, b, |d| {
            put32(d, o, ino);
            d[o + 7] = ftype(mode);
        })?;
        self.dir_changed(dir)
    }

    fn remove_entry(&self, dir: u32, name: &str) -> Result<(), i64> {
        let r = self.dir_writable(dir)?;
        if r.flags() & INDEX_FL != 0 {
            match self.dx_remove(dir, r.gen_no(), name) {
                Err(e) if e == -11 => {}
                res => return res.and_then(|_| self.dir_changed(dir)),
            }
        }
        let (_, at) = self.entry_at(dir, &r, name)?.ok_or(-ENOENT)?;
        self.remove_at(dir, r, at)
    }

    /// 索引のないディレクトリ dir (inode r) の name の項目: (inode, 場所)。消すところ (unlink_node) は、これで引いて
    /// remove_at で消す (前は引くのと消すので 2 度なめていた)
    fn entry_at(&self, dir: u32, r: &Raw, name: &str) -> Result<Option<(u32, Entry)>, i64> {
        self.scan_dir(dir, r.clone(), |b, d, o, prev| {
            let (i, rl, nl) = (u32_at(d, o), u16_at(d, o + 4) as usize, d[o + 6] as usize);
            (i != 0 && nl <= rl - 8 && &d[o + 8..o + 8 + nl] == name.as_bytes()).then(|| (i, (b, o, rl, prev.map(|p| (p, u16_at(d, p + 4) as usize)))))
        })
    }

    /// entry_at で見つけた項目を消す (前のものがあれば吸収させる)
    fn remove_at(&self, dir: u32, mut r: Raw, (b, o, rl, prev): Entry) -> Result<(), i64> {
        self.modify_dir_block(dir, r.gen_no(), b, |d| match prev {
            // 前のものに吸収させる
            Some((p, prl)) => put16(d, p + 4, (prl + rl) as u16),
            None => put32(d, o, 0),
        })?;
        r.touch();
        self.write_inode(dir, &r)
    }

    /// 中身が変わったので時刻を進める
    fn dir_changed(&self, dir: u32) -> Result<(), i64> {
        let mut r = self.read_inode(dir)?;
        r.touch();
        self.write_inode(dir, &r)
    }

    fn set_dotdot(&self, dir: u32, parent: u32) -> Result<(), i64> {
        let r = self.read_inode(dir)?;
        let ents = self.dir_entries(dir)?;
        let e = ents.iter().find(|e| e.4 == "..").ok_or(-EIO)?;
        self.modify_dir_block(dir, r.gen_no(), e.0, |d| put32(d, e.1, parent))
    }

    fn add_links(&self, ino: u32, delta: i32) -> Result<Raw, i64> {
        let mut r = self.read_inode(ino)?;
        r.set_links((r.links() as i32 + delta).max(0) as u16);
        put32(&mut r.0, 12, now_secs());
        self.write_inode(ino, &r)?;
        Ok(r)
    }

    /// リンクが 0 になった inode を片付ける
    fn release(&self, ino: u32, r: &mut Raw) -> Result<(), i64> {
        // ブロックを放す: 眠って待っている読みはやりなおす
        self.wgen.set(self.wgen.get() + 1);
        let dir = r.mode() & S_IFMT == S_IFDIR;
        if !is_fast_symlink(r) && r.mode() & S_IFMT != S_IFCHR {
            self.trunc_blocks(ino, r, 0)?;
        }
        put32(&mut r.0, 20, now_secs()); // dtime
        r.set_size(0);
        self.write_inode(ino, r)?;
        self.free_inode(ino, dir)
    }

    /// 作るものの inode の中身 (create_node と fast_create): mode (種類のビットこみ)、持ち主 (pr は親)、時刻、generation
    fn new_raw(&self, mode: u32, pr: &Raw) -> Raw {
        let mut r = Raw(vec![0; self.inode_size]);
        put16(&mut r.0, 0, mode as u16);
        r.init_owner(pr);
        let t = now_secs();
        for o in [8, 12, 16] {
            put32(&mut r.0, o, t);
        }
        put32(&mut r.0, 0x64, crate::rand::u64() as u32); // i_generation
        if self.inode_size > 128 {
            put16(&mut r.0, 0x80, 32); // i_extra_isize
        }
        r
    }

    /// maybe_flush が書き出すころか。大きなロックなしの作る・消すは書き出さない (ディスクを待つのは大きなロックの中の仕事):
    /// そうなら、ふつうの道に任せる (そちらが書き出す)
    fn flush_due(&self) -> bool {
        let limit = match self.journal.borrow().as_ref() {
            Some(j) => (j.capacity() / 4).min(512),
            None => 512,
        };
        let data_limit = (cache_limit(self.bsize) / 4).max(512);
        let c = self.cache.borrow();
        c.dirty.len() + 16 >= limit || c.data.len() >= data_limit || crate::timer::ticks() >= self.last_flush.get() + 5 * crate::timer::HZ
    }

    /// 大きなロックなしの書きかえ (fast_create、fast_unlink) の中: write_op と同じく、眠って待っている読みに知らせ、
    /// この中の読みは眠らせない (noio なので、キャッシュになければ EAGAIN)。書き出しはしない
    fn fast_write<T>(&self, f: impl FnOnce() -> T) -> T {
        self.wgen.set(self.wgen.get() + 1);
        self.writing.set(self.writing.get() + 1);
        let r = f();
        self.writing.set(self.writing.get() - 1);
        self.wgen.set(self.wgen.get() + 1);
        r
    }

    /// 大きなロックなしの O_CREAT の openat (fastdir.rs。ext4 のロックを持って): ディレクトリ dir に name を、mode (umask の
    /// あと) のふつうのファイルとして作る。ふつうの道 (create_node) と同じ書きかたで、同じ答えになるときだけ。書きかえる前に
    /// みな確かめ (読むブロックがみなキャッシュにある、名前がまだない、すき間がある、inode が取れる、書き出すころでない、
    /// 呼んだ人が書けて通れる)、どれかがだめなら何も変えずに None (ふつうの道へ。しくじりのわけもそちらで)
    pub fn fast_create(self: &Rc<Self>, dir: u32, name: &str, mode: u32, c: &crate::cred::Cred) -> Option<InodeRef> {
        if self.ro || name.len() > 255 || name.is_empty() || name == "." || name == ".." || self.flush_due() {
            return None;
        }
        let ino = crate::proc::noio(|| {
            let pr = self.read_inode(dir).ok()?;
            if pr.mode() & S_IFMT != S_IFDIR || pr.links() == 0 || pr.flags() & (INDEX_FL | IMMUTABLE_FL | APPEND_FL) != 0 || !c.may(&pr.perm_meta(), crate::cred::W | crate::cred::X, false) {
                return None;
            }
            let slot = self.new_slot(dir, &pr, name).ok()??;
            let (g, bit) = self.pick_inode(self.group_of(dir))?;
            let ino = g * self.ipg + bit + 1;
            if !self.cached(self.inode_loc(ino).0) {
                return None;
            }
            // ここから書きかえる (読むブロックはみなキャッシュにあるので、しくじらない)
            self.fast_write(|| {
                self.modify_block(self.inode_bitmap(g), |d| d[(bit / 8) as usize] |= 1 << (bit % 8)).ok()?;
                let ino = self.inode_taken(g, bit, false);
                let mode = S_IFREG | (mode & 0o7777);
                let mut r = self.new_raw(mode, &pr);
                r.set_links(1);
                if self.extents {
                    ExtFs::ext_init_root(&mut r);
                }
                let done = self.write_inode(ino, &r).and_then(|_| self.add_entry_in(dir, pr, name, ino, mode, Some(Some(slot))));
                if done.is_err() {
                    println!("extfs: fast create of {} in {} broke off after taking inode {}", name, dir, ino);
                    return None;
                }
                Some(ino)
            })
        })?;
        Some(ExtInode::make(self, ino))
    }

    /// 大きなロックなしの unlinkat (fastdir.rs。ext4 のロックを持って): ディレクトリ dir のふつうのファイル name を消す。
    /// ふつうの道 (unlink_node) と同じ書きかたで、同じ答えになるときだけ (fast_create と同じく、書きかえる前にみな確かめる)。
    /// だれも使っていない (開いている fd、パスの覚え、棚、mmap のどれも ExtInode を持っていない) ものだけ: 使っているものは
    /// ふつうの道が、パスの覚えを捨ててから (forget_path) 消す。消したら true
    pub fn fast_unlink(&self, dir: u32, name: &str, c: &crate::cred::Cred) -> bool {
        if self.ro || name.is_empty() || name == "." || name == ".." || self.flush_due() {
            return false;
        }
        crate::proc::noio(|| {
            let Ok(dr) = self.read_inode(dir) else { return false };
            if dr.mode() & S_IFMT != S_IFDIR || dr.flags() & (INDEX_FL | IMMUTABLE_FL | APPEND_FL) != 0 || !c.may(&dr.perm_meta(), crate::cred::W | crate::cred::X, false) {
                return false;
            }
            let Ok(Some((child, at))) = self.entry_at(dir, &dr, name) else { return false };
            let Ok(mut r) = self.read_inode(child) else { return false };
            if r.mode() & S_IFMT != S_IFREG || r.flags() & (IMMUTABLE_FL | APPEND_FL) != 0 || r.links() == 0 {
                return false;
            }
            // sticky なディレクトリでは、持ち主 (ファイルかディレクトリ) か root だけ (sysfile の sticky_ok)
            let dm = dr.perm_meta();
            if dm.mode & crate::cred::S_ISVTX != 0 && c.euid != 0 && c.euid != dm.uid && r.perm_meta().uid != c.euid {
                return false;
            }
            if self.users.borrow().get(&child).is_some_and(|&n| n > 0) || self.orphans.borrow().contains(&child) || self.linkable.borrow().contains(&child) || self.swapfiles.borrow().contains(&child) {
                return false;
            }
            if r.links() == 1 && !self.release_cached(child, &r) {
                return false;
            }
            // ここから書きかえる (ふつうの道の unlink_node と同じ)
            self.fast_write(|| {
                if self.remove_at(dir, dr, at).is_err() {
                    println!("extfs: fast unlink of {} in {} broke off", name, dir);
                    return false;
                }
                r.set_links(r.links() - 1);
                put32(&mut r.0, 12, now_secs());
                let done = if r.links() == 0 { self.release(child, &mut r) } else { self.write_inode(child, &r) };
                if done.is_err() {
                    println!("extfs: fast unlink of {} in {} broke off after removing the name", name, dir);
                }
                true
            })
        })
    }

    /// release (消したファイルの後片づけ) が読むブロックがみなキャッシュにあるか: inode のビットマップと、中身のブロックの
    /// ビットマップ。中身は extents の根 (inode の中) にあるものだけ (索引のある木や、ext2 の間接ブロックはふつうの道へ)
    fn release_cached(&self, ino: u32, r: &Raw) -> bool {
        if !self.cached(self.inode_bitmap(self.group_of(ino))) {
            return false;
        }
        if r.blocks512(self.bsize) == 0 {
            return true;
        }
        if r.flags() & EXTENTS_FL == 0 {
            return false;
        }
        let node = &r.0[40..100];
        if u16_at(node, 0) != EXT_MAGIC || u16_at(node, 6) != 0 {
            return false;
        }
        let n = (u16_at(node, 2) as usize).min(4);
        (0..n).all(|i| {
            let ent = &node[12 + i * 12..24 + i * 12];
            let raw_len = u16_at(ent, 4) as u32;
            let len = if raw_len > EXT_INIT_MAX_LEN { raw_len - EXT_INIT_MAX_LEN } else { raw_len } as u64;
            let start = ExtFs::ext_leaf_start(ent);
            if len == 0 || start < self.first_data_block || start + len > self.blocks_count() {
                return false;
            }
            let (g0, g1) = (((start - self.first_data_block) / self.bpg as u64) as u32, ((start + len - 1 - self.first_data_block) / self.bpg as u64) as u32);
            (g0..=g1).all(|g| self.cached(self.block_bitmap(g)))
        })
    }

    fn is_empty_dir(&self, ino: u32) -> Result<bool, i64> {
        Ok(self.dir_entries(ino)?.iter().all(|e| e.2 == 0 || e.4 == "." || e.4 == ".."))
    }

    /// dir は ino の子孫か (dir 自身も含む)
    fn is_under(&self, mut dir: u32, ino: u32) -> Result<bool, i64> {
        for _ in 0..4096 {
            if dir == ino {
                return Ok(true);
            }
            if dir == ROOT_INO {
                return Ok(false);
            }
            dir = self.find(dir, "..")?;
        }
        Err(-ELOOP)
    }
}

pub struct ExtInode {
    /// Rc<ExtFs> の数を変えるのは ext4 のロックの中だけ (Drop も、ロックの中で落とす)
    fs: ManuallyDrop<Rc<ExtFs>>,
    ino: u32,
    /// meta() の覚え (ExtFs の meta_ver が同じあいだ)。開いたファイルの fstat、lseek が metas の木を引かないように
    /// (Linux は struct inode に i_size などを持っていて、fstat はそれを読むだけ)
    meta: Cell<Option<(u64, Meta)>>,
}

impl Drop for ExtInode {
    fn drop(&mut self) {
        let _e = fs_lock();
        self.release_ref();
        unsafe { ManuallyDrop::drop(&mut self.fs) };
    }
}

impl ExtInode {
    /// 数を減らし、最後のもので、消したが使われていた (orphan) なら片づける (Drop から、ext4 のロックの中で)
    fn release_ref(&mut self) {
        let last = {
            let mut u = self.fs.users.borrow_mut();
            let n = u.entry(self.ino).or_insert(1);
            *n -= 1;
            let last = *n == 0;
            if last {
                u.remove(&self.ino);
            }
            last
        };
        if last && self.fs.orphans.borrow_mut().remove(&self.ino) {
            self.fs.linkable.borrow_mut().remove(&self.ino);
            // 消したファイルの後片づけ (ブロックと inode を返す)。最後の参照がなくなったところ (unlinkat の中など) で動く
            let _p = crate::prof::span("extfs:release");
            let fs = &self.fs;
            let res = fs.orphan_del(self.ino).and_then(|_| fs.read_inode(self.ino)).and_then(|mut r| if r.links() == 0 { fs.release(self.ino, &mut r) } else { Ok(()) });
            if res.is_err() || fs.maybe_flush().is_err() {
                println!("extfs: could not free orphan inode {}", self.ino);
            }
        }
    }
}

impl ExtInode {
    /// 作るたびに数え、Drop で減らす
    fn make(fs: &Rc<ExtFs>, ino: u32) -> InodeRef {
        Rc::new(ExtInode::counted(fs, ino))
    }

    fn counted(fs: &Rc<ExtFs>, ino: u32) -> ExtInode {
        *fs.users.borrow_mut().entry(ino).or_insert(0) += 1;
        ExtInode { fs: ManuallyDrop::new(fs.clone()), ino, meta: Cell::new(None) }
    }

    fn at(&self, ino: u32) -> InodeRef {
        ExtInode::make(&self.fs, ino)
    }

    fn other(&self, i: &InodeRef) -> Result<u32, i64> {
        let o = i.as_any().downcast_ref::<ExtInode>().ok_or(-EXDEV)?;
        if o.fs.fs != self.fs.fs {
            return Err(-EXDEV);
        }
        Ok(o.ino)
    }

    fn raw(&self) -> Result<Raw, i64> {
        self.fs.read_inode(self.ino)
    }

    /// スワップに使っている間は書けない (ETXTBSY)
    fn not_swapfile(&self) -> Result<(), i64> {
        const ETXTBSY: i64 = 26;
        if self.fs.swapfiles.borrow().contains(&self.ino) {
            return Err(-ETXTBSY);
        }
        Ok(())
    }

    /// スワップファイルにする (swapon): ファイルのページがディスク (root の区画) のどこにあるかを
    /// (ファイルのページ, セクタ, ページ数) の並びで返す。穴や、まだ書いていないところ、
    /// 1 ページの中でブロックがとぎれているところがあれば EINVAL。
    /// 書きかけの中身を先にディスクへ出し、覚えているブロックは捨てる (このあとは直接読み書きされる)
    pub fn swap_map(&self) -> Result<Vec<(u64, u64, u64)>, i64> {
        let _e = fs_lock();
        let fs = &self.fs;
        fs.check_rw()?;
        let mut r = self.raw()?;
        if r.mode() & S_IFMT != S_IFREG {
            return Err(crate::why::inval());
        }
        fs.flush()?;
        let per = PGSIZE / fs.bsize;
        let spb = (fs.bsize / crate::block::SECTOR) as u64;
        let pages = r.size() / PGSIZE as u64;
        let mut out: Vec<(u64, u64, u64)> = Vec::new();
        let mut blocks = Vec::new();
        for pg in 0..pages {
            let first = fs.map_any(self.ino, &mut r, pg * per as u64)?;
            if first == 0 {
                return Err(crate::why::inval());
            }
            for k in 1..per as u64 {
                if fs.map_any(self.ino, &mut r, pg * per as u64 + k)? != first + k {
                    return Err(crate::why::inval());
                }
            }
            blocks.extend(first..first + per as u64);
            let sector = first * spb;
            match out.last_mut() {
                Some((p, s, n)) if *p + *n == pg && *s + *n * (PGSIZE / crate::block::SECTOR) as u64 == sector => *n += 1,
                _ => out.push((pg, sector, 1)),
            }
        }
        {
            let mut c = fs.cache.borrow_mut();
            for b in blocks {
                if c.dirty.contains(&b) || c.data.contains(&b) {
                    continue;
                }
                c.forget(b);
            }
        }
        Ok(out)
    }

    /// スワップに使っている印 (swapon で true、swapoff で false)
    pub fn set_swapfile(&self, on: bool) {
        let _e = fs_lock();
        // スワップファイルには書けない (ETXTBSY): 大きなロックなしの pwrite の許しも捨てる
        crate::vm::write_permit_drop(self.id());
        let mut s = self.fs.swapfiles.borrow_mut();
        if on {
            s.insert(self.ino);
        } else {
            s.remove(&self.ino);
        }
    }

    fn dir_only(&self) -> Result<(), i64> {
        // meta の覚えから (inode を読みなおさない)。読めなければ raw で EIO などを返す
        let m = self.meta();
        let mode = if m.mode != 0 { m.mode } else { self.raw()?.mode() };
        if mode & S_IFMT != S_IFDIR {
            return Err(-ENOTDIR);
        }
        Ok(())
    }

    fn update(&self, f: impl FnOnce(&mut Raw)) -> Result<(), i64> {
        self.fs.check_rw()?;
        let mut r = self.raw()?;
        f(&mut r);
        self.fs.write_inode(self.ino, &r)
    }

    /// 書きかえる操作: 終わったらディスクへ書き出す
    fn write_op<T>(&self, f: impl FnOnce() -> Result<T, i64>) -> Result<T, i64> {
        self.fs.check_rw()?;
        // このファイルを変えるあいだは、大きなロックなしの pwrite (vm::write_fast) をさせない (許しは書いたあとに
        // write_at がまた出す)。ロックなしで書いているものは、ここで終わるのを待つ
        crate::vm::write_permit_drop(self.id());
        let fs = &self.fs;
        // 眠ってディスクを待っている読みに、書いたことを知らせる (起きたらやりなおす)。この中の読みは眠らない
        fs.wgen.set(fs.wgen.get() + 1);
        fs.writing.set(fs.writing.get() + 1);
        let r = f();
        let fl = fs.maybe_flush();
        fs.writing.set(fs.writing.get() - 1);
        fs.wgen.set(fs.wgen.get() + 1);
        r.and_then(|v| fl.map(|_| v))
    }

    fn create_node(&self, name: &str, mode: u32, node: NewNode) -> Result<InodeRef, i64> {
        self.dir_only()?;
        let fs = &self.fs;
        // あれば EEXIST。なければ名前を入れるすき間も (なめるのは 1 度)
        let pr = fs.read_inode(self.ino)?;
        let slot = fs.new_slot(self.ino, &pr, name)?;
        // 消したディレクトリには作れない (inode をとる前に)
        if pr.links() == 0 {
            return Err(-ENOENT);
        }
        let mode = node.type_bits() | (mode & 0o7777);
        let is_dir = matches!(node, NewNode::Dir);
        let ino = fs.alloc_inode(fs.group_of(self.ino), is_dir)?;
        let mut r = fs.new_raw(mode, &pr);
        r.set_links(if is_dir { 2 } else { 1 });
        let long_link = matches!(&node, NewNode::Symlink(t) if t.len() >= 60);
        match &node {
            NewNode::Dev(ma, mi) | NewNode::Blk(ma, mi) => r.set_block(0, (ma << 8) | mi),
            NewNode::Symlink(t) if t.len() < 60 => {
                r.0[40..40 + t.len()].copy_from_slice(t.as_bytes());
                r.set_size(t.len() as u64);
            }
            _ => {}
        }
        if fs.extents && (matches!(node, NewNode::File | NewNode::Dir) || long_link) {
            ExtFs::ext_init_root(&mut r);
        }
        fs.write_inode(ino, &r)?;
        match &node {
            NewNode::Dir => {
                let blk = fs.map(ino, &mut r, 0, true)?;
                let me = self.ino;
                let end = if fs.csum { fs.bsize - DIR_TAIL } else { fs.bsize };
                fs.modify_dir_block(ino, r.gen_no(), blk, |d| {
                    fs.init_dir_block(d);
                    put32(d, 0, ino);
                    put16(d, 4, 12);
                    d[6] = 1;
                    d[7] = 2;
                    d[8] = b'.';
                    put32(d, 12, me);
                    put16(d, 16, (end - 12) as u16);
                    d[18] = 2;
                    d[19] = 2;
                    d[20..22].copy_from_slice(b"..");
                })?;
                r.set_size(fs.bsize as u64);
                fs.write_inode(ino, &r)?;
                fs.add_links(self.ino, 1)?;
            }
            NewNode::Symlink(t) if long_link => {
                fs.write_data(ino, &mut r, 0, t.as_bytes(), None)?;
                fs.write_inode(ino, &r)?;
            }
            _ => {}
        }
        // ディレクトリを作ったなら、親のリンクの数を書きかえている (読みなおす)
        let pr = if is_dir { fs.dir_writable(self.ino)? } else { pr };
        fs.add_entry_in(self.ino, pr, name, ino, mode, Some(slot))?;
        Ok(self.at(ino))
    }

    /// O_TMPFILE: 名前のないふつうのファイル。はじめから orphan (消したが開いているもの) にしておくので、
    /// 最後に閉じたとき (や、いきなり電源が切れたあとのマウント) に片付く
    fn tmpfile_node(&self, mode: u32, linkable: bool) -> Result<InodeRef, i64> {
        self.dir_only()?;
        let fs = &self.fs;
        let pr = fs.read_inode(self.ino)?;
        let ino = fs.alloc_inode(fs.group_of(self.ino), false)?;
        let mut r = Raw(vec![0; fs.inode_size]);
        put16(&mut r.0, 0, (S_IFREG | (mode & 0o7777)) as u16);
        r.init_owner(&pr);
        let t = now_secs();
        for o in [8, 12, 16] {
            put32(&mut r.0, o, t);
        }
        put32(&mut r.0, 0x64, crate::rand::u64() as u32); // i_generation
        if fs.inode_size > 128 {
            put16(&mut r.0, 0x80, 32); // i_extra_isize
        }
        r.set_links(0);
        if fs.extents {
            ExtFs::ext_init_root(&mut r);
        }
        fs.orphan_add(ino, &mut r);
        fs.write_inode(ino, &r)?;
        fs.orphans.borrow_mut().insert(ino);
        if linkable {
            fs.linkable.borrow_mut().insert(ino);
        }
        Ok(self.at(ino))
    }

    fn unlink_node(&self, name: &str, rmdir: bool) -> Result<(), i64> {
        self.dir_only()?;
        if name == "." || name == ".." {
            return Err(crate::why::inval());
        }
        let fs = &self.fs;
        // 索引のないディレクトリは、引いた場所をそのまま消す (なめるのは 1 度)
        let dr = fs.dir_writable(self.ino)?;
        let (child, at) = match dr.flags() & INDEX_FL {
            0 => fs.entry_at(self.ino, &dr, name)?.map(|(i, e)| (i, Some(e))).ok_or(-ENOENT)?,
            _ => (fs.find(self.ino, name)?, None),
        };
        let mut r = fs.read_inode(child)?;
        let is_dir = r.mode() & S_IFMT == S_IFDIR;
        match (is_dir, rmdir) {
            (true, false) => return Err(-EISDIR),
            (false, true) => return Err(-ENOTDIR),
            (true, true) if !fs.is_empty_dir(child)? => return Err(-ENOTEMPTY),
            _ => {}
        }
        match at {
            Some(e) => fs.remove_at(self.ino, dr, e)?,
            None => fs.remove_entry(self.ino, name)?,
        }
        if is_dir {
            fs.add_links(self.ino, -1)?;
            r.set_links(0);
        } else {
            r.set_links(r.links().saturating_sub(1));
        }
        put32(&mut r.0, 12, now_secs());
        if r.links() == 0 {
            // 閉じたファイルの棚 (file::Shelf) に残っているものは、もう開きなおせない: 捨てる (ほかに開いている人が
            // いなければ、下で片づく)
            crate::file::shelf_forget((fs.fs, child as u64));
            if fs.users.borrow().get(&child).is_some_and(|&n| n > 0) {
                // まだ開かれている (実行中のプログラムなど): 最後に閉じられたときに片付ける
                fs.orphans.borrow_mut().insert(child);
                fs.orphan_add(child, &mut r);
                fs.write_inode(child, &r)
            } else {
                fs.release(child, &mut r)
            }
        } else {
            fs.write_inode(child, &r)
        }
    }

    fn rename_node(&self, old: &str, newdir: &InodeRef, new: &str) -> Result<(), i64> {
        self.dir_only()?;
        let nd = self.other(newdir)?;
        let fs = &self.fs;
        let child = fs.find(self.ino, old)?;
        let r = fs.read_inode(child)?;
        let is_dir = r.mode() & S_IFMT == S_IFDIR;
        if is_dir && fs.is_under(nd, child)? {
            return Err(crate::why::inval());
        }
        if let Ok(existing) = fs.find(nd, new) {
            if existing == child {
                return Ok(());
            }
            ExtInode::counted(fs, nd).unlink_node(new, is_dir)?;
        }
        fs.add_entry(nd, new, child, r.mode())?;
        fs.remove_entry(self.ino, old)?;
        if is_dir && nd != self.ino {
            fs.set_dotdot(child, nd)?;
            fs.add_links(self.ino, -1)?;
            fs.add_links(nd, 1)?;
        }
        Ok(())
    }
}

impl ExtInode {
    /// old (このディレクトリ) と new (nd) の項目が指す inode を入れかえる。ディレクトリがほかの親へ移るなら、
    /// その .. と親のリンクの数も
    fn exchange_node(&self, old: &str, newdir: &InodeRef, new: &str) -> Result<(), i64> {
        self.dir_only()?;
        let nd = self.other(newdir)?;
        let fs = &self.fs;
        let a = fs.find(self.ino, old)?;
        let b = fs.find(nd, new)?;
        if a == b {
            return Ok(());
        }
        let (am, bm) = (fs.read_inode(a)?.mode(), fs.read_inode(b)?.mode());
        let (adir, bdir) = (am & S_IFMT == S_IFDIR, bm & S_IFMT == S_IFDIR);
        if (adir && fs.is_under(nd, a)?) || (bdir && fs.is_under(self.ino, b)?) {
            return Err(crate::why::inval());
        }
        fs.set_entry(self.ino, old, b, bm)?;
        fs.set_entry(nd, new, a, am)?;
        if self.ino != nd {
            if adir {
                fs.set_dotdot(a, nd)?;
            }
            if bdir {
                fs.set_dotdot(b, self.ino)?;
            }
            if adir != bdir {
                let (gain, lose) = if adir { (nd, self.ino) } else { (self.ino, nd) };
                fs.add_links(gain, 1)?;
                fs.add_links(lose, -1)?;
            }
        }
        Ok(())
    }
}

impl Inode for ExtInode {
    fn id(&self) -> (usize, u64) {
        (self.fs.fs, self.ino as u64)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn meta(&self) -> Meta {
        let _e = fs_lock();
        let ver = self.fs.meta_ver.get();
        if let Some((v, m)) = self.meta.get()
            && v == ver
        {
            return m;
        }
        if let Some(m) = self.fs.metas.borrow().get(&self.ino) {
            self.meta.set(Some((ver, *m)));
            return *m;
        }
        // 覚え (metas) になかったものだけ (あれば BTreeMap を 1 度引くだけ)
        let _p = crate::prof::span("extfs:meta");
        let Ok(r) = self.raw() else {
            return Meta { ino: self.ino as u64, mode: 0, nlink: 0, uid: 0, gid: 0, size: 0, rdev: 0, blocks: 0, mtime: 0, ctime: 0 };
        };
        let b = &r.0;
        let uid = u16_at(b, 2) as u32 | (u16_at(b, 120) as u32) << 16;
        let gid = u16_at(b, 24) as u32 | (u16_at(b, 122) as u32) << 16;
        let rdev = if matches!(r.mode() & S_IFMT, S_IFCHR | S_IFBLK) {
            let old = r.block(0);
            if old != 0 { old as u64 } else { let new = r.block(1); (((new >> 8) & 0xfff) << 8 | (new & 0xff)) as u64 }
        } else {
            0
        };
        let m = Meta {
            ino: self.ino as u64,
            mode: r.mode(),
            nlink: r.links() as u32,
            uid,
            gid,
            size: r.size(),
            rdev,
            blocks: r.blocks512(self.fs.bsize),
            mtime: u32_at(b, 16) as u64 * 1_000_000_000,
            ctime: u32_at(b, 12) as u64 * 1_000_000_000,
        };
        let mut c = self.fs.metas.borrow_mut();
        if c.len() >= METAS_MAX {
            c.clear();
        }
        c.insert(self.ino, m);
        self.meta.set(Some((ver, m)));
        m
    }

    fn read_at(&self, off: usize, buf: &mut [u8]) -> Result<usize, i64> {
        let _e = fs_lock();
        let _p = crate::prof::span("extfs:read");
        let r = self.raw()?;
        match r.mode() & S_IFMT {
            S_IFREG => self.fs.read_data_raw(self.ino, r, off, buf),
            S_IFDIR => Err(-EISDIR),
            _ => Err(crate::why::inval()),
        }
    }

    fn fast_stat(&self) -> bool {
        true
    }

    fn path_cacheable(&self) -> bool {
        true
    }

    fn page_cacheable(&self) -> bool {
        true
    }

    /// ファイルの foff のページそのもの: ブロックのキャッシュのページ (ブロックが 1 ページのとき。参照をひとつ足して)。
    /// 穴 (と未初期化のブロック) は None。alloc なら、ブロックをとって (写しの穴のページを引きとって: adopt_hole)
    /// 書き出す印をつけ、更新時刻をいまに (写しに書くとき: Linux の page_mkwrite)。終わりの先 (同じページの中) は 0 に
    fn page_frame(&self, foff: usize, alloc: bool) -> Result<Option<*mut u8>, i64> {
        let _e = fs_lock();
        if self.fs.bsize != PGSIZE || foff % PGSIZE != 0 {
            return Err(-ENODEV);
        }
        let lb = (foff / PGSIZE) as u64;
        if !alloc {
            let mut r = self.raw()?;
            if r.mode() & S_IFMT != S_IFREG || r.flags() & INLINE_DATA_FL != 0 {
                return Err(-ENODEV);
            }
            let b = self.fs.map(self.ino, &mut r, lb, false)?;
            if b == 0 {
                return Ok(None);
            }
            let p = self.fs.frame(b)?;
            let size = r.size() as usize;
            if foff < size && size - foff < PGSIZE {
                unsafe { core::ptr::write_bytes(p.add(size - foff), 0, PGSIZE - (size - foff)) };
            }
            return Ok(Some(p));
        }
        self.not_swapfile()?;
        self.write_op(|| {
            let mut r = self.raw()?;
            if r.mode() & S_IFMT != S_IFREG || r.flags() & INLINE_DATA_FL != 0 {
                return Err(-ENODEV);
            }
            let fresh = self.fs.map(self.ino, &mut r, lb, false)? == 0;
            let b = self.fs.map(self.ino, &mut r, lb, true)?;
            let p = self.fs.modify_data_block_adopt(b, fresh, Some((self.ino, lb)), |d| d.as_mut_ptr())?;
            kalloc::get(p);
            r.touch();
            self.fs.write_inode(self.ino, &r)?;
            Ok(Some(p))
        })
    }

    /// 写しで foff のページに書いた: そのブロックに書き出す印 (ディスクへは flush で直接)。touch なら更新時刻もいまに。
    /// flush の中 (vm::sync_shared) からも呼ばれるので、ここでは flush しない
    fn page_written(&self, foff: usize, touch: bool) -> Result<(), i64> {
        let _e = fs_lock();
        self.fs.check_rw()?;
        let mut r = self.raw()?;
        if r.mode() & S_IFMT != S_IFREG {
            return Err(-ENODEV);
        }
        let b = self.fs.map(self.ino, &mut r, (foff / self.fs.bsize) as u64, false)?;
        if b != 0 {
            let mut c = self.fs.cache.borrow_mut();
            if c.map.contains_key(&b) {
                c.dirty.remove(&b);
                c.data.insert(b);
            }
        }
        if touch {
            r.touch();
            self.fs.write_inode(self.ino, &r)?;
        }
        Ok(())
    }

    /// posix_fadvise の DONTNEED: off から len バイト (0 なら終わりまで) のブロックを、まだ書いていなければキャッシュから
    /// 外す (そのあとの RWF_NOWAIT は EAGAIN: Linux と同じ)
    fn uncache(&self, off: u64, len: u64) {
        let _e = fs_lock();
        let Ok(mut r) = self.raw() else { return };
        if r.mode() & S_IFMT != S_IFREG || r.flags() & INLINE_DATA_FL != 0 {
            return;
        }
        let bs = self.fs.bsize as u64;
        let size = r.size();
        let end = if len == 0 { u64::MAX } else { off.saturating_add(len) };
        // まるごと入るブロックだけ (かかっているだけのものは外さない: Linux と同じ)。終わりのブロックは、ファイルの終わりまで
        let (mut fb, last) = (off.div_ceil(bs), (end.min(size.next_multiple_of(bs)) / bs).saturating_sub(1));
        if size == 0 || last < fb || fb * bs >= size {
            return;
        }
        while fb <= last {
            let Ok(b) = self.fs.map(self.ino, &mut r, fb, false) else { break };
            if b != 0 {
                let mut c = self.fs.cache.borrow_mut();
                // 写しているもの (参照が表だけでない) は外さない
                if !c.data.contains(&b) && !c.dirty.contains(&b) && c.map.get(&b).is_some_and(|&p| kalloc::refs(p) <= 1) {
                    c.forget(b);
                }
            }
            fb += 1;
        }
    }

    /// aios がえらんだちがい (seek-unwritten): fallocate でとってまだ書いていないところ (未初期化の extent) は、いつも
    /// 穴 (ext_next_data が飛ばす)。Linux の ext4 は、そこのページがキャッシュにあれば (読んだあと) データとするので、
    /// 答えがキャッシュのぐあいで変わる。doc/aios.md の「aios がえらんだちがい」
    fn seek_data(&self, off: u64, hole: bool) -> Option<Result<u64, i64>> {
        let _e = fs_lock();
        const ENXIO: i64 = 6;
        let r = match self.raw() {
            Ok(r) => r,
            Err(e) => return Some(Err(e)),
        };
        // extent のないもの (ext2/3 のブロックの表) は、全部がデータとして
        if r.flags() & EXTENTS_FL == 0 || r.mode() & S_IFMT != S_IFREG {
            return None;
        }
        let (size, bs) = (r.size(), self.fs.bsize as u64);
        if off >= size {
            return Some(Err(-ENXIO));
        }
        let root = &r.0[40..100];
        let res = (|| -> Result<u64, i64> {
            if !hole {
                // データ: その範囲の初め (off がデータの中なら off)。ファイルの終わりより先なら ENXIO
                return match self.fs.ext_next_data(root, off / bs, 0)? {
                    Some((b, _)) if (b * bs).max(off) < size => Ok((b * bs).max(off)),
                    _ => Err(-ENXIO),
                };
            }
            // 穴: off が穴の中なら off、データの中なら続くデータの終わり (終わりはファイルの大きさまで)
            let mut cur = off / bs;
            while let Some((b, n)) = self.fs.ext_next_data(root, cur, 0)?
                && b == cur
            {
                cur = b + n;
            }
            Ok((cur * bs).max(off).min(size))
        })();
        Some(res)
    }

    fn write_at(&self, off: usize, buf: &[u8]) -> Result<usize, i64> {
        let _e = fs_lock();
        let _p = crate::prof::span("extfs:write");
        // 中身が変わる。ブロックが 1 ページなら、ページキャッシュ (vm.rs) のページはブロックのキャッシュのページそのもので、
        // その場で書きかわる: そのまま版をあたらしくし、書いたページも入れる (Linux と同じく、書いたものはページキャッシュに
        // ある)。でなければ (読んだ写し)、書く前と後に捨てる (書いているあいだに読まれたものも)
        let id = self.id();
        let in_place = self.fs.bsize == PGSIZE;
        if in_place {
            crate::vm::file_writing(id);
        } else {
            crate::vm::file_changed(id);
        }
        let mut pages = Vec::new();
        let mut size0 = 0;
        // 書いたあとの inode が、この秒のうちの書きかえで変わらないもの (時刻は秒まで、setuid / setgid なし) なら、その秒
        let mut quiet = None;
        let r = self.not_swapfile().and_then(|_| self.write_op(|| {
            let mut r = self.raw()?;
            match r.mode() & S_IFMT {
                S_IFREG => {}
                S_IFDIR => return Err(-EISDIR),
                _ => return Err(crate::why::inval()),
            }
            size0 = r.size() as usize;
            let before = r.clone();
            let res = self.fs.write_data(self.ino, &mut r, off, buf, in_place.then_some(&mut pages));
            // inode が 1 バイトも変わらなければ (同じ秒の中で、大きさもブロックの場所も変わらない書きかえ) 書かない:
            // 書くと inode のブロックをジャーナルに入れ、meta の覚え (と stat の覚え、statc.rs) を捨てて読みなおすことになる。
            // Linux も時刻が変わったときだけ inode を書く (file_update_time、inode_needs_update_time)
            if r.0 != before.0 {
                self.fs.write_inode(self.ino, &r)?;
            }
            let (ctime, mtime) = (u32_at(&r.0, 12), u32_at(&r.0, 16));
            if res.is_ok() && r.mode() & 0o6000 == 0 && r.flags() & (IMMUTABLE_FL | APPEND_FL | INLINE_DATA_FL) == 0 && ctime == mtime {
                quiet = Some(mtime as u64);
            }
            res
        }));
        if in_place {
            // まだブロックのキャッシュのページそのものか (書いたあとの flush のあいだに捨てられていないか)
            let pages: Vec<(usize, *mut u8)> = {
                let c = self.fs.cache.borrow();
                pages
                    .into_iter()
                    .filter_map(|(lb, b, p)| {
                        if c.map.get(&b) == Some(&p) {
                            Some((lb as usize, p))
                        } else {
                            kalloc::put(p);
                            None
                        }
                    })
                    .collect()
            };
            // 書きかわったところ: 終わりの先から書いたなら、もとの終わりから (zero_gap が 0 にする)
            let (first, end) = (off.min(size0) / PGSIZE, (off + buf.len()).div_ceil(PGSIZE));
            crate::vm::file_written(id, crate::vm::meta_ver(&self.meta()), first, end.saturating_sub(1), pages);
            // この秒のうちに、いまの大きさの中を書きかえるだけなら、大きなロックなしで書いてよい (vm::write_fast)。
            // IN_MODIFY を見張っているもの (ふつうの道ができごとを出す) や RWF_DONTCACHE があるときはしない
            if let (Ok(_), Some(sec)) = (&r, quiet)
                && !crate::proc::dontcache_on()
                && !crate::inotify::modify_watched()
            {
                crate::vm::write_permit(id, sec);
            }
        } else {
            crate::vm::file_changed(id);
        }
        r
    }

    fn fallocate(&self, mode: u32, off: u64, len: u64, modified: &dyn Fn() -> Result<(), i64>) -> Result<(), i64> {
        let _e = fs_lock();
        let _p = crate::prof::span("extfs:fallocate");
        const KEEP_SIZE: u32 = 0x01;
        const PUNCH_HOLE: u32 = 0x02;
        const ZERO_RANGE: u32 = 0x10;
        // Linux の ext4_fallocate の順: できない mode (COLLAPSE_RANGE と INSERT_RANGE はまだ) は EOPNOTSUPP、
        // file_modified (setuid と setgid を落とし、時刻をいまに)、それから mode ごと。場所をとるとき (mode 0) と
        // ZERO_RANGE は、Linux と同じく未初期化のブロックをとる (0 として読み、SEEK_DATA では穴、書くと初期化ずみ)
        if mode & !(KEEP_SIZE | PUNCH_HOLE | ZERO_RANGE) != 0 {
            return Err(-EOPNOTSUPP);
        }
        self.not_swapfile()?;
        modified()?;
        crate::vm::file_changed(self.id());
        let res = self.write_op(|| {
            let mut r = self.raw()?;
            if r.mode() & S_IFMT != S_IFREG {
                return Err(-EOPNOTSUPP);
            }
            r.touch();
            let (size, end, bs) = (r.size(), off + len, self.fs.bsize as u64);
            let extents = r.flags() & EXTENTS_FL != 0;
            // 穴をあける (0 にする) ところのまるごとのページは、写しているところから外す (Linux の truncate_pagecache_range)
            let drop_pages = |from: u64, to: u64| {
                if from < to {
                    let (first, last) = ((from as usize).div_ceil(PGSIZE), (to as usize) / PGSIZE);
                    if first < last {
                        crate::vm::drop_file_pages(self.id(), first, last - 1);
                    }
                }
            };
            let res = match mode & !KEEP_SIZE {
                // extent のないファイル (ブロックの表) は、どれもできない
                _ if !extents => Err(-EOPNOTSUPP),
                PUNCH_HOLE => {
                    // 大きさより先は見ない。終わりは大きさのあるページの終わりまで (ext4_punch_hole)
                    if off < size {
                        let to = end.min(size.next_multiple_of(PGSIZE as u64));
                        drop_pages(off, to);
                        self.fs.punch(self.ino, &mut r, off, to)
                    } else {
                        Ok(())
                    }
                }
                // 0 にして (まるごとのブロックは外し、はしは中を 0 に)、穴になったところに未初期化のブロック
                ZERO_RANGE => {
                    drop_pages(off, end);
                    self.fs.punch(self.ino, &mut r, off, end).and_then(|_| self.fs.ext_prealloc(self.ino, &mut r, off / bs, end.div_ceil(bs)))
                }
                _ => self.fs.ext_prealloc(self.ino, &mut r, off / bs, end.div_ceil(bs)),
            };
            if res.is_ok() && mode & (KEEP_SIZE | PUNCH_HOLE) == 0 && end > size {
                // 伸ばす: 終わりのブロックの、終わりの先を 0 に (写しで書いたものがあれば消える)
                self.fs.zero_gap(self.ino, &mut r, size as usize, end as usize);
                r.set_size(end);
            }
            self.fs.write_inode(self.ino, &r)?;
            res
        });
        crate::vm::file_changed(self.id());
        res
    }

    fn truncate(&self, len: usize) -> Result<(), i64> {
        let _e = fs_lock();
        let _p = crate::prof::span("extfs:truncate");
        // 中身が変わる: ページキャッシュ (vm.rs) にある分を、前と後に捨てる
        crate::vm::file_changed(self.id());
        let r = (|| -> Result<(), i64> {
        self.not_swapfile()?;
        self.write_op(|| {
            let mut r = self.raw()?;
            if r.mode() & S_IFMT != S_IFREG {
                return Err(if r.mode() & S_IFMT == S_IFDIR { -EISDIR } else { -EINVAL });
            }
            let bs = self.fs.bsize;
            let size = r.size() as usize;
            if len < size {
                // 写しているところから、なくなるページを外す (vm.rs。残るページの終わりの先は下で 0 に: 写しにも映る)
                crate::vm::drop_file_pages(self.id(), len.div_ceil(PGSIZE), usize::MAX);
                self.fs.trunc_blocks(self.ino, &mut r, len.div_ceil(bs) as u64)?;
                // 残ったブロックの末尾を 0 にしておく (あとで伸ばしたとき用)
                if len % bs != 0 {
                    let b = self.fs.map(self.ino, &mut r, (len / bs) as u64, false)?;
                    if b != 0 {
                        self.fs.modify_data_block(b, |d| d[len % bs..].fill(0))?;
                    }
                }
            } else if len > size {
                // 伸ばす: 終わりのブロックの、終わりの先を 0 に (写しで書いたものがあれば消える)
                self.fs.zero_gap(self.ino, &mut r, size, len);
            }
            r.set_size(len as u64);
            r.touch();
            self.fs.write_inode(self.ino, &r)
        })
            })();
        crate::vm::file_changed(self.id());
        r
    }


    fn readlink(&self) -> Result<String, i64> {
        let _e = fs_lock();
        let _p = crate::prof::span("extfs:readlink");
        let r = self.raw()?;
        if r.mode() & S_IFMT != S_IFLNK {
            return Err(crate::why::inval());
        }
        let size = r.size() as usize;
        if is_fast_symlink(&r) {
            return Ok(String::from_utf8_lossy(&r.0[40..40 + size]).to_string());
        }
        let mut buf = vec![0u8; size];
        let n = self.fs.read_data(self.ino, 0, &mut buf)?;
        buf.truncate(n);
        Ok(String::from_utf8_lossy(&buf).to_string())
    }

    fn lookup(&self, name: &str) -> Result<InodeRef, i64> {
        let _e = fs_lock();
        let _p = crate::prof::span("extfs:lookup");
        self.dir_only()?;
        Ok(self.at(self.fs.find(self.ino, name)?))
    }

    fn readdir(&self) -> Result<Vec<DirEntry>, i64> {
        let _e = fs_lock();
        let _p = crate::prof::span("extfs:readdir");
        self.dir_only()?;
        let generation = self.fs.names_gen.get();
        let ents = self.fs.dir_entries(self.ino)?;
        // 一覧を読んだら、あとで名前で引かれる (find、ls -l): 名前のキャッシュにも入れておく
        if self.fs.names_gen.get() == generation {
            let mut n = self.fs.names.borrow_mut();
            if ents.len() <= NAMES_MAX / 2 {
                if n.len + ents.len() > NAMES_MAX {
                    n.dirs.clear();
                    n.len = 0;
                }
                for e in ents.iter().filter(|e| e.2 != 0) {
                    n.insert(self.ino, e.4.clone(), e.2);
                }
            }
        }
        let mut out = vec![];
        for e in ents {
            if e.2 == 0 || e.4 == "." || e.4 == ".." {
                continue;
            }
            out.push(DirEntry { mode: self.fs.mode_of_ft(e.2, e.5), name: e.4, ino: e.2 as u64 });
        }
        Ok(out)
    }

    fn readdir_cookies(&self) -> Option<Result<Vec<(u64, DirEntry)>, i64>> {
        let _e = fs_lock();
        let _p = crate::prof::span("extfs:readdir");
        if let Err(e) = self.dir_only() {
            return Some(Err(e));
        }
        match self.fs.readdir_hashed(self.ino) {
            Ok(Some(v)) => Some(Ok(v.into_iter().map(|(p, ino, name, ft)| (p, DirEntry { mode: self.fs.mode_of_ft(ino, ft), name, ino: ino as u64 })).collect())),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        }
    }

    fn create(&self, name: &str, mode: u32, node: NewNode) -> Result<InodeRef, i64> {
        let _e = fs_lock();
        let _p = crate::prof::span("extfs:create");
        crate::vfs::name_changed(self.id(), name);
        self.write_op(|| self.create_node(name, mode, node))
    }

    fn link(&self, name: &str, target: &InodeRef) -> Result<(), i64> {
        let _e = fs_lock();
        let _p = crate::prof::span("extfs:link");
        crate::vfs::name_changed(self.id(), name);
        self.write_op(|| {
            self.dir_only()?;
            let t = self.other(target)?;
            let r = self.fs.read_inode(t)?;
            if r.mode() & S_IFMT == S_IFDIR {
                return Err(-EPERM);
            }
            // リンクが 0 (消したが開いているもの) は、O_TMPFILE の linkable なものだけ (Linux の vfs_link)
            let tmp = r.links() == 0;
            if tmp && !self.fs.linkable.borrow().contains(&t) {
                return Err(-ENOENT);
            }
            if self.fs.find(self.ino, name).is_ok() {
                return Err(-EEXIST);
            }
            self.fs.add_entry(self.ino, name, t, r.mode())?;
            self.fs.add_links(t, 1)?;
            // 名前がついた: もう orphan ではない (リストから外し、i_dtime のつなぎも消す)
            if tmp {
                self.fs.linkable.borrow_mut().remove(&t);
                if self.fs.orphans.borrow_mut().remove(&t) {
                    self.fs.orphan_del(t)?;
                    let mut r = self.fs.read_inode(t)?;
                    put32(&mut r.0, 20, 0);
                    self.fs.write_inode(t, &r)?;
                }
            }
            Ok(())
        })
    }

    fn tmpfile(&self, mode: u32, linkable: bool) -> Result<InodeRef, i64> {
        let _e = fs_lock();
        let _p = crate::prof::span("extfs:create");
        self.write_op(|| self.tmpfile_node(mode, linkable))
    }

    fn unlink(&self, name: &str, rmdir: bool) -> Result<(), i64> {
        let _e = fs_lock();
        let _p = crate::prof::span("extfs:unlink");
        crate::vfs::name_changed(self.id(), name);
        self.write_op(|| self.unlink_node(name, rmdir))
    }

    fn rename(&self, old: &str, newdir: &InodeRef, new: &str) -> Result<(), i64> {
        let _e = fs_lock();
        let _p = crate::prof::span("extfs:rename");
        crate::vfs::name_changed(self.id(), old);
        crate::vfs::name_changed(newdir.id(), new);
        self.write_op(|| self.rename_node(old, newdir, new))
    }

    fn exchange(&self, old: &str, newdir: &InodeRef, new: &str) -> Result<(), i64> {
        let _e = fs_lock();
        let _p = crate::prof::span("extfs:rename");
        crate::vfs::name_changed(self.id(), old);
        crate::vfs::name_changed(newdir.id(), new);
        self.write_op(|| self.exchange_node(old, newdir, new))
    }

    fn set_mode(&self, mode: u32) -> Result<(), i64> {
        let _e = fs_lock();
        let _p = crate::prof::span("extfs:setattr");
        if self.meta().is_dir() {
            crate::vfs::dir_changed(self.id());
        }
        self.write_op(|| {
            self.update(|r| {
                let m = (r.mode() & S_IFMT) | (mode & 0o7777);
                put16(&mut r.0, 0, m as u16);
                put32(&mut r.0, 12, now_secs());
            })
        })
    }

    fn set_owner(&self, uid: Option<u32>, gid: Option<u32>) -> Result<(), i64> {
        let _e = fs_lock();
        let _p = crate::prof::span("extfs:setattr");
        if self.meta().is_dir() {
            crate::vfs::dir_changed(self.id());
        }
        self.write_op(|| {
            self.update(|r| {
                if let Some(u) = uid {
                    put16(&mut r.0, 2, u as u16);
                    put16(&mut r.0, 120, (u >> 16) as u16);
                }
                if let Some(g) = gid {
                    put16(&mut r.0, 24, g as u16);
                    put16(&mut r.0, 122, (g >> 16) as u16);
                }
                put32(&mut r.0, 12, now_secs());
            })
        })
    }

    fn set_mtime(&self, ns: u64) -> Result<(), i64> {
        let _e = fs_lock();
        let _p = crate::prof::span("extfs:setattr");
        self.write_op(|| self.update(|r| put32(&mut r.0, 16, (ns / 1_000_000_000) as u32)))
    }

    fn sync(&self) -> Result<(), i64> {
        let _e = fs_lock();
        self.fs.flush()
    }

    fn max_bytes(&self) -> u64 {
        self.fs.max_bytes
    }

    fn fs_stats(&self) -> Option<String> {
        let _e = fs_lock();
        Some(self.fs.stats_json())
    }

    fn statfs(&self) -> [u8; 120] {
        let _e = fs_lock();
        let sb = self.fs.sb.borrow();
        let free = u32_at(&sb, 12) as u64;
        statfs_bytes(MAGIC as u64, self.fs.bsize as u64, self.fs.blocks_count(), free, u32_at(&sb, 0) as u64, u32_at(&sb, 16) as u64)
    }
}

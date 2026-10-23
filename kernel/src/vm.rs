// ユーザー空間 (TTBR0, 4KiB granule, 39bit VA, L1-L3) とデマンドページング
//
// アドレス空間は「領域」(Vma: 範囲、PROT_*、無名かファイルか) の並びとページテーブルからなる。
// ページは最初に触れたときに作る (ページフォールト):
//   無名      0 で埋めたページ
//   ファイル  そのときにファイルから読む (ファイルの終わりより先は 0)
// fork はページを写さずに共有し、書けるページは両方で読み取り専用 + COW の印にする。
// 書こうとしたときに、まだ共有されていれば写し、自分だけなら書けるように戻す (コピーオンライト)。
// ページの共有の数は kalloc が持つ。
//
// ファイルのページは 1 枚: ファイルの (ファイルシステム, inode, ページ番号) のページは、ファイルシステムが持っている
// その 1 枚 (ext4 ならブロックのキャッシュのページ、tmpfs ならファイルのページそのもの: Inode::page_frame) を、
// read()/write() も、MAP_SHARED の写しも、MAP_PRIVATE の読むだけの写しも、ページキャッシュ (PRIV) も、みなそのまま見る
// (Linux のページキャッシュと同じ)。だから write() で書いたものは写しにすぐ映り、写しに書いたものは read() にすぐ見える。
// 写しているファイルのページの表 (SHARED_FILE) は、MAP_SHARED で写しているページと、書いたか (dirty) を覚える: 最初は
// 読み取り専用で写し、書かれたら印をつけて書けるようにし (そのときブロックをとり、更新時刻をいまに: Linux の page_mkwrite)、
// msync、munmap、プロセスの終わり、sync、ファイルシステムの書き出しで、ディスクに書き出す印をつけなおす (page_written。
// ファイルの終わりの先に書いたものはそのとき 0 にする: Linux の書き出しと同じ)。穴 (ブロックのないところ) の写しは表が
// 0 のページを持ち (MAP_PRIVATE でも)、ブロックをとるときにファイルシステムがそのページをそのまま使う (adopt_hole)。
// mmap で写したファイルの終わりより先のページに触れると SIGBUS (伸びればまた写せる)、切りつめたところは写しからみな外す
// (drop_file_pages。Linux の unmap_mapping_range)。ページで持たないファイルシステムのものは、前のように写しを読む (own)
//
// PTE のうち、ハードウェアが見ないビットを使う:
//   COW       (55) 書けるはずだが共有しているので読み取り専用にしてあるページ
//   PROTNONE  (56) PROT_NONE にしたので無効にしてあるが、中身は持っているページ (VALID は 0)
//   SWAP      (57) スワップへ追い出したページ (VALID は 0)。アドレスのところにスロットの番号 (swap.rs)
//
// スワップ: 自分だけの領域 (MAP_SHARED でない) の、ほかと共有していないページを追い出せる。
// 選ぶのは clock: AF (アクセスフラグ) を落としておき、次に見たときにまだ落ちていれば追い出す。
// AF が落ちたページに触れるとアクセスフラグのフォールトになり、fault_page が立てなおす。
use crate::cell::Bkl;
use crate::kalloc;
use crate::spinlock::SpinLock;
use crate::swap;
use crate::memlayout::{p2v, v2p, PGSIZE};
use crate::vfs::InodeRef;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::sync::atomic::{fence, AtomicU64, AtomicUsize, Ordering};

const PTE_VALID: u64 = 1 << 0;
const PTE_TABLE: u64 = 1 << 1; // L1/L2 ではテーブル、L3 ではページ
const PTE_ATTR_NORMAL: u64 = 1 << 2; // MAIR attr1
const PTE_USER: u64 = 1 << 6; // AP[1]: EL0 から触れる
const PTE_RDONLY: u64 = 1 << 7; // AP[2]
const PTE_SH_INNER: u64 = 3 << 8;
const PTE_AF: u64 = 1 << 10;
const PTE_NG: u64 = 1 << 11;
const PTE_PXN: u64 = 1 << 53;
const PTE_UXN: u64 = 1 << 54;
const PTE_COW: u64 = 1 << 55;
const PTE_PROTNONE: u64 = 1 << 56;
const PTE_SWAP: u64 = 1 << 57;
const PTE_ADDR: u64 = 0x0000_ffff_ffff_f000;

pub const MAXVA: usize = 1 << 39;

pub const PROT_READ: u8 = 1;
pub const PROT_WRITE: u8 = 2;
pub const PROT_EXEC: u8 = 4;
pub const PROT_RW: u8 = PROT_READ | PROT_WRITE;

pub const fn pg_down(a: usize) -> usize {
    a & !(PGSIZE - 1)
}

pub const fn pg_up(a: usize) -> usize {
    (a + PGSIZE - 1) & !(PGSIZE - 1)
}

// ---- MAP_SHARED のファイルのページ ----

type FileKey = (usize, u64, usize);

struct Cached {
    page: *mut u8,
    dirty: bool,
    /// 穴の 0 のページ (ブロックがない)。表だけが持ち、ファイルシステムがブロックをとるときにそのまま使う (adopt_hole)
    hole: bool,
    /// ファイルシステムのページでなく、読んだ写し (ページで持たないもの)。書き戻しは write_at で
    own: bool,
    ino: InodeRef,
}

/// 表も 1 つ参照を持つ。だれも写さなくなったら (参照が表とファイルシステムとページキャッシュだけになったら) 書き戻して捨てる
static SHARED_FILE: Bkl<BTreeMap<FileKey, Cached>> = Bkl::new(BTreeMap::new());

#[track_caller]
fn shared_file() -> &'static mut BTreeMap<FileKey, Cached> {
    SHARED_FILE.get()
}

fn file_key(ino: &InodeRef, off: usize) -> FileKey {
    let (fs, i) = ino.id();
    (fs, i, off / PGSIZE)
}

/// 写しで書いたページをファイルへ。ファイルシステムのページそのものなら中身はもうある: 終わりの先 (同じページの中) を
/// 0 にして (Linux の書き出しと同じ: 写しで終わりの先に書いたものはファイルに残らない)、ディスクに書き出す印をつけなおす。
/// 読んだ写し (own) なら write_at で書く
fn write_back(key: &FileKey, c: &Cached) {
    if c.hole {
        return;
    }
    let size = c.ino.meta().size as usize;
    let off = key.2 * PGSIZE;
    if off >= size {
        return;
    }
    let n = (size - off).min(PGSIZE);
    let r = if c.own {
        let data = unsafe { core::slice::from_raw_parts(c.page, n) };
        c.ino.write_at(off, data).map(|_| ())
    } else {
        if n < PGSIZE {
            unsafe { core::ptr::write_bytes(c.page.add(n), 0, PGSIZE - n) };
        }
        c.ino.page_written(off, false)
    };
    if r.is_err() {
        println!("vm: write back of a shared mapping failed");
    }
}

/// ファイルシステムが id のページ index のブロックをとる: 写しの表が穴の 0 のページを持っていれば、それをそのまま
/// ブロックのページにしてもらう (参照をひとつ足して返す。写している人のページがそのままブロックになるので、書いたものが映る)
pub fn adopt_hole(id: (usize, u64), index: usize) -> Option<*mut u8> {
    let c = shared_file().get_mut(&(id.0, id.1, index))?;
    if !c.hole {
        return None;
    }
    c.hole = false;
    kalloc::get(c.page);
    Some(c.page)
}

/// ファイル id のページ [first, last] がなくなる (切りつめ、穴あけ。ファイルシステムが、ブロックを外す前に呼ぶ): 写している
/// ところからみな外し (private の写しに書いたものも: Linux の unmap_mapping_range。次に触れたら SIGBUS か作りなおし)、
/// 表のページも捨てる (書いたものも: 切りつめたところは消える)
pub fn drop_file_pages(id: (usize, u64), first: usize, last: usize) {
    if first > last {
        return;
    }
    crate::proc::each_pagetable(|pt| pt.drop_file_range(id, first, last));
    let keys: Vec<FileKey> = shared_file().range((id.0, id.1, first)..=(id.0, id.1, last)).map(|(k, _)| *k).collect();
    for k in keys {
        if let Some(c) = shared_file().remove(&k) {
            kalloc::put(c.page);
        }
    }
}

// ---- 読むだけのファイルのページ (ページキャッシュ) ----
// MAP_PRIVATE のファイル (プログラムやライブラリ) のページは、読むだけのあいだはここの 1 枚をみんなで写す
// (書きこみ禁止で写し、書いたときに fault_page のコピーオンライトで自分のものに)。同じプログラムを何度
// 動かしても、ファイルを読みなおさない。ファイルが変わったら (更新時刻か大きさ) 読みなおす。
// 表も 1 つ参照を持つ。priv_max をこえたら、だれも写していないもの (参照が表だけ) から捨てる。
// 大きなロックなしのページフォルト (fast_fault) も読むので、表はスピンロックで守る。そちらはファイルに
// さわれないので、領域を写したときの版 (Backing::File の ver) と同じ版のファイルのページだけを使う。
// 版はファイルに 1 つ (Linux の i_size と同じ: 書いて伸びても、ページごとに書きなおさない)

struct PrivPage {
    page: *mut u8,
    /// ファイルシステムのページそのもの (ファイルシステムも 1 つ参照を持つ)。でなければ読んだ写し
    owned: bool,
    /// 命令キャッシュをあわせた (sync_icache)。実行できるように写すときに一度 (Linux の PG_dcache_clean)。
    /// 読むだけのページ (read のページキャッシュ) はしない
    synced: bool,
    /// 大きなロックなしの pwrite (write_fast) が書いて、ファイルシステムにまだ渡していない (PageCache の written にある)
    written: bool,
}

/// 大きなロックなしの pwrite (write_fast) を許すしるし。ファイルシステムが大きなロックの中で書いたあと (ext4 の write_at)、
/// この秒 (sec) のうちに、いまの大きさの中の、表にあるファイルシステムのページそのもの (owned) を書きかえるだけなら、
/// inode は 1 バイトも変わらない (ext4 の時刻は秒まで。setuid / setgid もない) と知らせたもの。inode が書きかわった
/// (statc の箱の版 ino)、IN_MODIFY の見張りが置かれた (WRITE_EPOCH)、ファイルシステムが書きはじめた (file_writing) か
/// 変わった (file_changed) ら使わない
#[derive(Clone, Copy)]
struct WritePermit {
    sec: u64,
    ino: u64,
    epoch: u64,
}

struct PageCache {
    pages: BTreeMap<FileKey, PrivPage>,
    /// ファイルごとの版 (更新時刻と大きさ)。表にあるそのファイルのページは、みなこの版のいまの中身
    vers: BTreeMap<(usize, u64), (u64, u64)>,
    permits: BTreeMap<(usize, u64), WritePermit>,
    /// write_fast が書いたページ (参照をひとつ持つ)。ファイルシステムが書き出す前に受けとって (take_written)、
    /// 中身のブロックに書き出す印をつける。since は列が空でなくなったときの ticks
    written: Vec<Written>,
    written_since: u64,
}

/// written の列の 1 つ: (鍵, ページ)
struct Written(FileKey, *mut u8);

// ページはカーネルのもの (どの CPU からでも)
unsafe impl Send for Written {}

impl PageCache {
    /// key のページ (ファイルの版が ver のとき)
    fn get_mut(&mut self, key: &FileKey, ver: (u64, u64)) -> Option<&mut PrivPage> {
        if self.vers.get(&(key.0, key.1)) != Some(&ver) {
            return None;
        }
        self.pages.get_mut(key)
    }

    /// ページを外したあとに: ファイルのページがもうなければ、版も
    fn forget_if_empty(&mut self, id: (usize, u64)) {
        if self.pages.range((id.0, id.1, 0)..=(id.0, id.1, usize::MAX)).next().is_none() {
            self.vers.remove(&id);
            self.permits.remove(&id);
        }
    }

    /// ファイル id のページをみな外す (参照は呼んだほうが返す)
    fn remove_file(&mut self, id: (usize, u64), freed: &mut Vec<*mut u8>) {
        let keys: Vec<FileKey> = self.pages.range((id.0, id.1, 0)..=(id.0, id.1, usize::MAX)).map(|(k, _)| *k).collect();
        for k in keys {
            if let Some(p) = self.pages.remove(&k) {
                freed.push(p.page);
            }
        }
        self.vers.remove(&id);
        self.permits.remove(&id);
    }
}

impl PrivPage {
    /// だれも写していない (参照が表 (とファイルシステム) だけ)
    fn idle(&self) -> bool {
        kalloc::refs(self.page) <= 1 + self.owned as u16
    }
}

// ページはカーネルのもの (どの CPU からでも)
unsafe impl Send for PrivPage {}

/// 表の大きさの目安: メモリの 1/4 (少なくとも 64 MiB)。Firefox の libxul (170 MB) のように、写されたままで
/// 捨てられないページが多くても、読みなおしを繰りかえさないように。メモリが足りなくなったら、
/// こことは別にスワップの側から捨てる (swap.rs)
fn priv_max() -> usize {
    // 表は 1 枚につきヒープを 100 バイトほど使う (BTreeMap)。ヒープは決まった大きさなので、その 1/4 まで
    // (メモリの多いマシンでも、表がヒープを使いきらないように: 2 GiB で 340 MB ほど)
    (crate::memlayout::ram_size() / PGSIZE / 4).max(16384).min(crate::heap::size() / 4 / PRIV_ENTRY_BYTES)
}

/// 表の 1 枚がヒープを使うおおよそのバイト (はかったもの)
const PRIV_ENTRY_BYTES: usize = 96;

/// ヒープがこれより混んでいたら、表に入れない (読んだページはそのまま使う)
fn heap_crowded() -> bool {
    crate::heap::used() > crate::heap::size() / 4 * 3
}

/// 次に捨てる (表をなめる) のは、表がこの数をこえたとき。捨てても減らなければ (みな写されている)、
/// しばらく (priv_max / 8 枚ふえるまで) なめない: 読みこむたびに表をなめると、それだけで重い
static NEXT_SHRINK: AtomicUsize = AtomicUsize::new(0);

static PRIV: SpinLock<PageCache> = SpinLock::new(PageCache { pages: BTreeMap::new(), vers: BTreeMap::new(), permits: BTreeMap::new(), written: Vec::new(), written_since: 0 });

/// ページキャッシュのページの数 (/proc/meminfo の Cached)
pub fn cached_pages() -> usize {
    PRIV.lock().pages.len()
}

/// shrink_cache で捨てたページの数
static SHRUNK: AtomicUsize = AtomicUsize::new(0);

/// /proc/ai/mem の page_cache: ページの数と上限、変わった数、読みそこね (ページキャッシュになかった) の数と時間、
/// 捨てた数、ファイルごとのページの数 (多い順に 20 まで。大きさと、名前はパスの覚えにあれば)
pub fn page_cache_json() -> alloc::string::String {
    use alloc::format;
    let mut files: Vec<((usize, u64), usize)> = Vec::new();
    let total = {
        let c = PRIV.lock();
        // 鍵は (fs, ino, ページ) の順なので、同じファイルは並ぶ
        for (k, _) in c.pages.iter() {
            match files.last_mut() {
                Some((id, n)) if *id == (k.0, k.1) => *n += 1,
                _ => files.push(((k.0, k.1), 1)),
            }
        }
        c.pages.len()
    };
    files.sort_by_key(|(_, n)| core::cmp::Reverse(*n));
    files.truncate(20);
    let mut list = Vec::new();
    for ((fs, ino), n) in files {
        let size = PRIV.lock().vers.get(&(fs, ino)).map_or(0, |v| v.1);
        let name = crate::vfs::path_of_id((fs, ino)).map_or(alloc::string::String::new(), |p| format!(",\"name\":\"{}\"", crate::aiproc::esc(&p)));
        list.push(format!("{{\"fs\":{},\"ino\":{},\"pages\":{},\"size\":{}{}}}", fs, ino, n, size, name));
    }
    let (miss, ns) = (FAULT_KIND[9].0.load(Ordering::Relaxed), FAULT_KIND[9].1.load(Ordering::Relaxed));
    format!(
        "{{\"pages\":{},\"max\":{},\"changes\":{},\"misses\":{},\"miss_ns\":{},\"shrunk\":{},\"files\":[{}]}}",
        total,
        priv_max(),
        CHANGES.load(Ordering::Relaxed),
        miss,
        ns,
        SHRUNK.load(Ordering::Relaxed),
        list.join(",")
    )
}

/// posix_fadvise の DONTNEED: ino の off から len バイト (0 なら終わりまで) にまるごと入るページを、だれも写していなければ
/// 表から外す (かかっているだけのページは外さない: Linux と同じ)
pub fn uncache(ino: &InodeRef, off: usize, len: usize) {
    let id = ino.id();
    let first = off.div_ceil(PGSIZE);
    let last = if len == 0 { usize::MAX } else { ((off + len) / PGSIZE).saturating_sub(1) };
    if last < first {
        return;
    }
    let mut c = PRIV.lock();
    let keys: Vec<FileKey> = c.pages.range((id.0, id.1, first)..=(id.0, id.1, last)).filter(|(_, p)| p.idle()).map(|(k, _)| *k).collect();
    for k in keys {
        if let Some(p) = c.pages.remove(&k) {
            kalloc::put(p.page);
        }
    }
    c.forget_if_empty(id);
    drop(c);
    // 穴の 0 のページも、だれも写していなければ (参照が表だけ)
    let holes: Vec<FileKey> = shared_file().range((id.0, id.1, first)..=(id.0, id.1, last)).filter(|(_, s)| s.hole && kalloc::refs(s.page) <= 1).map(|(k, _)| *k).collect();
    for k in holes {
        if let Some(s) = shared_file().remove(&k) {
            kalloc::put(s.page);
        }
    }
}

/// だれも写していないページを捨てる (n 枚まで)。捨てた数
pub fn shrink_cache(n: usize) -> usize {
    let mut freed = Vec::new();
    {
        let mut c = PRIV.lock();
        let victims: Vec<FileKey> = c.pages.iter().filter(|(_, p)| p.idle()).map(|(k, _)| *k).take(n).collect();
        for k in &victims {
            if let Some(p) = c.pages.remove(k) {
                freed.push(p.page);
            }
        }
        // 鍵の順なので、同じファイルは並ぶ
        for (i, k) in victims.iter().enumerate() {
            if victims.get(i + 1).is_none_or(|n| (n.0, n.1) != (k.0, k.1)) {
                c.forget_if_empty((k.0, k.1));
            }
        }
    }
    for &p in &freed {
        kalloc::put(p);
    }
    SHRUNK.fetch_add(freed.len(), Ordering::Relaxed);
    freed.len()
}

/// ヒープの遅い道 (heap::reclaim) から: 表のロックが取れれば、だれも写していないページを n 枚まで捨てる。
/// ヒープを使わない (使うと、またそこへ来る)。捨てた数
pub fn shrink_cache_try(n: usize) -> usize {
    let Some(mut c) = PRIV.try_lock() else { return 0 };
    let mut freed = 0;
    let mut from: Option<FileKey> = None;
    while freed < n {
        let next = match from {
            None => c.pages.iter().find(|(_, p)| p.idle()).map(|(k, _)| *k),
            Some(k) => c.pages.range(k..).find(|(_, p)| p.idle()).map(|(k, _)| *k),
        };
        let Some(k) = next else { break };
        if let Some(p) = c.pages.remove(&k) {
            kalloc::put(p.page);
            freed += 1;
        }
        c.forget_if_empty((k.0, k.1));
        from = Some(k);
    }
    SHRUNK.fetch_add(freed, Ordering::Relaxed);
    freed
}

/// file_changed の数 (読みこんでいるあいだに変わったら、表に入れない)
static CHANGES: AtomicUsize = AtomicUsize::new(0);

/// ファイル (id) の中身が変わる (書く、切りつめる): 表にあるそのファイルのページを捨てる。
/// 写しているところは自分の参照を持っているのでそのまま (MAP_PRIVATE で、写したあとの変更が見えるかは決まっていない)。
/// 更新時刻は秒までなので、版 (ver) だけでは同じ秒の同じ大きさの書きかえを見分けられない
pub fn file_changed(id: (usize, u64)) {
    CHANGES.fetch_add(1, Ordering::SeqCst);
    let mut freed = Vec::new();
    // 大きなロックなしの pwrite がいま写しているものは、終わるのを待つ (写しているページは参照を持っている)
    let _w = write_lock(id);
    PRIV.lock().remove_file(id, &mut freed);
    for p in freed {
        kalloc::put(p);
    }
}

/// ファイルシステムが id に書く前に (file_written とくみで。表にあるそのファイルのページが、みなファイルシステムの
/// ページそのもの (owned) で、その場で書きかえるとき: ext4)。いま読みこんでいるものは表に入れない
pub fn file_writing(id: (usize, u64)) {
    CHANGES.fetch_add(1, Ordering::SeqCst);
    // 大きなロックなしの pwrite は、書きおわる (write_permit) まで待たせる: 同じところへの書きこみが、まざらないように
    // (Linux も i_rwsem で 1 つずつ)。いま書いているもの (表のロックの中) は、ロックを取ったときには終わっている
    write_permit_drop(id);
}

/// write_fast の数: [0] 書いた、ふつうの道へ行ったわけ: [1] 許しがない・古い、[2] 大きさの外かページが表にない、
/// [3] ユーザーのページが写っていない、[4] 書いたページの列が長いか古い (ファイルシステムに書き出させる)。/proc/bkl に出す
pub static WRITE_FAST: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

/// IN_MODIFY の見張りが置かれるたびに増える: それより前の write_fast の許しは使わない
static WRITE_EPOCH: AtomicU64 = AtomicU64::new(0);

/// write_fast が書いたページの列の長さと、古さ (ticks) の上限。こえたらふつうの道へ (そこでファイルシステムが書き出す)
const WRITTEN_MAX: usize = 4096;
const WRITTEN_AGE: u64 = 5 * crate::timer::HZ;

/// 大きなロックなしの pwrite (write_fast) が写すあいだ持つ、ファイルごとのロック (id で 64 に分ける: 同じ箱のほかの
/// ファイルとは 1 つずつ)。同じファイルへの書きこみはまざらない (Linux の i_rwsem)。ファイルシステムが書きはじめるとき
/// (write_permit_drop)、ページを捨てるとき (file_changed)、許しをみな古くするとき (write_permits_revoke) も取って、写して
/// いる途中のものが終わるのを待つ。取る順は WRITE_LOCKS → PRIV
static WRITE_LOCKS: [SpinLock<()>; 64] = [const { SpinLock::new(()) }; 64];

fn write_lock(id: (usize, u64)) -> crate::spinlock::Guard<'static, ()> {
    WRITE_LOCKS[(id.0.wrapping_mul(31) ^ id.1 as usize) % WRITE_LOCKS.len()].lock()
}

/// ファイルシステムが id に書いた (大きなロックの中で): 秒 sec のうちは、write_fast で書いてよい (WritePermit)
pub fn write_permit(id: (usize, u64), sec: u64) {
    let p = WritePermit { sec, ino: crate::statc::ino_ver(id), epoch: WRITE_EPOCH.load(Ordering::SeqCst) };
    let mut c = PRIV.lock();
    if c.vers.contains_key(&id) {
        c.permits.insert(id, p);
    }
}

pub fn write_permit_drop(id: (usize, u64)) {
    let _w = write_lock(id);
    let mut c = PRIV.lock();
    if !c.permits.is_empty() {
        c.permits.remove(&id);
    }
}

/// IN_MODIFY の見張りが置かれた: write_fast の許しをみな古くする (inotify のできごとはふつうの道が出す)
pub fn write_permits_revoke() {
    WRITE_EPOCH.fetch_add(1, Ordering::SeqCst);
    // いま write_fast の中で写しているもの (ファイルごとのロックを持っている) が終わるのを待つ
    for l in WRITE_LOCKS.iter() {
        drop(l.lock());
    }
}

/// 大きなロックなしの pwrite64 (file::fast_pwrite): ファイル id の off から len バイトを、ユーザーの va から、表にある
/// ファイルシステムのページそのもの (ext4 のブロックのキャッシュのページ) へ写す。許し (WritePermit) がいまの秒で新しく、
/// 大きさの中で、ページがみな表にあるときだけ。書いたページは written の列に入れ、ファイルシステムが書き出す前に
/// 受けとる (take_written)。Linux の generic_perform_write と同じく、ページキャッシュに書くだけ (書き出しはあとで)。
/// ほかは None でふつうの道へ (書きかけでやめたら、そこまでのページも列に入れてから: ふつうの道がもう一度書く)
pub fn write_fast(pt: &PageTable, id: (usize, u64), off: usize, va: usize, len: usize) -> Option<usize> {
    let end = off.checked_add(len)?;
    // ユーザーのところが写っていなければ、ページにさわる前にふつうの道へ (フォルトで写す)
    if !pt.readable_nofault(va, len) {
        WRITE_FAST[3].fetch_add(1, Ordering::Relaxed);
        return None;
    }
    // 秒は粗い時計で (ext4 の時刻も粗い時計: 同じ秒なら、ふつうの道が書いても時刻は変わらない。時計は読まない)
    let sec = crate::timer::coarse_epoch_ns() / 1_000_000_000;
    let ino = crate::statc::ino_ver(id);
    let (first, last) = (off / PGSIZE, (end - 1) / PGSIZE);
    // 写すあいだは、このファイルのロックだけを持つ (表のロック PRIV は、確かめるあいだと印をつけるあいだだけ: ちがう
    // ファイルなら、いくつもの CPU で同時に写す)。写すページは参照を持っておく (表から外されても捨てられない)
    let _w = write_lock(id);
    let mut held = [core::mem::MaybeUninit::<(FileKey, *mut u8)>::uninit(); WRITE_PAGES];
    let mut n = 0;
    // 書いた印がみなもうついていて (written の列にある)、写すあいだにファイルシステムが列を受けとらなければ
    // (TAKEN が同じ)、あとで表のロックを取りなおさなくてよい
    let mut all_written = true;
    let taken = TAKEN.load(Ordering::Acquire);
    let now = crate::timer::coarse_ticks();
    {
        let mut g = PRIV.lock();
        let c = &mut *g;
        let ok = c.permits.get(&id).is_some_and(|w| w.sec == sec && w.ino == ino && w.epoch == WRITE_EPOCH.load(Ordering::SeqCst));
        if !ok {
            WRITE_FAST[1].fetch_add(1, Ordering::Relaxed);
            return None;
        }
        if !c.written.is_empty() && (c.written.len() >= WRITTEN_MAX || now >= c.written_since + WRITTEN_AGE) {
            WRITE_FAST[4].fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let have = c.pages.range((id.0, id.1, first)..=(id.0, id.1, last)).filter(|(_, p)| p.owned).count();
        if c.vers.get(&id).is_none_or(|v| (v.1 as usize) < end) || have != last - first + 1 || have > WRITE_PAGES {
            WRITE_FAST[2].fetch_add(1, Ordering::Relaxed);
            return None;
        }
        for (k, p) in c.pages.range_mut((id.0, id.1, first)..=(id.0, id.1, last)) {
            // 書きかわるページは、実行するときに命令キャッシュをあわせなおす
            p.synced = false;
            all_written &= p.written;
            kalloc::get(p.page);
            held[n].write((*k, p.page));
            n += 1;
        }
    }
    let held = unsafe { core::slice::from_raw_parts(held.as_ptr() as *const (FileKey, *mut u8), n) };
    let mut done = 0;
    let mut copied = true;
    for &(k, page) in held {
        let pos = k.2 * PGSIZE;
        let (from, to) = (off.max(pos), end.min(pos + PGSIZE));
        let dst = unsafe { core::slice::from_raw_parts_mut(page.add(from - pos), to - from) };
        copied = pt.copy_in_nofault(va + (from - off), dst);
        done += 1;
        if !copied {
            break;
        }
    }
    // 書いたページに印をつけて written の列へ (持っていた参照を列にわたす。書きかけでやめたら、そこまでのページも:
    // ふつうの道がもう一度書く)。ほかの道 (file_page) が表から外していても、列には入れる (take_written はそのまま
    // ファイルシステムに渡し、まだブロックのキャッシュのページなら書き出す印がつく)
    if !(copied && all_written && TAKEN.load(Ordering::Acquire) == taken) {
        let mut put = [core::ptr::null_mut::<u8>(); WRITE_PAGES];
        let mut np = 0;
        {
            let mut g = PRIV.lock();
            let PageCache { pages, written, written_since, .. } = &mut *g;
            for (i, &(k, page)) in held.iter().enumerate() {
                let in_table = pages.get_mut(&k).filter(|p| p.page == page);
                if i >= done || in_table.as_ref().is_some_and(|p| p.written) {
                    put[np] = page;
                    np += 1;
                    continue;
                }
                if let Some(p) = in_table {
                    p.written = true;
                }
                if written.is_empty() {
                    *written_since = now;
                }
                written.push(Written(k, page));
            }
        }
        for &p in &put[..np] {
            kalloc::put(p);
        }
    } else {
        // みな列にあるまま: 持っていた参照を返すだけ
        for &(_, page) in held {
            kalloc::put(page);
        }
    }
    if !copied {
        WRITE_FAST[3].fetch_add(1, Ordering::Relaxed);
        return None;
    }
    WRITE_FAST[0].fetch_add(1, Ordering::Relaxed);
    Some(len)
}

/// take_written がページを受けとった回数 (write_fast が、写すあいだに列から出されたかを見る)
static TAKEN: AtomicU64 = AtomicU64::new(0);

/// write_fast が 1 度に写すページの数の上限 (fast_pwrite の 256 KiB と、端のページ)
const WRITE_PAGES: usize = 256 * 1024 / PGSIZE + 1;

/// ファイルシステム fs が書き出す前に (大きなロックの中で): write_fast が書いたページ (ino, ページの番号, ページ)。
/// ページの参照をひとつずつ持ったもの (呼んだほうが、書き出す印をつけてから返す)
pub fn take_written(fs: usize) -> Vec<(u64, usize, *mut u8)> {
    let mut g = PRIV.lock();
    let c = &mut *g;
    if c.written.is_empty() {
        return Vec::new();
    }
    let PageCache { pages, written, written_since, .. } = c;
    let mut out = Vec::new();
    TAKEN.fetch_add(1, Ordering::Release);
    written.retain(|Written(k, page)| {
        if k.0 != fs {
            return true;
        }
        // 印を落とすのは書き出す前: このあと write_fast がまた書いたら、また列に入る (次に書き出す)
        if let Some(p) = pages.get_mut(k)
            && p.page == *page
        {
            p.written = false;
        }
        out.push((k.1, k.2, *page));
        false
    });
    if !written.is_empty() && !out.is_empty() {
        *written_since = crate::timer::coarse_ticks();
    }
    out
}

/// ファイルシステムが id のページ [first, last] に書いた (file_writing のあと): 表にあるそのファイルのページはそのまま
/// (その場で書きかわった)、版を ver (書いたあとの更新時刻と大きさ) に。pages (書いたページの (番号, ファイルシステムの
/// ページ)。参照をひとつずつ持ったもの) も表に入れる: Linux と同じく、書いたものはページキャッシュにある (書いて
/// すぐ読むとき、ファイルシステムを引きなおさない)。表がいっぱいか、RWF_DONTCACHE なら入れない
pub fn file_written(id: (usize, u64), ver: (u64, u64), first: usize, last: usize, pages: Vec<(usize, *mut u8)>) {
    CHANGES.fetch_add(1, Ordering::SeqCst);
    // その場で書きかえただけ (書いたページはどれも、もう表にあるファイルシステムのページそのもので、版も同じ。
    // 上書きをくりかえすデータベースやログ): 命令キャッシュの印を落とし、書くために足した参照を返すだけ
    if !pages.is_empty() && !crate::proc::dontcache_on() {
        let mut c = PRIV.lock();
        if c.vers.get(&id) == Some(&ver) && pages.iter().all(|(i, p)| c.pages.get(&(id.0, id.1, *i)).is_some_and(|e| e.owned && e.page == *p)) {
            if first <= last {
                for (_, p) in c.pages.range_mut((id.0, id.1, first)..=(id.0, id.1, last)) {
                    p.synced = false;
                }
            }
            drop(c);
            for (_, p) in pages {
                kalloc::put(p);
            }
            return;
        }
    }
    let room = !pages.is_empty() && !crate::proc::dontcache_on() && make_room();
    let max = priv_max();
    let mut freed = Vec::new();
    {
        let mut c = PRIV.lock();
        // 書きかわったページは、実行するときに命令キャッシュをあわせなおす
        if first <= last {
            for (_, p) in c.pages.range_mut((id.0, id.1, first)..=(id.0, id.1, last)) {
                p.synced = false;
            }
        }
        let have = c.pages.range((id.0, id.1, 0)..=(id.0, id.1, usize::MAX)).next().is_some();
        if have || room {
            c.vers.insert(id, ver);
        }
        for (index, page) in pages {
            let key = (id.0, id.1, index);
            if !room || c.pages.len() >= max || heap_crowded() {
                if !c.pages.get(&key).is_some_and(|p| p.page == page) {
                    // 表に入れられない。前の版のページが残っていたら (ないはず) それも
                    if let Some(o) = c.pages.remove(&key) {
                        freed.push(o.page);
                    }
                }
                freed.push(page);
                continue;
            }
            if let Some(o) = c.pages.insert(key, PrivPage { page, owned: true, synced: false, written: false }) {
                freed.push(o.page);
            }
        }
        if !have {
            c.forget_if_empty(id);
        }
    }
    for p in freed {
        kalloc::put(p);
    }
    // RWF_DONTCACHE: 書いたところのページは、だれも写していなければ外す (Linux は書き出したあとに捨てる。pages の参照を
    // 返したあとで: 返す前はだれかが持っているように見える)
    if crate::proc::dontcache_on() && first <= last {
        let mut c = PRIV.lock();
        let keys: Vec<FileKey> = c.pages.range((id.0, id.1, first)..=(id.0, id.1, last)).filter(|(_, p)| p.idle()).map(|(k, _)| *k).collect();
        let mut gone = Vec::new();
        for k in keys {
            if let Some(p) = c.pages.remove(&k) {
                gone.push(p.page);
            }
        }
        c.forget_if_empty(id);
        drop(c);
        for p in gone {
            kalloc::put(p);
        }
    }
}

/// 表に入れる前に: 上限をこえていたら、だれも写していないものを捨てる。ヒープが混んでいて入れられなければ false
fn make_room() -> bool {
    let max = priv_max();
    if PRIV.lock().pages.len() >= max.max(NEXT_SHRINK.load(Ordering::Relaxed)) {
        shrink_cache(max / 8);
        NEXT_SHRINK.store(PRIV.lock().pages.len() + max / 8, Ordering::Relaxed);
    }
    // ヒープが混んでいたら、写されていないものを捨てる。それでも混んでいれば表に入れない
    if heap_crowded() {
        shrink_cache(max / 8);
    }
    !heap_crowded()
}

/// ページキャッシュを通して読む (大きなロックを持って。ino は page_cacheable なもの)。ふつうのファイルでなければ
/// そのまま read_at。読んだページは表に残るので、次からは read_fast (ロックなし) で読める
pub fn read_cached(ino: &InodeRef, off: usize, buf: &mut [u8]) -> Result<usize, i64> {
    let _p = crate::prof::span("vm:read_cached");
    let m = ino.meta();
    if m.mode & crate::vfs::S_IFMT != crate::vfs::S_IFREG {
        return ino.read_at(off, buf);
    }
    let size = m.size as usize;
    if off >= size {
        return Ok(0);
    }
    let n = buf.len().min(size - off);
    let mut done = 0;
    while done < n {
        let pos = off + done;
        let po = pos % PGSIZE;
        let k = (PGSIZE - po).min(n - done);
        match file_page(ino, pos - po, false) {
            Some(page) => {
                unsafe { core::ptr::copy_nonoverlapping(page.add(po), buf[done..].as_mut_ptr(), k) };
                kalloc::put(page);
                done += k;
            }
            // ページがとれない (メモリが足りない、または noio でキャッシュになかった): 残りはじかに
            // (noio なら EAGAIN か、ここまでの分)
            None => {
                return match ino.read_at(pos, &mut buf[done..n]) {
                    Ok(k) => Ok(done + k),
                    Err(_) if done > 0 => Ok(done),
                    Err(e) => Err(e),
                };
            }
        }
    }
    Ok(n)
}

/// ページキャッシュが記録しているファイル id の大きさ (大きなロックなしの lseek の SEEK_END。read_fast と同じもの)
pub fn cached_size(id: (usize, u64)) -> Option<u64> {
    PRIV.lock().vers.get(&id).map(|v| v.1)
}

/// 大きなロックなしで、ページキャッシュから読む (syscall::fast の read)。ファイル id の off から len バイトの
/// ページが全部表にあり、ユーザーのページも写っているときだけ Some(読んだ数)。表にあるページはいまの中身で
/// (書けばその場で書きかわる (file_written) か捨てる (file_changed))、記録した大きさもいまの大きさ
pub fn read_fast(pt: &PageTable, id: (usize, u64), off: usize, va: usize, len: usize) -> Option<usize> {
    let c = PRIV.lock();
    let size = c.vers.get(&id)?.1 as usize;
    if off >= size {
        return Some(0);
    }
    let n = len.min(size - off);
    let mut done = 0;
    while done < n {
        let pos = off + done;
        let po = pos % PGSIZE;
        let k = (PGSIZE - po).min(n - done);
        let p = c.pages.get(&(id.0, id.1, pos / PGSIZE))?;
        let src = unsafe { core::slice::from_raw_parts(p.page.add(po), k) };
        if !pt.copy_out_nofault(va + done, src) {
            return None;
        }
        done += k;
    }
    Some(n)
}

/// 大きなロックなしで、ページキャッシュの off から len バイト (ファイルの終わりまで) を f に渡す
/// (syscall::fast の splice: ファイル → パイプ)。ページが全部表にあるときだけ、表のロックを持ったまま
/// f(ページの切れはしの並び) を呼ぶ。f が false なら None。ファイルの終わりより先なら Some(0)
pub fn with_cached(id: (usize, u64), off: usize, len: usize, f: impl FnOnce(&[&[u8]]) -> bool) -> Option<usize> {
    let c = PRIV.lock();
    let size = c.vers.get(&id)?.1 as usize;
    if off >= size {
        return Some(0);
    }
    let n = len.min(size - off);
    let mut parts: alloc::vec::Vec<&[u8]> = alloc::vec::Vec::with_capacity(n / PGSIZE + 2);
    let mut done = 0;
    while done < n {
        let pos = off + done;
        let po = pos % PGSIZE;
        let k = (PGSIZE - po).min(n - done);
        let p = c.pages.get(&(id.0, id.1, pos / PGSIZE))?;
        parts.push(unsafe { core::slice::from_raw_parts(p.page.add(po), k) });
        done += k;
    }
    if f(&parts) { Some(n) } else { None }
}

/// 表にある、版が ver のページ (参照をひとつ足して)。exec (実行できるように写す) なら命令キャッシュもあわせて。
/// ロックなしでも呼べる
fn cached_page(key: &FileKey, ver: (u64, u64), exec: bool) -> Option<*mut u8> {
    let mut c = PRIV.lock();
    let p = c.get_mut(key, ver)?;
    if exec && !p.synced {
        sync_icache(p.page as usize, PGSIZE);
        p.synced = true;
    }
    kalloc::get(p.page);
    Some(p.page)
}

/// ファイルの版 (更新時刻と大きさ)
pub fn file_ver(ino: &InodeRef) -> (u64, u64) {
    meta_ver(&ino.meta())
}

pub fn meta_ver(m: &crate::vfs::Meta) -> (u64, u64) {
    (m.mtime, m.size)
}

/// ino の foff (ページの境目) からの 1 ページ (ファイルの終わりのあとは 0)、参照をひとつ足して。
/// ページの境目でなければ None
/// 調べもの用: ページキャッシュに無かったものをシリアルに出す (/proc/strace に "pcmiss")
pub static MISS_LOG: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// exec なら、実行できるように写すページ (命令キャッシュもあわせる)
fn file_page(ino: &InodeRef, foff: usize, exec: bool) -> Option<*mut u8> {
    if foff % PGSIZE != 0 {
        return None;
    }
    let ver = file_ver(ino);
    let key = file_key(ino, foff);
    if let Some(p) = cached_page(&key, ver, exec) {
        return Some(p);
    }
    // ディスクを読まない区間 (RWF_NOWAIT): 写しの表にある (穴の 0 のページか、写しているページ) ものだけ。なければ、
    // ファイルシステムがキャッシュにあるブロックだけで読む (read_cached の None。穴もキャッシュになければ EAGAIN: Linux は
    // ページキャッシュにないページを NOWAIT で読まない)
    if crate::proc::noio_on() {
        let c = shared_file().get(&key)?;
        kalloc::get(c.page);
        return Some(c.page);
    }
    let t0 = crate::timer::uptime_ns();
    struct Miss(u64);
    impl Drop for Miss {
        fn drop(&mut self) {
            FAULT_KIND[9].0.fetch_add(1, Ordering::Relaxed);
            FAULT_KIND[9].1.fetch_add(crate::timer::uptime_ns().saturating_sub(self.0), Ordering::Relaxed);
        }
    }
    let _miss = Miss(t0);
    if MISS_LOG.load(Ordering::Relaxed) {
        let c = PRIV.lock();
        let have = c.pages.get(&key).and(c.vers.get(&ino.id()).copied());
        let n = c.pages.len();
        drop(c);
        crate::println!("pcmiss ino {:?} off {:#x} ver {:?} had {:?} n {}", ino.id(), foff, ver, have, n);
    }
    let changes = CHANGES.load(Ordering::SeqCst);
    // ファイルのページそのもの (ファイルシステムが持っている 1 枚)。穴なら、写しの表が持つ 0 のページ (表には入れない)。
    // ページで持たないものなら読んだ写し
    let (page, owned) = match ino.page_frame(foff, false) {
        Ok(Some(p)) => (p, true),
        Ok(None) => return hole_page(ino, foff, exec),
        Err(e) if e == -crate::vfs::ENODEV => {
            let page = kalloc::alloc()?;
            let size = ver.1 as usize;
            if foff < size {
                let buf = unsafe { core::slice::from_raw_parts_mut(page, (size - foff).min(PGSIZE)) };
                if ino.read_at(foff, buf).is_err() {
                    kalloc::free(page);
                    return None;
                }
            }
            (page, false)
        }
        Err(_) => return None,
    };
    Some(keep_page(key, ver, page, owned, exec, changes))
}

/// fault-around のまわりのページ: ページキャッシュか、ファイルシステムのキャッシュにあるものだけ (ディスクは読まない。
/// 穴と、ページで持たないファイルのものは None)。参照をひとつ足して
fn file_page_cached(ino: &InodeRef, foff: usize, exec: bool) -> Option<*mut u8> {
    let ver = file_ver(ino);
    let key = file_key(ino, foff);
    if let Some(p) = cached_page(&key, ver, exec) {
        return Some(p);
    }
    let changes = CHANGES.load(Ordering::SeqCst);
    let page = match crate::proc::noio(|| ino.page_frame(foff, false)) {
        Ok(Some(p)) => p,
        _ => return None,
    };
    Some(keep_page(key, ver, page, true, exec, changes))
}

/// ファイルシステムからもらったページ (参照をひとつ持って) を、ページキャッシュの表にも入れて返す
fn keep_page(key: FileKey, ver: (u64, u64), page: *mut u8, owned: bool, exec: bool, changes: usize) -> *mut u8 {
    // 実行するなら命令キャッシュも (表に入れるものは、その印をつけて一度だけ)
    if exec {
        sync_icache(page as usize, PGSIZE);
    }
    let room = make_room();
    // 読んでいるあいだ (ディスクを待って眠ったとき) にどこかのファイルが変わったら、表には入れずにこのまま使う。
    // RWF_DONTCACHE (proc::dontcache) も、新しく読んだものは入れない
    if CHANGES.load(Ordering::SeqCst) != changes || !room || crate::proc::dontcache_on() {
        return page;
    }
    kalloc::get(page);
    let mut freed = Vec::new();
    {
        let mut c = PRIV.lock();
        // ファイルの版がちがう (file_changed なしで更新時刻か大きさが変わった: utimensat など): そのファイルのページは
        // みな外して、この版で
        let id = (key.0, key.1);
        if c.vers.get(&id).is_some_and(|v| *v != ver) {
            c.remove_file(id, &mut freed);
        }
        c.vers.insert(id, ver);
        // ほかが先に入れたものは外す。写しているところは自分の参照を持っている
        if let Some(o) = c.pages.insert(key, PrivPage { page, owned, synced: exec, written: false }) {
            freed.push(o.page);
        }
    }
    for p in freed {
        kalloc::put(p);
    }
    page
}

/// 穴 (ブロックのないところ) の写しの 0 のページ: 写しの表が持つ (なければ作る)。参照をひとつ足して。
/// ファイルシステムがそこにブロックをとるとき、このページをそのまま使う (adopt_hole) ので、書いたものが写しに映る
fn hole_page(ino: &InodeRef, foff: usize, exec: bool) -> Option<*mut u8> {
    let key = file_key(ino, foff);
    let page = match shared_file().get(&key) {
        Some(c) => c.page,
        None => {
            let page = kalloc::alloc()?;
            shared_file().insert(key, Cached { page, dirty: false, hole: true, own: false, ino: ino.clone() });
            page
        }
    };
    // 0 のページでも、写しで書いたものかもしれない (MAP_SHARED の穴は書けないので、ないはず)
    if exec {
        sync_icache(page as usize, PGSIZE);
    }
    kalloc::get(page);
    Some(page)
}

/// 写しているファイルのページ (参照をひとつ足して): 表にあればそれ。なければファイルシステムのページそのもの
/// (穴なら 0 のページ、ページで持たないものなら読んだ写し) を表に入れて
fn shared_page(ino: &InodeRef, key: &FileKey, foff: usize, exec: bool) -> Result<*mut u8, FaultErr> {
    if let Some(c) = shared_file().get(key) {
        // ほかが実行しないで写したものかもしれない
        if exec {
            sync_icache(c.page as usize, PGSIZE);
        }
        kalloc::get(c.page);
        return Ok(c.page);
    }
    let (page, hole, own) = match ino.page_frame(foff, false) {
        Ok(Some(p)) => (p, false, false),
        Ok(None) => (kalloc::alloc().ok_or(FaultErr::NoMem)?, true, false),
        Err(e) if e == -crate::vfs::ENODEV => {
            let page = kalloc::alloc().ok_or(FaultErr::NoMem)?;
            let size = ino.meta().size as usize;
            if foff < size {
                let buf = unsafe { core::slice::from_raw_parts_mut(page, (size - foff).min(PGSIZE)) };
                if ino.read_at(foff, buf).is_err() {
                    kalloc::free(page);
                    return Err(FaultErr::Bus);
                }
            }
            (page, false, true)
        }
        Err(_) => return Err(FaultErr::Bus),
    };
    if exec {
        sync_icache(page as usize, PGSIZE);
    }
    kalloc::get(page);
    shared_file().insert(*key, Cached { page, dirty: false, hole, own, ino: ino.clone() });
    Ok(page)
}

/// MAP_SHARED で写しているファイルのページの数 (/proc/meminfo の Shmem)
pub fn shared_pages() -> usize {
    shared_file().len()
}

/// (MAP_SHARED のページの数, そのうち書いてまだファイルに書き戻していないもの)。/proc/ai/mem
pub fn shared_stats() -> (usize, usize) {
    let m = shared_file();
    (m.len(), m.values().filter(|c| c.dirty).count())
}

/// 書いたページをファイルに書き戻す (sync)。印はそのまま (まだ書けるように写っているかもしれない)
pub fn sync_shared() {
    for (k, c) in shared_file().iter().filter(|(_, c)| c.dirty) {
        write_back(k, c);
    }
}

/// keys のページを書き戻し、もうだれも写していないものは捨てる
fn release_shared(keys: &[FileKey]) {
    for k in keys {
        let Some(c) = shared_file().get(k) else { continue };
        if c.dirty {
            write_back(k, c);
        }
        // 表のほかに持っているのは、ファイルシステム (そのページそのものなら) と、ページキャッシュ (同じページを入れていれば)
        let mut others = 1u16;
        if !c.hole && !c.own {
            others += 1;
        }
        if PRIV.lock().pages.get(k).is_some_and(|p| p.page == c.page) {
            others += 1;
        }
        if kalloc::refs(c.page) <= others {
            let c = shared_file().remove(k).unwrap();
            kalloc::put(c.page);
        }
    }
}

/// 領域の中身のもと
#[derive(Clone)]
pub enum Backing {
    Anon,
    /// 領域の先頭がファイルの off。va が fend 以上のところは 0 (ELF の .bss の始まりなど)。fend が MAX なら mmap で
    /// 写したもの: ファイルの終わりは触れたときの大きさで見る (その先のページは SIGBUS、伸びればまた写せる)
    /// ver は写したときのファイルの版 (file_ver。ロックなしのページフォルトがページキャッシュをくらべる)
    File { ino: InodeRef, off: usize, fend: usize, ver: (u64, u64) },
    /// カーネルが持っているページをそのまま見せる (/dev/fb0 のフレームバッファ)。off はバイト。hold は写しが持つ
    /// 開いたファイル (TCP のソケット: Linux の vma の vm_file と同じく、fd を閉じても写しがあるあいだは閉じない)
    Pages {
        pages: alloc::rc::Rc<Vec<*mut u8>>,
        off: usize,
        #[allow(dead_code)]
        hold: Option<crate::file::FileRef>,
    },
}

#[derive(Clone)]
pub struct Vma {
    pub end: usize,
    pub prot: u8,
    /// MAP_SHARED (fork しても共有のまま)
    pub shared: bool,
    pub back: Backing,
    /// 写したファイルのパス (/proc/PID/maps と、落ちたときの知らせ)
    pub name: Option<alloc::rc::Rc<str>>,
}

/// ページフォールトの結果
pub enum FaultErr {
    /// 領域がない (SEGV_MAPERR)
    NoMap,
    /// 権限がない (SEGV_ACCERR)
    Access,
    NoMem,
    /// 写したファイルの終わりより先、またはそのページがとれない (SIGBUS)
    Bus,
}

/// ルートは kalloc したページ (カーネル仮想アドレス)
pub struct PageTable {
    root: *mut u64,
    vmas: BTreeMap<usize, Vma>,
    /// 大きなロックなしでこの表を読み書きしている CPU の数 (copy_out_nofault、fast_fault)。
    /// ページを手放す前に 0 になるのを待つ (quiesce)
    fast_users: AtomicUsize,
    /// 大きなロックを持ってこの表 (領域か PTE) を変えている途中の数。0 でないあいだ fast_fault は
    /// ふつうの道 (大きなロック) へ回る。変える側は増やしてから quiesce する (Mutating)
    mutators: AtomicUsize,
    /// clock の針 (次にスワップへ追い出すページを探しはじめる va)
    clock: usize,
    /// 最近の map / protect / unmap (操作, 始め, 終わり, prot)。落ちたときに、そのアドレスにかかわったものを出す (調べもの用)
    hist: alloc::collections::VecDeque<(u8, usize, usize, u8)>,
}

/// hist に覚える数
const HIST: usize = 512;

/// 表を変えている途中 (PageTable::mutating)。落とすと終わり。
/// &mut self のメソッドの中で持つので、表そのものではなく数えるところだけを指す (表はそのあいだ動かない)
pub struct Mutating(*const AtomicUsize);

impl Drop for Mutating {
    fn drop(&mut self) {
        unsafe { (*self.0).fetch_sub(1, Ordering::SeqCst) };
    }
}

fn index(va: usize, level: usize) -> usize {
    (va >> (12 + 9 * (3 - level))) & 0x1ff
}

fn table_at(e: u64) -> *mut u64 {
    p2v((e & PTE_ADDR) as usize) as *mut u64
}

/// ページを持っている PTE か (PROT_NONE で無効にしてあるものも)
fn has_page(e: u64) -> bool {
    e & PTE_VALID != 0 || e & PTE_PROTNONE != 0
}

/// スワップへ追い出したページの PTE か
fn is_swap(e: u64) -> bool {
    e & PTE_VALID == 0 && e & PTE_SWAP != 0
}

fn swap_pte(slot: swap::Slot) -> u64 {
    (slot << 12) & PTE_ADDR | PTE_SWAP
}

fn slot_of(e: u64) -> swap::Slot {
    (e & PTE_ADDR) >> 12
}

/// カーネルが書いたところを命令として実行できるように: データキャッシュを PoU まで書き出し、
/// 命令キャッシュを (すべての CPU で) 消す。QEMU の TCG では要らないが、本物の CPU (HVF) では要る
pub fn sync_icache(kva: usize, len: usize) {
    let ctr: u64;
    unsafe { core::arch::asm!("mrs {}, ctr_el0", out(reg) ctr) };
    // CTR_EL0: IDC (bit 28) ならデータキャッシュを PoU まで書き出さなくてよい、DIC (bit 29) なら命令キャッシュを
    // 消さなくてよい (Apple の CPU など)。L1Ip (bit 15:14) が PIPT (0b11) なら、そのアドレスの分だけ消せば
    // (ic ivau) ほかの写しにも効く。それ以外 (VIPT など) は、いままでどおり全部 (ic ialluis)。
    // ic ialluis は QEMU の TCG ではすべての CPU の訳した命令を捨てるので、ページごとにすると重い
    // (キャッシュにないファイルを読むと 1 ページに 1 回)
    if ctr & (1 << 28) == 0 {
        let line = 4usize << ((ctr >> 16) & 0xf);
        let mut a = kva & !(line - 1);
        while a < kva + len {
            unsafe { core::arch::asm!("dc cvau, {}", in(reg) a) };
            a += line;
        }
    }
    unsafe { core::arch::asm!("dsb ish") };
    if ctr & (1 << 29) == 0 {
        if (ctr >> 14) & 3 == 3 {
            let line = 4usize << (ctr & 0xf);
            let mut a = kva & !(line - 1);
            while a < kva + len {
                unsafe { core::arch::asm!("ic ivau, {}", in(reg) a) };
                a += line;
            }
            unsafe { core::arch::asm!("dsb ish") };
        } else {
            unsafe { core::arch::asm!("ic ialluis", "dsb ish") };
        }
    }
    unsafe { core::arch::asm!("isb") };
}

fn page_of(e: u64) -> *mut u8 {
    table_at(e) as *mut u8
}

/// prot の、書けないほう (まだ書いていない共有のファイルのページ、ページキャッシュのページ、COW の前)。
/// PROT_WRITE だけの領域も読める (arm64 の Linux と同じ: 書けるページは読める) ので、READ を足してから WRITE を外す
/// (外すだけだと PROT_NONE になり、読むたびにフォルトして止まらない)
fn ro(prot: u8) -> u8 {
    (if prot & PROT_WRITE != 0 { prot | PROT_READ } else { prot }) & !PROT_WRITE
}

/// pa のページを prot で。cow なら書けるはずでも読み取り専用にして印をつける
fn make_pte(pa: u64, prot: u8, cow: bool) -> u64 {
    if prot & (PROT_READ | PROT_WRITE | PROT_EXEC) == 0 {
        return pa | PTE_PROTNONE;
    }
    let mut b = pa | PTE_VALID | PTE_TABLE | PTE_ATTR_NORMAL | PTE_SH_INNER | PTE_AF | PTE_NG | PTE_USER | PTE_PXN;
    if prot & PROT_WRITE == 0 || cow {
        b |= PTE_RDONLY;
    }
    if cow && prot & PROT_WRITE != 0 {
        b |= PTE_COW;
    }
    if prot & PROT_EXEC == 0 {
        b |= PTE_UXN;
    }
    b
}

/// 共有のぐあいを見て PTE を作りなおす (private で他と共有していれば COW)
fn remake_pte(e: u64, prot: u8, shared_vma: bool) -> u64 {
    let pa = e & PTE_ADDR;
    let cow = !shared_vma && kalloc::refs(page_of(e)) > 1;
    make_pte(pa, prot, cow)
}

/// 調べもの用: ページフォルトの種類ごとの (回数, ns。ns は SAMPLE 回に 1 回測った見つもり)。/proc/bkl に出す。
/// 1 その他 (権限だけ) 2 共有ファイル 3 カーネルのページ 4 スワップ 5 ページキャッシュ 6 無名の新しいページ
/// 7 ファイルから読む新しいページ 8 コピーオンライトで写す 9 (5 のうち) ページキャッシュになくて読んだもの
/// 10 fault-around でいっしょに写したページ (フォルトではない。ns は 0)
pub static FAULT_KIND: [(core::sync::atomic::AtomicU64, core::sync::atomic::AtomicU64); 11] =
    [const { (core::sync::atomic::AtomicU64::new(0), core::sync::atomic::AtomicU64::new(0)) }; 11];

/// Linux の fault_around_bytes (64 KiB) のページの数: ファイルを読むフォルトで、まわりのページもキャッシュにあれば
/// いっしょに写す (ディスクは読まない)。プログラムの起動は写したファイルをつぎつぎにさわるので、フォルトが 1/16 に
const FAULT_AROUND: usize = 16;

/// fault-around のまど: page_va をふくむ FAULT_AROUND ページの境目のまど (同じ PTE の表の中) を、領域 [start, end) と、
/// ファイルのページがまるごと写せるところ (fend の前) に切ったもの
fn around(start: usize, end: usize, fend: usize, page_va: usize) -> (usize, usize) {
    let w = FAULT_AROUND * PGSIZE;
    let lo = (page_va & !(w - 1)).max(start);
    let hi = ((page_va & !(w - 1)) + w).min(end).min(pg_down(fend));
    (lo, hi)
}

// TLB はすべての CPU に (inner shareable) 消す: 同じアドレス空間のスレッドがほかの CPU で動いているかも
fn flush_va(va: usize) {
    unsafe {
        core::arch::asm!("dsb ishst", "tlbi vaae1is, {}", "dsb ish", "isb", in(reg) (va >> 12) as u64);
    }
}

fn flush_all() {
    unsafe { core::arch::asm!("dsb ishst", "tlbi vmalle1is", "dsb ish", "isb") };
}

/// すべての CPU に 1 ページずつ消させるのは、これより多くのページまで (TLBI_BATCH。calibrate_tlbi が決める)
static TLBI_BATCH: AtomicUsize = AtomicUsize::new(64);

/// すべての CPU で動いてから 1 度: すべての CPU に 1 ページ消させる (tlbi vaae1is) のにかかる時間を測る。本物の CPU では
/// 1 µs もかからないので、いままでどおり 64 ページまでは 1 ページずつ (全部消すと、ASID を使わない aios では、すべての
/// CPU のすべてのプロセスの TLB を捨てる)。QEMU の TCG では 1 回ごとに全部の CPU を止めて合わせるので 40〜60 µs かかり、
/// 全部消す 1 回 (100 µs ほど) のほうが 2 ページから安い (スレッドのあるプロセスの 64 ページの munmap が 2.8 ms だった)
pub fn calibrate_tlbi() {
    const N: u64 = 8;
    let t0 = crate::timer::uptime_ns();
    for _ in 0..N {
        unsafe { core::arch::asm!("dsb ishst", "tlbi vaae1is, xzr", "dsb ish") };
    }
    let each = crate::timer::uptime_ns().saturating_sub(t0) / N;
    let batch = if each > 5_000 { 1 } else { 64 };
    TLBI_BATCH.store(batch, Ordering::Relaxed);
    println!("vm: a TLB invalidation on every CPU takes {} ns: more than {} {} at once flush the whole TLB", each, batch, if batch == 1 { "page" } else { "pages" });
}

/// 消した (無効にした) ページの TLB を消す: 少なければ 1 つずつ、多ければ全部。
/// 全部消す (vmalle1is) はすべての CPU のすべてのプロセスの TLB を捨てるので、小さな munmap のたびにはしない
fn flush_pages(vas: &[usize]) {
    if vas.is_empty() {
        return;
    }
    if vas.len() <= TLBI_BATCH.load(Ordering::Relaxed) {
        unsafe { core::arch::asm!("dsb ishst") };
        for &va in vas {
            unsafe { core::arch::asm!("tlbi vaae1is, {}", in(reg) (va >> 12) as u64) };
        }
        unsafe { core::arch::asm!("dsb ish", "isb") };
    } else {
        flush_all();
    }
}

// ---- この CPU だけの TLB ----
// aios は ASID を使わず、ページ表を載せるたび (activate) と外すたび (deactivate) にこの CPU の TLB をみな消す。
// なので、あるページ表の古い中身が TLB に残っているのは、いまそれを載せている CPU だけ。ほかの CPU が載せて
// いなければ、すべての CPU に消させる (…is: QEMU の TCG では全部の CPU を止めて合わせるので重い。Firefox の
// munmap の半分がこれだった) かわりに、この CPU の分だけ消せばよい。載せる・外すは大きなロックの中なので、
// 大きなロックを持って PTE を変えるところ (local_ok) からはずれない (ロックなしの fast_fault は flush_va_unlocked)

/// CPU ごとの、いま TTBR0 に載せているページ表 (物理アドレス。0 は何も)
static ACTIVE_ROOT: [AtomicUsize; 16] = [const { AtomicUsize::new(0) }; 16];

/// root を載せているのがこの CPU だけ (かどこにもない) か。大きなロックを持って呼ぶこと (持たないなら flush_va_unlocked)
fn local_ok(root: usize) -> bool {
    let me = crate::smp::id();
    (0..crate::smp::online()).all(|c| c == me || ACTIVE_ROOT[c].load(Ordering::Relaxed) != root)
}

fn flush_va_in(root: usize, va: usize) {
    if local_ok(root) {
        unsafe { core::arch::asm!("dsb nshst", "tlbi vaae1, {}", "dsb nsh", "isb", in(reg) (va >> 12) as u64) };
    } else {
        flush_va(va);
    }
}

/// old (変える前の PTE) が TLB に入っているかもしれないときだけ消す。無効な PTE と、アクセスフラグの落ちた PTE は
/// TLB に入らない (arm64 のきまり。Linux も新しいページで TLB を消さない): 書いたものを表を読む側に見せるだけ。
/// TCG では …is がすべての CPU を止めるので、スレッドのあるプロセス (Firefox) でページごとに消すと重い
fn flush_changed(root: usize, va: usize, old: u64) {
    if old & PTE_VALID == 0 || old & PTE_AF == 0 {
        unsafe { core::arch::asm!("dsb ishst", "isb") };
    } else {
        flush_va_in(root, va);
    }
}

/// 大きなロックなしで PTE を変えたあと (fast_fault): local_ok を、PTE を書いてから見る。載せる CPU (activate) は
/// ACTIVE_ROOT を書いてから TLB を消し (dsb) て表を読むので、ここで載せていないと見えた CPU は新しい PTE を読む。
/// 外す CPU (deactivate) は、ACTIVE_ROOT を 0 にしてから TLB を消すまで、ユーザーのアドレスに触らない
fn flush_va_unlocked(root: usize, va: usize) {
    fence(Ordering::SeqCst);
    flush_va_in(root, va);
}

fn flush_pages_in(root: usize, vas: &[usize]) {
    if vas.is_empty() {
        return;
    }
    if !local_ok(root) {
        return flush_pages(vas);
    }
    if vas.len() <= 64 {
        unsafe { core::arch::asm!("dsb nshst") };
        for &va in vas {
            unsafe { core::arch::asm!("tlbi vaae1, {}", in(reg) (va >> 12) as u64) };
        }
        unsafe { core::arch::asm!("dsb nsh", "isb") };
    } else {
        unsafe { core::arch::asm!("dsb nshst", "tlbi vmalle1", "dsb nsh", "isb") };
    }
}

fn flush_all_in(root: usize) {
    if local_ok(root) {
        unsafe { core::arch::asm!("dsb nshst", "tlbi vmalle1", "dsb nsh", "isb") };
    } else {
        flush_all();
    }
}

/// 何も写していない L1 (スケジューラの中で TTBR0 に載せる)
#[repr(C, align(4096))]
struct Empty([u64; 512]);
static EMPTY_L1: Empty = Empty([0; 512]);

/// この CPU の TTBR0 からプロセスのページ表を外す
pub fn deactivate() {
    ACTIVE_ROOT[crate::smp::id()].store(0, Ordering::Relaxed);
    unsafe {
        core::arch::asm!(
            "msr ttbr0_el1, {}",
            "isb",
            "tlbi vmalle1",
            "dsb ish",
            "isb",
            in(reg) v2p(&raw const EMPTY_L1 as usize),
        );
    }
}

impl PageTable {
    pub fn new() -> Option<Self> {
        Some(Self { root: kalloc::alloc()? as *mut u64, vmas: BTreeMap::new(), fast_users: AtomicUsize::new(0), mutators: AtomicUsize::new(0), clock: 0, hist: alloc::collections::VecDeque::new() })
    }

    /// TTBR0 に載せる物理アドレス
    pub fn root_pa(&self) -> usize {
        v2p(self.root as usize)
    }

    /// L3 の PTE へのポインタ。alloc なら途中のテーブルを作る
    fn walk(&self, va: usize, alloc: bool) -> Option<*mut u64> {
        if va >= MAXVA {
            return None;
        }
        let mut table = self.root;
        for level in 1..3 {
            let pte = unsafe { table.add(index(va, level)) };
            let e = unsafe { *pte };
            if e & PTE_VALID != 0 {
                table = table_at(e);
            } else {
                if !alloc {
                    return None;
                }
                let next = kalloc::alloc()? as *mut u64;
                // 0 にした中身が見えてから、ほかの CPU (copy_out_nofault) にたどらせる
                unsafe { core::arch::asm!("dmb ishst") };
                unsafe { *pte = v2p(next as usize) as u64 | PTE_VALID | PTE_TABLE };
                table = next;
            }
        }
        Some(unsafe { table.add(index(va, 3)) })
    }

    /// [start, end) のページを持っている PTE ごとに f (途中のテーブルがないところは飛ばす)
    fn each_pte(&self, start: usize, end: usize, mut f: impl FnMut(usize, *mut u64)) {
        self.each_slot(start, end, |va, pte| {
            if has_page(unsafe { *pte }) {
                f(va, pte);
            }
        });
    }

    /// each_pte と同じだが、スワップへ追い出したページの PTE も
    fn each_entry(&self, start: usize, end: usize, mut f: impl FnMut(usize, *mut u64)) {
        self.each_slot(start, end, |va, pte| {
            let e = unsafe { *pte };
            if has_page(e) || is_swap(e) {
                f(va, pte);
            }
        });
    }

    /// [start, end) の L3 の PTE の場所をみな (空のものも)。L3 のテーブル (2 MiB ぶん、512 個) は 1 度だけたどって、
    /// その中は並びのまま見る (ページごとに上から 4 段たどると、Firefox の大きな munmap が 1 回 0.5 ms かかった)。
    /// テーブルのないところは飛ばす: L2 のテーブルがなければ 1 GiB を、L3 のがなければ 2 MiB をまとめて (JS や wasm の
    /// 何 GiB もの予約を外すとき、2 MiB ずつたどらない)
    fn each_slot(&self, start: usize, end: usize, mut f: impl FnMut(usize, *mut u64)) {
        let mut va = pg_down(start);
        while va < end && va < MAXVA {
            let l1 = unsafe { *self.root.add(index(va, 1)) };
            if l1 & PTE_VALID == 0 {
                va = (va & !0x3fff_ffff) + 0x4000_0000;
                continue;
            }
            let chunk = va & !0x1f_ffff;
            let next = chunk + 0x20_0000;
            let l2 = unsafe { *table_at(l1).add(index(va, 2)) };
            if l2 & PTE_VALID != 0 {
                let first = table_at(l2);
                let stop = end.min(next);
                let mut i = (va - chunk) / PGSIZE;
                while chunk + i * PGSIZE < stop {
                    f(chunk + i * PGSIZE, unsafe { first.add(i) });
                    i += 1;
                }
            }
            va = next;
        }
    }

    /// ユーザーのページを 1 枚。足りなければ、ほかのアドレス空間のページをスワップへ追い出して作る
    /// (自分のページ表は借りられているので触らない)
    fn alloc_page(&self) -> Option<*mut u8> {
        kalloc::alloc().or_else(|| {
            swap::reclaim(swap::BATCH, self.root as usize);
            kalloc::alloc()
        })
    }

    // ---- 領域 ----

    /// va を含む領域 (先頭, 領域)
    pub fn find(&self, va: usize) -> Option<(usize, &Vma)> {
        self.vmas.range(..=va).next_back().filter(|(_, v)| va < v.end).map(|(&s, v)| (s, v))
    }

    /// addr で領域を 2 つに分ける (addr が領域の中なら)
    fn split(&mut self, addr: usize) {
        let Some((start, v)) = self.find(addr).map(|(s, v)| (s, v.clone())) else { return };
        if start == addr {
            return;
        }
        let mut hi = v.clone();
        match &mut hi.back {
            Backing::File { off, .. } | Backing::Pages { off, .. } => *off += addr - start,
            Backing::Anon => {}
        }
        self.vmas.get_mut(&start).unwrap().end = addr;
        self.vmas.insert(addr, hi);
    }

    /// [start, end) に写しているファイルのページのうち、写しの表にあるものの鍵 (MAP_SHARED のもの。MAP_PRIVATE のものは
    /// 穴の 0 のページ)
    fn shared_keys(&self, start: usize, end: usize) -> Vec<FileKey> {
        let mut keys = Vec::new();
        for (&s, v) in self.vmas.range(..end) {
            if v.end <= start {
                continue;
            }
            if let Backing::File { ino, off, .. } = &v.back {
                // 表を range で引く (領域のページをみな数えると、大きな MAP_SHARED があれば、munmap と exec (fork した
                // 写しを捨てる) のたびに何万もの鍵を作って 1 つずつ表を引く)
                let (fs, id) = ino.id();
                let first = (off + (s.max(start) - s)) / PGSIZE;
                let last = (off + (v.end.min(end) - PGSIZE - s)) / PGSIZE + 1;
                keys.extend(shared_file().range((fs, id, first)..(fs, id, last)).map(|(k, _)| *k));
            }
        }
        keys
    }

    /// msync: 範囲の書いたページをファイルへ (書き出す印をつけなおす)。sync なら MS_SYNC: そのファイルをディスクへ。
    /// 領域のないところがまじっていれば Err (ENOMEM。写っているところはしてから: Linux と同じ)
    /// 写したファイルを書き出す (sync なら MS_SYNC: ファイルも)。すきまがあれば ENOMEM、MS_SYNC で共有のカーネルのページ
    /// (フレームバッファ、音、TCP: Linux では fsync のないファイル) にあたれば EINVAL (Linux の vfs_fsync_range)
    pub fn msync(&self, start: usize, end: usize, sync: bool) -> Result<(), i64> {
        let mut gap = false;
        let mut einval = false;
        let mut inos: Vec<InodeRef> = Vec::new();
        let mut va = start;
        while va < end {
            let Some((s, v)) = self.find(va) else {
                gap = true;
                match self.vmas.range(va..end).next() {
                    Some((&n, _)) => {
                        va = n;
                        continue;
                    }
                    None => break,
                }
            };
            if sync && v.shared && matches!(v.back, Backing::Pages { .. }) {
                einval = true;
                break;
            }
            if v.shared && let Backing::File { ino, off, .. } = &v.back {
                let mut p = va;
                while p < v.end.min(end) {
                    let k = file_key(ino, off + (p - s));
                    if let Some(c) = shared_file().get(&k).filter(|c| c.dirty) {
                        write_back(&k, c);
                    }
                    p += PGSIZE;
                }
                if sync && !inos.iter().any(|i| i.id() == ino.id()) {
                    inos.push(ino.clone());
                }
            }
            va = v.end;
        }
        for i in inos {
            let _ = i.sync();
        }
        if einval {
            Err(-22)
        } else if gap {
            Err(-12)
        } else {
            Ok(())
        }
    }

    /// ファイル id のページ [first, last] を写しているところを外す (領域は残す。次に触れたら作りなおすか SIGBUS)。
    /// drop_file_pages から、すべてのアドレス空間に
    fn drop_file_range(&mut self, id: (usize, u64), first: usize, last: usize) {
        let _m = self.mutating();
        let mut ranges = Vec::new();
        for (&s, v) in &self.vmas {
            if let Backing::File { ino, off, .. } = &v.back
                && ino.id() == id
            {
                // 領域の中で、ファイルのページ first..=last にあたるところ
                let lo = s + (first * PGSIZE).saturating_sub(*off);
                let hi = match last.checked_add(1).and_then(|l| l.checked_mul(PGSIZE)) {
                    Some(e) if e > *off => s.saturating_add(e - off),
                    Some(_) => s,
                    None => v.end,
                };
                let (lo, hi) = (lo.max(s), hi.min(v.end));
                if lo < hi {
                    ranges.push((lo, hi));
                }
            }
        }
        if ranges.is_empty() {
            return;
        }
        let mut pages = Vec::new();
        let mut vas = Vec::new();
        for (lo, hi) in ranges {
            self.each_entry(lo, hi, |va, pte| unsafe {
                if is_swap(*pte) {
                    swap::free(slot_of(*pte));
                } else {
                    pages.push(page_of(*pte));
                    if *pte & PTE_VALID != 0 {
                        vas.push(va);
                    }
                }
                *pte = 0;
            });
        }
        flush_pages_in(self.root_pa(), &vas);
        if !vas.is_empty() {
            self.quiesce();
        }
        for p in pages {
            kalloc::put(p);
        }
    }

    /// [start, end) に領域を置く (前にあったものは外す)。中身は触れたときに作る
    pub fn map(&mut self, start: usize, end: usize, prot: u8, shared: bool, back: Backing) -> Option<()> {
        let _m = self.mutating();
        if start >= end || end > MAXVA {
            return None;
        }
        self.unmap_inner(start, end);
        self.note(b'm', start, end, prot);
        self.vmas.insert(start, Vma { end, prot, shared, back, name: None });
        // 共有の無名メモリは、fork のあとも同じページを指すように今作る
        if shared && matches!(self.vmas[&start].back, Backing::Anon) {
            let mut va = start;
            while va < end {
                self.fault_page(va, false, true).ok()?;
                va += PGSIZE;
            }
        }
        Some(())
    }

    /// [start, end) の領域とページを外す
    pub fn unmap(&mut self, start: usize, end: usize) {
        let _p = crate::prof::span("vm:unmap");
        let (s, e) = (pg_down(start), pg_up(end));
        if s < e {
            self.note(b'u', s, e, 0);
        }
        self.unmap_inner(start, end);
    }

    fn unmap_inner(&mut self, start: usize, end: usize) {
        let _m = self.mutating();
        let (start, end) = (pg_down(start), pg_up(end));
        if start >= end {
            return;
        }
        // 範囲に領域が 1 つもなければ (mmap が新しい場所に写すときはいつもそう)、することがない。
        // 領域は重ならないので、end より前で最後のものだけ見ればよい
        if !self.vmas.range(..end).next_back().is_some_and(|(_, v)| v.end > start) {
            return;
        }
        self.split(start);
        self.split(end);
        let file_keys = self.shared_keys(start, end);
        let keys: Vec<usize> = self.vmas.range(start..end).map(|(&k, _)| k).collect();
        for k in keys {
            self.vmas.remove(&k);
        }
        // PTE を消して TLB を消してから、ページを返す (ほかの CPU がまだ使っているかもしれない)
        let mut pages = Vec::new();
        let mut vas = Vec::new();
        self.each_entry(start, end, |va, pte| unsafe {
            if is_swap(*pte) {
                swap::free(slot_of(*pte));
            } else {
                pages.push(page_of(*pte));
                if *pte & PTE_VALID != 0 {
                    vas.push(va);
                }
            }
            *pte = 0;
        });
        flush_pages_in(self.root_pa(), &vas);
        if !vas.is_empty() {
            self.quiesce();
        }
        for p in pages {
            kalloc::put(p);
        }
        release_shared(&file_keys);
    }

    /// [start, end) のページを捨てる (領域は残す。次に触れたら作りなおす: MADV_DONTNEED)
    pub fn discard(&mut self, start: usize, end: usize) {
        let _m = self.mutating();
        let mut shared = Vec::new();
        for (&s, v) in self.vmas.range(..end) {
            if v.end > start && v.shared {
                shared.push((s.max(start), v.end.min(end)));
            }
        }
        let mut pages = Vec::new();
        let mut vas = Vec::new();
        self.each_entry(start, end, |va, pte| unsafe {
            // 共有のメモリは捨てない (ほかのプロセスが使っている)
            if !shared.iter().any(|&(s, e)| s <= va && va < e) {
                if is_swap(*pte) {
                    swap::free(slot_of(*pte));
                } else {
                    pages.push(page_of(*pte));
                    if *pte & PTE_VALID != 0 {
                        vas.push(va);
                    }
                }
                *pte = 0;
            }
        });
        flush_pages_in(self.root_pa(), &vas);
        if !vas.is_empty() {
            self.quiesce();
        }
        for p in pages {
            kalloc::put(p);
        }
    }

    /// mprotect。領域でないところが混じっていれば ENOMEM (Err)
    pub fn protect(&mut self, start: usize, end: usize, prot: u8) -> Result<(), ()> {
        let _m = self.mutating();
        let (start, end) = (pg_down(start), pg_up(end));
        self.note(b'p', start, end, prot);
        // すき間がないか
        let mut va = start;
        while va < end {
            let (_, v) = self.find(va).ok_or(())?;
            va = v.end;
        }
        self.split(start);
        self.split(end);
        let mut shared_at = Vec::new();
        for (&s, v) in self.vmas.range_mut(start..end) {
            // ファイルを写したところが実行できるようになる: 写っているページの命令キャッシュをあわせる (ページキャッシュの
            // ページは、実行できるように写すときにだけあわせている: file_page)
            let exec_now = prot & PROT_EXEC != 0 && v.prot & PROT_EXEC == 0 && matches!(v.back, Backing::File { .. });
            v.prot = prot;
            shared_at.push((s, v.end, v.shared, exec_now));
        }
        for (s, e, shared, exec_now) in shared_at {
            self.each_pte(s, e, |_, pte| unsafe {
                if exec_now {
                    sync_icache(page_of(*pte) as usize, PGSIZE);
                }
                *pte = remake_pte(*pte, prot, shared)
            });
        }
        // 共有のファイルのページが書けるようになった: 写っているものはもう書いたものとして扱う (書き出す印と更新時刻)。
        // 穴の 0 のページは書けないままに (書いたときにブロックをとる: fault_page)
        if prot & PROT_WRITE != 0 {
            let areas: Vec<(usize, usize, InodeRef, usize)> = self
                .vmas
                .range(start..end)
                .filter(|(_, v)| v.shared)
                .filter_map(|(&s, v)| match &v.back {
                    Backing::File { ino, off, .. } => Some((s, v.end, ino.clone(), *off)),
                    _ => None,
                })
                .collect();
            for (s, e, ino, off) in areas {
                self.each_pte(s, e, |va, pte| {
                    let foff = off + (va - s);
                    if let Some(c) = shared_file().get_mut(&file_key(&ino, foff)) {
                        if c.hole {
                            unsafe { *pte = make_pte(*pte & PTE_ADDR, ro(prot), false) };
                        } else if !c.dirty {
                            c.dirty = true;
                            if !c.own {
                                let _ = ino.page_written(foff, true);
                            }
                        }
                    }
                });
            }
        }
        flush_all_in(self.root_pa());
        self.quiesce();
        Ok(())
    }

    /// 領域の終わり (brk で伸ばす)。start から始まる領域を end まで伸ばすか縮める
    pub fn resize(&mut self, start: usize, old_end: usize, new_end: usize) -> Option<()> {
        let _m = self.mutating();
        if new_end > old_end {
            // 伸ばす先に別の領域があればだめ
            if self.vmas.range(old_end..new_end).next().is_some() {
                return None;
            }
            match self.vmas.get_mut(&start) {
                Some(v) if v.end == old_end => v.end = new_end,
                _ => {
                    self.vmas.insert(old_end, Vma { end: new_end, prot: PROT_RW, shared: false, back: Backing::Anon, name: None });
                }
            }
        } else if new_end < old_end {
            self.unmap(new_end, old_end);
        }
        Some(())
    }

    /// start から始まる領域に名前 (写したファイルのパス) をつける
    pub fn set_name(&mut self, start: usize, name: &str) {
        let _m = self.mutating();
        if let Some(v) = self.vmas.get_mut(&start) {
            v.name = Some(alloc::rc::Rc::from(name));
        }
    }

    /// va のある領域の (名前, ファイルの中の場所)
    pub fn name_at(&self, va: usize) -> Option<(alloc::rc::Rc<str>, usize)> {
        let (s, v) = self.find(va)?;
        let off = match &v.back {
            Backing::File { off, .. } => *off,
            _ => 0,
        };
        Some((v.name.clone()?, off + (va - s)))
    }

    fn note(&mut self, op: u8, start: usize, end: usize, prot: u8) {
        if self.hist.len() >= HIST {
            self.hist.pop_front();
        }
        self.hist.push_back((op, start, end, prot));
    }

    /// va のページにかかわった最近の map / protect / unmap (古い順。落ちたときの知らせ)
    pub fn hist_text(&self, va: usize) -> alloc::vec::Vec<alloc::string::String> {
        let page = pg_down(va);
        let n = self.hist.len();
        self.hist
            .iter()
            .enumerate()
            .filter(|(_, (_, s, e, _))| *s <= page && page < *e)
            .map(|(i, &(op, s, e, prot))| {
                let p = |b: u8, c: char| if prot & b != 0 { c } else { '-' };
                let what = match op {
                    b'm' => "map",
                    b'p' => "protect",
                    _ => "unmap",
                };
                let pr = if op == b'u' { alloc::string::String::new() } else { alloc::format!(" {}{}{}", p(PROT_READ, 'r'), p(PROT_WRITE, 'w'), p(PROT_EXEC, 'x')) };
                alloc::format!("-{} {} [{:#x}-{:#x}){}", n - i, what, s, e, pr)
            })
            .collect()
    }

    /// va がどの領域か (落ちたときの知らせ): 中なら「[始め-終わり) 権限 名前」、外なら上と下の近い領域からの距離
    pub fn region_text(&self, va: usize) -> alloc::string::String {
        let desc = |s: usize, v: &Vma| {
            let p = |b: u8, c: char| if v.prot & b != 0 { c } else { '-' };
            alloc::format!(
                "[{:#x}-{:#x}) {}{}{}{} {}",
                s,
                v.end,
                p(PROT_READ, 'r'),
                p(PROT_WRITE, 'w'),
                p(PROT_EXEC, 'x'),
                if v.shared { 's' } else { 'p' },
                v.name.as_deref().unwrap_or("(anon)")
            )
        };
        if let Some((s, v)) = self.find(va) {
            let guard = if v.prot & (PROT_READ | PROT_WRITE | PROT_EXEC) == 0 { " (PROT_NONE: stack guard?)" } else { "" };
            return alloc::format!("inside {}{}", desc(s, v), guard);
        }
        let below = self.vmas.range(..=va).next_back().map(|(&s, v)| alloc::format!("{:#x} above the end of {}", va - v.end, desc(s, v)));
        let above = self.vmas.range(va..).next().map(|(&s, v)| alloc::format!("{:#x} below {}", s - va, desc(s, v)));
        match (below, above) {
            (Some(b), Some(a)) => alloc::format!("unmapped: {}; {}", b, a),
            (Some(x), None) | (None, Some(x)) => alloc::format!("unmapped: {}", x),
            (None, None) => "unmapped".into(),
        }
    }

    /// /proc/PID/maps (Linux と同じ形)。パスは / から (中では先頭の / なしで持っている。
    /// VLC などは自分の行の最初の / から先を自分のディレクトリと見る)
    pub fn maps_text(&self) -> alloc::string::String {
        let mut out = alloc::string::String::new();
        for (&s, v) in self.vmas.iter() {
            let off = match &v.back {
                Backing::File { off, .. } => *off,
                _ => 0,
            };
            let r = if v.prot & PROT_READ != 0 { 'r' } else { '-' };
            let w = if v.prot & PROT_WRITE != 0 { 'w' } else { '-' };
            let x = if v.prot & PROT_EXEC != 0 { 'x' } else { '-' };
            let p = if v.shared { 's' } else { 'p' };
            let name = v.name.as_deref().map_or(alloc::string::String::new(), |n| alloc::format!("/{}", n.trim_start_matches('/')));
            out.push_str(&alloc::format!("{:08x}-{:08x} {}{}{}{} {:08x} 00:00 0 {}\n", s, v.end, r, w, x, p, off, name));
        }
        out
    }

    /// start から始まる領域の終わりを new_end まで伸ばす (その先が空いているときだけ。mremap)
    pub fn extend(&mut self, start: usize, new_end: usize) -> Option<()> {
        let _m = self.mutating();
        let old_end = self.vmas.get(&start)?.end;
        if new_end <= old_end || self.vmas.range(old_end..new_end).next().is_some() {
            return None;
        }
        self.vmas.get_mut(&start)?.end = new_end;
        Some(())
    }

    /// 次に mmap に渡せる、hint 以上で len のすき間。上の端 (MAXVA) までになければ、MMAP_BASE からさがしなおす
    /// (mmap と munmap をくりかえすプログラムが、外したところを使えないまま上の端に着いて ENOMEM にならないように。
    /// Linux はいつも空いているところからさがす)。どこにもなければ MAXVA より先 (map が断る)
    pub fn free_area(&self, hint: usize, len: usize) -> usize {
        let va = self.gap_from(hint, len);
        if va.checked_add(len).is_some_and(|e| e <= MAXVA) || hint <= crate::proc::MMAP_BASE {
            return va;
        }
        self.gap_from(crate::proc::MMAP_BASE, len)
    }

    /// [start, end) に領域がかかっているか
    pub fn overlaps(&self, start: usize, end: usize) -> bool {
        self.vmas.range(..end).next_back().is_some_and(|(_, v)| v.end > start)
    }

    /// hint 以上で len の、はじめのすき間
    fn gap_from(&self, hint: usize, len: usize) -> usize {
        let mut va = hint;
        // hint より前の領域は、hint にかかりうる最後の 1 つ (領域は重ならない) から見ればよい
        let from = self.vmas.range(..=hint).next_back().map_or(hint, |(&s, _)| s);
        for (&s, v) in self.vmas.range(from..) {
            if v.end <= va {
                continue;
            }
            if s >= va + len {
                break;
            }
            va = pg_up(v.end);
        }
        va
    }

    // ---- ページフォールト ----

    /// ユーザーが va で write / exec しようとして止まった
    pub fn fault(&mut self, va: usize, write: bool, exec: bool) -> Result<(), FaultErr> {
        crate::trace::fault(va, write);
        let (_, v) = self.find(va).ok_or(FaultErr::NoMap)?;
        let ok = if write {
            v.prot & PROT_WRITE != 0
        } else if exec {
            v.prot & PROT_EXEC != 0
        } else {
            v.prot & (PROT_READ | PROT_WRITE | PROT_EXEC) != 0
        };
        if !ok {
            return Err(FaultErr::Access);
        }
        self.fault_page(va, write, false)
    }

    /// ファイルを読むフォルトで page_va を写したあと: まど (around) の、まだ何もない PTE に、キャッシュにあるページを
    /// 書きこみ禁止で (Linux の do_fault_around。ファイルの終わりより先は写さない)。大きなロックを持って (mutating の中)
    fn map_around(&mut self, start: usize, back: &Backing, prot: u8, page_va: usize, pte: *mut u64) {
        let _p = crate::prof::span("vm:around");
        let Backing::File { ino, off, fend, .. } = back else { return };
        let end = self.find(page_va).map_or(page_va + PGSIZE, |(_, v)| v.end);
        let (lo, hi) = around(start, end, *fend, page_va);
        let size = pg_up(ino.meta().size as usize);
        let mut n = 0;
        for va in (lo..hi).step_by(PGSIZE) {
            let foff = off + (va - start);
            if foff >= size {
                break;
            }
            // 同じ PTE の表の中 (まどは表をまたがない)
            let p = unsafe { pte.offset((va as isize - page_va as isize) / PGSIZE as isize) };
            if va == page_va || unsafe { *p } != 0 {
                continue;
            }
            let Some(page) = file_page_cached(ino, foff, prot & PROT_EXEC != 0) else { continue };
            unsafe { *p = make_pte(v2p(page as usize) as u64, ro(prot), false) };
            n += 1;
        }
        if n > 0 {
            FAULT_KIND[10].0.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// MAP_SHARED のファイルを読むフォルトの fault-around (map_around と同じまど): 写しの表にあるか、ファイルシステムの
    /// キャッシュにあるページだけを、写しの表に入れて (書いたものは書けるように、まだなら書きこみ禁止で)
    fn map_around_shared(&mut self, start: usize, back: &Backing, prot: u8, page_va: usize, pte: *mut u64) {
        let _p = crate::prof::span("vm:around");
        let Backing::File { ino, off, fend, .. } = back else { return };
        let end = self.find(page_va).map_or(page_va + PGSIZE, |(_, v)| v.end);
        let (lo, hi) = around(start, end, *fend, page_va);
        let size = pg_up(ino.meta().size as usize);
        let exec = prot & PROT_EXEC != 0;
        let mut n = 0;
        for va in (lo..hi).step_by(PGSIZE) {
            let foff = off + (va - start);
            if foff >= size || foff % PGSIZE != 0 {
                break;
            }
            let p = unsafe { pte.offset((va as isize - page_va as isize) / PGSIZE as isize) };
            if va == page_va || unsafe { *p } != 0 {
                continue;
            }
            let key = file_key(ino, foff);
            let page = match shared_file().get(&key) {
                Some(c) => {
                    if exec {
                        sync_icache(c.page as usize, PGSIZE);
                    }
                    kalloc::get(c.page);
                    c.page
                }
                None => {
                    // ファイルのページそのもの (キャッシュにあるときだけ。穴やページで持たないものは、フォルトで)
                    let Ok(Some(page)) = crate::proc::noio(|| ino.page_frame(foff, false)) else { continue };
                    if exec {
                        sync_icache(page as usize, PGSIZE);
                    }
                    kalloc::get(page);
                    shared_file().insert(key, Cached { page, dirty: false, hole: false, own: false, ino: ino.clone() });
                    page
                }
            };
            let dirty = shared_file().get(&key).is_some_and(|c| c.dirty);
            unsafe { *p = make_pte(v2p(page as usize) as u64, if dirty { prot } else { ro(prot) }, false) };
            n += 1;
        }
        if n > 0 {
            FAULT_KIND[10].0.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// va のページを用意する。write なら自分だけのものにして書けるように。
    /// force はカーネルが書く (exec で読み取り専用の領域に読み込む): 権限は変えずに自分だけのものに
    fn fault_page(&mut self, va: usize, write: bool, force: bool) -> Result<(), FaultErr> {
        // 時間は SAMPLE 回に 1 回だけ時計で測って SAMPLE 倍 (大きなロックの統計と同じ。TCG では時計を読むのが重く、
        // いくつもの CPU が同時に読むと 1 つずつになる: timer の粗い時計)
        let _p = crate::prof::span("vm:fault");
        let timed = crate::smp::draw_here();
        let t0 = if timed { crate::timer::uptime_ns() } else { 0 };
        let mut kind = 0usize;
        let r = self.fault_page_inner(va, write, force, &mut kind);
        FAULT_KIND[kind].0.fetch_add(1, Ordering::Relaxed);
        if timed {
            FAULT_KIND[kind].1.fetch_add(crate::timer::uptime_ns().saturating_sub(t0) * crate::smp::SAMPLE, Ordering::Relaxed);
        }
        r
    }

    fn fault_page_inner(&mut self, va: usize, write: bool, force: bool, kind: &mut usize) -> Result<(), FaultErr> {
        let _m = self.mutating();
        *kind = 1;
        let (start, v) = self.find(va).ok_or(FaultErr::NoMap)?;
        let (prot, shared, back) = (v.prot, v.shared, v.back.clone());
        let page_va = pg_down(va);
        let pte = self.walk(page_va, true).ok_or(FaultErr::NoMem)?;
        let e = unsafe { *pte };
        if let (true, Backing::File { ino, off, fend, .. }) = (shared, &back) {
            // MAP_SHARED のファイル: ファイルのページそのもの (1 枚) をみんなで。書くときに印をつける
            *kind = 2;
            let foff = off + (page_va - start);
            let key = file_key(ino, foff);
            let wr = write || force;
            let page = if has_page(e) {
                page_of(e)
            } else {
                // mmap で写したもの (fend が MAX) は、ファイルの終わりより先のページなら SIGBUS (伸びればまた写せる)
                if *fend == usize::MAX && foff >= pg_up(ino.meta().size as usize) {
                    return Err(FaultErr::Bus);
                }
                shared_page(ino, &key, foff, prot & PROT_EXEC != 0)?
            };
            if wr {
                let (hole, dirty, own) = shared_file().get(&key).map_or((false, false, false), |c| (c.hole, c.dirty, c.own));
                if hole {
                    // 穴に書く: ブロックをとる。表の 0 のページがそのままブロックのページになる (adopt_hole)。とれなければ SIGBUS
                    match ino.page_frame(foff, true) {
                        Ok(Some(p)) => {
                            kalloc::put(p);
                            if p != page {
                                println!("vm: the block for a hole did not take the mapped page");
                            }
                            if let Some(c) = shared_file().get_mut(&key) {
                                c.hole = false;
                            }
                        }
                        _ => return Err(FaultErr::Bus),
                    }
                } else if !dirty && !own && ino.page_written(foff, true).is_err() {
                    // はじめて書く: 書き出す印と更新時刻 (Linux の page_mkwrite)
                    return Err(FaultErr::Bus);
                }
            }
            let c = shared_file().get_mut(&key).ok_or(FaultErr::NoMem)?;
            if wr {
                c.dirty = true;
            }
            let p = if c.dirty { prot } else { ro(prot) };
            unsafe { *pte = make_pte(v2p(page as usize) as u64, p, false) };
            flush_changed(self.root_pa(), page_va, e);
            if !wr && !has_page(e) {
                self.map_around_shared(start, &back, prot, page_va, pte);
            }
            return Ok(());
        }
        if let (false, Backing::File { ino, off, fend, .. }) = (shared, &back)
            && *fend == usize::MAX
            && !has_page(e)
            && !is_swap(e)
            && off + (page_va - start) >= pg_up(ino.meta().size as usize)
        {
            // mmap で写したファイル (MAP_PRIVATE) の、終わりより先のページ: SIGBUS
            return Err(FaultErr::Bus);
        }
        if let (false, Backing::Pages { pages, off, .. }) = (has_page(e), &back) {
            // カーネルのページ (フレームバッファ) をそのまま。共有で、外すときに参照を返す
            *kind = 3;
            // ページのないところ (TCP のソケットの写しなど) は SIGBUS
            let page = *pages.get((off + (page_va - start)) / PGSIZE).ok_or(FaultErr::Bus)?;
            kalloc::get(page);
            unsafe { *pte = make_pte(v2p(page as usize) as u64, prot, false) };
            flush_changed(self.root_pa(), page_va, e);
            return Ok(());
        }
        if is_swap(e) {
            // スワップから読み戻す。スロットはほかのアドレス空間 (fork) とまだ共有しているかもしれない
            *kind = 4;
            let page = self.alloc_page().ok_or(FaultErr::NoMem)?;
            if swap::read(slot_of(e), page).is_err() {
                kalloc::free(page);
                return Err(FaultErr::NoMem);
            }
            swap::free(slot_of(e));
            if prot & PROT_EXEC != 0 {
                sync_icache(page as usize, PGSIZE);
            }
            unsafe { *pte = make_pte(v2p(page as usize) as u64, prot, false) };
        } else if let (false, false, false, Backing::File { ino, off, fend, .. }) = (has_page(e), write, force, &back)
            && fend.saturating_sub(page_va) >= PGSIZE
            && let Some(page) = file_page(ino, off + (page_va - start), prot & PROT_EXEC != 0)
        {
            // 読むだけ: ページキャッシュの 1 枚を書きこみ禁止で (書いたらコピーオンライト)。まわりもキャッシュにあれば
            *kind = 5;
            unsafe { *pte = make_pte(v2p(page as usize) as u64, ro(prot), false) };
            self.map_around(start, &back, prot, page_va, pte);
        } else if !has_page(e) {
            *kind = 6;
            let page = self.alloc_page().ok_or(FaultErr::NoMem)?;
            if let Backing::File { ino, off, fend, .. } = &back {
                *kind = 7;
                let n = PGSIZE.min(fend.saturating_sub(page_va));
                if n > 0 {
                    let buf = unsafe { core::slice::from_raw_parts_mut(page, n) };
                    if ino.read_at(off + (page_va - start), buf).is_err() {
                        kalloc::free(page);
                        return Err(FaultErr::NoMem);
                    }
                }
                if prot & PROT_EXEC != 0 {
                    sync_icache(page as usize, PGSIZE);
                }
            }
            unsafe { *pte = make_pte(v2p(page as usize) as u64, prot, false) };
        } else if write || force {
            let old = page_of(e);
            if !shared && kalloc::refs(old) > 1 {
                // 共有しているので写す
                *kind = 8;
                let page = self.alloc_page().ok_or(FaultErr::NoMem)?;
                unsafe { core::ptr::copy_nonoverlapping(old, page, PGSIZE) };
                if prot & PROT_EXEC != 0 {
                    sync_icache(page as usize, PGSIZE);
                }
                unsafe { *pte = make_pte(v2p(page as usize) as u64, prot, false) };
                flush_changed(self.root_pa(), page_va, e);
                self.quiesce();
                kalloc::put(old);
                return Ok(());
            } else {
                unsafe { *pte = make_pte(e & PTE_ADDR, prot, false) };
            }
        } else {
            unsafe { *pte = remake_pte(e, prot, shared) };
        }
        flush_changed(self.root_pa(), page_va, e);
        Ok(())
    }

    // ---- カーネルからユーザー空間へ ----

    /// ユーザーの va の物理アドレス。まだページがなければ作る (write なら書けるように)
    fn user_pa(&mut self, va: usize, write: bool, force: bool) -> Option<usize> {
        let ready = |e: u64| e & PTE_VALID != 0 && e & PTE_USER != 0 && (!write || e & PTE_RDONLY == 0 || (force && kalloc::refs(page_of(e)) == 1));
        let e = self.walk(va, false).map(|p| unsafe { *p }).unwrap_or(0);
        if !ready(e) {
            if force {
                self.fault_page(va, true, true).ok()?;
            } else {
                self.fault(va, write, false).ok()?;
            }
        }
        let e = unsafe { *self.walk(va, false)? };
        if e & PTE_VALID == 0 {
            return None;
        }
        Some((e & PTE_ADDR) as usize + (va & (PGSIZE - 1)))
    }

    /// 共有の領域 (MAP_SHARED) なら va の物理アドレス。プロセスをまたぐ futex は、同じページを
    /// 違うアドレスに写していても会えるように、これで待ち合わせる
    pub fn shared_pa(&mut self, va: usize) -> Option<usize> {
        if !self.find(va)?.1.shared {
            return None;
        }
        self.user_pa(va, false, false)
    }

    /// ユーザーの 32 ビットの語 (futex) を、カーネルから原子的にさわれるところ。写して (write なら書けるように:
    /// 共有のページは写しなおす)、そろっていなければ None
    pub fn user_u32(&mut self, va: usize, write: bool) -> Option<&'static core::sync::atomic::AtomicU32> {
        if va % 4 != 0 {
            return None;
        }
        let pa = self.user_pa(va, write, false)?;
        Some(unsafe { &*(p2v(pa) as *const core::sync::atomic::AtomicU32) })
    }

    /// ユーザー空間 src から dst へコピー
    pub fn copy_in(&mut self, dst: &mut [u8], src: usize) -> Option<()> {
        self.each_chunk(src, dst.len(), false, false, |pa, off, n| unsafe {
            core::ptr::copy_nonoverlapping(p2v(pa) as *const u8, dst[off..].as_mut_ptr(), n);
        })
    }

    /// src をユーザー空間 dst へコピー (書けない領域なら None)
    pub fn copy_out(&mut self, dst: usize, src: &[u8]) -> Option<()> {
        self.each_chunk(dst, src.len(), true, false, |pa, off, n| unsafe {
            core::ptr::copy_nonoverlapping(src[off..].as_ptr(), p2v(pa) as *mut u8, n);
        })
    }

    /// 書けない領域にも書く (exec が ELF を読み込むとき)
    pub fn copy_out_force(&mut self, dst: usize, src: &[u8]) -> Option<()> {
        // ELF のコードかもしれないので、命令キャッシュも合わせる
        self.each_chunk(dst, src.len(), true, true, |pa, off, n| unsafe {
            core::ptr::copy_nonoverlapping(src[off..].as_ptr(), p2v(pa) as *mut u8, n);
            sync_icache(p2v(pa), n);
        })
    }

    fn each_chunk(&mut self, mut va: usize, len: usize, write: bool, force: bool, mut f: impl FnMut(usize, usize, usize)) -> Option<()> {
        let mut done = 0;
        while done < len {
            let pa = self.user_pa(va, write, force)?;
            let n = (PGSIZE - (va & (PGSIZE - 1))).min(len - done);
            f(pa, done, n);
            done += n;
            va += n;
        }
        Some(())
    }

    /// NUL 終端の文字列を max バイトまで読む
    pub fn copy_in_str(&mut self, mut va: usize, max: usize) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            let pa = self.user_pa(va, false, false)?;
            let n = PGSIZE - (va & (PGSIZE - 1));
            let page = unsafe { core::slice::from_raw_parts(p2v(pa) as *const u8, n) };
            if let Some(i) = page.iter().position(|&c| c == 0) {
                out.extend_from_slice(&page[..i]);
                return (out.len() <= max).then_some(out);
            }
            out.extend_from_slice(page);
            if out.len() > max {
                return None;
            }
            va += n;
        }
    }

    /// va に決まったページを写す (vDSO)。ページはカーネルも持ちつづける
    pub fn install(&mut self, va: usize, page: *mut u8, prot: u8) -> Option<()> {
        let _m = self.mutating();
        let pte = self.walk(va, true)?;
        kalloc::get(page);
        unsafe { *pte = make_pte(v2p(page as usize) as u64, prot, false) };
        Some(())
    }

    /// 大きなロックなしで、もう写っていて書けるページにだけ書く (clock_gettime などの速い道)。
    /// ページが写っていない・書けないなら false (ロックを取ってふつうの道で)
    pub fn copy_out_nofault(&self, dst: usize, src: &[u8]) -> bool {
        self.fast_users.fetch_add(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        let mut pas = [(0usize, 0usize); 2];
        let mut n = 0;
        let mut ok = true;
        let (mut va, mut done) = (dst, 0);
        while done < src.len() {
            let e = self.walk(va, false).map_or(0, |p| unsafe { core::ptr::read_volatile(p) });
            let len = (PGSIZE - (va & (PGSIZE - 1))).min(src.len() - done);
            if e & PTE_VALID == 0 || e & PTE_USER == 0 || e & PTE_RDONLY != 0 || n == pas.len() {
                ok = false;
                break;
            }
            pas[n] = ((e & PTE_ADDR) as usize + (va & (PGSIZE - 1)), len);
            n += 1;
            done += len;
            va += len;
        }
        if ok {
            let mut off = 0;
            for &(pa, len) in &pas[..n] {
                unsafe { core::ptr::copy_nonoverlapping(src[off..].as_ptr(), p2v(pa) as *mut u8, len) };
                off += len;
            }
        }
        self.fast_users.fetch_sub(1, Ordering::SeqCst);
        ok
    }

    /// 領域か PTE を変えはじめる (大きなロックを持って)。fast_fault を止めて、ロックなしで触っている CPU が
    /// いなくなるのを待つ。返ったものを持っているあいだが「変えている途中」
    fn mutating(&self) -> Mutating {
        self.mutators.fetch_add(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        self.quiesce();
        Mutating(&self.mutators)
    }

    /// 大きなロックなしのページフォルト。自分だけの (MAP_SHARED でない) 無名の領域で、
    ///   ページがまだない / 共有していて書く (コピーオンライト) / アクセスフラグが落ちている
    /// ものを、途中のテーブルがもうあるときだけ、PTE の compare-exchange で片づける。
    /// それ以外 (ファイル、スワップ、共有、テーブルがない、だれかが表を変えている途中) は false で、
    /// 呼んだほうが大きなロックを取ってふつうの道 (fault) へ。
    /// 同じ表のほかのスレッドと同時に走ってよい: PTE は compare-exchange で、負けたら false (やりなおし)
    pub fn fast_fault(&self, va: usize, write: bool, exec: bool) -> bool {
        if va >= MAXVA {
            return false;
        }
        self.fast_users.fetch_add(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        let (ok, put) = if self.mutators.load(Ordering::SeqCst) == 0 { self.fast_fault_inner(va, write, exec) } else { (false, None) };
        self.fast_users.fetch_sub(1, Ordering::SeqCst);
        if let Some(old) = put {
            // 写しとった元のページを手放す。ロックなしで元のページを読んでいる CPU がいなくなってから
            self.quiesce();
            kalloc::put(old);
        }
        ok
    }

    /// (片づいたか, あとで手放すページ)
    fn fast_fault_inner(&self, va: usize, write: bool, exec: bool) -> (bool, Option<*mut u8>) {
        use core::sync::atomic::AtomicU64;
        let Some((start, v)) = self.find(va) else { return (false, None) };
        // 無名と、MAP_PRIVATE のファイル (まだないページはページキャッシュにあるときだけ)
        if v.shared || !matches!(v.back, Backing::Anon | Backing::File { .. }) {
            return (false, None);
        }
        let prot = v.prot;
        if (write && prot & PROT_WRITE == 0) || (exec && prot & PROT_EXEC == 0) || prot & (PROT_READ | PROT_WRITE | PROT_EXEC) == 0 {
            return (false, None);
        }
        let page_va = pg_down(va);
        let Some(pte) = self.walk(page_va, false) else { return (false, None) };
        let slot = unsafe { AtomicU64::from_ptr(pte) };
        let e = slot.load(Ordering::Acquire);
        if is_swap(e) {
            return (false, None);
        }
        let mut around_file = false;
        let (new, fresh, put) = if let (false, Backing::File { ino, off, fend, ver }) = (has_page(e), &v.back) {
            // 読むだけで、ページキャッシュに同じ版があるとき (ないか書くなら大きなロックを持って読みこむ)
            let foff = off + (page_va - start);
            if fend.saturating_sub(page_va) < PGSIZE || foff % PGSIZE != 0 {
                return (false, None);
            }
            let Some(page) = cached_page(&file_key(ino, foff), *ver, !write && prot & PROT_EXEC != 0) else { return (false, None) };
            if write {
                // 書く (.data など): はじめから自分のものに写す
                let Some(mine) = kalloc::alloc() else {
                    kalloc::put(page);
                    return (false, None);
                };
                unsafe { core::ptr::copy_nonoverlapping(page, mine, PGSIZE) };
                kalloc::put(page);
                if prot & PROT_EXEC != 0 {
                    sync_icache(mine as usize, PGSIZE);
                }
                (make_pte(v2p(mine as usize) as u64, prot, false), Some(mine), None)
            } else {
                around_file = true;
                (make_pte(v2p(page as usize) as u64, ro(prot), false), Some(page), None)
            }
        } else if !has_page(e) {
            let Some(page) = kalloc::alloc() else { return (false, None) };
            (make_pte(v2p(page as usize) as u64, prot, false), Some(page), None)
        } else if write {
            if e & PTE_RDONLY == 0 && e & PTE_AF != 0 {
                // もう書ける (ほかの CPU が先に片づけた)。TLB が古いだけ
                flush_va_unlocked(self.root_pa(), page_va);
                return (true, None);
            }
            let old = page_of(e);
            if kalloc::refs(old) > 1 {
                // 共有しているので写す
                let Some(page) = kalloc::alloc() else { return (false, None) };
                unsafe { core::ptr::copy_nonoverlapping(old, page, PGSIZE) };
                (make_pte(v2p(page as usize) as u64, prot, false), Some(page), Some(old))
            } else {
                (make_pte(e & PTE_ADDR, prot, false), None, None)
            }
        } else {
            // 読む: アクセスフラグが落ちているだけ
            (remake_pte(e, prot, false), None, None)
        };
        if slot.compare_exchange(e, new, Ordering::AcqRel, Ordering::Acquire).is_err() {
            // ほかの CPU (同じ表のスレッド) が先に変えた。もらったページは返して、やりなおし
            if let Some(page) = fresh {
                // ページキャッシュのページなら参照を返すだけ (新しいページはこれで 0 になって空く)
                kalloc::put(page);
            }
            return (false, None);
        }
        // TLB に入っているかもしれない PTE を変えたときだけ消す (flush_changed。いつも …is で消していたころは、
        // ファイルを写したところのフォルトが 1 ページ 40 µs だった)
        if around_file {
            self.map_around_fast(start, v, page_va, pte);
        }
        if e & PTE_VALID == 0 || e & PTE_AF == 0 {
            unsafe { core::arch::asm!("dsb ishst", "isb") };
        } else {
            flush_va_unlocked(self.root_pa(), page_va);
        }
        (true, put)
    }

    /// fast_fault の fault-around (map_around と同じまど): まだ何もない PTE に、ページキャッシュに同じ版があるページを
    /// compare-exchange で (負けたら参照を返して次へ)。表のロックは 1 回で、まどのページを木の 1 回の range で引く
    /// (ページごとに引くと、いくつものプロセスが同時に起動するとき表のロックで並ぶ)
    fn map_around_fast(&self, start: usize, v: &Vma, page_va: usize, pte: *mut u64) {
        use core::sync::atomic::AtomicU64;
        let Backing::File { ino, off, fend, ver } = &v.back else { return };
        if off % PGSIZE != 0 {
            return;
        }
        let (lo, hi) = around(start, v.end, *fend, page_va);
        // ファイルのページの番号で [first, last)。ファイルの終わりより先は写さない
        let first = (off + (lo - start)) / PGSIZE;
        let last = ((off + (hi - start)) / PGSIZE).min(pg_up(ver.1 as usize) / PGSIZE);
        if first >= last {
            return;
        }
        let exec = v.prot & PROT_EXEC != 0;
        let (fs, id) = ino.id();
        let mut n = 0;
        let mut c = PRIV.lock();
        if c.vers.get(&(fs, id)) != Some(ver) {
            return;
        }
        for (k, p) in c.pages.range_mut((fs, id, first)..(fs, id, last)) {
            let va = start + k.2 * PGSIZE - off;
            if va == page_va {
                continue;
            }
            // 同じ PTE の表の中 (まどは表をまたがない)
            let slot = unsafe { AtomicU64::from_ptr(pte.offset((va as isize - page_va as isize) / PGSIZE as isize)) };
            if slot.load(Ordering::Acquire) != 0 {
                continue;
            }
            if exec && !p.synced {
                sync_icache(p.page as usize, PGSIZE);
                p.synced = true;
            }
            kalloc::get(p.page);
            if slot.compare_exchange(0, make_pte(v2p(p.page as usize) as u64, ro(v.prot), false), Ordering::AcqRel, Ordering::Acquire).is_err() {
                kalloc::put(p.page);
                continue;
            }
            n += 1;
        }
        drop(c);
        if n > 0 {
            FAULT_KIND[10].0.fetch_add(n, Ordering::Relaxed);
        }
    }

    /// 大きなロックなしで、もう写っているページからだけ読む (futex の速い道)。写っていなければ false
    pub fn copy_in_nofault(&self, src: usize, dst: &mut [u8]) -> bool {
        self.fast_users.fetch_add(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        let mut ok = true;
        let (mut va, mut done) = (src, 0);
        while done < dst.len() {
            let e = self.walk(va, false).map_or(0, |p| unsafe { core::ptr::read_volatile(p) });
            let len = (PGSIZE - (va & (PGSIZE - 1))).min(dst.len() - done);
            if e & PTE_VALID == 0 || e & PTE_USER == 0 {
                ok = false;
                break;
            }
            let pa = (e & PTE_ADDR) as usize + (va & (PGSIZE - 1));
            unsafe { core::ptr::copy_nonoverlapping(p2v(pa) as *const u8, dst[done..].as_mut_ptr(), len) };
            done += len;
            va += len;
        }
        self.fast_users.fetch_sub(1, Ordering::SeqCst);
        ok
    }

    /// 大きなロックなしで、va から len バイトがみな写っているか (読める) だけを見る (write_fast が、ページを書きかえる前に)
    pub fn readable_nofault(&self, va: usize, len: usize) -> bool {
        self.fast_users.fetch_add(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        let end = va.saturating_add(len);
        let mut ok = end <= MAXVA;
        let mut v = va & !(PGSIZE - 1);
        while ok && v < end {
            let e = self.walk(v, false).map_or(0, |p| unsafe { core::ptr::read_volatile(p) });
            ok = e & PTE_VALID != 0 && e & PTE_USER != 0;
            v += PGSIZE;
        }
        self.fast_users.fetch_sub(1, Ordering::SeqCst);
        ok
    }

    /// PTE を消した (TLB も消した) あと、ページを手放す前に: ロックなしで書いている CPU を待つ
    fn quiesce(&self) {
        fence(Ordering::SeqCst);
        while self.fast_users.load(Ordering::SeqCst) != 0 {
            core::hint::spin_loop();
        }
    }

    /// 同じ中身の新しいアドレス空間 (fork)。ページは写さずに共有し、書けるものは COW にする
    pub fn fork(&mut self) -> Option<PageTable> {
        let _m = self.mutating();
        let mut new = PageTable::new()?;
        new.vmas = self.vmas.clone();
        let areas: Vec<(usize, usize, u8, bool)> = self.vmas.iter().map(|(&s, v)| (s, v.end, v.prot, v.shared)).collect();
        for (s, e, prot, shared) in areas {
            let mut fail = false;
            self.each_entry(s, e, |va, pte| unsafe {
                if fail {
                    return;
                }
                let swapped = is_swap(*pte);
                if swapped {
                    // スワップのスロットも共有する (先に読み戻した方が自分のページを作る)
                    swap::dup(slot_of(*pte));
                } else {
                    kalloc::get(page_of(*pte));
                    // 共有の領域はそのまま (書いた印はページの表にある)。private は COW に
                    if !shared {
                        *pte = remake_pte(*pte, prot, false);
                    }
                }
                match new.walk(va, true) {
                    Some(np) => *np = *pte,
                    None => {
                        if swapped {
                            swap::free(slot_of(*pte));
                        } else {
                            kalloc::put(page_of(*pte));
                        }
                        fail = true;
                    }
                }
            });
            if fail {
                flush_all_in(self.root_pa());
                return None;
            }
        }
        flush_all_in(self.root_pa());
        self.quiesce();
        Some(new)
    }

    /// この CPU に載っていなければ載せる。同じアドレス空間のスレッドへの切りかえでは、TTBR0 も TLB もそのまま
    /// (Linux も同じ mm なら switch_mm しない)。載っている印は、ページ表を捨てるときに消す (Drop) ので、同じ場所に
    /// 作りなおされた別のページ表をまちがえない
    pub fn activate_lazy(&self) {
        if ACTIVE_ROOT[crate::smp::id()].load(Ordering::Relaxed) != self.root_pa() {
            self.activate();
        }
    }

    pub fn activate(&self) {
        ACTIVE_ROOT[crate::smp::id()].store(self.root_pa(), Ordering::Relaxed);
        unsafe {
            core::arch::asm!(
                "msr ttbr0_el1, {}",
                "isb",
                "tlbi vmalle1",
                "dsb ish",
                "isb",
                in(reg) self.root_pa(),
            );
        }
    }

    /// 領域の大きさの合計 (/proc の VmSize)
    pub fn vsize(&self) -> usize {
        self.vmas.iter().map(|(&s, v)| v.end - s).sum()
    }

    // ---- スワップ ----

    /// clock の針から、追い出せるページを want 枚までスワップへ書き出す。追い出した数を返す。
    /// 見たページの AF が立っていれば落とすだけ (次に回ってきたときまで触れられなければ追い出す)
    pub fn swap_out(&mut self, want: usize) -> usize {
        let _m = self.mutating();
        // 自分だけの領域を、針のところから一周
        let mut ranges = Vec::new();
        let hand = self.clock;
        for (&s, v) in &self.vmas {
            if !v.shared && v.end > hand {
                ranges.push((s.max(hand), v.end));
            }
        }
        for (&s, v) in &self.vmas {
            if !v.shared && s < hand {
                ranges.push((s, v.end.min(hand)));
            }
        }
        let mut victims = Vec::new();
        let mut aged = false;
        let mut next = 0;
        for (s, e) in ranges {
            self.each_pte(s, e, |va, pte| unsafe {
                if victims.len() >= want {
                    return;
                }
                let e = *pte;
                // PROT_NONE のものや、fork や vDSO で共有しているものは追い出さない
                if e & PTE_VALID == 0 || kalloc::refs(page_of(e)) != 1 {
                    return;
                }
                if e & PTE_AF != 0 {
                    *pte = e & !PTE_AF;
                    aged = true;
                } else {
                    victims.push((va, pte, e));
                }
                next = va + PGSIZE;
            });
            if victims.len() >= want {
                break;
            }
        }
        // 一周したら針は先頭へ
        self.clock = if victims.len() >= want { next } else { 0 };
        // PTE を外してから (ほかの CPU のスレッドがもう触れないようにして) 書き出す
        let mut out = Vec::new();
        for &(_, pte, e) in &victims {
            let Some(slot) = swap::alloc() else { break };
            unsafe { *pte = swap_pte(slot) };
            out.push((pte, e, slot));
        }
        if aged || !out.is_empty() {
            flush_all();
            self.quiesce();
        }
        let mut n = 0;
        for (pte, e, slot) in out {
            let page = page_of(e);
            if swap::write(slot, page).is_err() {
                unsafe { *pte = e };
                swap::free(slot);
                continue;
            }
            kalloc::put(page);
            n += 1;
        }
        n
    }

    /// スワップの区画 area に追い出したページを、ぜんぶ読み戻す (swapoff)。足りなければ Err
    pub fn swap_in_area(&mut self, area: usize) -> Result<(), ()> {
        let _m = self.mutating();
        let vmas: Vec<(usize, usize, u8)> = self.vmas.iter().map(|(&s, v)| (s, v.end, v.prot)).collect();
        let mut ok = true;
        for (s, e, prot) in vmas {
            self.each_entry(s, e, |_, pte| unsafe {
                let e = *pte;
                if !ok || !is_swap(e) || swap::area_of(slot_of(e)) != area {
                    return;
                }
                let Some(page) = kalloc::alloc() else {
                    ok = false;
                    return;
                };
                if swap::read(slot_of(e), page).is_err() {
                    kalloc::free(page);
                    ok = false;
                    return;
                }
                swap::free(slot_of(e));
                *pte = make_pte(v2p(page as usize) as u64, prot, false);
            });
        }
        if ok { Ok(()) } else { Err(()) }
    }

    /// ページ表のルート (スワップの回収で、自分を除くのに使う)
    pub fn id(&self) -> usize {
        self.root as usize
    }

    /// 持っているページの数 (/proc の VmRSS)
    pub fn resident(&self) -> usize {
        let mut n = 0;
        for (&s, v) in &self.vmas {
            self.each_pte(s, v.end, |_, _| n += 1);
        }
        n
    }
}

impl Drop for PageTable {
    fn drop(&mut self) {
        // どの CPU にも「載っている」と思わせない (activate_lazy が、同じ場所に作りなおされたものを載せなおすように)
        let root = self.root_pa();
        for a in ACTIVE_ROOT.iter() {
            let _ = a.compare_exchange(root, 0, Ordering::Relaxed, Ordering::Relaxed);
        }
        let _m = self.mutating();
        let file_keys = self.shared_keys(0, MAXVA);
        fn free_level(table: *mut u64, level: usize) {
            for i in 0..512 {
                let e = unsafe { *table.add(i) };
                if level < 3 {
                    if e & PTE_VALID != 0 {
                        free_level(table_at(e), level + 1);
                    }
                } else if has_page(e) {
                    kalloc::put(page_of(e));
                } else if is_swap(e) {
                    swap::free(slot_of(e));
                }
            }
            kalloc::free(table as *mut u8);
        }
        free_level(self.root, 1);
        release_shared(&file_keys);
    }
}

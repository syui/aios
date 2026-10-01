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
use core::cell::RefCell;

#[path = "htree.rs"]
mod htree;
#[path = "jbd2.rs"]
mod jbd2;

const MAGIC: u16 = 0xef53;
const ROOT_INO: u32 = 2;
/// 覚えておくブロックの数の上限 (RAM の 1/8 まで)
const CACHE_BLOCKS: usize = 16384;

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

fn crc32c(mut crc: u32, data: &[u8]) -> u32 {
    for &b in data {
        crc = CRC_TABLE[((crc ^ b as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    crc
}

/// ブロックの読み書き。1 ブロックを 1 ページに入れて覚えておく
impl Cache {
    /// 書いていないもの (メタデータもファイルの中身も) でない、いちばん古いブロックを 1 つ捨てる
    fn evict_one(&mut self) -> bool {
        let victim = self.order.iter().position(|x| !self.dirty.contains(x) && !self.data.contains(x));
        let Some(old) = victim.and_then(|i| self.order.remove(i)) else { return false };
        if let Some(p) = self.map.remove(&old) {
            kalloc::free(p);
        }
        true
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
    /// スワップに使っているファイル (書きかえさせない)
    swapfiles: RefCell<BTreeSet<u32>>,
}

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
    fn touch(&mut self) {
        let t = now_secs();
        put32(&mut self.0, 12, t); // ctime
        put32(&mut self.0, 16, t); // mtime
    }
}

fn ftype(mode: u32) -> u8 {
    match mode & S_IFMT {
        S_IFREG => 1,
        S_IFDIR => 2,
        S_IFCHR => 3,
        S_IFIFO => 5,
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
            }),
            journal: RefCell::new(None),
            last_flush: core::cell::Cell::new(0),
            users: RefCell::new(BTreeMap::new()),
            orphans: RefCell::new(BTreeSet::new()),
            swapfiles: RefCell::new(BTreeSet::new()),
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
        ExtInode::make(self, ROOT_INO)
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
        let page = match c.map.get(&b) {
            Some(&p) => p,
            None => {
                if c.map.len() >= cache_limit(self.bsize) {
                    c.evict_one();
                }
                // ページがなければ、書き終わったブロックを捨てて作る
                let p = match kalloc::alloc() {
                    Some(p) => p,
                    None if c.evict_one() => kalloc::alloc().ok_or(-ENOSPC)?,
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

    fn read_block(&self, b: u64) -> Result<Vec<u8>, i64> {
        self.with_block(b, |d| d.to_vec())
    }

    /// メタデータのブロックを書きかえる (ディスクへは flush でジャーナルを通して)
    fn modify_block<T>(&self, b: u64, f: impl FnOnce(&mut [u8]) -> T) -> Result<T, i64> {
        let r = self.with_block(b, f)?;
        let mut c = self.cache.borrow_mut();
        c.data.remove(&b);
        c.dirty.insert(b);
        Ok(r)
    }

    /// ファイルの中身のブロックを書きかえる (flush で、ジャーナルより先に直接)
    fn modify_data_block<T>(&self, b: u64, f: impl FnOnce(&mut [u8]) -> T) -> Result<T, i64> {
        let r = self.with_block(b, f)?;
        let mut c = self.cache.borrow_mut();
        c.dirty.remove(&b);
        c.data.insert(b);
        Ok(r)
    }

    /// 覚えているブロックをぜんぶ捨てる (書いていないものはないこと)
    fn drop_cache(&self) {
        let mut c = self.cache.borrow_mut();
        for (_, p) in core::mem::take(&mut c.map) {
            kalloc::free(p);
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
            for bit in start..n {
                let (byte, mask) = ((bit / 8) as usize, 1u8 << (bit % 8));
                if d[byte] & mask == 0 {
                    d[byte] |= mask;
                    return Some(bit);
                }
            }
            None
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
                self.modify_block(b, |d| d.fill(0))?;
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
        c.dirty.remove(&b);
        if let Some(p) = c.map.remove(&b) {
            kalloc::free(p);
            c.order.retain(|&x| x != b);
        }
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
            return Ok(g * self.ipg + bit + 1);
        }
        Err(-ENOSPC)
    }

    fn free_inode(&self, ino: u32, dir: bool) -> Result<(), i64> {
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
    /// たまっていれば (ジャーナルの 1/4 か 512 ブロック)、または 5 秒たっていれば書き出す。
    /// ほかは sync、暇なとき (vfs::idle_sync)、再起動のときにまとめて 1 つのトランザクションで
    fn maybe_flush(&self) -> Result<(), i64> {
        let limit = match self.journal.borrow().as_ref() {
            Some(j) => (j.capacity() / 4).min(512),
            None => 512,
        };
        let pending = {
            let c = self.cache.borrow();
            c.dirty.len() + c.data.len()
        };
        if pending >= limit || crate::timer::ticks() >= self.last_flush.get() + 5 * crate::timer::HZ {
            return self.flush();
        }
        Ok(())
    }

    pub fn flush(&self) -> Result<(), i64> {
        self.last_flush.set(crate::timer::ticks());
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
        // ファイルの中身を先に (data=ordered)
        let data: Vec<u64> = core::mem::take(&mut self.cache.borrow_mut().data).into_iter().collect();
        self.write_blocks(&data)?;
        let meta: Vec<u64> = core::mem::take(&mut self.cache.borrow_mut().dirty).into_iter().collect();
        if meta.is_empty() {
            return Ok(());
        }
        let mut j = self.journal.borrow_mut();
        if let Some(jr) = j.as_mut() {
            if self.journal_commit(jr, &meta)? {
                self.write_blocks(&meta)?;
                return self.journal_done(jr);
            }
        }
        // ジャーナルがない (ext2 など) か、1 つのトランザクションに入りきらない
        self.write_blocks(&meta)
    }

    /// ブロックをディスクの本当の場所へ。となりあうものは 1 回の要求にまとめる
    fn write_blocks(&self, dirty: &[u64]) -> Result<(), i64> {
        const RUN: usize = 64;
        let spb = (self.bsize / crate::block::SECTOR) as u64;
        let mut i = 0;
        while i < dirty.len() {
            let mut j = i + 1;
            while j < dirty.len() && j - i < RUN && dirty[j] == dirty[j - 1] + 1 {
                j += 1;
            }
            let mut buf = vec![0u8; (j - i) * self.bsize];
            for (k, &b) in dirty[i..j].iter().enumerate() {
                self.with_block(b, |d| buf[k * self.bsize..(k + 1) * self.bsize].copy_from_slice(d))?;
            }
            crate::block::write(dirty[i] * spb, &buf)?;
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
        let mut r = r.clone();
        self.set_inode_csum(ino, &mut r);
        let (b, off) = self.inode_loc(ino);
        self.modify_block(b, |d| d[off..off + r.0.len()].copy_from_slice(&r.0))
    }

    fn group_of(&self, ino: u32) -> u32 {
        (ino - 1) / self.ipg
    }

    /// ファイルの lb 番目のブロックの場所。alloc なら無いところを作る (0 は穴)
    fn map(&self, ino: u32, r: &mut Raw, lb: u64, alloc: bool) -> Result<u64, i64> {
        if r.flags() & EXTENTS_FL != 0 {
            let p = self.ext_find(r, lb)?;
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
        let mut node = r.0[40..100].to_vec();
        for _ in 0..8 {
            if u16_at(&node, 0) != EXT_MAGIC {
                return Err(-EIO);
            }
            let n = u16_at(&node, 2) as usize;
            if u16_at(&node, 6) == 0 {
                for i in 0..n {
                    let e = &node[12 + i * 12..24 + i * 12];
                    let (b, len) = (u32_at(e, 0) as u64, u16_at(e, 4) as u32);
                    let (len, uninit) = if len > EXT_INIT_MAX_LEN { (len - EXT_INIT_MAX_LEN, true) } else { (len, false) };
                    if lb >= b && lb < b + len as u64 {
                        return Ok(if uninit { 0 } else { ExtFs::ext_leaf_start(e) + (lb - b) });
                    }
                }
                return Ok(0);
            }
            let pick = (0..n).rev().find(|&i| u32_at(&node, 12 + i * 12) as u64 <= lb);
            let Some(i) = pick else { return Ok(0) };
            let child = ExtFs::ext_index_child(&node[12 + i * 12..24 + i * 12]);
            node = self.read_block(child)?;
        }
        Err(-EIO)
    }

    fn ext_init_root(r: &mut Raw) {
        let ib = r.iblock();
        ib.fill(0);
        put16(ib, 0, EXT_MAGIC);
        put16(ib, 4, 4);
        r.set_flags(r.flags() | EXTENTS_FL);
    }

    /// 葉 (node) で lb → pb をつなげられるならつなげる
    fn leaf_merge(node: &mut [u8], lb: u32, pb: u64) -> bool {
        let n = u16_at(node, 2) as usize;
        for i in 0..n {
            let e = 12 + i * 12;
            let (b, len) = (u32_at(node, e), u16_at(node, e + 4) as u32);
            if len < EXT_INIT_MAX_LEN && b + len == lb && ExtFs::ext_leaf_start(&node[e..e + 12]) + len as u64 == pb {
                put16(node, e + 4, (len + 1) as u16);
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

    fn leaf_ent(lb: u32, pb: u64) -> [u8; 12] {
        let mut e = [0u8; 12];
        put32(&mut e, 0, lb);
        put16(&mut e, 4, 1);
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

    /// node (深さ depth) の下に lb → pb を入れる。node が分かれたら (キー, 新しいブロック)
    fn insert_rec(&self, ino: u32, r: &mut Raw, node: &mut [u8], lb: u32, pb: u64) -> Result<Option<(u32, u64)>, i64> {
        if u16_at(node, 0) != EXT_MAGIC {
            return Err(-EIO);
        }
        if u16_at(node, 6) == 0 {
            if ExtFs::leaf_merge(node, lb, pb) {
                return Ok(None);
            }
            return self.put_or_split(ino, r, node, &ExtFs::leaf_ent(lb, pb));
        }
        let n = u16_at(node, 2) as usize;
        let i = (0..n).rev().find(|&i| u32_at(node, 12 + i * 12) <= lb).unwrap_or(0);
        if u32_at(node, 12 + i * 12) > lb {
            put32(node, 12 + i * 12, lb); // 一番左より前に入るので、キーを下げる
        }
        let child = ExtFs::ext_index_child(&node[12 + i * 12..24 + i * 12]);
        let mut cnode = self.read_block(child)?;
        let split = self.insert_rec(ino, r, &mut cnode, lb, pb)?;
        self.write_node(ino, r.gen_no(), child, &cnode)?;
        match split {
            None => Ok(None),
            Some((key, b)) => self.put_or_split(ino, r, node, &ExtFs::index_ent(key, b)),
        }
    }

    /// lb → pb を extents に足す。根があふれたら中身を新しいブロックへ下ろして 1 段深くする
    fn ext_insert(&self, ino: u32, r: &mut Raw, lb: u32, pb: u64) -> Result<(), i64> {
        let mut root = r.0[40..100].to_vec();
        let split = self.insert_rec(ino, r, &mut root, lb, pb)?;
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
        let mut r = self.read_inode(ino)?;
        if r.flags() & INLINE_DATA_FL != 0 {
            return Err(-EIO);
        }
        let size = r.size() as usize;
        let len = buf.len().min(size.saturating_sub(off));
        let mut done = 0;
        while done < len {
            let pos = off + done;
            let (fb, bo) = ((pos / self.bsize) as u64, pos % self.bsize);
            let n = (self.bsize - bo).min(len - done);
            let b = self.map(ino, &mut r, fb, false)?;
            if b == 0 {
                buf[done..done + n].fill(0);
            } else {
                self.with_block(b, |d| buf[done..done + n].copy_from_slice(&d[bo..bo + n]))?;
            }
            done += n;
        }
        Ok(len)
    }

    fn write_data(&self, ino: u32, r: &mut Raw, off: usize, buf: &[u8]) -> Result<usize, i64> {
        let mut done = 0;
        while done < buf.len() {
            let pos = off + done;
            let (fb, bo) = ((pos / self.bsize) as u64, pos % self.bsize);
            let n = (self.bsize - bo).min(buf.len() - done);
            let b = match self.map(ino, r, fb, true) {
                Ok(b) => b,
                Err(e) if done > 0 => {
                    let _ = e;
                    break;
                }
                Err(e) => return Err(e),
            };
            self.modify_data_block(b, |d| d[bo..bo + n].copy_from_slice(&buf[done..done + n]))?;
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

    fn find(&self, dir: u32, name: &str) -> Result<u32, i64> {
        // 索引があれば 1 つの葉だけ (決められなければ全部)
        if self.read_inode(dir)?.flags() & INDEX_FL != 0 && name != "." && name != ".." {
            match self.dx_find(dir, name) {
                Ok(Some(ino)) => return Ok(ino),
                Ok(None) => return Err(-ENOENT),
                Err(_) => {}
            }
        }
        self.dir_entries(dir)?
            .into_iter()
            .find(|e| e.2 != 0 && e.4 == name)
            .map(|e| e.2)
            .ok_or(-ENOENT)
    }

    /// ディレクトリのブロックを書きかえ、チェックサムを直す
    fn modify_dir_block(&self, dir: u32, generation: u32, b: u64, f: impl FnOnce(&mut [u8])) -> Result<(), i64> {
        self.modify_block(b, |d| {
            f(d);
            self.set_dir_csum(dir, generation, d);
        })
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
        let mut r = self.dir_writable(dir)?;
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
        for (b, o, i, rl, ename, _) in self.dir_entries(dir)? {
            let used = if i == 0 { 0 } else { rec_len_for(ename.len()) };
            if rl - used >= need {
                self.modify_dir_block(dir, generation, b, |d| {
                    if used == 0 {
                        write_entry(d, o, rl);
                    } else {
                        put16(d, o + 4, used as u16);
                        write_entry(d, o + used, rl - used);
                    }
                })?;
                return self.dir_changed(dir);
            }
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
        self.write_inode(dir, &r)?;
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
        let ents = self.dir_entries(dir)?;
        let pos = ents.iter().position(|e| e.2 != 0 && e.4 == name).ok_or(-ENOENT)?;
        let (b, o, _, rl, _, _) = ents[pos];
        let prev = pos.checked_sub(1).map(|p| &ents[p]).filter(|p| p.0 == b);
        self.modify_dir_block(dir, r.gen_no(), b, |d| match prev {
            // 前のものに吸収させる
            Some(p) => put16(d, p.1 + 4, (p.3 + rl) as u16),
            None => put32(d, o, 0),
        })?;
        self.dir_changed(dir)
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
        let dir = r.mode() & S_IFMT == S_IFDIR;
        if !is_fast_symlink(r) && r.mode() & S_IFMT != S_IFCHR {
            self.trunc_blocks(ino, r, 0)?;
        }
        put32(&mut r.0, 20, now_secs()); // dtime
        r.set_size(0);
        self.write_inode(ino, r)?;
        self.free_inode(ino, dir)
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
    fs: Rc<ExtFs>,
    ino: u32,
}

impl Drop for ExtInode {
    fn drop(&mut self) {
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
        ExtInode { fs: fs.clone(), ino }
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
        let fs = &self.fs;
        fs.check_rw()?;
        let mut r = self.raw()?;
        if r.mode() & S_IFMT != S_IFREG {
            return Err(-EINVAL);
        }
        fs.flush()?;
        let per = PGSIZE / fs.bsize;
        let spb = (fs.bsize / crate::block::SECTOR) as u64;
        let pages = r.size() / PGSIZE as u64;
        let mut out: Vec<(u64, u64, u64)> = Vec::new();
        let mut blocks = Vec::new();
        for pg in 0..pages {
            let first = fs.map(self.ino, &mut r, pg * per as u64, false)?;
            if first == 0 {
                return Err(-EINVAL);
            }
            for k in 1..per as u64 {
                if fs.map(self.ino, &mut r, pg * per as u64 + k, false)? != first + k {
                    return Err(-EINVAL);
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
                if let Some(p) = c.map.remove(&b) {
                    kalloc::free(p);
                    c.order.retain(|&x| x != b);
                }
            }
        }
        Ok(out)
    }

    /// スワップに使っている印 (swapon で true、swapoff で false)
    pub fn set_swapfile(&self, on: bool) {
        let mut s = self.fs.swapfiles.borrow_mut();
        if on {
            s.insert(self.ino);
        } else {
            s.remove(&self.ino);
        }
    }

    fn dir_only(&self) -> Result<(), i64> {
        if self.raw()?.mode() & S_IFMT != S_IFDIR {
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
        let r = f();
        let fl = self.fs.maybe_flush();
        r.and_then(|v| fl.map(|_| v))
    }

    fn create_node(&self, name: &str, mode: u32, node: NewNode) -> Result<InodeRef, i64> {
        self.dir_only()?;
        let fs = &self.fs;
        if fs.find(self.ino, name).is_ok() {
            return Err(-EEXIST);
        }
        let mode = node.type_bits() | (mode & 0o7777);
        let is_dir = matches!(node, NewNode::Dir);
        let ino = fs.alloc_inode(fs.group_of(self.ino), is_dir)?;
        let mut r = Raw(vec![0; fs.inode_size]);
        put16(&mut r.0, 0, mode as u16);
        let t = now_secs();
        for o in [8, 12, 16] {
            put32(&mut r.0, o, t);
        }
        put32(&mut r.0, 0x64, crate::rand::next() as u32); // i_generation
        if fs.inode_size > 128 {
            put16(&mut r.0, 0x80, 32); // i_extra_isize
        }
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
                fs.write_data(ino, &mut r, 0, t.as_bytes())?;
                fs.write_inode(ino, &r)?;
            }
            _ => {}
        }
        fs.add_entry(self.ino, name, ino, mode)?;
        Ok(self.at(ino))
    }

    fn unlink_node(&self, name: &str, rmdir: bool) -> Result<(), i64> {
        self.dir_only()?;
        if name == "." || name == ".." {
            return Err(-EINVAL);
        }
        let fs = &self.fs;
        let child = fs.find(self.ino, name)?;
        let mut r = fs.read_inode(child)?;
        let is_dir = r.mode() & S_IFMT == S_IFDIR;
        match (is_dir, rmdir) {
            (true, false) => return Err(-EISDIR),
            (false, true) => return Err(-ENOTDIR),
            (true, true) if !fs.is_empty_dir(child)? => return Err(-ENOTEMPTY),
            _ => {}
        }
        fs.remove_entry(self.ino, name)?;
        if is_dir {
            fs.add_links(self.ino, -1)?;
            r.set_links(0);
        } else {
            r.set_links(r.links().saturating_sub(1));
        }
        put32(&mut r.0, 12, now_secs());
        if r.links() == 0 {
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
            return Err(-EINVAL);
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

impl Inode for ExtInode {
    fn id(&self) -> (usize, u64) {
        (self.fs.fs, self.ino as u64)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn meta(&self) -> Meta {
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
        Meta {
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
        }
    }

    fn read_at(&self, off: usize, buf: &mut [u8]) -> Result<usize, i64> {
        match self.raw()?.mode() & S_IFMT {
            S_IFREG => self.fs.read_data(self.ino, off, buf),
            S_IFDIR => Err(-EISDIR),
            _ => Err(-EINVAL),
        }
    }

    fn write_at(&self, off: usize, buf: &[u8]) -> Result<usize, i64> {
        self.not_swapfile()?;
        self.write_op(|| {
            let mut r = self.raw()?;
            match r.mode() & S_IFMT {
                S_IFREG => {}
                S_IFDIR => return Err(-EISDIR),
                _ => return Err(-EINVAL),
            }
            let res = self.fs.write_data(self.ino, &mut r, off, buf);
            self.fs.write_inode(self.ino, &r)?;
            res
        })
    }

    fn truncate(&self, len: usize) -> Result<(), i64> {
        self.not_swapfile()?;
        self.write_op(|| {
            let mut r = self.raw()?;
            if r.mode() & S_IFMT != S_IFREG {
                return Err(if r.mode() & S_IFMT == S_IFDIR { -EISDIR } else { -EINVAL });
            }
            let bs = self.fs.bsize;
            if (len as u64) < r.size() {
                self.fs.trunc_blocks(self.ino, &mut r, len.div_ceil(bs) as u64)?;
                // 残ったブロックの末尾を 0 にしておく (あとで伸ばしたとき用)
                if len % bs != 0 {
                    let b = self.fs.map(self.ino, &mut r, (len / bs) as u64, false)?;
                    if b != 0 {
                        self.fs.modify_data_block(b, |d| d[len % bs..].fill(0))?;
                    }
                }
            }
            r.set_size(len as u64);
            r.touch();
            self.fs.write_inode(self.ino, &r)
        })
    }

    fn readlink(&self) -> Result<String, i64> {
        let r = self.raw()?;
        if r.mode() & S_IFMT != S_IFLNK {
            return Err(-EINVAL);
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
        self.dir_only()?;
        Ok(self.at(self.fs.find(self.ino, name)?))
    }

    fn readdir(&self) -> Result<Vec<DirEntry>, i64> {
        self.dir_only()?;
        let mut out = vec![];
        for e in self.fs.dir_entries(self.ino)? {
            if e.2 == 0 || e.4 == "." || e.4 == ".." {
                continue;
            }
            let mode = match e.5 {
                1 => S_IFREG,
                2 => S_IFDIR,
                3 => S_IFCHR,
                5 => S_IFIFO,
                7 => S_IFLNK,
                _ => self.fs.read_inode(e.2).map_or(0, |r| r.mode()),
            };
            out.push(DirEntry { name: e.4, ino: e.2 as u64, mode });
        }
        Ok(out)
    }

    fn create(&self, name: &str, mode: u32, node: NewNode) -> Result<InodeRef, i64> {
        self.write_op(|| self.create_node(name, mode, node))
    }

    fn link(&self, name: &str, target: &InodeRef) -> Result<(), i64> {
        self.write_op(|| {
            self.dir_only()?;
            let t = self.other(target)?;
            let r = self.fs.read_inode(t)?;
            if r.mode() & S_IFMT == S_IFDIR {
                return Err(-EPERM);
            }
            if self.fs.find(self.ino, name).is_ok() {
                return Err(-EEXIST);
            }
            self.fs.add_entry(self.ino, name, t, r.mode())?;
            self.fs.add_links(t, 1)?;
            Ok(())
        })
    }

    fn unlink(&self, name: &str, rmdir: bool) -> Result<(), i64> {
        self.write_op(|| self.unlink_node(name, rmdir))
    }

    fn rename(&self, old: &str, newdir: &InodeRef, new: &str) -> Result<(), i64> {
        self.write_op(|| self.rename_node(old, newdir, new))
    }

    fn set_mode(&self, mode: u32) -> Result<(), i64> {
        self.write_op(|| {
            self.update(|r| {
                let m = (r.mode() & S_IFMT) | (mode & 0o7777);
                put16(&mut r.0, 0, m as u16);
                put32(&mut r.0, 12, now_secs());
            })
        })
    }

    fn set_owner(&self, uid: Option<u32>, gid: Option<u32>) -> Result<(), i64> {
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
        self.write_op(|| self.update(|r| put32(&mut r.0, 16, (ns / 1_000_000_000) as u32)))
    }

    fn sync(&self) -> Result<(), i64> {
        self.fs.flush()
    }

    fn statfs(&self) -> [u8; 120] {
        let sb = self.fs.sb.borrow();
        let free = u32_at(&sb, 12) as u64;
        statfs_bytes(MAGIC as u64, self.fs.bsize as u64, self.fs.blocks_count(), free, u32_at(&sb, 0) as u64, u32_at(&sb, 16) as u64)
    }
}

// ext2 (rev 1)。書きかえたブロックは印をつけておき、操作の終わりにまとめて書き出す
//
// 対応: 4KiB/1KiB ブロック、直接/間接ブロック、filetype、sparse_super、large_file
// 非対応: extents (ext4)、64bit、ジャーナル、htree (書くときは索引の印を消す)
use crate::kalloc;
use crate::memlayout::PGSIZE;
use crate::tmpfs::statfs_bytes;
use crate::vfs::*;
use crate::virtio_blk;
use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;
use core::cell::RefCell;

const MAGIC: u16 = 0xef53;
const ROOT_INO: u32 = 2;
const INCOMPAT_FILETYPE: u32 = 0x2;
const INCOMPAT_SUPPORTED: u32 = INCOMPAT_FILETYPE;
const INDEX_FL: u32 = 0x1000;
const CACHE_BLOCKS: usize = 16384;

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

/// ブロックの読み書き。1 ブロックを 1 ページに入れて覚えておく
struct Cache {
    map: BTreeMap<u32, *mut u8>,
    order: VecDeque<u32>,
    /// まだディスクに書いていないブロック
    dirty: BTreeSet<u32>,
    sb_dirty: bool,
    /// まだ書いていないグループディスクリプタ表のブロック (表の中の番号)
    gdt_dirty: BTreeSet<usize>,
}

pub struct Ext2 {
    fs: usize,
    bsize: usize,
    inode_size: usize,
    ipg: u32,
    bpg: u32,
    first_data_block: u32,
    groups: u32,
    /// スーパーブロック (1024 バイト)
    sb: RefCell<Vec<u8>>,
    /// グループディスクリプタ表
    gdt: RefCell<Vec<u8>>,
    cache: RefCell<Cache>,
}

/// ディスク上の inode (先頭 128 バイトだけ扱う)
#[derive(Clone)]
struct Raw([u8; 128]);

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
    fn blocks512(&self) -> u32 {
        u32_at(&self.0, 28)
    }
    fn set_blocks512(&mut self, n: u32) {
        put32(&mut self.0, 28, n);
    }
    fn flags(&self) -> u32 {
        u32_at(&self.0, 32)
    }
    fn block(&self, i: usize) -> u32 {
        u32_at(&self.0, 40 + i * 4)
    }
    fn set_block(&mut self, i: usize, v: u32) {
        put32(&mut self.0, 40 + i * 4, v);
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

impl Ext2 {
    /// ディスクの先頭にある ext2 を開く
    pub fn mount() -> Result<Rc<Ext2>, &'static str> {
        let mut sb = vec![0u8; 1024];
        virtio_blk::read(2, &mut sb).map_err(|_| "read error")?;
        if u16_at(&sb, 56) != MAGIC {
            return Err("not ext2");
        }
        let incompat = u32_at(&sb, 96);
        if incompat & !INCOMPAT_SUPPORTED != 0 {
            return Err("unsupported features (ext3/ext4?)");
        }
        let bsize = 1024usize << u32_at(&sb, 24);
        if bsize > PGSIZE {
            return Err("block size too large");
        }
        let rev = u32_at(&sb, 76);
        let inode_size = if rev == 0 { 128 } else { u16_at(&sb, 88) as usize };
        let blocks = u32_at(&sb, 4);
        let first_data_block = u32_at(&sb, 20);
        let bpg = u32_at(&sb, 32);
        let ipg = u32_at(&sb, 40);
        let groups = (blocks - first_data_block).div_ceil(bpg);
        let fs = Rc::new(Ext2 {
            fs: new_fs_id(),
            bsize,
            inode_size,
            ipg,
            bpg,
            first_data_block,
            groups,
            sb: RefCell::new(sb),
            gdt: RefCell::new(Vec::new()),
            cache: RefCell::new(Cache { map: BTreeMap::new(), order: VecDeque::new(), dirty: BTreeSet::new(), sb_dirty: false, gdt_dirty: BTreeSet::new() }),
        });
        let gdt_len = groups as usize * 32;
        let mut gdt = vec![0u8; gdt_len.div_ceil(bsize) * bsize];
        for (i, chunk) in gdt.chunks_mut(bsize).enumerate() {
            chunk.copy_from_slice(&fs.read_block(first_data_block + 1 + i as u32).map_err(|_| "read error")?);
        }
        *fs.gdt.borrow_mut() = gdt;
        Ok(fs)
    }

    pub fn root(self: &Rc<Self>) -> InodeRef {
        Rc::new(Ext2Inode { fs: self.clone(), ino: ROOT_INO })
    }

    // ---- ブロック ----

    fn with_block<T>(&self, b: u32, f: impl FnOnce(&mut [u8]) -> T) -> Result<T, i64> {
        let mut c = self.cache.borrow_mut();
        let page = match c.map.get(&b) {
            Some(&p) => p,
            None => {
                if c.map.len() >= CACHE_BLOCKS {
                    // 書いていないものは追い出さない
                    let victim = c.order.iter().position(|b| !c.dirty.contains(b));
                    if let Some(old) = victim.and_then(|i| c.order.remove(i)) {
                        if let Some(p) = c.map.remove(&old) {
                            kalloc::free(p);
                        }
                    }
                }
                let p = kalloc::alloc().ok_or(-ENOSPC)?;
                let buf = unsafe { core::slice::from_raw_parts_mut(p, self.bsize) };
                if virtio_blk::read(b as u64 * (self.bsize / virtio_blk::SECTOR) as u64, buf).is_err() {
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

    fn read_block(&self, b: u32) -> Result<Vec<u8>, i64> {
        self.with_block(b, |d| d.to_vec())
    }

    /// ブロックを書きかえる (ディスクへは flush で)
    fn modify_block<T>(&self, b: u32, f: impl FnOnce(&mut [u8]) -> T) -> Result<T, i64> {
        let r = self.with_block(b, f)?;
        self.cache.borrow_mut().dirty.insert(b);
        Ok(r)
    }

    fn write_sb(&self) -> Result<(), i64> {
        self.cache.borrow_mut().sb_dirty = true;
        Ok(())
    }

    fn write_gdt(&self, g: u32) -> Result<(), i64> {
        self.cache.borrow_mut().gdt_dirty.insert(g as usize * 32 / self.bsize);
        Ok(())
    }

    /// 書きかえたものをディスクへ。となりあうブロックは 1 回の要求にまとめる
    pub fn flush(&self) -> Result<(), i64> {
        let (sb_dirty, gdt_dirty) = {
            let mut c = self.cache.borrow_mut();
            (core::mem::take(&mut c.sb_dirty), core::mem::take(&mut c.gdt_dirty))
        };
        for blk in gdt_dirty {
            let data = self.gdt.borrow()[blk * self.bsize..(blk + 1) * self.bsize].to_vec();
            self.modify_block(self.first_data_block + 1 + blk as u32, |d| d.copy_from_slice(&data))?;
        }
        if sb_dirty {
            let sb = self.sb.borrow().clone();
            virtio_blk::write(2, &sb)?;
        }
        let dirty: Vec<u32> = core::mem::take(&mut self.cache.borrow_mut().dirty).into_iter().collect();
        const RUN: usize = 64;
        let spb = (self.bsize / virtio_blk::SECTOR) as u64;
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
            virtio_blk::write(dirty[i] as u64 * spb, &buf)?;
            i = j;
        }
        Ok(())
    }

    fn gd(&self, g: u32, field: usize) -> u32 {
        let gdt = self.gdt.borrow();
        let o = g as usize * 32 + field;
        if field >= 12 { u16_at(&gdt, o) as u32 } else { u32_at(&gdt, o) }
    }

    fn gd_add(&self, g: u32, field: usize, delta: i32) {
        let mut gdt = self.gdt.borrow_mut();
        let o = g as usize * 32 + field;
        let v = u16_at(&gdt, o) as i32 + delta;
        put16(&mut gdt, o, v as u16);
    }

    fn sb_add(&self, field: usize, delta: i32) {
        let mut sb = self.sb.borrow_mut();
        let v = u32_at(&sb, field) as i64 + delta as i64;
        put32(&mut sb, field, v as u32);
    }

    /// ビットマップから空きを 1 つ取る。見つけた (グループ, ビット)
    fn take_bit(&self, bitmap_field: usize, free_field: usize, per_group: u32, limit: impl Fn(u32, u32) -> bool, goal: u32) -> Result<(u32, u32), i64> {
        for k in 0..self.groups {
            let g = (goal + k) % self.groups;
            if self.gd(g, free_field) == 0 {
                continue;
            }
            let bm = self.gd(g, bitmap_field);
            let found = self.modify_block(bm, |d| {
                for bit in 0..per_group {
                    if !limit(g, bit) {
                        break;
                    }
                    let (byte, mask) = ((bit / 8) as usize, 1u8 << (bit % 8));
                    if d[byte] & mask == 0 {
                        d[byte] |= mask;
                        return Some(bit);
                    }
                }
                None
            })?;
            if let Some(bit) = found {
                return Ok((g, bit));
            }
        }
        Err(-ENOSPC)
    }

    fn alloc_block(&self, goal_group: u32) -> Result<u32, i64> {
        let blocks = u32_at(&self.sb.borrow(), 4);
        let (fdb, bpg) = (self.first_data_block, self.bpg);
        let (g, bit) = self.take_bit(0, 12, bpg, |g, bit| fdb + g * bpg + bit < blocks, goal_group)?;
        self.gd_add(g, 12, -1);
        self.sb_add(12, -1);
        self.write_gdt(g)?;
        self.write_sb()?;
        let b = fdb + g * bpg + bit;
        self.modify_block(b, |d| d.fill(0))?;
        Ok(b)
    }

    fn free_block(&self, b: u32) -> Result<(), i64> {
        let rel = b - self.first_data_block;
        let (g, bit) = (rel / self.bpg, rel % self.bpg);
        let bm = self.gd(g, 0);
        self.modify_block(bm, |d| d[(bit / 8) as usize] &= !(1 << (bit % 8)))?;
        self.gd_add(g, 12, 1);
        self.sb_add(12, 1);
        self.write_gdt(g)?;
        self.write_sb()?;
        let mut c = self.cache.borrow_mut();
        c.dirty.remove(&b);
        if let Some(p) = c.map.remove(&b) {
            kalloc::free(p);
            c.order.retain(|&x| x != b);
        }
        Ok(())
    }

    fn alloc_inode(&self, goal_group: u32, dir: bool) -> Result<u32, i64> {
        let first_ino = u32_at(&self.sb.borrow(), 84).max(11);
        let (ipg, total) = (self.ipg, u32_at(&self.sb.borrow(), 0));
        let (g, bit) = self.take_bit(4, 14, ipg, |g, bit| g * ipg + bit < total, goal_group)?;
        let ino = g * ipg + bit + 1;
        if ino < first_ino {
            // 予約された inode は使わない (ビットは立ったままで良い)
            return self.alloc_inode(goal_group, dir);
        }
        self.gd_add(g, 14, -1);
        if dir {
            self.gd_add(g, 16, 1);
        }
        self.sb_add(16, -1);
        self.write_gdt(g)?;
        self.write_sb()?;
        Ok(ino)
    }

    fn free_inode(&self, ino: u32, dir: bool) -> Result<(), i64> {
        let (g, bit) = ((ino - 1) / self.ipg, (ino - 1) % self.ipg);
        let bm = self.gd(g, 4);
        self.modify_block(bm, |d| d[(bit / 8) as usize] &= !(1 << (bit % 8)))?;
        self.gd_add(g, 14, 1);
        if dir {
            self.gd_add(g, 16, -1);
        }
        self.sb_add(16, 1);
        self.write_gdt(g)?;
        self.write_sb()
    }

    // ---- inode ----

    fn inode_loc(&self, ino: u32) -> (u32, usize) {
        let (g, idx) = ((ino - 1) / self.ipg, (ino - 1) % self.ipg);
        let byte = idx as usize * self.inode_size;
        (self.gd(g, 8) + (byte / self.bsize) as u32, byte % self.bsize)
    }

    fn read_inode(&self, ino: u32) -> Result<Raw, i64> {
        let (b, off) = self.inode_loc(ino);
        self.with_block(b, |d| Raw(d[off..off + 128].try_into().unwrap()))
    }

    fn write_inode(&self, ino: u32, r: &Raw) -> Result<(), i64> {
        let (b, off) = self.inode_loc(ino);
        self.modify_block(b, |d| d[off..off + 128].copy_from_slice(&r.0))
    }

    fn group_of(&self, ino: u32) -> u32 {
        (ino - 1) / self.ipg
    }

    /// ファイルの fb 番目のブロックの場所。alloc なら無いところを作る (0 は穴)
    fn bmap(&self, ino: u32, r: &mut Raw, fb: u64, alloc: bool) -> Result<u32, i64> {
        let n = (self.bsize / 4) as u64;
        let goal = self.group_of(ino);
        let per = (self.bsize / 512) as u32;
        let fresh = |r: &mut Raw| -> Result<u32, i64> {
            let b = self.alloc_block(goal)?;
            r.set_blocks512(r.blocks512() + per);
            Ok(b)
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
            return Err(-EINVAL);
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
            let mut next = self.with_block(b, |d| u32_at(d, i * 4))?;
            if next == 0 {
                if !alloc {
                    return Ok(0);
                }
                next = fresh(r)?;
                self.modify_block(b, |d| put32(d, i * 4, next))?;
            }
            b = next;
        }
        Ok(b)
    }

    /// keep 番目以降のブロックを外す
    fn trunc_blocks(&self, r: &mut Raw, keep: u64) -> Result<(), i64> {
        let n = (self.bsize / 4) as u64;
        let per = (self.bsize / 512) as u32;
        for i in 0..12 {
            let b = r.block(i);
            if b != 0 && i as u64 >= keep {
                self.free_block(b)?;
                r.set_block(i, 0);
                r.set_blocks512(r.blocks512().saturating_sub(per));
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
        let per = (self.bsize / 512) as u32;
        if blk == 0 {
            return Ok(true);
        }
        let span = n.pow(level);
        if first + span <= keep {
            return Ok(false);
        }
        if level > 0 {
            let child_span = n.pow(level - 1);
            let ptrs = self.with_block(blk, |d| (0..n as usize).map(|i| u32_at(d, i * 4)).collect::<Vec<_>>())?;
            for (i, c) in ptrs.into_iter().enumerate() {
                if c != 0 && self.trunc_level(r, c, level - 1, first + i as u64 * child_span, keep)? {
                    self.modify_block(blk, |d| put32(d, i * 4, 0))?;
                }
            }
        }
        if first >= keep {
            self.free_block(blk)?;
            r.set_blocks512(r.blocks512().saturating_sub(per));
            return Ok(true);
        }
        Ok(false)
    }

    fn read_data(&self, ino: u32, off: usize, buf: &mut [u8]) -> Result<usize, i64> {
        let mut r = self.read_inode(ino)?;
        let size = r.size() as usize;
        let len = buf.len().min(size.saturating_sub(off));
        let mut done = 0;
        while done < len {
            let pos = off + done;
            let (fb, bo) = ((pos / self.bsize) as u64, pos % self.bsize);
            let n = (self.bsize - bo).min(len - done);
            let b = self.bmap(ino, &mut r, fb, false)?;
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
            let b = self.bmap(ino, r, fb, true)?;
            self.modify_block(b, |d| d[bo..bo + n].copy_from_slice(&buf[done..done + n]))?;
            done += n;
        }
        if (off + done) as u64 > r.size() {
            r.set_size((off + done) as u64);
        }
        r.touch();
        Ok(done)
    }

    // ---- ディレクトリ ----

    /// (ブロック番号, ブロック内の位置, inode, rec_len, name) を順に
    fn dir_entries(&self, ino: u32) -> Result<Vec<(u32, usize, u32, usize, String, u8)>, i64> {
        let mut r = self.read_inode(ino)?;
        let nblocks = r.size().div_ceil(self.bsize as u64);
        let mut out = vec![];
        for fb in 0..nblocks {
            let b = self.bmap(ino, &mut r, fb, false)?;
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
                    let name = String::from_utf8_lossy(&d[o + 8..o + 8 + nl.min(rl - 8)]).to_string();
                    out.push((b, o, i, rl, name, ft));
                    o += rl;
                }
            })?;
        }
        Ok(out)
    }

    fn find(&self, dir: u32, name: &str) -> Result<u32, i64> {
        self.dir_entries(dir)?
            .into_iter()
            .find(|e| e.2 != 0 && e.4 == name)
            .map(|e| e.2)
            .ok_or(-ENOENT)
    }

    fn add_entry(&self, dir: u32, name: &str, ino: u32, mode: u32) -> Result<(), i64> {
        if name.len() > 255 {
            return Err(-ENAMETOOLONG);
        }
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
                self.modify_block(b, |d| {
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
        // 新しいブロックを足す
        let mut r = self.read_inode(dir)?;
        let fb = r.size() / self.bsize as u64;
        let b = self.bmap(dir, &mut r, fb, true)?;
        let bsize = self.bsize;
        self.modify_block(b, |d| write_entry(d, 0, bsize))?;
        r.set_size(r.size() + self.bsize as u64);
        self.write_inode(dir, &r)?;
        self.dir_changed(dir)
    }

    fn remove_entry(&self, dir: u32, name: &str) -> Result<(), i64> {
        let ents = self.dir_entries(dir)?;
        let pos = ents.iter().position(|e| e.2 != 0 && e.4 == name).ok_or(-ENOENT)?;
        let (b, o, _, rl, _, _) = ents[pos];
        let prev = pos.checked_sub(1).map(|p| &ents[p]).filter(|p| p.0 == b);
        self.modify_block(b, |d| match prev {
            // 前のものに吸収させる
            Some(p) => put16(d, p.1 + 4, (p.3 + rl) as u16),
            None => put32(d, o, 0),
        })?;
        self.dir_changed(dir)
    }

    /// 中身が変わったので時刻を進め、htree の印を消す
    fn dir_changed(&self, dir: u32) -> Result<(), i64> {
        let mut r = self.read_inode(dir)?;
        r.touch();
        if r.flags() & INDEX_FL != 0 {
            let flags = r.flags() & !INDEX_FL;
            put32(&mut r.0, 32, flags);
        }
        self.write_inode(dir, &r)
    }

    fn set_dotdot(&self, dir: u32, parent: u32) -> Result<(), i64> {
        let ents = self.dir_entries(dir)?;
        let e = ents.iter().find(|e| e.4 == "..").ok_or(-EIO)?;
        self.modify_block(e.0, |d| put32(d, e.1, parent))
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
            self.trunc_blocks(r, 0)?;
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

fn is_fast_symlink(r: &Raw) -> bool {
    r.mode() & S_IFMT == S_IFLNK && r.size() < 60 && r.blocks512() == 0
}

pub struct Ext2Inode {
    fs: Rc<Ext2>,
    ino: u32,
}

impl Ext2Inode {
    fn set_mtime_inner(&self, ns: u64) -> Result<(), i64> {
        self.update(|r| put32(&mut r.0, 16, (ns / 1_000_000_000) as u32))
    }

    fn set_owner_inner(&self, uid: Option<u32>, gid: Option<u32>) -> Result<(), i64> {
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
    }

    fn set_mode_inner(&self, mode: u32) -> Result<(), i64> {
        self.update(|r| {
            let m = (r.mode() & S_IFMT) | (mode & 0o7777);
            put16(&mut r.0, 0, m as u16);
            put32(&mut r.0, 12, now_secs());
        })
    }

    fn rename_inner(&self, old: &str, newdir: &InodeRef, new: &str) -> Result<(), i64> {
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
            Ext2Inode { fs: fs.clone(), ino: nd }.unlink(new, is_dir)?;
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

    fn unlink_inner(&self, name: &str, rmdir: bool) -> Result<(), i64> {
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
            fs.release(child, &mut r)
        } else {
            fs.write_inode(child, &r)
        }
    }

    fn link_inner(&self, name: &str, target: &InodeRef) -> Result<(), i64> {
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
    }

    fn create_inner(&self, name: &str, mode: u32, node: NewNode) -> Result<InodeRef, i64> {
        self.dir_only()?;
        if self.fs.find(self.ino, name).is_ok() {
            return Err(-EEXIST);
        }
        let fs = &self.fs;
        let mode = node.type_bits() | (mode & 0o7777);
        let is_dir = matches!(node, NewNode::Dir);
        let ino = fs.alloc_inode(fs.group_of(self.ino), is_dir)?;
        let mut r = Raw([0; 128]);
        put16(&mut r.0, 0, mode as u16);
        let t = now_secs();
        for o in [8, 12, 16] {
            put32(&mut r.0, o, t);
        }
        r.set_links(if is_dir { 2 } else { 1 });
        match &node {
            NewNode::Dev(ma, mi) => r.set_block(0, (ma << 8) | mi),
            NewNode::Symlink(t) if t.len() < 60 => {
                r.0[40..40 + t.len()].copy_from_slice(t.as_bytes());
                r.set_size(t.len() as u64);
            }
            _ => {}
        }
        // inode 表には 128 バイトより大きい場合の残りもあるので 0 にしてから書く
        let (b, off) = fs.inode_loc(ino);
        let isz = fs.inode_size;
        fs.modify_block(b, |d| d[off..off + isz].fill(0))?;
        fs.write_inode(ino, &r)?;
        match &node {
            NewNode::Dir => {
                let blk = fs.bmap(ino, &mut r, 0, true)?;
                let bsize = fs.bsize;
                let me = self.ino;
                fs.modify_block(blk, |d| {
                    put32(d, 0, ino);
                    put16(d, 4, 12);
                    d[6] = 1;
                    d[7] = 2;
                    d[8] = b'.';
                    put32(d, 12, me);
                    put16(d, 16, (bsize - 12) as u16);
                    d[18] = 2;
                    d[19] = 2;
                    d[20..22].copy_from_slice(b"..");
                })?;
                r.set_size(fs.bsize as u64);
                fs.write_inode(ino, &r)?;
                fs.add_links(self.ino, 1)?;
            }
            NewNode::Symlink(t) if t.len() >= 60 => {
                fs.write_data(ino, &mut r, 0, t.as_bytes())?;
                fs.write_inode(ino, &r)?;
            }
            _ => {}
        }
        fs.add_entry(self.ino, name, ino, mode)?;
        Ok(self.at(ino))
    }

    fn truncate_inner(&self, len: usize) -> Result<(), i64> {
        let mut r = self.raw()?;
        if r.mode() & S_IFMT != S_IFREG {
            return Err(if r.mode() & S_IFMT == S_IFDIR { -EISDIR } else { -EINVAL });
        }
        let bs = self.fs.bsize;
        if (len as u64) < r.size() {
            self.fs.trunc_blocks(&mut r, len.div_ceil(bs) as u64)?;
            // 残ったブロックの末尾を 0 にしておく (あとで伸ばしたとき用)
            if len % bs != 0 {
                let b = self.fs.bmap(self.ino, &mut r, (len / bs) as u64, false)?;
                if b != 0 {
                    self.fs.modify_block(b, |d| d[len % bs..].fill(0))?;
                }
            }
        }
        r.set_size(len as u64);
        r.touch();
        self.fs.write_inode(self.ino, &r)
    }

    fn write_at_inner(&self, off: usize, buf: &[u8]) -> Result<usize, i64> {
        let mut r = self.raw()?;
        match r.mode() & S_IFMT {
            S_IFREG => {}
            S_IFDIR => return Err(-EISDIR),
            _ => return Err(-EINVAL),
        }
        let res = self.fs.write_data(self.ino, &mut r, off, buf);
        self.fs.write_inode(self.ino, &r)?;
        res
    }

    fn at(&self, ino: u32) -> InodeRef {
        Rc::new(Ext2Inode { fs: self.fs.clone(), ino })
    }

    fn other(&self, i: &InodeRef) -> Result<u32, i64> {
        let o = i.as_any().downcast_ref::<Ext2Inode>().ok_or(-EXDEV)?;
        if o.fs.fs != self.fs.fs {
            return Err(-EXDEV);
        }
        Ok(o.ino)
    }

    fn raw(&self) -> Result<Raw, i64> {
        self.fs.read_inode(self.ino)
    }

    fn dir_only(&self) -> Result<(), i64> {
        if self.raw()?.mode() & S_IFMT != S_IFDIR {
            return Err(-ENOTDIR);
        }
        Ok(())
    }

    fn update(&self, f: impl FnOnce(&mut Raw)) -> Result<(), i64> {
        let mut r = self.raw()?;
        f(&mut r);
        self.fs.write_inode(self.ino, &r)
    }
}

impl Inode for Ext2Inode {
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
        let rdev = if r.mode() & S_IFMT == S_IFCHR {
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
            blocks: r.blocks512() as u64,
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
        let r = self.write_at_inner(off, buf);
        let f = self.fs.flush();
        r.and_then(|v| f.map(|_| v))
    }

    fn truncate(&self, len: usize) -> Result<(), i64> {
        let r = self.truncate_inner(len);
        let f = self.fs.flush();
        r.and_then(|v| f.map(|_| v))
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
        let r = self.create_inner(name, mode, node);
        let f = self.fs.flush();
        r.and_then(|v| f.map(|_| v))
    }

    fn link(&self, name: &str, target: &InodeRef) -> Result<(), i64> {
        let r = self.link_inner(name, target);
        let f = self.fs.flush();
        r.and_then(|v| f.map(|_| v))
    }

    fn unlink(&self, name: &str, rmdir: bool) -> Result<(), i64> {
        let r = self.unlink_inner(name, rmdir);
        let f = self.fs.flush();
        r.and_then(|v| f.map(|_| v))
    }

    fn rename(&self, old: &str, newdir: &InodeRef, new: &str) -> Result<(), i64> {
        let r = self.rename_inner(old, newdir, new);
        let f = self.fs.flush();
        r.and_then(|v| f.map(|_| v))
    }

    fn set_mode(&self, mode: u32) -> Result<(), i64> {
        let r = self.set_mode_inner(mode);
        let f = self.fs.flush();
        r.and_then(|v| f.map(|_| v))
    }

    fn set_owner(&self, uid: Option<u32>, gid: Option<u32>) -> Result<(), i64> {
        let r = self.set_owner_inner(uid, gid);
        let f = self.fs.flush();
        r.and_then(|v| f.map(|_| v))
    }

    fn set_mtime(&self, ns: u64) -> Result<(), i64> {
        let r = self.set_mtime_inner(ns);
        let f = self.fs.flush();
        r.and_then(|v| f.map(|_| v))
    }

    fn statfs(&self) -> [u8; 120] {
        let sb = self.fs.sb.borrow();
        statfs_bytes(MAGIC as u64, self.fs.bsize as u64, u32_at(&sb, 4) as u64, u32_at(&sb, 12) as u64, u32_at(&sb, 0) as u64, u32_at(&sb, 16) as u64)
    }
}

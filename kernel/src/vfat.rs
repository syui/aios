// FAT16 / FAT32 (vfat)。ESP やラズパイの boot の区画を /boot に入れて、aipkg で
// カーネル (/boot/Image) を入れかえられるようにする
//
// 長い名前 (LFN) を読み書きする。8.3 に収まる名前は短い名前だけにし、小文字は
// NT の小文字の印 (Linux の vfat や Windows と同じ) で表す。
// 持ち主やモードはないので、ディレクトリは 0755、ファイルは 0755 (実行もできるように) に見せる。
// FAT (クラスタの表) はメモリに持ち、書きかえたセクタを操作の終わりにすべての写しに書く。
// FAT32 の FSInfo (空きの数と次の空き) も FAT を書くときに書きなおす。
use crate::block::{self, Part, SECTOR};
use crate::tmpfs::statfs_bytes;
use crate::vfs::*;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::any::Any;
use core::cell::RefCell;

const ATTR_RO: u8 = 0x01;
const ATTR_VOLUME: u8 = 0x08;
const ATTR_DIR: u8 = 0x10;
const ATTR_ARCHIVE: u8 = 0x20;
const ATTR_LFN: u8 = 0x0f;
const LOWER_BASE: u8 = 0x08;
const LOWER_EXT: u8 = 0x10;
const MSDOS_SUPER_MAGIC: u64 = 0x4d44;

pub struct FatFs {
    id: usize,
    part: Part,
    fat32: bool,
    /// クラスタの大きさ (バイト)
    csize: usize,
    /// セクタの番号 (区画の中)
    fat_start: u64,
    fat_sectors: u64,
    nfats: u64,
    /// FAT16 のルートディレクトリ (決まった場所) と、そのエントリ数
    root_start: u64,
    root_entries: usize,
    data_start: u64,
    clusters: u32,
    root_cluster: u32,
    fat: RefCell<Vec<u8>>,
    /// 次に空きを探し始めるクラスタ
    hint: RefCell<u32>,
    /// 書きかえて、まだディスクに書いていない FAT のセクタ
    dirty: RefCell<BTreeSet<u64>>,
    /// 鎖の覚え (最初のクラスタ → 鎖)。大きいファイルを何度もたどらないように
    chains: RefCell<BTreeMap<u32, Rc<Vec<u32>>>>,
    /// FAT32 の FSInfo のセクタ (0 はなし)
    info: u64,
}

/// 1 回に読み書きする大きさの上限
const IO_MAX: usize = 64 * 1024;

/// ディレクトリの中のエントリの場所: (ディレクトリの最初のクラスタ (0 は FAT16 のルート), 何番目か)
#[derive(Clone, Copy, PartialEq)]
struct Loc {
    dir: u32,
    idx: usize,
}

pub struct FatNode {
    fs: Rc<FatFs>,
    /// None はルート
    loc: Option<Loc>,
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

impl FatFs {
    /// 区画 p の FAT を開く
    pub fn mount(p: Part) -> Result<Rc<FatFs>, &'static str> {
        let mut bs = vec![0u8; SECTOR];
        block::read_part(&p, 0, &mut bs).map_err(|_| "read error")?;
        if bs[510] != 0x55 || bs[511] != 0xaa || u16_at(&bs, 11) as usize != SECTOR {
            return Err("not FAT");
        }
        let spc = bs[13] as usize;
        let reserved = u16_at(&bs, 14) as u64;
        let nfats = bs[16] as u64;
        let root_entries = u16_at(&bs, 17) as usize;
        let total = match u16_at(&bs, 19) {
            0 => u32_at(&bs, 32) as u64,
            n => n as u64,
        };
        let fat_sectors = match u16_at(&bs, 22) {
            0 => u32_at(&bs, 36) as u64,
            n => n as u64,
        };
        if spc == 0 || nfats == 0 || fat_sectors == 0 {
            return Err("not FAT");
        }
        let root_sectors = (root_entries * 32).div_ceil(SECTOR) as u64;
        let fat_start = reserved;
        let root_start = fat_start + nfats * fat_sectors;
        let data_start = root_start + root_sectors;
        let clusters = ((total - data_start) / spc as u64) as u32;
        if clusters < 4085 {
            return Err("FAT12 is not supported");
        }
        let fat32 = clusters >= 65525;
        let root_cluster = if fat32 { u32_at(&bs, 44) } else { 0 };
        let mut fat = vec![0u8; fat_sectors as usize * SECTOR];
        for (i, chunk) in fat.chunks_mut(IO_MAX).enumerate() {
            block::read_part(&p, fat_start + (i * IO_MAX / SECTOR) as u64, chunk).map_err(|_| "read error")?;
        }
        let mut info = if fat32 { u16_at(&bs, 48) as u64 } else { 0 };
        if info != 0 {
            let mut sec = vec![0u8; SECTOR];
            if block::read_part(&p, info, &mut sec).is_err() || u32_at(&sec, 0) != 0x4161_5252 || u32_at(&sec, 484) != 0x6141_7272 {
                info = 0;
            }
        }
        Ok(Rc::new(FatFs {
            id: new_fs_id(),
            part: p,
            fat32,
            csize: spc * SECTOR,
            fat_start,
            fat_sectors,
            nfats,
            root_start,
            root_entries,
            data_start,
            clusters,
            root_cluster,
            fat: RefCell::new(fat),
            hint: RefCell::new(2),
            dirty: RefCell::new(BTreeSet::new()),
            chains: RefCell::new(BTreeMap::new()),
            info,
        }))
    }

    pub fn kind(&self) -> &'static str {
        if self.fat32 { "FAT32" } else { "FAT16" }
    }

    pub fn root(self: &Rc<Self>) -> InodeRef {
        Rc::new(FatNode { fs: self.clone(), loc: None })
    }

    // ---- FAT (クラスタの鎖) ----

    fn eoc(&self) -> u32 {
        if self.fat32 { 0x0fff_ffff } else { 0xffff }
    }

    fn is_end(&self, c: u32) -> bool {
        c < 2 || c >= if self.fat32 { 0x0fff_fff8 } else { 0xfff8 }
    }

    fn next(&self, c: u32) -> u32 {
        let f = self.fat.borrow();
        if self.fat32 {
            u32_at(&f, c as usize * 4) & 0x0fff_ffff
        } else {
            u16_at(&f, c as usize * 2) as u32
        }
    }

    fn set_next(&self, c: u32, v: u32) -> Result<(), i64> {
        // 鎖の覚えを直す: 終わりに足すならその鎖に足し、空きをとるだけならそのまま、ほかは忘れる
        let old = self.next(c);
        {
            let mut ch = self.chains.borrow_mut();
            if old == 0 {
            } else if self.is_end(old) && !self.is_end(v) {
                if let Some(chain) = ch.values_mut().find(|k| k.last() == Some(&c)) {
                    Rc::make_mut(chain).push(v);
                }
            } else {
                ch.clear();
            }
        }
        let off = {
            let mut f = self.fat.borrow_mut();
            if self.fat32 {
                let o = c as usize * 4;
                let old = u32_at(&f, o) & 0xf000_0000;
                f[o..o + 4].copy_from_slice(&(old | (v & 0x0fff_ffff)).to_le_bytes());
                o
            } else {
                let o = c as usize * 2;
                f[o..o + 2].copy_from_slice(&(v as u16).to_le_bytes());
                o
            }
        };
        self.dirty.borrow_mut().insert((off / SECTOR) as u64);
        Ok(())
    }

    /// 書きかえた FAT のセクタを、すべての写しに書く
    fn flush(&self) -> Result<(), i64> {
        let dirty = core::mem::take(&mut *self.dirty.borrow_mut());
        if dirty.is_empty() {
            return Ok(());
        }
        if self.info != 0 {
            let mut sec = [0u8; SECTOR];
            block::read_part(&self.part, self.info, &mut sec)?;
            sec[488..492].copy_from_slice(&(self.free_count() as u32).to_le_bytes());
            sec[492..496].copy_from_slice(&self.hint.borrow().to_le_bytes());
            block::write_part(&self.part, self.info, &sec)?;
        }
        let fat = self.fat.borrow();
        for s in dirty {
            let data = &fat[s as usize * SECTOR..(s as usize + 1) * SECTOR];
            for i in 0..self.nfats {
                block::write_part(&self.part, self.fat_start + i * self.fat_sectors + s, data)?;
            }
        }
        Ok(())
    }

    /// 空いたクラスタを 1 つとって鎖の終わりにする (zero なら中身を 0 に)
    fn alloc(&self, zero: bool) -> Result<u32, i64> {
        let start = *self.hint.borrow();
        for k in 0..self.clusters {
            let c = 2 + (start - 2 + k) % self.clusters;
            if self.next(c) == 0 {
                self.set_next(c, self.eoc())?;
                *self.hint.borrow_mut() = c + 1;
                if zero {
                    self.write_cluster(c, 0, &vec![0u8; self.csize])?;
                }
                return Ok(c);
            }
        }
        Err(-ENOSPC)
    }

    /// c から始まる鎖をすべて空きに
    fn free_chain(&self, mut c: u32) -> Result<(), i64> {
        while !self.is_end(c) {
            let n = self.next(c);
            self.set_next(c, 0)?;
            c = n;
        }
        Ok(())
    }

    fn chain(&self, first: u32) -> Rc<Vec<u32>> {
        if let Some(v) = self.chains.borrow().get(&first) {
            return v.clone();
        }
        let mut v = Vec::new();
        let mut c = first;
        while !self.is_end(c) && v.len() <= self.clusters as usize {
            v.push(c);
            c = self.next(c);
        }
        let v = Rc::new(v);
        if !v.is_empty() {
            let mut ch = self.chains.borrow_mut();
            if ch.len() >= 64 {
                ch.clear();
            }
            ch.insert(first, v.clone());
        }
        v
    }

    fn cluster_sector(&self, c: u32) -> u64 {
        self.data_start + (c as u64 - 2) * (self.csize / SECTOR) as u64
    }

    /// 鎖 chain の中の off バイト目から len バイトを、つながったクラスタはまとめて読み書きする
    fn file_io(&self, chain: &[u32], off: usize, len: usize, mut f: impl FnMut(u64, usize, usize, usize) -> Result<(), i64>) -> Result<(), i64> {
        let cs = self.csize;
        let mut done = 0;
        while done < len {
            let pos = off + done;
            let k = pos / cs;
            // k から続いているクラスタの数
            let mut run = 1;
            while k + run < chain.len() && chain[k + run] == chain[k] + run as u32 && run * cs < IO_MAX {
                run += 1;
            }
            let n = ((k + run) * cs - pos).min(len - done).min(IO_MAX);
            f(self.cluster_sector(chain[k]), pos - k * cs, done, n)?;
            done += n;
        }
        Ok(())
    }

    fn write_cluster(&self, c: u32, off: usize, buf: &[u8]) -> Result<(), i64> {
        self.io(self.cluster_sector(c), off, buf.len(), |d, r| d[r].copy_from_slice(buf), true)
    }

    /// base セクタから off バイト目の len バイトを読み (書き) する。セクタの途中なら読んでから
    fn io(&self, base: u64, off: usize, len: usize, f: impl FnOnce(&mut [u8], core::ops::Range<usize>), write: bool) -> Result<(), i64> {
        let first = off / SECTOR;
        let last = (off + len).div_ceil(SECTOR);
        let mut tmp = vec![0u8; (last - first) * SECTOR];
        let skip = off - first * SECTOR;
        let whole = write && skip == 0 && len == tmp.len();
        if !whole {
            block::read_part(&self.part, base + first as u64, &mut tmp)?;
        }
        f(&mut tmp, skip..skip + len);
        if write {
            block::write_part(&self.part, base + first as u64, &tmp)?;
        }
        Ok(())
    }

    // ---- ディレクトリのエントリ ----

    /// ディレクトリ (dir は最初のクラスタ、0 は FAT16 のルート) の idx 番目のエントリの (セクタ, その中の位置)。
    /// 足りなければ extend のときクラスタを足す
    fn entry_pos(&self, dir: u32, idx: usize, extend: bool) -> Result<Option<(u64, usize)>, i64> {
        let byte = idx * 32;
        if dir == 0 && !self.fat32 {
            if idx >= self.root_entries {
                return if extend { Err(-ENOSPC) } else { Ok(None) };
            }
            return Ok(Some((self.root_start + (byte / SECTOR) as u64, byte % SECTOR)));
        }
        let dir = if dir == 0 { self.root_cluster } else { dir };
        let k = byte / self.csize;
        let mut chain = (*self.chain(dir)).clone();
        while chain.len() <= k {
            if !extend {
                return Ok(None);
            }
            let c = self.alloc(true)?;
            self.set_next(*chain.last().unwrap(), c)?;
            chain.push(c);
        }
        let within = byte % self.csize;
        Ok(Some((self.cluster_sector(chain[k]) + (within / SECTOR) as u64, within % SECTOR)))
    }

    fn read_entry(&self, dir: u32, idx: usize) -> Result<Option<[u8; 32]>, i64> {
        let Some((s, o)) = self.entry_pos(dir, idx, false)? else { return Ok(None) };
        let mut sec = [0u8; SECTOR];
        block::read_part(&self.part, s, &mut sec)?;
        Ok(Some(sec[o..o + 32].try_into().unwrap()))
    }

    fn write_entry(&self, dir: u32, idx: usize, e: &[u8; 32]) -> Result<(), i64> {
        let (s, o) = self.entry_pos(dir, idx, true)?.ok_or(-EIO)?;
        let mut sec = [0u8; SECTOR];
        block::read_part(&self.part, s, &mut sec)?;
        sec[o..o + 32].copy_from_slice(e);
        block::write_part(&self.part, s, &sec)
    }

    /// ディレクトリの中身: (名前, 短い名前のエントリの位置, その 32 バイト, LFN を含めた最初の位置)
    fn list(&self, dir: u32) -> Result<Vec<(String, usize, [u8; 32], usize)>, i64> {
        let mut out = Vec::new();
        let mut lfn: Vec<(u8, [u16; 13])> = Vec::new();
        let mut lfn_start = 0;
        let mut idx = 0;
        while let Some(e) = self.read_entry(dir, idx)? {
            match e[0] {
                0x00 => break,
                0xe5 => lfn.clear(),
                _ if e[11] == ATTR_LFN => {
                    if e[0] & 0x40 != 0 {
                        lfn.clear();
                        lfn_start = idx;
                    }
                    let mut part = [0u16; 13];
                    for (k, o) in [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30].iter().enumerate() {
                        part[k] = u16_at(&e, *o);
                    }
                    lfn.push((e[13], part));
                }
                _ => {
                    if e[11] & ATTR_VOLUME == 0 {
                        let short = short_name(&e);
                        let sum = checksum(&e[..11]);
                        let name = if !lfn.is_empty() && lfn.iter().all(|(c, _)| *c == sum) {
                            // LFN は後ろの部分から並んでいる
                            let mut units: Vec<u16> = Vec::new();
                            for (_, p) in lfn.iter().rev() {
                                units.extend(p.iter().copied().take_while(|&u| u != 0 && u != 0xffff));
                            }
                            String::from_utf16_lossy(&units)
                        } else {
                            short
                        };
                        let first = if lfn.is_empty() { idx } else { lfn_start };
                        if name != "." && name != ".." {
                            out.push((name, idx, e, first));
                        }
                    }
                    lfn.clear();
                }
            }
            idx += 1;
        }
        Ok(out)
    }

    fn find(&self, dir: u32, name: &str) -> Result<(usize, [u8; 32], usize), i64> {
        self.list(dir)?
            .into_iter()
            .find(|(n, ..)| n.eq_ignore_ascii_case(name))
            .map(|(_, i, e, f)| (i, e, f))
            .ok_or(-ENOENT)
    }

    /// n 個続けて空いているエントリの先頭 (なければ終わりに足す)
    fn free_run(&self, dir: u32, n: usize) -> Result<usize, i64> {
        let mut idx = 0;
        let mut run = 0;
        loop {
            match self.read_entry(dir, idx)? {
                Some(e) if e[0] == 0x00 || e[0] == 0xe5 => {
                    run += 1;
                    if run == n {
                        return Ok(idx + 1 - n);
                    }
                }
                Some(_) => run = 0,
                // 終わり: ここから先に足していく
                None => return Ok(idx - run),
            }
            idx += 1;
        }
    }

    /// dir に name のエントリ (LFN + 短い名前) を作る。短い名前のエントリの位置を返す
    fn add_entry(&self, dir: u32, name: &str, attr: u8, cluster: u32, size: u32) -> Result<usize, i64> {
        if name.is_empty() || name.len() > 255 || name.contains(['/', '\\', ':', '*', '?', '"', '<', '>', '|']) {
            return Err(-EINVAL);
        }
        let existing: Vec<[u8; 11]> = self.list(dir)?.iter().map(|(_, _, e, _)| e[..11].try_into().unwrap()).collect();
        let (short, case, need_lfn) = make_short(name, &existing);
        let units: Vec<u16> = name.encode_utf16().collect();
        let nlfn = if need_lfn { units.len().div_ceil(13) } else { 0 };
        let start = self.free_run(dir, nlfn + 1)?;
        let sum = checksum(&short);
        for k in 0..nlfn {
            // 名前の後ろの部分から先に並べる
            let seq = nlfn - k;
            let mut e = [0u8; 32];
            e[0] = seq as u8 | if k == 0 { 0x40 } else { 0 };
            e[11] = ATTR_LFN;
            e[13] = sum;
            for (j, o) in [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30].iter().enumerate() {
                let i = (seq - 1) * 13 + j;
                let u = match i.cmp(&units.len()) {
                    core::cmp::Ordering::Less => units[i],
                    core::cmp::Ordering::Equal => 0,
                    core::cmp::Ordering::Greater => 0xffff,
                };
                e[*o..*o + 2].copy_from_slice(&u.to_le_bytes());
            }
            self.write_entry(dir, start + k, &e)?;
        }
        let mut e = [0u8; 32];
        e[..11].copy_from_slice(&short);
        e[11] = attr;
        e[12] = case;
        set_cluster(&mut e, cluster);
        e[28..32].copy_from_slice(&size.to_le_bytes());
        stamp(&mut e, crate::timer::epoch_ns() / 1_000_000_000);
        let idx = start + nlfn;
        self.write_entry(dir, idx, &e)?;
        Ok(idx)
    }

    /// first から idx までのエントリ (LFN と短い名前) を消す
    fn remove_entries(&self, dir: u32, first: usize, idx: usize) -> Result<(), i64> {
        for i in first..=idx {
            if let Some(mut e) = self.read_entry(dir, i)? {
                e[0] = 0xe5;
                self.write_entry(dir, i, &e)?;
            }
        }
        Ok(())
    }

    fn free_count(&self) -> u64 {
        (2..self.clusters + 2).filter(|&c| self.next(c) == 0).count() as u64
    }
}

fn checksum(n: &[u8]) -> u8 {
    n.iter().fold(0u8, |s, &c| (s >> 1 | (s & 1) << 7).wrapping_add(c))
}

fn cluster_of(e: &[u8; 32]) -> u32 {
    (u16_at(e, 20) as u32) << 16 | u16_at(e, 26) as u32
}

fn set_cluster(e: &mut [u8; 32], c: u32) {
    e[20..22].copy_from_slice(&((c >> 16) as u16).to_le_bytes());
    e[26..28].copy_from_slice(&(c as u16).to_le_bytes());
}

/// 短い名前 (NT の小文字の印も見る)
fn short_name(e: &[u8; 32]) -> String {
    let base: String = e[0..8].iter().map(|&c| c as char).collect::<String>().trim_end().into();
    let ext: String = e[8..11].iter().map(|&c| c as char).collect::<String>().trim_end().into();
    let base = if e[12] & LOWER_BASE != 0 { base.to_ascii_lowercase() } else { base };
    let ext = if e[12] & LOWER_EXT != 0 { ext.to_ascii_lowercase() } else { ext };
    if ext.is_empty() { base } else { alloc::format!("{}.{}", base, ext) }
}

fn short_ok(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"!#$%&'()-@^_`{}~".contains(&c)
}

/// name の短い名前 (11 バイト)、NT の小文字の印、LFN が要るか
fn make_short(name: &str, existing: &[[u8; 11]]) -> ([u8; 11], u8, bool) {
    let (base, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i + 1..]),
        _ => (name, ""),
    };
    let same_case = |s: &str| s.bytes().all(|c| !c.is_ascii_lowercase()) || s.bytes().all(|c| !c.is_ascii_uppercase());
    let fits = !base.is_empty() && base.len() <= 8 && ext.len() <= 3 && base.bytes().chain(ext.bytes()).all(short_ok) && same_case(base) && same_case(ext);
    let mut s = [b' '; 11];
    if fits {
        for (i, c) in base.bytes().enumerate() {
            s[i] = c.to_ascii_uppercase();
        }
        for (i, c) in ext.bytes().enumerate() {
            s[8 + i] = c.to_ascii_uppercase();
        }
        let mut case = 0;
        if base.bytes().any(|c| c.is_ascii_lowercase()) {
            case |= LOWER_BASE;
        }
        if ext.bytes().any(|c| c.is_ascii_lowercase()) {
            case |= LOWER_EXT;
        }
        if !existing.contains(&s) {
            return (s, case, false);
        }
    }
    // 長い名前: BASE~N.EXT の別名を作る
    let clean = |x: &str, n: usize| -> Vec<u8> { x.bytes().map(|c| c.to_ascii_uppercase()).filter(|&c| short_ok(c)).take(n).collect() };
    let b = clean(base, 6);
    let e = clean(ext, 3);
    for n in 1..1000 {
        let tail = alloc::format!("~{}", n);
        let mut s = [b' '; 11];
        let keep = b.len().min(8 - tail.len());
        s[..keep].copy_from_slice(&b[..keep]);
        s[keep..keep + tail.len()].copy_from_slice(tail.as_bytes());
        s[8..8 + e.len()].copy_from_slice(&e);
        if !existing.contains(&s) {
            return (s, 0, true);
        }
    }
    (s, 0, true)
}

/// FAT の日時 (UTC) を書く (作った時と書いた時)
fn stamp(e: &mut [u8; 32], secs: u64) {
    let (date, time) = fat_datetime(secs);
    e[14..16].copy_from_slice(&time.to_le_bytes());
    e[16..18].copy_from_slice(&date.to_le_bytes());
    e[18..20].copy_from_slice(&date.to_le_bytes());
    e[22..24].copy_from_slice(&time.to_le_bytes());
    e[24..26].copy_from_slice(&date.to_le_bytes());
}

fn fat_datetime(secs: u64) -> (u16, u16) {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    // 1970-01-01 からの日数を年月日に (Howard Hinnant の方法)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    let y = (y - 1980).clamp(0, 127) as u16;
    let date = y << 9 | (m as u16) << 5 | d as u16;
    let time = ((rem / 3600) as u16) << 11 | (((rem / 60) % 60) as u16) << 5 | ((rem % 60) / 2) as u16;
    (date, time)
}

fn fat_to_epoch(date: u16, time: u16) -> u64 {
    let (y, m, d) = (1980 + (date >> 9) as i64, ((date >> 5) & 15) as i64, (date & 31) as i64);
    if m == 0 || d == 0 {
        return 0;
    }
    let (y2, m2) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * m2 + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    days.max(0) as u64 * 86400 + (time >> 11) as u64 * 3600 + ((time >> 5) & 63) as u64 * 60 + (time & 31) as u64 * 2
}

impl FatNode {
    fn node(&self, loc: Option<Loc>) -> InodeRef {
        Rc::new(FatNode { fs: self.fs.clone(), loc })
    }

    /// 自分のエントリ (ルートは None)
    fn entry(&self) -> Result<Option<[u8; 32]>, i64> {
        match self.loc {
            None => Ok(None),
            Some(l) => self.fs.read_entry(l.dir, l.idx)?.map(Some).ok_or(-EIO),
        }
    }

    fn save(&self, e: &[u8; 32]) -> Result<(), i64> {
        let l = self.loc.ok_or(-EPERM)?;
        self.fs.write_entry(l.dir, l.idx, e)
    }

    /// ディレクトリとしての番号 (子のエントリの dir に入る): 最初のクラスタ、ルートは 0
    fn dir_id(&self) -> Result<u32, i64> {
        match self.entry()? {
            None => Ok(0),
            Some(e) if e[11] & ATTR_DIR != 0 => Ok(cluster_of(&e)),
            Some(_) => Err(-ENOTDIR),
        }
    }

    fn check_dir(&self) -> Result<u32, i64> {
        self.dir_id()
    }
}

impl Inode for FatNode {
    fn id(&self) -> (usize, u64) {
        let ino = match self.loc {
            None => 1,
            Some(l) => ((l.dir as u64) << 20) | (l.idx as u64 + 2),
        };
        (self.fs.id, ino)
    }

    fn meta(&self) -> Meta {
        let e = self.entry().ok().flatten();
        let (dir, size, mtime) = match &e {
            None => (true, 0, 0),
            Some(e) => (e[11] & ATTR_DIR != 0, u32_at(e, 28) as u64, fat_to_epoch(u16_at(e, 24), u16_at(e, 22)) * 1_000_000_000),
        };
        let ro = e.as_ref().is_some_and(|e| e[11] & ATTR_RO != 0);
        let mode = if dir { S_IFDIR | 0o755 } else { S_IFREG | if ro { 0o555 } else { 0o755 } };
        Meta {
            ino: self.id().1,
            mode,
            nlink: if dir { 2 } else { 1 },
            uid: 0,
            gid: 0,
            size,
            rdev: 0,
            blocks: size.div_ceil(512),
            mtime,
            ctime: mtime,
        }
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn read_at(&self, off: usize, buf: &mut [u8]) -> Result<usize, i64> {
        let e = self.entry()?.ok_or(-EISDIR)?;
        if e[11] & ATTR_DIR != 0 {
            return Err(-EISDIR);
        }
        let size = u32_at(&e, 28) as usize;
        if off >= size {
            return Ok(0);
        }
        let len = buf.len().min(size - off);
        let chain = self.fs.chain(cluster_of(&e));
        let len = len.min((chain.len() * self.fs.csize).saturating_sub(off));
        self.fs.file_io(&chain, off, len, |base, o, d, n| self.fs.io(base, o, n, |t, r| buf[d..d + n].copy_from_slice(&t[r]), false))?;
        Ok(len)
    }

    fn write_at(&self, off: usize, buf: &[u8]) -> Result<usize, i64> {
        // 中身が変わる: ページキャッシュ (vm.rs) にある分を捨てる
        crate::vm::file_changed(self.id());
        let mut e = self.entry()?.ok_or(-EISDIR)?;
        if e[11] & ATTR_DIR != 0 {
            return Err(-EISDIR);
        }
        let end = off + buf.len();
        if end > u32::MAX as usize {
            return Err(-27); // EFBIG
        }
        let cs = self.fs.csize;
        let (mut have, mut last) = {
            let chain = self.fs.chain(cluster_of(&e));
            (chain.len(), chain.last().copied())
        };
        // 足りないクラスタを足す (まるごと書くところのほかは 0 で埋める)
        while have * cs < end {
            let c = self.fs.alloc(!(have * cs >= off && (have + 1) * cs <= end))?;
            match last {
                Some(l) => self.fs.set_next(l, c)?,
                None => set_cluster(&mut e, c),
            }
            last = Some(c);
            have += 1;
        }
        self.fs.flush()?;
        let chain = self.fs.chain(cluster_of(&e));
        self.fs.file_io(&chain, off, buf.len(), |base, o, d, n| self.fs.io(base, o, n, |t, r| t[r].copy_from_slice(&buf[d..d + n]), true))?;
        if end > u32_at(&e, 28) as usize {
            e[28..32].copy_from_slice(&(end as u32).to_le_bytes());
        }
        e[11] |= ATTR_ARCHIVE;
        stamp_write(&mut e);
        self.save(&e)?;
        Ok(buf.len())
    }

    fn truncate(&self, len: usize) -> Result<(), i64> {
        // 中身が変わる: ページキャッシュ (vm.rs) にある分を捨てる
        crate::vm::file_changed(self.id());
        let mut e = self.entry()?.ok_or(-EISDIR)?;
        if e[11] & ATTR_DIR != 0 {
            return Err(-EISDIR);
        }
        let size = u32_at(&e, 28) as usize;
        if len > size {
            // 伸ばすときは 0 を書く
            let zero = vec![0u8; len - size];
            return self.write_at(size, &zero).map(|_| ());
        }
        let cs = self.fs.csize;
        let chain = self.fs.chain(cluster_of(&e));
        let keep = len.div_ceil(cs);
        if keep == 0 {
            if let Some(&c) = chain.first() {
                self.fs.free_chain(c)?;
            }
            set_cluster(&mut e, 0);
        } else if chain.len() > keep {
            self.fs.free_chain(chain[keep])?;
            self.fs.set_next(chain[keep - 1], self.fs.eoc())?;
        }
        e[28..32].copy_from_slice(&(len as u32).to_le_bytes());
        stamp_write(&mut e);
        self.save(&e)?;
        self.fs.flush()
    }

    fn readlink(&self) -> Result<String, i64> {
        Err(-EINVAL)
    }

    fn lookup(&self, name: &str) -> Result<InodeRef, i64> {
        let dir = self.check_dir()?;
        if name == "." {
            return Ok(self.node(self.loc));
        }
        let (idx, ..) = self.fs.find(dir, name)?;
        Ok(self.node(Some(Loc { dir, idx })))
    }

    fn readdir(&self) -> Result<Vec<DirEntry>, i64> {
        let dir = self.check_dir()?;
        Ok(self
            .fs
            .list(dir)?
            .into_iter()
            .map(|(name, idx, e, _)| DirEntry {
                name,
                ino: ((dir as u64) << 20) | (idx as u64 + 2),
                mode: if e[11] & ATTR_DIR != 0 { S_IFDIR | 0o755 } else { S_IFREG | 0o755 },
            })
            .collect())
    }

    fn create(&self, name: &str, _mode: u32, node: NewNode) -> Result<InodeRef, i64> {
        let dir = self.check_dir()?;
        if self.fs.find(dir, name).is_ok() {
            return Err(-EEXIST);
        }
        let idx = match node {
            NewNode::File => self.fs.add_entry(dir, name, ATTR_ARCHIVE, 0, 0)?,
            NewNode::Dir => {
                let c = self.fs.alloc(true)?;
                let idx = self.fs.add_entry(dir, name, ATTR_DIR, c, 0)?;
                // . と ..
                let mut dot = [0u8; 32];
                dot[..11].copy_from_slice(b".          ");
                dot[11] = ATTR_DIR;
                set_cluster(&mut dot, c);
                stamp(&mut dot, crate::timer::epoch_ns() / 1_000_000_000);
                let mut dotdot = dot;
                dotdot[..11].copy_from_slice(b"..         ");
                set_cluster(&mut dotdot, dir);
                self.fs.write_entry(c, 0, &dot)?;
                self.fs.write_entry(c, 1, &dotdot)?;
                idx
            }
            _ => return Err(-EPERM), // リンクやデバイスは作れない
        };
        self.fs.flush()?;
        Ok(self.node(Some(Loc { dir, idx })))
    }

    fn link(&self, _name: &str, _target: &InodeRef) -> Result<(), i64> {
        Err(-EPERM)
    }

    fn unlink(&self, name: &str, rmdir: bool) -> Result<(), i64> {
        let dir = self.check_dir()?;
        let (idx, e, first) = self.fs.find(dir, name)?;
        let is_dir = e[11] & ATTR_DIR != 0;
        if is_dir != rmdir {
            return Err(if is_dir { -EISDIR } else { -ENOTDIR });
        }
        if is_dir && !self.fs.list(cluster_of(&e))?.is_empty() {
            return Err(-ENOTEMPTY);
        }
        self.fs.remove_entries(dir, first, idx)?;
        self.fs.free_chain(cluster_of(&e))?;
        self.fs.flush()
    }

    fn rename(&self, old: &str, newdir: &InodeRef, new: &str) -> Result<(), i64> {
        let dir = self.check_dir()?;
        let nd = newdir.as_any().downcast_ref::<FatNode>().ok_or(-EXDEV)?;
        let ndir = nd.check_dir()?;
        let (idx, e, first) = self.fs.find(dir, old)?;
        if dir == ndir && old == new {
            return Ok(());
        }
        // 置きかえられる先があれば消す
        if let Ok((i2, e2, f2)) = self.fs.find(ndir, new) {
            if dir == ndir && i2 == idx {
                // 大文字小文字だけの違い: 消してから作りなおす
            } else {
                if e2[11] & ATTR_DIR != 0 {
                    if e[11] & ATTR_DIR == 0 {
                        return Err(-EISDIR);
                    }
                    if !self.fs.list(cluster_of(&e2))?.is_empty() {
                        return Err(-ENOTEMPTY);
                    }
                }
                self.fs.remove_entries(ndir, f2, i2)?;
                self.fs.free_chain(cluster_of(&e2))?;
            }
        }
        self.fs.remove_entries(dir, first, idx)?;
        let ni = self.fs.add_entry(ndir, new, e[11], cluster_of(&e), u32_at(&e, 28))?;
        // 日時はそのまま
        let mut ne = self.fs.read_entry(ndir, ni)?.ok_or(-EIO)?;
        ne[13..26].copy_from_slice(&e[13..26]);
        set_cluster(&mut ne, cluster_of(&e));
        self.fs.write_entry(ndir, ni, &ne)?;
        // ディレクトリを移したら .. を直す
        if e[11] & ATTR_DIR != 0 && dir != ndir {
            if let Some(mut dd) = self.fs.read_entry(cluster_of(&e), 1)? {
                set_cluster(&mut dd, ndir);
                self.fs.write_entry(cluster_of(&e), 1, &dd)?;
            }
        }
        self.fs.flush()
    }

    fn set_mode(&self, mode: u32) -> Result<(), i64> {
        // 書けるかどうかだけを読み取り専用の印に
        if let Some(mut e) = self.entry()? {
            if mode & 0o222 == 0 { e[11] |= ATTR_RO } else { e[11] &= !ATTR_RO }
            self.save(&e)?;
        }
        Ok(())
    }

    fn set_owner(&self, uid: Option<u32>, gid: Option<u32>) -> Result<(), i64> {
        // 持ち主はない (root のものとして見せる)
        if uid.is_some_and(|u| u != 0) || gid.is_some_and(|g| g != 0) {
            return Err(-EPERM);
        }
        Ok(())
    }

    fn set_mtime(&self, ns: u64) -> Result<(), i64> {
        if let Some(mut e) = self.entry()? {
            let (date, time) = fat_datetime(ns / 1_000_000_000);
            e[22..24].copy_from_slice(&time.to_le_bytes());
            e[24..26].copy_from_slice(&date.to_le_bytes());
            self.save(&e)?;
        }
        Ok(())
    }

    fn statfs(&self) -> [u8; 120] {
        let bs = self.fs.csize as u64;
        statfs_bytes(MSDOS_SUPER_MAGIC, bs, self.fs.clusters as u64, self.fs.free_count(), 0, 0)
    }
}

fn stamp_write(e: &mut [u8; 32]) {
    let (date, time) = fat_datetime(crate::timer::epoch_ns() / 1_000_000_000);
    e[22..24].copy_from_slice(&time.to_le_bytes());
    e[24..26].copy_from_slice(&date.to_le_bytes());
    e[18..20].copy_from_slice(&date.to_le_bytes());
}

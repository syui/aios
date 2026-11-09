// スワップ: 使っていないユーザーのページを、ディスクのスワップ区画へ追い出す
//
// 区画は mkswap の形 (Linux と同じ): ページ 0 が見出しで、1024 バイト目から
// version (1)、last_page、nr_badpages、そのあと悪いページの番号。4086 バイト目に "SWAPSPACE2"。
// ページ 1..=last_page に 1 枚ずつ書く (スロット)。
// スワップファイル (ext4/ext2 の上のファイル) は、swapon のときにファイルのページがディスクの
// どこにあるかを表 (extfs::swap_map) にしておき、あとはファイルシステムを通らずに直接読み書きする
// (Linux と同じ。穴のない、mkswap したファイル。使っている間は書きかえさせない)。
// 追い出したページの PTE はスロットの番号を持つ (vm.rs)。fork で共有されるので、スロットごとに参照の数を持つ。
//
// 回収 (reclaim: ページキャッシュ、写しているファイルのページ、そしてここへ追い出す) は vm.rs (Linux の vmscan.c と同じく
// core)。ここはスワップの区画とスロットだけ (Linux の swapfile.c、page_io.c)。vm.rs の入り口 (vm::Swap) から呼ばれる
// (main.rs の vm::register_swap)。どのページを追い出すかは各ページ表の clock (vm.rs)。
// ディスクへの書き込みは大きなロックを持ったまま待つので、書いている間にほかの CPU が同じページに触れても、
// フォールトしてロックを待つだけ (書き終わってからスワップから読み戻す)。
use crate::cell::Bkl;
use crate::block::{self, Part, SECTOR};
use crate::memlayout::PGSIZE;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

pub const MAX_AREAS: usize = 4;

const EPERM: i64 = 1;
const ENOMEM: i64 = 12;
const EBUSY: i64 = 16;

/// スロットの番号 (区画の番号 << 32 | 区画の中のページ。vm.rs の Slot: PTE に入れる)
pub use crate::vm::Slot;

const BAD: u16 = u16::MAX;

/// スワップに使うもの
pub enum Source {
    Part(Part),
    /// root の区画の上のファイル
    File(crate::vfs::InodeRef),
}

struct Area {
    part: Part,
    /// ファイルなら、そのページの場所 (ファイルのページ, 区画の中のセクタ, ページ数)。区画なら空
    map: Vec<(u64, u64, u64)>,
    file: Option<crate::vfs::InodeRef>,
    name: String,
    key: (usize, u64),
    /// スロットごとの参照の数 (0 は空き、BAD は見出しと悪いページ)
    refs: Vec<u16>,
    /// 使えるスロットの数と、使っている数
    size: usize,
    used: usize,
    /// 次に空きを探しはじめるところ
    next: usize,
    /// swapoff の途中: もう新しく追い出さない
    draining: bool,
}

static AREAS: Bkl<[Option<Area>; MAX_AREAS]> = Bkl::new([const { None }; MAX_AREAS]);

#[track_caller]
fn areas() -> &'static mut [Option<Area>; MAX_AREAS] {
    AREAS.get()
}

fn area(slot: Slot) -> Option<&'static mut Area> {
    areas().get_mut((slot >> 32) as usize)?.as_mut()
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}

/// スワップの区画やファイルを見分ける鍵 (区画は番号、ファイルは inode)
fn key(src: &Source) -> (usize, u64) {
    match src {
        Source::Part(p) => (usize::MAX, p.num as u64),
        Source::File(i) => i.id(),
    }
}

fn swap_file(a: &Area) -> Option<&crate::extfs::ExtInode> {
    a.file.as_ref()?.as_any().downcast_ref::<crate::extfs::ExtInode>()
}

/// swapon: 区画かファイル (名前は name) をスワップに使う
pub fn on(src: Source, name: String) -> Result<(), i64> {
    if areas().iter().flatten().any(|a| a.key == key(&src)) {
        return Err(-EBUSY);
    }
    let i = areas().iter().position(|a| a.is_none()).ok_or(-EPERM)?;
    let k = key(&src);
    let (part, map, file, max) = match src {
        Source::Part(p) => (p, Vec::new(), None, p.len as usize * SECTOR / PGSIZE),
        Source::File(ino) => {
            let ext = ino.as_any().downcast_ref::<crate::extfs::ExtInode>().ok_or_else(|| crate::why::inval())?;
            let map = ext.swap_map()?;
            let max = map.last().map_or(0, |&(p, _, n)| (p + n) as usize);
            // ファイルのページは、はじめから穴なくつづいている
            if map.first().is_none_or(|&(p, _, _)| p != 0) {
                return Err(crate::why::inval());
            }
            (block::root(), map, Some(ino), max)
        }
    };
    let mut a = Area { part, map, file, name, key: k, refs: Vec::new(), size: 0, used: 0, next: 1, draining: false };
    let mut h = vec![0u8; PGSIZE];
    block::read_part(&a.part, sector_of_page(&a, 0), &mut h)?;
    if &h[PGSIZE - 10..] != b"SWAPSPACE2" || u32_at(&h, 1024) != 1 {
        return Err(crate::why::inval());
    }
    let pages = (u32_at(&h, 1028) as usize + 1).min(max);
    if pages < 2 {
        return Err(crate::why::inval());
    }
    let mut refs = vec![0u16; pages];
    refs[0] = BAD;
    let nbad = (u32_at(&h, 1032) as usize).min((PGSIZE - 10 - 1536) / 4);
    for k in 0..nbad {
        let b = u32_at(&h, 1536 + k * 4) as usize;
        if b < pages {
            refs[b] = BAD;
        }
    }
    a.size = refs.iter().filter(|&&r| r == 0).count();
    a.refs = refs;
    if let Some(f) = swap_file(&a) {
        f.set_swapfile(true);
    }
    println!("swap: {} ({} KiB)", a.name, a.size * PGSIZE / 1024);
    areas()[i] = Some(a);
    Ok(())
}

/// swapoff: 追い出したページをぜんぶ読み戻してから外す
pub fn off(src: &Source) -> Result<(), i64> {
    let i = areas().iter().position(|a| a.as_ref().is_some_and(|a| a.key == key(src))).ok_or_else(|| crate::why::inval())?;
    areas()[i].as_mut().unwrap().draining = true;
    let mut ok = true;
    crate::proc::each_pagetable(|pt| {
        if ok && pt.swap_in_area(i).is_err() {
            ok = false;
        }
    });
    let a = areas()[i].as_mut().unwrap();
    if !ok || a.used != 0 {
        a.draining = false;
        return Err(-ENOMEM);
    }
    if let Some(f) = swap_file(a) {
        f.set_swapfile(false);
    }
    areas()[i] = None;
    Ok(())
}

/// 空いているスロットを 1 つ (参照 1)。どこにもなければ None
pub fn alloc() -> Option<Slot> {
    for (i, a) in areas().iter_mut().enumerate() {
        let Some(a) = a else { continue };
        if a.draining || a.used >= a.size {
            continue;
        }
        let n = a.refs.len();
        for k in 0..n {
            let s = (a.next + k) % n;
            if a.refs[s] == 0 {
                a.refs[s] = 1;
                a.used += 1;
                a.next = s + 1;
                return Some((i as u64) << 32 | s as u64);
            }
        }
    }
    None
}

/// スロットを共有する人が増える (fork)
pub fn dup(slot: Slot) {
    if let Some(r) = area(slot).and_then(|a| a.refs.get_mut(slot as u32 as usize)) {
        if *r != BAD && *r < BAD - 1 {
            *r += 1;
        }
    }
}

/// スロットの共有をやめる。だれも使わなくなったら空きに
pub fn free(slot: Slot) {
    let Some(a) = area(slot) else { return };
    let Some(r) = a.refs.get_mut(slot as u32 as usize) else { return };
    if *r == 0 || *r == BAD {
        return;
    }
    *r -= 1;
    if *r == 0 {
        a.used -= 1;
    }
}

const SECTORS: u64 = (PGSIZE / SECTOR) as u64;

/// 区画の中で、page 番目のページのはじめのセクタ
fn sector_of_page(a: &Area, page: u64) -> u64 {
    if a.map.is_empty() {
        return page * SECTORS;
    }
    let i = a.map.partition_point(|&(p, _, _)| p <= page);
    let (p, s, _) = a.map[i.saturating_sub(1)];
    s + (page - p) * SECTORS
}

fn sector_of(a: &Area, slot: Slot) -> u64 {
    sector_of_page(a, slot as u32 as u64)
}

/// スワップから読んだページと書いたページの数 (/proc/vmstat の pswpin、pswpout)
pub static PSWPIN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static PSWPOUT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// スロットからページ (カーネルの仮想アドレス) へ読む
pub fn read(slot: Slot, page: *mut u8) -> Result<(), i64> {
    PSWPIN.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let a = area(slot).ok_or_else(|| crate::why::inval())?;
    let buf = unsafe { core::slice::from_raw_parts_mut(page, PGSIZE) };
    block::read_part(&a.part, sector_of(a, slot), buf)
}

/// ページをスロットへ書く
pub fn write(slot: Slot, page: *const u8) -> Result<(), i64> {
    PSWPOUT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let a = area(slot).ok_or_else(|| crate::why::inval())?;
    let buf = unsafe { core::slice::from_raw_parts(page, PGSIZE) };
    block::write_part(&a.part, sector_of(a, slot), buf)
}

/// 追い出す余地があるか (swapoff の途中でない区画に空きがある)
pub fn has_room() -> bool {
    areas().iter().flatten().any(|a| !a.draining && a.used < a.size)
}

/// core の入り口 (vm::Swap) につなぐもの (main.rs)
pub struct Ops;

pub static OPS: Ops = Ops;

/// 区画のノード (major, minor) なら区画を引く (ない装置は ENXIO)。名前は区画の名前
fn source(t: crate::vm::SwapTarget, name: String) -> Result<(Source, String), i64> {
    match t {
        crate::vm::SwapTarget::Dev(ma, mi) => {
            let p = block::part_of_dev(ma, mi).ok_or(-6)?;
            let n = block::part_name(&p);
            Ok((Source::Part(p), n))
        }
        crate::vm::SwapTarget::File(i) => Ok((Source::File(i), name)),
    }
}

impl crate::vm::Swap for Ops {
    fn on(&self, t: crate::vm::SwapTarget, name: String) -> Result<(), i64> {
        let (src, name) = source(t, name)?;
        on(src, name)
    }
    fn off(&self, t: &crate::vm::SwapTarget) -> Result<(), i64> {
        let src = match t {
            crate::vm::SwapTarget::Dev(ma, mi) => Source::Part(block::part_of_dev(*ma, *mi).ok_or(-6)?),
            crate::vm::SwapTarget::File(i) => Source::File(i.clone()),
        };
        off(&src)
    }
    fn alloc(&self) -> Option<Slot> {
        alloc()
    }
    fn dup(&self, slot: Slot) {
        dup(slot);
    }
    fn free(&self, slot: Slot) {
        free(slot);
    }
    fn read(&self, slot: Slot, page: *mut u8) -> Result<(), i64> {
        read(slot, page)
    }
    fn write(&self, slot: Slot, page: *const u8) -> Result<(), i64> {
        write(slot, page)
    }
    fn has_room(&self) -> bool {
        has_room()
    }
    fn totals(&self) -> (usize, usize) {
        totals()
    }
}

/// (全体のページ数, 空いているページ数)
pub fn totals() -> (usize, usize) {
    areas().iter().flatten().fold((0, 0), |(t, f), a| (t + a.size, f + a.size - a.used))
}

/// /proc/swaps
pub fn proc_swaps() -> String {
    let mut s = String::from("Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority\n");
    for (i, a) in areas().iter().enumerate() {
        let Some(a) = a else { continue };
        s.push_str(&alloc::format!(
            "{:<40}{}\t{}\t\t{}\t\t-{}\n",
            a.name,
            if a.file.is_some() { "file\t" } else { "partition" },
            a.size * PGSIZE / 1024,
            a.used * PGSIZE / 1024,
            i + 2
        ));
    }
    s
}

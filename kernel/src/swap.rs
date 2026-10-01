// スワップ: 使っていないユーザーのページを、ディスクのスワップ区画へ追い出す
//
// 区画は mkswap の形 (Linux と同じ): ページ 0 が見出しで、1024 バイト目から
// version (1)、last_page、nr_badpages、そのあと悪いページの番号。4086 バイト目に "SWAPSPACE2"。
// ページ 1..=last_page に 1 枚ずつ書く (スロット)。
// 追い出したページの PTE はスロットの番号を持つ (vm.rs)。fork で共有されるので、スロットごとに参照の数を持つ。
//
// 回収 (reclaim) は、ユーザーへ戻る前に空きが LOW を割っていたら HIGH まで、と、
// ページが作れなかったときに BATCH 枚。どのページを追い出すかは各ページ表の clock (vm.rs)。
// ディスクへの書き込みは大きなロックを持ったまま待つので、書いている間にほかの CPU が同じページに触れても、
// フォールトしてロックを待つだけ (書き終わってからスワップから読み戻す)。
use crate::block::{self, Part, SECTOR};
use crate::memlayout::PGSIZE;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

pub const MAX_AREAS: usize = 4;
/// 空きがこれを割ったら回収する (4 MiB)
const LOW: usize = 1024;
/// 回収するときは、空きがこれになるまで (16 MiB)
const HIGH: usize = 4096;
/// ページが作れなかったときに回収する数
pub const BATCH: usize = 256;

const EPERM: i64 = 1;
const ENOMEM: i64 = 12;
const EBUSY: i64 = 16;
const EINVAL: i64 = 22;

/// スロットの番号: 区画の番号 << 32 | 区画の中のページ
pub type Slot = u64;

const BAD: u16 = u16::MAX;

struct Area {
    part: Part,
    name: String,
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

static mut AREAS: [Option<Area>; MAX_AREAS] = [const { None }; MAX_AREAS];

fn areas() -> &'static mut [Option<Area>; MAX_AREAS] {
    unsafe { &mut *(&raw mut AREAS) }
}

fn area(slot: Slot) -> Option<&'static mut Area> {
    areas().get_mut((slot >> 32) as usize)?.as_mut()
}

pub fn area_of(slot: Slot) -> usize {
    (slot >> 32) as usize
}

fn u32_at(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}

/// swapon: 区画 part (名前は name) をスワップに使う
pub fn on(part: Part, name: String) -> Result<(), i64> {
    if areas().iter().flatten().any(|a| a.name == name) {
        return Err(-EBUSY);
    }
    let i = areas().iter().position(|a| a.is_none()).ok_or(-EPERM)?;
    let mut h = vec![0u8; PGSIZE];
    block::read_part(&part, 0, &mut h)?;
    if &h[PGSIZE - 10..] != b"SWAPSPACE2" || u32_at(&h, 1024) != 1 {
        return Err(-EINVAL);
    }
    let pages = (u32_at(&h, 1028) as usize + 1).min(part.len as usize * SECTOR / PGSIZE);
    if pages < 2 {
        return Err(-EINVAL);
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
    let size = refs.iter().filter(|&&r| r == 0).count();
    println!("swap: {} ({} KiB)", name, size * PGSIZE / 1024);
    areas()[i] = Some(Area { part, name, refs, size, used: 0, next: 1, draining: false });
    Ok(())
}

/// swapoff: 追い出したページをぜんぶ読み戻してから外す
pub fn off(name: &str) -> Result<(), i64> {
    let i = areas().iter().position(|a| a.as_ref().is_some_and(|a| a.name == name)).ok_or(-EINVAL)?;
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

fn sector_of(slot: Slot) -> u64 {
    (slot as u32 as u64) * (PGSIZE / SECTOR) as u64
}

/// スロットからページ (カーネルの仮想アドレス) へ読む
pub fn read(slot: Slot, page: *mut u8) -> Result<(), i64> {
    let a = area(slot).ok_or(-EINVAL)?;
    let buf = unsafe { core::slice::from_raw_parts_mut(page, PGSIZE) };
    block::read_part(&a.part, sector_of(slot), buf)
}

/// ページをスロットへ書く
pub fn write(slot: Slot, page: *const u8) -> Result<(), i64> {
    let a = area(slot).ok_or(-EINVAL)?;
    let buf = unsafe { core::slice::from_raw_parts(page, PGSIZE) };
    block::write_part(&a.part, sector_of(slot), buf)
}

/// 次に回収をはじめるページ表 (順番に回す)
static mut HAND: usize = 0;

/// want 枚くらいをスワップへ追い出して空ける。ページ表のルートが exclude のものは触らない。
/// 空けた数を返す
pub fn reclaim(want: usize, exclude: usize) -> usize {
    if !areas().iter().flatten().any(|a| !a.draining && a.used < a.size) {
        return 0;
    }
    let mut pts = Vec::new();
    crate::proc::each_pagetable(|pt| {
        if pt.id() != exclude {
            pts.push(pt as *mut crate::vm::PageTable);
        }
    });
    if pts.is_empty() {
        return 0;
    }
    let mut freed = 0;
    // 1 周目は AF を落とすだけのことが多いので、何周か
    for _ in 0..3 {
        for k in 0..pts.len() {
            let hand = unsafe { HAND };
            let p: *mut crate::vm::PageTable = pts[(hand + k) % pts.len()];
            let pt = unsafe { &mut *p };
            // 1 つのアドレス空間から取りすぎないよう、少しずつ
            freed += pt.swap_out((want - freed).min(BATCH));
            if freed >= want {
                unsafe { HAND = hand + k + 1 };
                return freed;
            }
        }
        unsafe { HAND += 1 };
    }
    freed
}

/// ユーザーへ戻る前に: 空きが少なければ回収しておく
pub fn balance() {
    let free = crate::kalloc::nfree();
    if free < LOW {
        reclaim(HIGH - free, 0);
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
            "{:<40}partition\t{}\t\t{}\t\t-{}\n",
            a.name,
            a.size * PGSIZE / 1024,
            a.used * PGSIZE / 1024,
            i + 2
        ));
    }
    s
}

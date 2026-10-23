//! futex (Linux の kernel/futex/ と同じ決まり)。待ち合わせの場所 (key) ごとに、待っているものの行列 (来た順) を持つ:
//!   起こす数はそのとおり (0 や負なら 1)、bitset、REQUEUE / CMP_REQUEUE は行列から行列へ移す (うしろに)、WAKE_OP は
//!   ユーザーの語を原子的に変えて、くらべた答えで 2 つめも起こす、PI のロック (持ち主の番号と WAITERS の印、来た順に
//!   ゆずる、EDEADLK / EPERM / ESRCH)、futex_waitv (いくつもの語でまとめて待ち、起こされたものの番号を返す)。
//! key は Linux と同じく 3 つ: PRIVATE はアドレス空間とアドレス、共有の op は、自分だけの領域ならアドレス空間と
//! アドレス (PRIVATE のものとは会わない: Linux の FUT_OFF_MMSHARED)、MAP_SHARED の領域ならページの物理アドレス
//! (プロセスをまたぐ)。待つスレッドはみな同じ chan で眠り、起こす側が行列から出して、そのスレッドだけを起こす。
//! ぜんぶ大きなロックの中。ロックなしの速い道 (syscall.rs の fast) のために、PRIVATE の key ごとの待つ数を数える

use crate::cell::Bkl;
use crate::proc;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

const EPERM: i64 = 1;
const ESRCH: i64 = 3;
const EAGAIN: i64 = 11;
const EFAULT: i64 = 14;
const EINVAL: i64 = 22;
const EDEADLK: i64 = 35;
const ENOSYS: i64 = 38;
const ETIMEDOUT: i64 = 110;

pub const WAIT: u64 = 0;
pub const WAKE: u64 = 1;
pub const REQUEUE: u64 = 3;
pub const CMP_REQUEUE: u64 = 4;
pub const WAKE_OP: u64 = 5;
pub const LOCK_PI: u64 = 6;
pub const UNLOCK_PI: u64 = 7;
pub const TRYLOCK_PI: u64 = 8;
pub const WAIT_BITSET: u64 = 9;
pub const WAKE_BITSET: u64 = 10;
pub const WAIT_REQUEUE_PI: u64 = 11;
pub const LOCK_PI2: u64 = 13;
pub const PRIVATE_FLAG: u64 = 128;
pub const CLOCK_REALTIME: u64 = 256;

const WAITERS: u32 = 0x8000_0000;
const OWNER_DIED: u32 = 0x4000_0000;
const TID_MASK: u32 = 0x3fff_ffff;
const BITSET_ANY: u32 = u32::MAX;

/// futex_waitv の 1 つ (struct futex_waitv) と、まとめて待てる数
const WAITV_SIZE: usize = 24;
const WAITV_MAX: u32 = 128;
const FUTEX2_SIZE_U32: u32 = 2;
const FUTEX2_SIZE_MASK: u32 = 3;
const FUTEX2_PRIVATE: u32 = 128;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Key {
    /// FUTEX_PRIVATE_FLAG: アドレス空間とアドレス
    Private(usize, usize),
    /// 共有の op で、自分だけの領域
    Mm(usize, usize),
    /// MAP_SHARED の領域: ページの物理アドレス
    Phys(usize),
}

/// 待っているもの 1 つ (futex_waitv はいくつも)
struct Entry {
    key: Key,
    /// 眠っているスレッド (本当の番号)
    tid: u32,
    bitset: u32,
    /// 待つ呼び出しの番号と、その中の何番め
    wait: u64,
    idx: u32,
    /// FUTEX_LOCK_PI で待つもの
    pi: bool,
}

struct Table {
    next: u64,
    entries: BTreeMap<u64, Entry>,
    /// key ごとの行列 (来た順。REQUEUE で移ったものはうしろに)
    queues: BTreeMap<Key, VecDeque<u64>>,
    /// 起こされた呼び出し → 起こされたもののいちばん大きな番号 (futex_waitv の答え)
    woken: BTreeMap<u64, u32>,
    /// PI の futex の持ち主 (Linux の pi_state: 待つものがいるあいだだけ)
    pi: BTreeMap<Key, u32>,
}

static TABLE: Bkl<Table> = Bkl::new(Table { next: 1, entries: BTreeMap::new(), queues: BTreeMap::new(), woken: BTreeMap::new(), pi: BTreeMap::new() });

#[track_caller]
fn t() -> &'static mut Table {
    TABLE.get()
}

/// futex で待つスレッドの眠る場所 (みな同じ。起こすときはスレッドを選ぶ)
static WAIT_CHAN: u8 = 0;

fn chan() -> usize {
    &raw const WAIT_CHAN as usize
}

// ---- ロックなしの速い道のための数 ----

/// PRIVATE の key で待っている (待とうとしている) ものの数を、key のハッシュごとに。起こす側が大きなロックなしに
/// 「ここにはだれもいない」と知るため。待つ側は数を増やしてから値を読む。ハッシュがぶつかれば、ふつうの道へ行くだけ
static SLEEPERS: [AtomicUsize; 1024] = [const { AtomicUsize::new(0) }; 1024];

pub fn sleepers(mm: usize, uaddr: usize) -> &'static AtomicUsize {
    let h = mm ^ (uaddr << 1) ^ 1;
    &SLEEPERS[(h.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 54) as usize]
}

fn count(k: Key, up: bool) {
    if let Key::Private(mm, a) = k {
        let c = sleepers(mm, a);
        if up {
            c.fetch_add(1, Ordering::SeqCst);
        } else {
            c.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

// ---- key と語 ----

/// uaddr の key (Linux の get_futex_key): そろっていなければ EINVAL、ユーザーの空間の外は EFAULT。共有の op は
/// ページがなければ EFAULT (PRIVATE はメモリを見ない)
fn key_of(uaddr: usize, private: bool) -> Result<Key, i64> {
    if uaddr % 4 != 0 {
        return Err(crate::why::inval());
    }
    if uaddr >= crate::vm::MAXVA {
        return Err(-EFAULT);
    }
    let p = proc::current();
    if private {
        return Ok(Key::Private(p.mm_id(), uaddr));
    }
    if let Some(pa) = p.pt().shared_pa(uaddr) {
        return Ok(Key::Phys(pa));
    }
    let mut b = [0u8; 4];
    p.pt().copy_in(&mut b, uaddr).ok_or(-EFAULT)?;
    Ok(Key::Mm(p.mm_id(), uaddr))
}

fn get(uaddr: usize) -> Result<u32, i64> {
    let mut b = [0u8; 4];
    proc::current().pt().copy_in(&mut b, uaddr).ok_or(-EFAULT)?;
    Ok(u32::from_le_bytes(b))
}

/// ユーザーの語を原子的に (書けるように写して)
fn word(uaddr: usize) -> Result<&'static core::sync::atomic::AtomicU32, i64> {
    proc::current().pt().user_u32(uaddr, true).ok_or(-EFAULT)
}

/// 語を old から new に (ほかの CPU のユーザーと取りあう)。今の値を返す (old なら変えた)
fn cmpxchg(uaddr: usize, old: u32, new: u32) -> Result<u32, i64> {
    Ok(match word(uaddr)?.compare_exchange(old, new, Ordering::SeqCst, Ordering::SeqCst) {
        Ok(v) | Err(v) => v,
    })
}

/// いまのスレッドから見たスレッドの番号 (語に入っている番号)
fn vtid(tid: u32) -> u32 {
    crate::ns::local_or_0(tid)
}

// ---- 行列 ----

fn enqueue(k: Key, wait: u64, idx: u32, bitset: u32, pi: bool, counted: bool) -> u64 {
    let tb = t();
    let id = tb.next;
    tb.next += 1;
    if !counted {
        count(k, true);
    }
    tb.entries.insert(id, Entry { key: k, tid: proc::current().pid, bitset, wait, idx, pi });
    tb.queues.entry(k).or_default().push_back(id);
    id
}

/// 行列から出す (まだいれば)
fn unqueue(id: u64) -> Option<Entry> {
    let tb = t();
    let e = tb.entries.remove(&id)?;
    if let Some(q) = tb.queues.get_mut(&e.key) {
        if let Some(i) = q.iter().position(|&x| x == id) {
            q.remove(i);
        }
        if q.is_empty() {
            tb.queues.remove(&e.key);
        }
    }
    count(e.key, false);
    Some(e)
}

/// 起こす: 行列から出し、その呼び出しが起こされたと覚えて、スレッドを起こす
fn wake_entry(id: u64) {
    let Some(e) = unqueue(id) else { return };
    let w = t().woken.entry(e.wait).or_insert(e.idx);
    *w = (*w).max(e.idx);
    proc::wake_tid(e.tid, chan());
}

fn queue_ids(k: &Key) -> Vec<u64> {
    t().queues.get(k).map(|q| q.iter().copied().collect()).unwrap_or_default()
}

/// 行列の中のものを k2 のうしろへ (同じ key ならそのまま)
fn requeue(id: u64, k2: Key) {
    let tb = t();
    let Some(e) = tb.entries.get_mut(&id) else { return };
    let k1 = e.key;
    if k1 == k2 {
        return;
    }
    e.key = k2;
    count(k2, true);
    count(k1, false);
    if let Some(q) = tb.queues.get_mut(&k1) {
        if let Some(i) = q.iter().position(|&x| x == id) {
            q.remove(i);
        }
        if q.is_empty() {
            tb.queues.remove(&k1);
        }
    }
    tb.queues.entry(k2).or_default().push_back(id);
}

fn has_pi_waiter(k: &Key) -> bool {
    t().queues.get(k).is_some_and(|q| q.iter().any(|id| t().entries.get(id).is_some_and(|e| e.pi)))
}

/// 待つ (呼び出し wait の ids を行列に入れたあと): 起こされたら起こされたものの番号、時間切れは ETIMEDOUT、
/// シグナルは EINTR。どの終わりかたでも、のこりは行列から出す。わけもなく起きたら、また眠る
fn sleep(wait: u64, ids: &[u64], deadline: u64) -> Result<u32, i64> {
    let r = loop {
        let r = proc::sleep_until(chan(), deadline);
        if let Some(i) = t().woken.remove(&wait) {
            break Ok(i);
        }
        match r {
            Err(e) => break Err(e),
            Ok(false) => break Err(-ETIMEDOUT),
            Ok(true) => {}
        }
    };
    for &id in ids {
        unqueue(id);
    }
    t().woken.remove(&wait);
    r
}

fn new_wait() -> u64 {
    let tb = t();
    tb.next += 1;
    tb.next
}

// ---- 時間 ----

/// 待つ時間 (Linux の futex_init_timeout): FUTEX_WAIT は相対、LOCK_PI は CLOCK_REALTIME の時刻、ほかは時刻
/// (CLOCK_REALTIME の印がなければ CLOCK_MONOTONIC)。tick の期限 (0 は過ぎている) にして返す
fn deadline(cmd: u64, op: u64, ts: usize) -> Result<Option<u64>, i64> {
    if ts == 0 {
        return Ok(None);
    }
    let mut b = [0u8; 16];
    proc::current().pt().copy_in(&mut b, ts).ok_or(-EFAULT)?;
    let sec = i64::from_le_bytes(b[..8].try_into().unwrap());
    let nsec = i64::from_le_bytes(b[8..].try_into().unwrap());
    if sec < 0 || !(0..1_000_000_000).contains(&nsec) {
        return Err(crate::why::inval());
    }
    let ns = (sec as u64).saturating_mul(1_000_000_000).saturating_add(nsec as u64);
    let rel = if cmd == WAIT {
        ns
    } else {
        let now = if cmd == LOCK_PI || op & CLOCK_REALTIME != 0 { crate::timer::epoch_ns() } else { crate::timer::uptime_ns() };
        if ns <= now {
            return Ok(Some(0));
        }
        ns - now
    };
    if rel == 0 {
        return Ok(Some(0));
    }
    Ok(Some(crate::timer::ticks_after(rel)))
}

// ---- システムコール ----

/// futex(2) (Linux の sys_futex と do_futex)。val2 は timeout の場所の数 (REQUEUE、CMP_REQUEUE、WAKE_OP)
pub fn futex(uaddr: usize, op: u64, val: u32, timeout: usize, uaddr2: usize, val3: u32) -> Result<i64, i64> {
    let cmd = op & !(PRIVATE_FLAG | CLOCK_REALTIME);
    let private = op & PRIVATE_FLAG != 0;
    let has_timeout = matches!(cmd, WAIT | LOCK_PI | LOCK_PI2 | WAIT_BITSET | WAIT_REQUEUE_PI);
    let dl = if has_timeout { deadline(cmd, op, timeout)? } else { None };
    let val2 = timeout as u32;
    // CLOCK_REALTIME の印は、時刻で待つものだけ (Linux 7.3 は FUTEX_WAIT にも ENOSYS)
    if op & CLOCK_REALTIME != 0 && !matches!(cmd, WAIT_BITSET | WAIT_REQUEUE_PI | LOCK_PI2) {
        return Err(-ENOSYS);
    }
    match cmd {
        WAIT => wait(uaddr, private, val, dl, BITSET_ANY),
        WAIT_BITSET => wait(uaddr, private, val, dl, val3),
        WAKE => wake(uaddr, private, val as i32, BITSET_ANY),
        WAKE_BITSET => wake(uaddr, private, val as i32, val3),
        REQUEUE => requeue_op(uaddr, private, uaddr2, val as i32, val2 as i32, None),
        CMP_REQUEUE => requeue_op(uaddr, private, uaddr2, val as i32, val2 as i32, Some(val3)),
        WAKE_OP => wake_op(uaddr, private, uaddr2, val as i32, val2 as i32, val3),
        LOCK_PI | LOCK_PI2 => lock_pi(uaddr, private, dl, false),
        TRYLOCK_PI => lock_pi(uaddr, private, None, true),
        UNLOCK_PI => unlock_pi(uaddr, private),
        _ => Err(-ENOSYS),
    }
}

/// FUTEX_WAIT / WAIT_BITSET (Linux の __futex_wait): bitset 0 は EINVAL、値がちがえば EAGAIN
fn wait(uaddr: usize, private: bool, val: u32, dl: Option<u64>, bitset: u32) -> Result<i64, i64> {
    if bitset == 0 {
        return Err(crate::why::inval());
    }
    let k = key_of(uaddr, private)?;
    // 数を増やしてから値を読む: 速い道の起こす側は値を書いてから数を読むので、どちらかが相手に気づく
    count(k, true);
    core::sync::atomic::fence(Ordering::SeqCst);
    let cur = match get(uaddr) {
        Ok(v) => v,
        Err(e) => {
            count(k, false);
            return Err(e);
        }
    };
    if cur != val {
        count(k, false);
        return Err(-EAGAIN);
    }
    if dl == Some(0) {
        count(k, false);
        return Err(-ETIMEDOUT);
    }
    let w = new_wait();
    let id = enqueue(k, w, 0, bitset, false, true);
    sleep(w, &[id], dl.unwrap_or(0)).map(|_| 0)
}

/// FUTEX_WAKE / WAKE_BITSET (Linux の futex_wake): 来た順に bitset の重なるものを nr まで (nr が 0 以下でも 1 つ)。
/// PI で待つものに当たれば EINVAL (それまでに起こしたものは起きる)
fn wake(uaddr: usize, private: bool, nr: i32, bitset: u32) -> Result<i64, i64> {
    if bitset == 0 {
        return Err(crate::why::inval());
    }
    let k = key_of(uaddr, private)?;
    wake_key(k, nr, Some(bitset))
}

fn wake_key(k: Key, nr: i32, bitset: Option<u32>) -> Result<i64, i64> {
    let mut n = 0i32;
    for id in queue_ids(&k) {
        let Some(e) = t().entries.get(&id) else { continue };
        if e.pi {
            return Err(crate::why::inval());
        }
        if bitset.is_some_and(|b| e.bitset & b == 0) {
            continue;
        }
        wake_entry(id);
        n += 1;
        if n >= nr {
            break;
        }
    }
    Ok(n as i64)
}

/// FUTEX_REQUEUE / CMP_REQUEUE (Linux の futex_requeue): nr_wake まで起こし、のこりを nr_requeue まで uaddr2 の
/// 行列のうしろへ。答えは起こした数と移した数の合計。CMP は uaddr の値が cmp でなければ EAGAIN
fn requeue_op(uaddr: usize, private: bool, uaddr2: usize, nr_wake: i32, nr_requeue: i32, cmp: Option<u32>) -> Result<i64, i64> {
    if nr_wake < 0 || nr_requeue < 0 {
        return Err(crate::why::inval());
    }
    let k1 = key_of(uaddr, private)?;
    let k2 = key_of(uaddr2, private)?;
    if let Some(c) = cmp {
        if get(uaddr)? != c {
            return Err(-EAGAIN);
        }
    }
    let mut n = 0i32;
    for id in queue_ids(&k1) {
        if n - nr_wake >= nr_requeue {
            break;
        }
        let Some(e) = t().entries.get(&id) else { continue };
        if e.pi {
            return Err(crate::why::inval());
        }
        n += 1;
        if n <= nr_wake {
            wake_entry(id);
        } else {
            requeue(id, k2);
        }
    }
    Ok(n as i64)
}

/// FUTEX_WAKE_OP (Linux の futex_wake_op と futex_atomic_op_inuser): uaddr2 の語を op で変え、uaddr を nr_wake まで
/// 起こし、前の値と cmparg をくらべて真なら uaddr2 も nr_wake2 まで。op が知らないものなら ENOSYS (語はそのまま)、
/// cmp が知らないものなら ENOSYS (語は変わったまま)
fn wake_op(uaddr: usize, private: bool, uaddr2: usize, nr_wake: i32, nr_wake2: i32, enc: u32) -> Result<i64, i64> {
    let k1 = key_of(uaddr, private)?;
    let k2 = key_of(uaddr2, private)?;
    let op = (enc >> 28) & 7;
    let cmp = (enc >> 24) & 15;
    let sext = |v: u32| ((v << 20) as i32) >> 20;
    let mut oparg = sext((enc >> 12) & 0xfff);
    let cmparg = sext(enc & 0xfff);
    if enc & (8 << 28) != 0 {
        oparg = 1 << (oparg & 31);
    }
    if op > 4 {
        return Err(-ENOSYS);
    }
    let w = word(uaddr2)?;
    let a = oparg as u32;
    let old = match op {
        0 => w.swap(a, Ordering::SeqCst),
        1 => w.fetch_add(a, Ordering::SeqCst),
        2 => w.fetch_or(a, Ordering::SeqCst),
        3 => w.fetch_and(!a, Ordering::SeqCst),
        _ => w.fetch_xor(a, Ordering::SeqCst),
    } as i32;
    let hit = match cmp {
        0 => old == cmparg,
        1 => old != cmparg,
        2 => old < cmparg,
        3 => old <= cmparg,
        4 => old > cmparg,
        5 => old >= cmparg,
        _ => return Err(-ENOSYS),
    };
    let mut n = wake_key(k1, nr_wake, None)?;
    if hit {
        n += wake_key(k2, nr_wake2, None)?;
    }
    Ok(n)
}

/// FUTEX_LOCK_PI / LOCK_PI2 / TRYLOCK_PI (Linux の futex_lock_pi と futex_lock_pi_atomic): 語が 0 なら自分の番号を
/// 入れて取る (OWNER_DIED はのこす)。自分のものなら EDEADLK。ほかのものなら WAITERS の印を立てて持ち主に
/// つながり (持ち主がいなければ ESRCH)、来た順に待つ。TRYLOCK は待たずに EAGAIN (印はのこる)。時間切れは
/// ETIMEDOUT (印はのこる)
fn lock_pi(uaddr: usize, private: bool, dl: Option<u64>, trylock: bool) -> Result<i64, i64> {
    let k = key_of(uaddr, private)?;
    let me = vtid(proc::current().pid);
    loop {
        let uval = word(uaddr)?.load(Ordering::SeqCst);
        if uval & TID_MASK == me {
            return Err(-EDEADLK);
        }
        if has_pi_waiter(&k) {
            // 待つものがもういる (Linux の attach_to_pi_state): 語の持ち主が覚えているものとちがえば EINVAL
            let pid = uval & TID_MASK;
            let owner = t().pi.get(&k).copied().unwrap_or(0);
            if pid != 0 && pid != vtid(owner) {
                return Err(crate::why::inval());
            }
        } else if uval & TID_MASK == 0 {
            if cmpxchg(uaddr, uval, (uval & OWNER_DIED) | me)? != uval {
                continue;
            }
            return Ok(0);
        } else {
            // はじめて待つもの (Linux の attach_to_pi_owner): 印を立ててから持ち主を探す
            if cmpxchg(uaddr, uval, uval | WAITERS)? != uval {
                continue;
            }
            let pid = uval & TID_MASK;
            let Some(owner) = crate::ns::to_global(pid).filter(|&g| proc::find_thread(g).is_some()) else {
                if get(uaddr)? != uval | WAITERS {
                    continue;
                }
                return Err(-ESRCH);
            };
            t().pi.insert(k, owner);
        }
        if trylock {
            if !has_pi_waiter(&k) {
                t().pi.remove(&k);
            }
            return Err(-EAGAIN);
        }
        if dl == Some(0) {
            if !has_pi_waiter(&k) {
                t().pi.remove(&k);
            }
            return Err(-ETIMEDOUT);
        }
        let w = new_wait();
        let id = enqueue(k, w, 0, BITSET_ANY, true, false);
        let r = sleep(w, &[id], dl.unwrap_or(0));
        if r.is_err() && !has_pi_waiter(&k) && t().pi.get(&k) != Some(&proc::current().pid) {
            t().pi.remove(&k);
        }
        // ゆずられた: もう持ち主 (語は unlock_pi が書いた)
        return r.map(|_| 0);
    }
}

/// FUTEX_UNLOCK_PI (Linux の futex_unlock_pi): 自分のものでなければ EPERM。待つものがいれば、いちばん前のものに
/// ゆずる (語は WAITERS | その番号)。いなければ語を 0 に (ほかで変わっていれば EAGAIN)
fn unlock_pi(uaddr: usize, private: bool) -> Result<i64, i64> {
    let me_g = proc::current().pid;
    let me = vtid(me_g);
    let uval = get(uaddr)?;
    if uval & TID_MASK != me {
        return Err(-EPERM);
    }
    let k = key_of(uaddr, private)?;
    let first = queue_ids(&k).into_iter().find(|id| t().entries.get(id).is_some_and(|e| e.pi));
    if let Some(id) = first {
        if t().pi.get(&k) != Some(&me_g) {
            return Err(crate::why::inval());
        }
        let new = t().entries.get(&id).map_or(0, |e| e.tid);
        let cur = cmpxchg(uaddr, uval, WAITERS | vtid(new))?;
        if cur != uval {
            return Err(if cur & TID_MASK == uval { -EAGAIN } else { -EINVAL });
        }
        wake_entry(id);
        if has_pi_waiter(&k) {
            t().pi.insert(k, new);
        } else {
            t().pi.remove(&k);
        }
        return Ok(0);
    }
    let cur = cmpxchg(uaddr, uval, 0)?;
    if cur != uval {
        return Err(-EAGAIN);
    }
    Ok(0)
}

/// futex_waitv(waiters, nr, flags, timeout, clockid) (Linux 5.16): どれかが起こされるまで待ち、その番号 (いくつも
/// なら大きいもの)。どれかの値がちがえば EAGAIN。flags は 0、clockid は REALTIME か MONOTONIC (時間があるとき)
pub fn waitv(uwaiters: usize, nr: u32, flags: u32, timeout: usize, clockid: u32) -> Result<i64, i64> {
    if flags != 0 {
        return Err(crate::why::inval());
    }
    if nr == 0 || nr > WAITV_MAX || uwaiters == 0 {
        return Err(crate::why::inval());
    }
    let dl = if timeout != 0 {
        if clockid > 1 {
            return Err(crate::why::inval());
        }
        deadline(WAIT_BITSET, if clockid == 0 { CLOCK_REALTIME } else { 0 }, timeout)?
    } else {
        None
    };
    let mut ws = Vec::new();
    for i in 0..nr as usize {
        let mut b = [0u8; WAITV_SIZE];
        proc::current().pt().copy_in(&mut b, uwaiters + i * WAITV_SIZE).ok_or(-EFAULT)?;
        let val = u64::from_le_bytes(b[..8].try_into().unwrap());
        let ua = u64::from_le_bytes(b[8..16].try_into().unwrap()) as usize;
        let fl = u32::from_le_bytes(b[16..20].try_into().unwrap());
        let reserved = u32::from_le_bytes(b[20..24].try_into().unwrap());
        if fl & !(FUTEX2_SIZE_MASK | FUTEX2_PRIVATE) != 0 || reserved != 0 || fl & FUTEX2_SIZE_MASK != FUTEX2_SIZE_U32 || val >> 32 != 0 {
            return Err(crate::why::inval());
        }
        ws.push((ua, val as u32, fl & FUTEX2_PRIVATE != 0));
    }
    let mut keys = Vec::new();
    for &(ua, _, private) in &ws {
        keys.push(key_of(ua, private)?);
    }
    let w = new_wait();
    let mut ids = Vec::new();
    for (i, &(ua, val, _)) in ws.iter().enumerate() {
        count(keys[i], true);
        core::sync::atomic::fence(Ordering::SeqCst);
        let r = get(ua);
        if r != Ok(val) {
            count(keys[i], false);
            for &id in &ids {
                unqueue(id);
            }
            return Err(r.err().unwrap_or(-EAGAIN));
        }
        ids.push(enqueue(keys[i], w, i as u32, BITSET_ANY, false, true));
    }
    if dl == Some(0) {
        for &id in &ids {
            unqueue(id);
        }
        return Err(-ETIMEDOUT);
    }
    sleep(w, &ids, dl.unwrap_or(0)).map(|i| i as i64)
}

// ---- カーネルから ----

/// スレッドが終わる (CLONE_CHILD_CLEARTID): その語を共有の op で 1 つ起こす (Linux の mm_release)
pub fn wake_exit(uaddr: usize) {
    if let Ok(k) = key_of(uaddr, false) {
        let _ = wake_key(k, 1, None);
    }
}

/// スレッドが終わる: 持っている PI の futex を、待っているいちばん前のものにゆずる (Linux の exit_pi_state_list)
pub fn exit(tid: u32) {
    let owned: Vec<Key> = t().pi.iter().filter(|&(_, &o)| o == tid).map(|(&k, _)| k).collect();
    for k in owned {
        t().pi.remove(&k);
        let Some(id) = queue_ids(&k).into_iter().find(|id| t().entries.get(id).is_some_and(|e| e.pi)) else { continue };
        let new = t().entries.get(&id).map_or(0, |e| e.tid);
        wake_entry(id);
        if has_pi_waiter(&k) {
            t().pi.insert(k, new);
        }
    }
    // 終わるスレッドの待ちは、もう起こされない
    let mine: Vec<u64> = t().entries.iter().filter(|(_, e)| e.tid == tid).map(|(&id, _)| id).collect();
    for id in mine {
        unqueue(id);
    }
}

/// 調べもの用 (/proc/strace に "kick PID"): tgid の futex で待っている (PI でない) スレッドをみな起こす。futex は
/// わけもなく起きてよい (待つ側が値を見なおす) ので、起こしが消えて眠りつづけているのかを確かめられる
pub fn kick(tgid: u32) -> usize {
    let ids: Vec<u64> = t()
        .entries
        .iter()
        .filter(|(_, e)| !e.pi && proc::find_thread(e.tid).is_some_and(|p| p.tgid == tgid))
        .map(|(&id, _)| id)
        .collect();
    for &id in &ids {
        wake_entry(id);
    }
    ids.len()
}

/// シグナルで割り込まれたときのやり直しかた (Linux の答え): LOCK_PI はいつもやり直す (ERESTARTNOINTR)、時間の
/// ある WAIT はハンドラが動けば EINTR (ERESTART_RESTARTBLOCK)、ほかは SA_RESTART なら (ERESTARTSYS)
pub fn restart_kind(op: u64, timeout: usize) -> (bool, bool) {
    let cmd = op & !(PRIVATE_FLAG | CLOCK_REALTIME);
    match cmd {
        LOCK_PI | LOCK_PI2 => (true, true),
        WAIT | WAIT_BITSET if timeout != 0 => (false, false),
        _ => (true, false),
    }
}

// 調べもの用: カーネルの中の区間ごとの回数と時間。/proc/ai/ctl に prof on で数えはじめ (0 から)、prof off で止める。
// /proc/ai/prof で読む (1 行 1 つの JSON: name、count、total_us、avg_us。合計の多い順)。
// 区間は let _p = crate::prof::span("名前"); から、その変数の終わりまで (中の区間もふくむ。眠っていた時間もふくむ)。
// 数えていないときは、区間の始めに atomic を 1 つ読むだけ
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};

pub static ON: AtomicBool = AtomicBool::new(false);

const N: usize = 64;

struct Slot {
    /// 名前 (&'static str) の場所と長さ。0 は空き
    name: AtomicUsize,
    len: AtomicUsize,
    count: AtomicU64,
    ticks: AtomicU64,
}

static SLOTS: [Slot; N] = [const { Slot { name: AtomicUsize::new(0), len: AtomicUsize::new(0), count: AtomicU64::new(0), ticks: AtomicU64::new(0) } }; N];

fn now() -> u64 {
    let c: u64;
    unsafe { core::arch::asm!("isb", "mrs {}, cntvct_el0", out(reg) c) };
    c
}

pub struct Span {
    slot: usize,
    t0: u64,
}

/// 区間の始め (数えていなければ None)
#[inline]
pub fn span(name: &'static str) -> Option<Span> {
    if !ON.load(Relaxed) {
        return None;
    }
    Some(Span { slot: slot_of(name)?, t0: now() })
}

impl Drop for Span {
    fn drop(&mut self) {
        let s = &SLOTS[self.slot];
        s.count.fetch_add(1, Relaxed);
        s.ticks.fetch_add(now().wrapping_sub(self.t0), Relaxed);
    }
}

/// 名前の場所で表を引く (なければ空きに入れる。いっぱいなら None)
fn slot_of(name: &'static str) -> Option<usize> {
    let p = name.as_ptr() as usize;
    for (i, s) in SLOTS.iter().enumerate() {
        let q = s.name.load(Relaxed);
        if q == p {
            return Some(i);
        }
        if q == 0 {
            s.len.store(name.len(), Relaxed);
            match s.name.compare_exchange(0, p, Relaxed, Relaxed) {
                Ok(_) => return Some(i),
                Err(q) if q == p => return Some(i),
                Err(_) => {}
            }
        }
    }
    None
}

/// prof on (0 から数えはじめる) / prof off
pub fn set(on: bool) {
    if on {
        for s in &SLOTS {
            s.count.store(0, Relaxed);
            s.ticks.store(0, Relaxed);
        }
    }
    ON.store(on, Relaxed);
}

/// /proc/ai/prof: 同じ名前のものはまとめて、合計の多い順に
pub fn json() -> alloc::string::String {
    use alloc::vec::Vec;
    let freq = crate::timer::freq().max(1);
    let mut rows: Vec<(&'static str, u64, u64)> = Vec::new();
    for s in &SLOTS {
        let p = s.name.load(Relaxed);
        let c = s.count.load(Relaxed);
        if p == 0 || c == 0 {
            continue;
        }
        let name = unsafe { core::str::from_utf8_unchecked(core::slice::from_raw_parts(p as *const u8, s.len.load(Relaxed))) };
        let t = s.ticks.load(Relaxed);
        match rows.iter_mut().find(|r| r.0 == name) {
            Some(r) => {
                r.1 += c;
                r.2 += t;
            }
            None => rows.push((name, c, t)),
        }
    }
    rows.sort_by(|a, b| b.2.cmp(&a.2));
    let mut out = alloc::format!("{{\"on\":{}}}\n", ON.load(Relaxed));
    for (name, c, t) in rows {
        let us = t as f64 * 1e6 / freq as f64;
        out.push_str(&alloc::format!("{{\"name\":\"{}\",\"count\":{},\"total_us\":{:.0},\"avg_us\":{:.2}}}\n", name, c, us, us / c as f64));
    }
    out
}

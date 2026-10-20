// 調べもの用: カーネルの中の区間ごとの回数と時間。/proc/ai/ctl に prof on で数えはじめ (0 から)、prof off で止める。
// /proc/ai/prof で読む (1 行 1 つの JSON):
//   part  名前の「:」の前 (kernel.json の名前。extfs:read なら extfs) ごとの self_us の合計と、数えた時間のうちの割合 (pct)。
//         区間がきちんと入れ子なら self_us を足したものが数えた時間のすべてなので、いちばん多い part がボトルネック。多い順
//   name  区間ごとの count、total_us (中の区間も、眠っていた時間もふくむ)、self_us (中の区間と眠っていた時間をのぞいた、
//         そこだけで CPU を使った時間)、wait_us (そこで眠っていた時間: 相手やディスクを待った)、avg_us (total_us / count)。
//         total_us の多い順。part の行にも wait_us (pct は CPU の時間 self_us だけで)
// 区間は let _p = crate::prof::span("part:what"); から、その変数の終わりまで。中の区間の時間は、スレッドごとの数え
// (CHILD) で引く。眠っていた時間 (proc.rs の sched で CPU を手放してから戻るまで) は、いちばん内側の開いている区間
// (INNER) の wait に足し、CHILD にも足して self からのぞく。数えていないときは、区間の始めに atomic を 1 つ読むだけ
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};

pub static ON: AtomicBool = AtomicBool::new(false);

const N: usize = 64;

struct Slot {
    /// 名前 (&'static str) の場所と長さ。0 は空き
    name: AtomicUsize,
    len: AtomicUsize,
    count: AtomicU64,
    ticks: AtomicU64,
    /// 中の区間と眠っていた時間をのぞいた時間
    own: AtomicU64,
    /// その区間がいちばん内側だったときに眠っていた時間
    wait: AtomicU64,
}

static SLOTS: [Slot; N] =
    [const { Slot { name: AtomicUsize::new(0), len: AtomicUsize::new(0), count: AtomicU64::new(0), ticks: AtomicU64::new(0), own: AtomicU64::new(0), wait: AtomicU64::new(0) } }; N];

/// スレッドごと (proc の表の番号) の、いま開いている区間の中で終わった区間の時間の合計。最後のものはスレッドの
/// ないとき (起動のとき、アイドル)
static CHILD: [AtomicU64; crate::proc::NPROC + 1] = [const { AtomicU64::new(0) }; crate::proc::NPROC + 1];

/// スレッドごとの、いま開いているいちばん内側の区間 (SLOTS の番号 + 1。0 はなし)
static INNER: [AtomicUsize; crate::proc::NPROC + 1] = [const { AtomicUsize::new(0) }; crate::proc::NPROC + 1];

fn me() -> usize {
    crate::proc::cur_index().unwrap_or(crate::proc::NPROC)
}

/// proc の表の i 番に新しいスレッドが入った (前のスレッドが区間の中で終わっていても、数えを残さない)
pub fn thread_start(i: usize) {
    if i < crate::proc::NPROC {
        CHILD[i].store(0, Relaxed);
        INNER[i].store(0, Relaxed);
    }
}

/// CPU を手放す (proc.rs の sched)。数えていなければ 0
#[inline]
pub fn off_cpu() -> u64 {
    if ON.load(Relaxed) { now() } else { 0 }
}

/// CPU に戻った: 手放していた時間を、いちばん内側の区間の wait に (self からはのぞく)
#[inline]
pub fn on_cpu(t0: u64) {
    if t0 == 0 {
        return;
    }
    let i = me();
    let inner = INNER[i].load(Relaxed);
    if inner == 0 {
        return;
    }
    let d = now().wrapping_sub(t0);
    SLOTS[inner - 1].wait.fetch_add(d, Relaxed);
    CHILD[i].fetch_add(d, Relaxed);
}

fn now() -> u64 {
    let c: u64;
    unsafe { core::arch::asm!("isb", "mrs {}, cntvct_el0", out(reg) c) };
    c
}

pub struct Span {
    slot: usize,
    t0: u64,
    /// 外の区間の、中で終わった区間の時間 (この区間が終わったら、これに自分の時間を足して戻す)
    outer: u64,
    /// 外の区間 (INNER に戻すもの)
    outer_slot: usize,
}

/// 区間の始め (数えていなければ None)
#[inline]
pub fn span(name: &'static str) -> Option<Span> {
    if !ON.load(Relaxed) {
        return None;
    }
    let slot = slot_of(name)?;
    let i = me();
    let outer = CHILD[i].swap(0, Relaxed);
    let outer_slot = INNER[i].swap(slot + 1, Relaxed);
    Some(Span { slot, t0: now(), outer, outer_slot })
}

impl Drop for Span {
    fn drop(&mut self) {
        let d = now().wrapping_sub(self.t0);
        let i = me();
        INNER[i].store(self.outer_slot, Relaxed);
        let inner = CHILD[i].swap(self.outer.wrapping_add(d), Relaxed);
        let s = &SLOTS[self.slot];
        s.count.fetch_add(1, Relaxed);
        s.ticks.fetch_add(d, Relaxed);
        s.own.fetch_add(d.saturating_sub(inner), Relaxed);
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
            s.own.store(0, Relaxed);
            s.wait.store(0, Relaxed);
        }
    }
    ON.store(on, Relaxed);
}

/// /proc/ai/prof: part ごとの合計 (self_us の多い順) と、区間ごと (同じ名前のものはまとめて、total_us の多い順)
pub fn json() -> alloc::string::String {
    use alloc::vec::Vec;
    let freq = crate::timer::freq().max(1);
    let us = |t: u64| t as f64 * 1e6 / freq as f64;
    // (名前, 回数, 合計, 自分だけ, 眠っていた)
    let mut rows: Vec<(&'static str, u64, u64, u64, u64)> = Vec::new();
    for s in &SLOTS {
        let p = s.name.load(Relaxed);
        let c = s.count.load(Relaxed);
        if p == 0 || c == 0 {
            continue;
        }
        let name = unsafe { core::str::from_utf8_unchecked(core::slice::from_raw_parts(p as *const u8, s.len.load(Relaxed))) };
        let (t, o, w) = (s.ticks.load(Relaxed), s.own.load(Relaxed), s.wait.load(Relaxed));
        match rows.iter_mut().find(|r| r.0 == name) {
            Some(r) => {
                r.1 += c;
                r.2 += t;
                r.3 += o;
                r.4 += w;
            }
            None => rows.push((name, c, t, o, w)),
        }
    }
    let mut parts: Vec<(&'static str, u64, u64, u64)> = Vec::new();
    for &(name, c, _, o, w) in &rows {
        let part = name.split(':').next().unwrap_or(name);
        match parts.iter_mut().find(|p| p.0 == part) {
            Some(p) => {
                p.1 += c;
                p.2 += o;
                p.3 += w;
            }
            None => parts.push((part, c, o, w)),
        }
    }
    let all = parts.iter().map(|p| p.2).sum::<u64>().max(1);
    parts.sort_by(|a, b| b.2.cmp(&a.2));
    rows.sort_by(|a, b| b.2.cmp(&a.2));
    let mut out = alloc::format!("{{\"on\":{}}}\n", ON.load(Relaxed));
    for (part, c, o, w) in parts {
        out.push_str(&alloc::format!("{{\"part\":\"{}\",\"count\":{},\"self_us\":{:.0},\"pct\":{:.1},\"wait_us\":{:.0}}}\n", part, c, us(o), o as f64 * 100.0 / all as f64, us(w)));
    }
    for (name, c, t, o, w) in rows {
        out.push_str(&alloc::format!(
            "{{\"name\":\"{}\",\"count\":{},\"total_us\":{:.0},\"self_us\":{:.0},\"wait_us\":{:.0},\"avg_us\":{:.2}}}\n",
            name,
            c,
            us(t),
            us(o),
            us(w),
            us(t) / c as f64
        ));
    }
    out
}

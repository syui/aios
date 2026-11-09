// 調べもの (Linux の「Kernel hacking」: tracepoint、fault-inject と、aios の prof) の core の入り口。core は trace.rs・
// prof.rs・fail.rs を名前で呼ばずに、ここを呼ぶ (Linux の include/linux/tracepoint.h の trace_*() と
// include/linux/fault-inject.h の should_fail*() が、CONFIG がなければ何もしないのと同じ)。それぞれは main.rs が起動の
// ときに register_* でつなぐ。つないでいなければ何もしない。止まっているあいだは、それぞれの場所で ON (TRACE_ON、
// PROF_ON、FAIL_ON: それぞれが /proc/ai/ctl で立てる) を 1 回見るだけ (つなぐ前と同じ重さ)。
// しくじったわけをためるほう (whylog) の入り口は why.rs。/proc/ai の JSON の文字列 (esc) もここ: core の proc・vm・vfs が
// /proc/ai に出すものも使うので
use crate::cell::Early;
use alloc::string::String;
use core::sync::atomic::{AtomicBool, Ordering::Relaxed};

/// JSON の文字列の中身 (" と \ と制御文字をエスケープ)
pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => o.push_str(&alloc::format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o
}

// ---- trace: カーネルの中の出来事の記録 (/proc/ai/trace。trace.rs。Linux の tracepoint) ----

/// 記録しているか (trace.rs が立てる)
pub static TRACE_ON: AtomicBool = AtomicBool::new(false);

/// trace.rs が受けるもの。TRACE_ON のときだけ呼ぶ (name と names_changed はいつも: ON にしたときに、もう走っているものの
/// 名前もわかるように)
pub trait Tracer: Sync {
    fn sys_enter(&self, _nr: u64, _a0: u64) {}
    fn sys_exit(&self, _nr: u64, _r: i64) {}
    /// 大きなロックなしの道 (syscall::fast) で終わったもの: 入りと出をいっしょに
    // [lockless]
    fn sys_fast(&self, _nr: u64, _a0: u64, _r: i64) {}
    /// この CPU が tid を走らせる
    fn switch(&self, _tid: u32, _comm: &[u8; 16]) {}
    fn sleep(&self, _chan: usize) {}
    /// tid を chan から起こした (起こしたのはいまのスレッド)
    fn wake(&self, _tid: u32, _chan: usize) {}
    fn irq(&self, _n: u32) {}
    fn fault(&self, _va: usize, _write: bool) {}
    /// exec と fork で: 名前を覚える
    fn name(&self, _tid: u32, _comm: &[u8; 16]) {}
    /// 名前が変わった (exec、prctl の PR_SET_NAME)
    fn names_changed(&self) {}
    fn exit(&self, _tid: u32, _code: i64) {}
    /// /proc/strace: STRACE_ON のときだけ (システムコールの終わり)
    fn strace(&self, _nr: u64, _a: &[u64], _r: Result<i64, i64>) {}
    /// /proc/sysstat: COUNT_ON のときだけ (大きなロックの道と、近道で終わったもの)
    fn count(&self, _nr: u64) {}
    // [lockless]
    fn count_fast(&self, _nr: u64) {}
}

struct NoTrace;

impl Tracer for NoTrace {}

static TRACER: Early<&'static dyn Tracer> = Early::new(&NoTrace);

/// 起動のとき (main.rs) に trace をつなぐ
pub fn register_trace(t: &'static dyn Tracer) {
    TRACER.set(t);
}

// システムコールごとに通るものは、ON を見るところだけ呼ぶ側に入れる (関数を呼ぶのも、システムコールの重さのうち)
#[inline]
pub fn trace_sys_enter(nr: u64, a0: u64) {
    if TRACE_ON.load(Relaxed) {
        TRACER.val().sys_enter(nr, a0);
    }
}

#[inline]
pub fn trace_sys_exit(nr: u64, r: i64) {
    if TRACE_ON.load(Relaxed) {
        TRACER.val().sys_exit(nr, r);
    }
}

#[inline]
// [lockless]
pub fn trace_sys_fast(nr: u64, a0: u64, r: i64) {
    if TRACE_ON.load(Relaxed) {
        TRACER.val().sys_fast(nr, a0, r);
    }
}

#[inline]
pub fn trace_switch(tid: u32, comm: &[u8; 16]) {
    if TRACE_ON.load(Relaxed) {
        TRACER.val().switch(tid, comm);
    }
}

#[inline]
pub fn trace_sleep(chan: usize) {
    if TRACE_ON.load(Relaxed) {
        TRACER.val().sleep(chan);
    }
}

#[inline]
pub fn trace_wake(tid: u32, chan: usize) {
    if TRACE_ON.load(Relaxed) {
        TRACER.val().wake(tid, chan);
    }
}

#[inline]
pub fn trace_irq(n: u32) {
    if TRACE_ON.load(Relaxed) {
        TRACER.val().irq(n);
    }
}

#[inline]
pub fn trace_fault(va: usize, write: bool) {
    if TRACE_ON.load(Relaxed) {
        TRACER.val().fault(va, write);
    }
}

pub fn trace_name(tid: u32, comm: &[u8; 16]) {
    TRACER.val().name(tid, comm);
}

pub fn trace_names_changed() {
    TRACER.val().names_changed();
}

#[inline]
pub fn trace_exit(tid: u32, code: i64) {
    if TRACE_ON.load(Relaxed) {
        TRACER.val().exit(tid, code);
    }
}

/// /proc/strace に名前が書かれているか、/proc/sysstat を読みはじめたか (trace.rs が立てる)
pub static STRACE_ON: AtomicBool = AtomicBool::new(false);
pub static COUNT_ON: AtomicBool = AtomicBool::new(false);

/// システムコールの終わり (r が答え): /proc/strace に名前が書かれていれば
#[inline]
pub fn strace(nr: u64, a: &[u64], r: Result<i64, i64>) {
    if STRACE_ON.load(Relaxed) {
        TRACER.val().strace(nr, a, r);
    }
}

#[inline]
pub fn count_sys(nr: u64) {
    if COUNT_ON.load(Relaxed) {
        TRACER.val().count(nr);
    }
}

#[inline]
// [lockless]
pub fn count_fast(nr: u64) {
    if COUNT_ON.load(Relaxed) {
        TRACER.val().count_fast(nr);
    }
}

// ---- prof: 区間ごとの回数と時間 (/proc/ai/prof。prof.rs) ----

/// 数えているか (prof.rs が立てる)
pub static PROF_ON: AtomicBool = AtomicBool::new(false);

/// prof.rs が受けるもの
pub trait Profiler: Sync {
    /// 区間の始め (数えられなければ None)。中身は prof.rs だけが知る
    fn begin(&self, _name: &'static str) -> Option<[u64; 4]> {
        None
    }
    /// 区間の終わり (begin が返したもの)
    fn end(&self, _s: &[u64; 4]) {}
    /// proc の表の i 番に新しいスレッドが入った
    fn thread_start(&self, _i: usize) {}
    /// CPU を手放す (proc.rs の sched)。0 なら on_cpu を呼ばない
    fn off_cpu(&self) -> u64 {
        0
    }
    /// CPU に戻った (off_cpu が返したもの)
    fn on_cpu(&self, _t0: u64) {}
    /// システムコール nr の区間の名前 (なければ None: 数えない)
    fn sys_name(&self, _nr: u64) -> Option<&'static str> {
        None
    }
}

struct NoProf;

impl Profiler for NoProf {}

static PROFILER: Early<&'static dyn Profiler> = Early::new(&NoProf);

/// 起動のとき (main.rs) に prof をつなぐ
pub fn register_prof(p: &'static dyn Profiler) {
    PROFILER.set(p);
}

/// 区間: let _p = debug::span("part:what"); から、その変数の終わりまで (数えていなければ None)
pub struct Span([u64; 4]);

impl Drop for Span {
    fn drop(&mut self) {
        PROFILER.val().end(&self.0);
    }
}

#[inline]
pub fn span(name: &'static str) -> Option<Span> {
    if !PROF_ON.load(Relaxed) {
        return None;
    }
    PROFILER.val().begin(name).map(Span)
}

/// システムコール nr の区間 (prof.rs の表の名前で)
#[inline]
pub fn sys_span(nr: u64) -> Option<Span> {
    if !PROF_ON.load(Relaxed) {
        return None;
    }
    let p = PROFILER.val();
    p.sys_name(nr).and_then(|n| p.begin(n)).map(Span)
}

pub fn prof_thread_start(i: usize) {
    PROFILER.val().thread_start(i);
}

#[inline]
pub fn prof_off_cpu() -> u64 {
    if PROF_ON.load(Relaxed) { PROFILER.val().off_cpu() } else { 0 }
}

#[inline]
pub fn prof_on_cpu(t0: u64) {
    if t0 != 0 {
        PROFILER.val().on_cpu(t0);
    }
}

// ---- fail: わざとしくじらせる (/proc/ai/ctl の fail、/proc/ai/fail。fail.rs。Linux の fault-inject) ----

/// 決まりがあるか (fail.rs が立てる)。ロックなしの近道 (file、statc、fastdir、unix) は、あるあいだ通らない (決まりが効くように)
pub static FAIL_ON: AtomicBool = AtomicBool::new(false);

/// fail.rs が受けるもの。FAIL_ON のときだけ呼ぶ
pub trait FaultInject: Sync {
    /// システムコール nr をしくじらせるなら、その errno (正の数)
    fn syscall(&self, _nr: u64) -> Option<i64> {
        None
    }
    /// カーネルのヒープの alloc を取れなかったことにするか (fallible は耐える区間の alloc: heap.rs)
    fn heap(&self, _fallible: bool) -> bool {
        false
    }
    /// 物理ページの alloc をしくじらせるか
    fn page(&self) -> bool {
        false
    }
}

struct NoFail;

impl FaultInject for NoFail {}

static FAULT: Early<&'static dyn FaultInject> = Early::new(&NoFail);

/// 起動のとき (main.rs) に fail をつなぐ
pub fn register_fail(f: &'static dyn FaultInject) {
    FAULT.set(f);
}

/// わざとしくじらせる決まりがあるか (ヒープの CPU ごとの置き場は、あるあいだ通らない: heap.rs)
#[inline]
pub fn fail_armed() -> bool {
    FAIL_ON.load(Relaxed)
}

/// システムコール nr をしくじらせるなら、その errno (正の数。syscall::dispatch。Linux の fail_function)
#[inline]
pub fn should_fail_syscall(nr: u64) -> Option<i64> {
    if !FAIL_ON.load(Relaxed) {
        return None;
    }
    FAULT.val().syscall(nr)
}

/// カーネルのヒープの alloc を、ふつうの道も遅い道もなしに取れなかったことにするか (Linux の should_failslab)
pub fn should_failslab(fallible: bool) -> bool {
    FAIL_ON.load(Relaxed) && FAULT.val().heap(fallible)
}

/// 物理ページの alloc をしくじらせるか (Linux の should_fail_alloc_page)
// [lockless]
pub fn should_fail_alloc_page() -> bool {
    let hit = FAIL_ON.load(Relaxed) && FAULT.val().page();
    if hit {
        PAGE_HIT.store(true, Relaxed);
    }
    hit
}

/// should_fail_alloc_page がしくじらせた (oom::on_fault が見て下ろす: わざと取れなくしたのなら、メモリは足りているので
/// 殺す相手を選ばない)
static PAGE_HIT: AtomicBool = AtomicBool::new(false);

pub fn fail_page_hit() -> bool {
    PAGE_HIT.swap(false, Relaxed)
}

// ---- SysRq (Linux の CONFIG_MAGIC_SYSRQ: handle_sysrq。sysrq.rs) ----

static SYSRQ: Early<Option<fn(u8)>> = Early::new(None);

/// 起動のとき (main.rs) に sysrq.rs をつなぐ
pub fn register_sysrq(f: fn(u8)) {
    SYSRQ.set(Some(f));
}

/// シリアルの Ctrl-P (p) と Ctrl-] (t) から
pub fn sysrq(key: u8) {
    if let Some(f) = SYSRQ.val() {
        f(key);
    }
}

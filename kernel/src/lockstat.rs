// 大きなロック (BKL) を測るもの: /proc/bkl と /proc/ai/bkl の統計の見せ方 (数えるのは core の smp.rs と cell.rs。Linux の
// kernel/locking/lockdep_proc.c の lock_stat にあたる)、/proc/ai/ctl の lockbench (ロックの部品ごとの 1 回の時間を kmsg に。
// Linux の locktorture のような、測るためだけのもの。kernel.json の spinlock、heap、mem、smp の bench)
use crate::{cell, smp, trap};
use core::sync::atomic::Ordering;

/// 持っていた時間の長いもの (名前, 回数, 時間) の多い順。fd のシステムコールは「read pipe」のように種類ごと
/// (途中で眠ったものは、眠るまでと起きてからの分)
fn hold_rows() -> alloc::vec::Vec<(alloc::string::String, u64, u64)> {
    use alloc::format;
    let mut rows = alloc::vec::Vec::new();
    let sys_name = |i: u64| crate::syscall::name(i).map_or(format!("sys{}", i), |x| x.to_ascii_lowercase());
    for (i, (n, t)) in smp::STATS.sys.iter().enumerate() {
        let n = n.load(Ordering::Relaxed);
        if n > 0 {
            rows.push((sys_name(i as u64), n, t.load(Ordering::Relaxed)));
        }
    }
    for (j, by) in smp::STATS.fd.iter().enumerate() {
        for (k, (n, t)) in by.iter().enumerate() {
            let n = n.load(Ordering::Relaxed);
            if n > 0 {
                rows.push((format!("{} {}", sys_name(crate::syscall::FD_SYS[j]), crate::file::CLASSES[k]), n, t.load(Ordering::Relaxed)));
            }
        }
    }
    for (name, slot) in [("(page fault)", &smp::STATS.fault), ("(irq)", &smp::STATS.irq), ("(kernel thread)", &smp::STATS.kthread)] {
        let n = slot.0.load(Ordering::Relaxed);
        if n > 0 {
            rows.push((name.into(), n, slot.1.load(Ordering::Relaxed)));
        }
    }
    rows.sort_by_key(|r| core::cmp::Reverse(r.2));
    rows
}

/// 数えなおす (/proc/bkl に書く、/proc/ai/ctl の bkl reset)
pub fn reset() {
    for (n, t) in smp::DEV_WAIT.iter().chain(crate::vm::FAULT_KIND.iter()) {
        n.store(0, Ordering::Relaxed);
        t.store(0, Ordering::Relaxed);
    }
    for c in &smp::STATS.cpu {
        c.wait.store(0, Ordering::Relaxed);
        c.hold.store(0, Ordering::Relaxed);
        c.locks.store(0, Ordering::Relaxed);
    }
    for (n, t) in smp::STATS.sys.iter().chain([&smp::STATS.fault, &smp::STATS.irq, &smp::STATS.kthread]).chain(smp::STATS.fd.iter().flatten()) {
        n.store(0, Ordering::Relaxed);
        t.store(0, Ordering::Relaxed);
    }
    smp::STATS.slept.store(0, Ordering::Relaxed);
    smp::FAST_FAULTS.store(0, Ordering::Relaxed);
    smp::PREEMPTS.store(0, Ordering::Relaxed);
    for c in crate::file::FAST_RW.iter().chain(crate::fastdir::FAST_NS.iter()).chain(smp::FS_WAIT.iter()) {
        c.store(0, Ordering::Relaxed);
    }
    smp::STATS.start.store(crate::timer::uptime_ns(), Ordering::Relaxed);
}

/// /proc/ai/bkl: /proc/bkl と同じものを 1 行の JSON で (AI が読む)
pub fn json() -> alloc::string::String {
    use alloc::format;
    use alloc::string::String;
    use alloc::vec::Vec;
    let ms = |ns: u64| ns as f64 / 1e6;
    let span = crate::timer::uptime_ns().saturating_sub(smp::STATS.start.load(Ordering::Relaxed)).max(1);
    let pct = |ns: u64| ns as f64 * 100.0 / span as f64;
    let (mut tw, mut th) = (0, 0);
    let mut cpus = Vec::new();
    for c in smp::STATS.cpu.iter().take(smp::online()) {
        let (w, h) = (c.wait.load(Ordering::Relaxed), c.hold.load(Ordering::Relaxed));
        tw += w;
        th += h;
        cpus.push(format!("{{\"wait_pct\":{:.1},\"hold_pct\":{:.1},\"locks\":{}}}", pct(w), pct(h), c.locks.load(Ordering::Relaxed)));
    }
    let rows = hold_rows();
    let top: Vec<String> = rows.iter().take(25).map(|(name, n, t)| format!("{{\"name\":\"{}\",\"n\":{},\"ms\":{:.1},\"avg_us\":{:.1}}}", name, n, ms(*t), *t as f64 / 1e3 / *n as f64)).collect();
    let dev: Vec<String> = ["disk", "gpu", "sched", "wake_to_run"].iter().enumerate().map(|(i, name)| format!("\"{}\":{{\"n\":{},\"ms\":{:.1}}}", name, smp::DEV_WAIT[i].0.load(Ordering::Relaxed), ms(smp::DEV_WAIT[i].1.load(Ordering::Relaxed)))).collect();
    let kinds = ["", "perm", "shared_file", "kernel_page", "swap", "page_cache", "anon_new", "file_new", "cow_copy", "page_cache_miss", "around"];
    let faults: Vec<String> = crate::vm::FAULT_KIND.iter().enumerate().skip(1).map(|(i, (n, t))| format!("\"{}\":{{\"n\":{},\"ms\":{:.1}}}", kinds[i], n.load(Ordering::Relaxed), ms(t.load(Ordering::Relaxed)))).collect();
    let fr: Vec<u64> = crate::file::FAST_RW.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    format!(
        "{{\"span_s\":{:.2},\"wait_pct\":{:.1},\"hold_pct\":{:.1},\"cpus\":[{}],\"top\":[{}],\"dev\":{{{}}},\"faults\":{{{}}},\"fast_pipe\":{{\"ok\":{},\"table_busy\":{}}},\"fast_faults\":{},\"slept\":{},\"hold_sample\":{}}}\n",
        span as f64 / 1e9,
        pct(tw),
        pct(th),
        cpus.join(","),
        top.join(","),
        dev.join(","),
        faults.join(","),
        fr[0],
        fr[3],
        smp::FAST_FAULTS.load(Ordering::Relaxed),
        smp::STATS.slept.load(Ordering::Relaxed),
        smp::SAMPLE
    )
}

/// /proc/bkl
pub fn text() -> alloc::string::String {
    use alloc::format;
    use alloc::string::String;
    use alloc::vec::Vec;
    let ms = |ns: u64| ns as f64 / 1e6;
    let span = crate::timer::uptime_ns().saturating_sub(smp::STATS.start.load(Ordering::Relaxed)).max(1);
    let pct = |ns: u64| ns as f64 * 100.0 / span as f64;
    let mut s = String::new();
    s.push_str(&format!("# 大きなロック (BKL)。{:.1} 秒のあいだ (echo > /proc/bkl で数えなおす)。hold は {} 回に 1 回ほど測って {} 倍した見つもり (待ったものはいつも)
", span as f64 / 1e9, smp::SAMPLE, smp::SAMPLE));
    s.push_str("cpu      wait (待った)          hold (持っていた)        locks
");
    let (mut tw, mut th) = (0, 0);
    for (i, c) in smp::STATS.cpu.iter().enumerate().take(smp::online()) {
        let (w, h) = (c.wait.load(Ordering::Relaxed), c.hold.load(Ordering::Relaxed));
        tw += w;
        th += h;
        s.push_str(&format!("{:<4} {:>10.1} ms {:>5.1}%  {:>10.1} ms {:>5.1}%  {:>10}
", i, ms(w), pct(w), ms(h), pct(h), c.locks.load(Ordering::Relaxed)));
    }
    s.push_str(&format!("all  {:>10.1} ms {:>5.1}%  {:>10.1} ms {:>5.1}%   (1 CPU = 100%)

", ms(tw), pct(tw), ms(th), pct(th)));
    let rows = hold_rows();
    s.push_str("hold の長いもの            回数       合計       平均
");
    for (name, n, t) in rows.iter().take(25) {
        s.push_str(&format!("{:<20} {:>10} {:>9.1} ms {:>8.1} us
", name, n, ms(*t), *t as f64 / 1e3 / *n as f64));
    }
    for (i, name) in ["ディスク", "画面", "スケジューラ", "起こされてから走るまで"].iter().enumerate() {
        let (n, t) = (smp::DEV_WAIT[i].0.load(Ordering::Relaxed), smp::DEV_WAIT[i].1.load(Ordering::Relaxed));
        s.push_str(&format!("デバイスを待った ({}): {} 回 {:.1} ms\n", name, n, ms(t)));
    }
    s.push_str("ページフォルトの種類 (回数 / ms):");
    for (i, (n, t)) in crate::vm::FAULT_KIND.iter().enumerate().skip(1) {
        s.push_str(&format!(" {}:{}/{:.1}", i, n.load(Ordering::Relaxed), ms(t.load(Ordering::Relaxed))));
    }
    s.push('\n');
    let fr: Vec<u64> = crate::file::FAST_RW.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    s.push_str(&format!("ディスクを眠って待った読み: {} 回 (大きなロックを放して)、書かれたのでやりなおした読み: {} 回\n", smp::DISK_SLEPT.load(Ordering::Relaxed), crate::file::IO_RETRIES.load(Ordering::Relaxed)));
    let (paths, missing) = crate::vfs::paths_len();
    s.push_str(&format!("パスの覚え: {} 個 (うちないもの {})、覚えから答えた {} 回\n", paths, missing, crate::vfs::PATHS_HIT.load(Ordering::Relaxed)));
    s.push_str(&format!("ロックなしの stat (statc.rs): {} 回 (ふつうの道へ {})\n", crate::statc::HITS.load(Ordering::Relaxed), crate::statc::MISSES.load(Ordering::Relaxed)));
    let uf: Vec<u64> = smp::UNIX_FAST.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    s.push_str(&format!("AF_UNIX のロックなしの読み書き: 読んだ {} 書いた {} (ふつうの道へ {})\n", uf[0], uf[1], uf[2]));
    s.push_str(&format!("パイプのロックなしの読み書き: {} (うち起こしを頼んだ {}、空で終わりか EAGAIN を返した {}。ふつうの道へ: 大きさ {} 表を変えている途中 {} パイプでない {} ページ {} いっぱい {} 空き {} 空 {} 写せない {})\n", fr[0], fr[6], fr[7], fr[2], fr[3], fr[4], fr[5], fr[8], fr[9], fr[10], fr[11]));
    let fo: Vec<u64> = crate::file::FAST_OPEN.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    s.push_str(&format!("ロックなしの open / close (file の棚): 棚から開いた {} 新しく開いた {} ない (ENOENT) と答えた {} 棚にしまった {} (ふつうの道へ: statc にない {} 棚になく新しくも開けない {} 開けない形 {} しまえない {})\n", fo[0], fo[7], fo[1], fo[2], fo[3], fo[4], fo[5], fo[6]));
    let ns: Vec<u64> = crate::fastdir::FAST_NS.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    let fw = [smp::FS_WAIT[0].load(Ordering::Relaxed), smp::FS_WAIT[1].load(Ordering::Relaxed)];
    s.push_str(&format!("ロックなしの作る・消す (fastdir、ext4 のロックで): 作った {} 消した {} (ふつうの道へ: 形 {} 親が statc にない {} ext4 が断った {})。ext4 のロックを待った {} 回 {:.1} ms\n", ns[0], ns[1], ns[2], ns[3], ns[4], fw[0], fw[1] as f64 / 1e6));
    let wf: Vec<u64> = crate::vm::WRITE_FAST.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    s.push_str(&format!("ファイルのロックなしの pwrite (vm::write_fast): {} (ふつうの道へ: 許しがない・古い {} 大きさの外かキャッシュにない {} ユーザーのページが写っていない {} 書いたページがたまった {})\n", wf[0], wf[1], wf[2], wf[3], wf[4]));
    let fa: Vec<u64> = crate::file::FAST_WRITE.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    s.push_str(&format!("ファイルのロックなしの書きたし (ExtFs::fast_append、ext4 のロックで): {} (ふつうの道へ: ファイルの形 {} 書き出すころ {} extent がつづかない {} 見張りかほか {})\n", fa[0], fa[1], fa[2], fa[3], fa[4]));
    let fd: Vec<u64> = crate::file::FAST_READ.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    s.push_str(&format!("ファイルのロックなしの read: {} (ふつうの道へ: 大きさ {} IN_ACCESS の見張り {} fd の表を分けている {} 開いたファイルを分けている {} ext4 のファイルでない {} キャッシュにない {})\n", fd[0], fd[1], fd[2], fd[3], fd[4], fd[5], fd[6]));
    s.push_str(&format!("(途中で眠ったもの: {}。ロックなしで片づけたページフォルト: {}。起こされたものにゆずらせた数: {})
", smp::STATS.slept.load(Ordering::Relaxed), smp::FAST_FAULTS.load(Ordering::Relaxed), smp::PREEMPTS.load(Ordering::Relaxed)));
    // BKL が守るもの (cell::Bkl) を、BKL を持たずに触った場所 (あってはいけない。test/vm-check.sh が見る)
    s.push_str(&unlocked_text());
    s
}


/// /proc/bkl の終わりに: BKL を持たずに触った場所と回数 (なければ空。selftest のものは出さない)
fn unlocked_text() -> alloc::string::String {
    use core::fmt::Write;
    let mut s = alloc::string::String::new();
    for (site, n) in cell::SITE.iter().zip(&cell::HITS) {
        let p = site.load(core::sync::atomic::Ordering::Acquire);
        if p.is_null() {
            break;
        }
        let at = unsafe { &*p };
        if at.file().ends_with("cell.rs") {
            continue;
        }
        let _ = writeln!(s, "unlocked {}:{} {}", at.file(), at.line(), n.load(core::sync::atomic::Ordering::Relaxed));
    }
    let more = cell::MORE.load(core::sync::atomic::Ordering::Relaxed);
    if more > 0 {
        let _ = writeln!(s, "unlocked (more places) {}", more);
    }
    s
}


/// /proc/ai/ctl の lockbench: ロックの部品ごとの 1 回の時間を kmsg に (どれも 10 万回の平均。memcpy と memset は 1000 回と 1 万回)。
/// 大きなロックを持って呼ぶ (それを放して取りなおすものも測る)
pub fn run() {
    use core::hint::black_box;
    let n = 100_000u64;
    let cnt = || {
        let c: u64;
        unsafe { core::arch::asm!("isb", "mrs {}, cntvct_el0", out(reg) c) };
        c
    };
    let ns = |t: u64| t as f64 * 1e9 / crate::timer::freq() as f64 / n as f64;
    let l = crate::spinlock::SpinLock::new(0u64);
    let t0 = cnt();
    for _ in 0..n {
        let g = l.lock();
        black_box(&*g);
    }
    let lock = ns(cnt() - t0);
    let t0 = cnt();
    for i in 0..n {
        black_box(alloc::boxed::Box::new(i));
    }
    let heap = ns(cnt() - t0);
    let t0 = cnt();
    for _ in 0..n {
        black_box(crate::timer::uptime_ns());
    }
    let clock = ns(cnt() - t0);
    // 物理ページ (kalloc): 1 枚取ってすぐ返す (すぐまた取るもの)、1024 枚取ってから返す (大きなかたまりを分けて、
    // まとめなおす)。どちらも 1 枚あたり
    let t0 = cnt();
    for _ in 0..n {
        if let Some(p) = crate::kalloc::alloc_dirty() {
            crate::kalloc::free(black_box(p));
        }
    }
    let page = ns(cnt() - t0);
    let t0 = cnt();
    let mut got = 0;
    for _ in 0..n / 1024 {
        let mut head: *mut usize = core::ptr::null_mut();
        for _ in 0..1024 {
            let Some(p) = crate::kalloc::alloc_dirty() else { break };
            unsafe { *(p as *mut usize) = head as usize };
            head = p as *mut usize;
            got += 1;
        }
        while !head.is_null() {
            let next = unsafe { *head } as *mut usize;
            crate::kalloc::free(head as *mut u8);
            head = next;
        }
    }
    let pages = (cnt() - t0) as f64 * 1e9 / crate::timer::freq() as f64 / got.max(1) as f64;
    // memcpy と memset (mem.S): 64 KiB を写す (ソケットやパイプの 1 回の書き読みの大きさ。src が 1 バイトずれたものも)、
    // 4 KiB を 0 にする (ページを 0 にする)。1 回あたり
    let per = |t: u64, k: u64| t as f64 * 1e9 / crate::timer::freq() as f64 / k as f64;
    let mut src = alloc::vec![1u8; 64 * 1024 + 64];
    let mut dst = alloc::vec![0u8; 64 * 1024 + 64];
    let k = 1000;
    let t0 = cnt();
    for _ in 0..k {
        dst[..64 * 1024].copy_from_slice(black_box(&src[..64 * 1024]));
        black_box(&mut dst);
    }
    let copy = per(cnt() - t0, k);
    let t0 = cnt();
    for _ in 0..k {
        dst[..64 * 1024].copy_from_slice(black_box(&src[1..64 * 1024 + 1]));
        black_box(&mut dst);
    }
    let copy_odd = per(cnt() - t0, k);
    let k = 10_000;
    let t0 = cnt();
    for _ in 0..k {
        black_box(&mut src)[..4096].fill(0);
    }
    let zero = per(cnt() - t0, k);
    let bkl = bkl();
    crate::println!(
        "lockbench: SpinLock lock+unlock {:.1} ns, heap alloc+free {:.1} ns, BKL unlock+lock {:.1} ns, uptime_ns {:.1} ns, page alloc+free {:.1} ns, page 1024 alloc+free {:.1} ns, memcpy 64KiB {:.1} ns, memcpy 64KiB src+1 {:.1} ns, memset0 4KiB {:.1} ns",
        lock, heap, bkl, clock, page, pages, copy, copy_odd, zero
    );
    trap();
}


/// lockbench の 2 行め: システムコール 1 回の、ベクタの asm (レジスタを積んでおろす、eret) のほかの重さ (ns)。
/// 偽の TrapFrame で trap_handler をそのまま呼ぶ: 速い道 (getppid、ロックなし)、大きなロックを取る道 (umask)。
/// その中から dispatch だけ (umask)、ユーザーへ戻るまえ (resched、killed、signal) だけ。ゆれるので 7 回まわして速いもの
fn trap() {
    use core::hint::black_box;
    let n = 10_000u64;
    let cnt = || {
        let c: u64;
        unsafe { core::arch::asm!("isb", "mrs {}, cntvct_el0", out(reg) c) };
        c
    };
    let ns = |t: u64| t as f64 * 1e9 / crate::timer::freq() as f64 / n as f64;
    let esr0 = trap::esr();
    // trap_handler は svc を esr で見分ける
    unsafe { core::arch::asm!("msr esr_el1, {}", in(reg) trap::EC_SVC64 << 26 | 1 << 25) };
    let umask = crate::proc::current().files().umask;
    let mut tf = trap::TrapFrame::zeroed();
    let sys = |tf: &mut trap::TrapFrame, nr: u64| {
        tf.x[8] = nr;
        tf.x[0] = umask as u64;
    };
    const GETPPID: u64 = 173;
    const UMASK: u64 = 166;
    let mut best = [f64::MAX; 6];
    for _ in 0..7 {
        let t0 = cnt();
        for _ in 0..n {
            black_box(trap::esr());
        }
        best[4] = best[4].min(ns(cnt() - t0));
        let t0 = cnt();
        for _ in 0..n {
            black_box(crate::smp::id());
        }
        best[5] = best[5].min(ns(cnt() - t0));
        // trap_handler は自分でロックを取るので、いったん放す
        crate::smp::unlock();
        let t0 = cnt();
        for _ in 0..n {
            sys(&mut tf, GETPPID);
            trap::trap_handler(black_box(&mut tf), trap::EL0_SYNC);
        }
        best[0] = best[0].min(ns(cnt() - t0));
        let t0 = cnt();
        for _ in 0..n {
            sys(&mut tf, UMASK);
            trap::trap_handler(black_box(&mut tf), trap::EL0_SYNC);
        }
        best[1] = best[1].min(ns(cnt() - t0));
        crate::smp::lock();
        let t0 = cnt();
        for _ in 0..n {
            sys(&mut tf, UMASK);
            black_box(crate::syscall::dispatch(black_box(&mut tf)));
        }
        best[2] = best[2].min(ns(cnt() - t0));
        let t0 = cnt();
        for _ in 0..n {
            if crate::proc::take_resched() {
                crate::proc::yield_now();
            }
            crate::proc::check_killed();
            crate::signal::deliver(black_box(&mut tf), None);
        }
        best[3] = best[3].min(ns(cnt() - t0));
    }
    crate::proc::current().files().umask = umask;
    unsafe { core::arch::asm!("msr esr_el1, {}", in(reg) esr0) };
    crate::println!(
        "trapbench: getppid (fast) {:.1} ns, umask (BKL) {:.1} ns, dispatch {:.1} ns, exit {:.1} ns, mrs esr {:.1} ns, cpu id {:.1} ns (hold timed 1 in {})",
        best[0],
        best[1],
        best[2],
        best[3],
        best[4],
        best[5],
        crate::smp::SAMPLE
    );
}


/// 大きなロックを放して取りなおす 1 回の時間 (ns、10 万回の平均)。持って呼ぶ
fn bkl() -> f64 {
    let n = 100_000u64;
    let cnt = || {
        let c: u64;
        unsafe { core::arch::asm!("isb", "mrs {}, cntvct_el0", out(reg) c) };
        c
    };
    let t0 = cnt();
    for _ in 0..n {
        crate::smp::unlock();
        crate::smp::lock();
    }
    (cnt() - t0) as f64 * 1e9 / crate::timer::freq() as f64 / n as f64
}


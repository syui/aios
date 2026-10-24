// メモリが足りないとき (OOM): ヒープが予備 (heap.rs の RESERVE) にまで手をつけたら (heap::EMERGENCY)、次のシステム
// コールの入り口 (syscall::dispatch → check) で、いちばんメモリ (rss) を持っているプロセスを SIGKILL で終わらせて空ける。
// Linux の OOM killer と同じ考え (そこでは badness で選ぶ)。alloc の中では殺せない (カーネルの表を変えている途中かも
// しれない) ので、安全なところまで待つ。init (pid 1) は殺さない。前の相手がまだ終わっていなければ待つ。
// ふつうの道に空きが戻れば (heap::recovered) EMERGENCY を下ろす。殺した数は /proc/ai/mem の heap の oom_kills
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering::Relaxed};

static VICTIM: AtomicU32 = AtomicU32::new(0);
static KILLS: AtomicUsize = AtomicUsize::new(0);
/// 殺せるものがなかった (init だけ): 1 度だけ出す
static NOBODY: AtomicBool = AtomicBool::new(false);

pub fn kills() -> usize {
    KILLS.load(Relaxed)
}

/// システムコールの入り口で。heap::EMERGENCY が立っていなければすぐ戻る (ヒープは使わない: 足りないときに呼ばれる)
#[inline]
pub fn check() {
    if crate::heap::EMERGENCY.load(Relaxed) {
        emergency();
    }
}

#[cold]
fn emergency() {
    // わざと (fail ... oom) のときは、空きがあっても 1 度は殺す (この道を確かめるため)
    if !crate::heap::FORCED.swap(false, Relaxed) && crate::heap::recovered() {
        crate::heap::EMERGENCY.store(false, Relaxed);
        VICTIM.store(0, Relaxed);
        NOBODY.store(false, Relaxed);
        crate::println!("heap: back to normal (used {} of {})", crate::heap::used(), crate::heap::size());
        return;
    }
    let v = VICTIM.load(Relaxed);
    if v != 0 && crate::proc::find_where(|p| p.tgid == v).is_some() {
        return; // まだ終わっていない (終われば持ち物が戻る)
    }
    // いちばんメモリを持っているもの (init のほか)
    let mut best: Option<(u32, usize, [u8; 16])> = None;
    crate::proc::each_leader(|p| {
        if p.pid == 1 || p.kthread {
            return;
        }
        let Some(mm) = p.mm.as_ref() else { return };
        let rss = mm.get().pt.resident();
        if best.is_none_or(|b| rss > b.1) {
            best = Some((p.tgid, rss, p.comm));
        }
    });
    let Some((tgid, rss, comm)) = best else {
        if !NOBODY.swap(true, Relaxed) {
            crate::println!("oom: nothing to kill (only init); the heap stays on the reserve");
        }
        return;
    };
    let (_, _, reserve, _) = crate::heap::stats_more();
    let name = core::str::from_utf8(&comm).unwrap_or("?").trim_end_matches('\0');
    crate::println!(
        "oom: killed pid {} ({}) holding {} pages, to free memory: heap used {} of {}, {} bytes of the reserve in use",
        tgid,
        name,
        rss,
        crate::heap::used(),
        crate::heap::size(),
        reserve
    );
    crate::proc::kill_group(tgid, crate::signal::SIGKILL);
    VICTIM.store(tgid, Relaxed);
    KILLS.fetch_add(1, Relaxed);
}

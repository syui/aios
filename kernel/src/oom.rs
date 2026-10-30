// メモリが足りないとき (OOM): ヒープが予備 (heap.rs の RESERVE) にまで手をつけたら (heap::EMERGENCY)、次のシステム
// コールの入り口 (syscall::dispatch → check) で、いちばんメモリ (rss) を持っているプロセスを SIGKILL で終わらせて空ける。
// Linux の OOM killer と同じ考え (そこでは badness で選ぶ)。alloc の中では殺せない (カーネルの表を変えている途中かも
// しれない) ので、安全なところまで待つ。init (pid 1) は殺さない。前の相手がまだ終わっていなければ待つ。
// ふつうの道に空きが戻れば (heap::recovered) EMERGENCY を下ろす。殺した数は /proc/ai/mem の heap の oom_kills。
// ページフォルトでページが取れなかったときも同じように選んで殺す (on_fault。困ったスレッドはやりなおす)
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
    if dying().is_some() {
        return; // まだ終わっていない (終われば持ち物が戻る)
    }
    let Some(best) = biggest() else {
        if !NOBODY.swap(true, Relaxed) {
            crate::println!("oom: nothing to kill (only init); the heap stays on the reserve");
        }
        return;
    };
    let (_, _, reserve, _) = crate::heap::stats_more();
    kill(best, format_args!("heap used {} of {}, {} bytes of the reserve in use", crate::heap::used(), crate::heap::size(), reserve));
}

/// ページフォルトでページが取れなかった (回収してもだめだった) とき: Linux の pagefault_out_of_memory と同じく、
/// いちばんメモリを持っているもの (init のほか) を SIGKILL で終わらせて空ける。困っているスレッドは、選ばれたのが
/// 自分のプロセスでなければ、少し待って同じ命令からやりなおす (true)。前に殺したものがまだ終わっていなければ、それを待つ。
/// 前は困ったスレッドをそのまま SIGKILL にしていたので、大きなもの (モデルを読む llama-server) がメモリを使いきると、
/// たまたまスタックをのばそうとした init が殺されてカーネルが止まった (init exited with status 9)。
/// init は殺さない: ほかに殺せるものがなくても、init のフォルトは待ってやりなおす。
/// メモリが足りているのに取れなかったとき: わざと取れなくした (fail ... page) なら、いままでどおり困ったものだけを
/// 殺す (init はやりなおす)。ほかが返したあとなら、だれも殺さずにやりなおす
pub fn on_fault() -> bool {
    let me = crate::proc::current().tgid;
    if crate::fail::take_page_hit() {
        return me == 1;
    }
    if crate::kalloc::nfree() >= crate::sysctl::MIN_FREE_PAGES.load(Relaxed) {
        return true;
    }
    if let Some(v) = dying() {
        return v != me;
    }
    let Some(best) = biggest() else {
        if !NOBODY.swap(true, Relaxed) {
            crate::println!("oom: a page fault found no free page and there is nothing to kill (only init)");
        }
        return me == 1;
    };
    let victim = best.0;
    kill(best, format_args!("a page fault of pid {} found no free page", crate::proc::current().pid));
    victim != me
}

/// 前に殺したもので、まだ終わっていないもの
fn dying() -> Option<u32> {
    let v = VICTIM.load(Relaxed);
    (v != 0 && crate::proc::find_where(|p| p.tgid == v).is_some()).then_some(v)
}

/// いちばんメモリ (rss) を持っているもの (init とカーネルのスレッドのほか): (tgid, rss, comm)
fn biggest() -> Option<(u32, usize, [u8; 16])> {
    let mut best: Option<(u32, usize, [u8; 16])> = None;
    crate::proc::each_leader(|p| {
        if p.pid == 1 || p.kthread {
            return;
        }
        let Some(mm) = p.mm.as_ref() else { return };
        let pt = &mm.get().pt;
        let _h = pt.hold();
        let rss = pt.resident();
        if best.is_none_or(|b| rss > b.1) {
            best = Some((p.tgid, rss, p.comm));
        }
    });
    best
}

fn kill((tgid, rss, comm): (u32, usize, [u8; 16]), why: core::fmt::Arguments) {
    let name = core::str::from_utf8(&comm).unwrap_or("?").trim_end_matches('\0');
    crate::println!("oom: killed pid {} ({}) holding {} pages, to free memory: {}", tgid, name, rss, why);
    crate::proc::kill_group(tgid, crate::signal::SIGKILL);
    VICTIM.store(tgid, Relaxed);
    KILLS.fetch_add(1, Relaxed);
}

// 複数 CPU
//
// カーネルは大きなロック (BKL) 1 つで守る: カーネルの中を走れるのはいつも 1 つの CPU だけで、
// ユーザーのプログラムはすべての CPU で同時に動く (Linux 2.0 のころと同じ)。
//   EL0 から例外で入ったらロックを取り、EL0 へ戻る直前に放す (trap.rs、forkret)。
//   スケジューラはロックを持ったまま回し、することがなければ放して割り込みを待つ。
// これでいままでの 1 CPU 向けのコード (static mut、RefCell) はそのまま使える。
// ロックなしで通すもの: 自分のことを読むだけのシステムコール (syscall::fast) と、自分だけの無名の
// 領域のページフォルト (vm.rs fast_fault。表を変える側は mutating() でそれを止めて待つ)、コンソールの
// 送信の割り込み (uart.rs intr_fast。輪から次の文字を出すだけ)。
//
// 2 つめからの CPU は DTB の cpu ノードの enable-method で起こす:
//   psci        PSCI の CPU_ON (QEMU virt、UEFI)
//   spin-table  cpu-release-addr に入口を書いて sev (ラズパイのファームウェア)
// 起きた CPU は boot.rs と同じページ表で MMU を入れ、自分のスタックでスケジューラに入る。
// TPIDR_EL1 に CPU の番号を入れておく。
use crate::memlayout::{p2v, v2p};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

pub const MAXCPU: usize = 8;
const STACK_SIZE: usize = 16 * 1024;

#[repr(C, align(16))]
struct Stacks([[u8; STACK_SIZE]; MAXCPU]);

#[unsafe(no_mangle)]
static secondary_stacks: crate::cell::Raw<Stacks> = crate::cell::Raw::new(Stacks([[0; STACK_SIZE]; MAXCPU]));

/// 動いている CPU の数
static ONLINE: AtomicUsize = AtomicUsize::new(1);
/// CPU の番号ごとの MPIDR の Aff0 (CPU 間の割り込みの宛先)
static TARGET: [AtomicUsize; MAXCPU] = [const { AtomicUsize::new(0) }; MAXCPU];

/// 仕事がなくて眠っている CPU (ビット)
static IDLE: AtomicUsize = AtomicUsize::new(0);

/// この CPU は眠る / 起きた (スケジューラが、大きなロックを持ったまま呼ぶ)
pub fn set_idle(on: bool) {
    let bit = 1 << id();
    if on {
        IDLE.fetch_or(bit, Ordering::AcqRel);
    } else {
        IDLE.fetch_and(!bit, Ordering::AcqRel);
    }
}

/// 眠っている CPU を起こす (動けるプロセスができた)。sev ではなく割り込みで:
/// Mac の Hypervisor.framework (HVF) では wfe が眠らず sev も届かないので、ひまな CPU は wfi で眠っている
/// 眠っている CPU を 1 つ起こす。起こしたものがあれば true
pub fn wake_idle() -> bool {
    wake_idle_n(1)
}

/// 眠っている CPU を n まで起こす (動けるようになったものの数だけ)。起こしたものがあれば true。
/// みな起こすと、起きた CPU がいっせいに大きなロックを取りあい、1 つずつ表を見て「仕事がない」と眠りなおす
/// あいだ、起こしたものが待たされる (QEMU の TCG で、起こしてから走るまでが 2.4 ms だった)
pub fn wake_idle_n(n: usize) -> bool {
    let idle = IDLE.load(Ordering::Acquire) & !(1 << id());
    let mut sent = 0;
    for cpu in 0..online() {
        if sent >= n {
            break;
        }
        if idle & (1 << cpu) != 0 {
            crate::irq::send_ipi(TARGET[cpu].load(Ordering::Relaxed));
            sent += 1;
        }
    }
    sent > 0
}

/// ほかの CPU に、EL0 から戻ってくるように知らせる (殺された、シグナルが来た)
pub fn kick(cpu: usize) {
    if cpu != id() && cpu < online() {
        crate::irq::send_ipi(TARGET[cpu].load(Ordering::Relaxed));
    }
}

/// いまの CPU の番号 (0 から)
// [lockless]
pub fn id() -> usize {
    let v: usize;
    unsafe { core::arch::asm!("mrs {}, tpidr_el1", out(reg) v) };
    v
}

// [lockless]
pub fn online() -> usize {
    ONLINE.load(Ordering::Acquire)
}

// ---- 大きなロック ----

/// 持っている CPU の番号 + 1 (0 なら空き)
static BKL: AtomicUsize = AtomicUsize::new(0);
/// チケット: 次に配る番号と、いま持てる番号。来た順に持つ (早い者勝ちだと、放したばかりの CPU が
/// すぐまた取り、ほかの CPU (起こされた音のスレッドなど) がいつまでも取れないことがある)
static NEXT: AtomicUsize = AtomicUsize::new(0);
static SERVING: AtomicUsize = AtomicUsize::new(0);

/// CPU ごとの、いま走らせているプロセスのページ表 (vm::PageTable のアドレス。スケジューラの中や眠っているあいだは 0)。
/// スケジューラがプロセスへ切りかえるときに置き、もどったときに 0 にする (set_user_pt)
static USER_PT: [AtomicUsize; MAXCPU] = [const { AtomicUsize::new(0) }; MAXCPU];
/// 大きなロックを持っている CPU が、プロセスの中で使っているページ表 (USER_PT。なければ 0)。スレッドで分けた表の大きな
/// ロックなしの mmap / munmap (vm::PageTable::write_enter) は、これが自分の表なら入らない。大きなロックの側は、これを
/// 置いてから、その表のロックなしの書き手が終わるのを待つ (vm::wait_writer)。どちらかがかならず相手を見る (SeqCst)。
/// これで、同じ表のスレッドが大きなロックを持って領域の木 (vmas) やページ表を読んだり、物理アドレスを引いてユーザーの
/// ページに書いたりしているあいだに、ほかのスレッドがロックなしで領域を外してページを返すことはない
static BKL_PT: AtomicUsize = AtomicUsize::new(0);

/// CPU ごとの、いま走らせているプロセスの fd の表 (proc::Shared<Files> の id。スケジューラの中や眠っているあいだは 0)。
/// USER_PT と同じところで置く (set_user_files。exec や unshare で表が入れかわったときも)
static USER_FILES: [AtomicUsize; MAXCPU] = [const { AtomicUsize::new(0) }; MAXCPU];
/// 大きなロックを持っている CPU の fd の表 (USER_FILES)。スレッドで分けた表の大きなロックなしの close
/// (file::fast_close_shared) は、表の箱 (FILES_WRITING) に印を立ててから、これが自分の表なら入らない。大きなロックの側は、
/// これを置いてから、その箱の印が消えるのを待つ。大きなロックの中で表を読む人 (file_of で OpenFile を写す) は門を
/// 通らないので、同じ表のスレッドが大きなロックを持っているあいだは、ロックなしで fd を外さない
static BKL_FILES: AtomicUsize = AtomicUsize::new(0);
/// fd の表のロックなしの close が表を変えている印 (表の id のハッシュの箱。ぶつかれば、ほかの表の大きなロックの側が
/// 少し待つだけ)
pub static FILES_WRITING: [AtomicUsize; 64] = [const { AtomicUsize::new(0) }; 64];

/// fd の表 (id) の箱
// [lockless]
pub fn files_box(files: usize) -> &'static AtomicUsize {
    &FILES_WRITING[((files >> 6) ^ (files >> 12)) % FILES_WRITING.len()]
}

/// 大きなロックの持ち主がいまのプロセスの fd の表を使う: BKL_FILES を置いて、その表のロックなしの close を待つ
fn own_user_files() {
    let f = USER_FILES[id()].load(Ordering::Relaxed);
    BKL_FILES.store(f, Ordering::SeqCst);
    if f != 0 {
        core::sync::atomic::fence(Ordering::SeqCst);
        let b = files_box(f);
        while b.load(Ordering::SeqCst) != 0 {
            core::hint::spin_loop();
        }
    }
}

/// スケジューラがプロセスへ切りかえる・もどった (0)、exec や unshare で fd の表が入れかわった。大きなロックを持って呼ぶ
pub fn set_user_files(files: usize) {
    USER_FILES[id()].store(files, Ordering::Relaxed);
    own_user_files();
}

/// 大きなロックを持っている CPU が、いま使っている fd の表 (file::fast_close_shared が見る)
// [lockless]
pub fn bkl_files() -> usize {
    BKL_FILES.load(Ordering::SeqCst)
}

/// 大きなロックの持ち主がいまのプロセスの表を使う: BKL_PT を置いて、その表のロックなしの書き手を待つ
fn own_user_pt() {
    let pt = USER_PT[id()].load(Ordering::Relaxed);
    BKL_PT.store(pt, Ordering::SeqCst);
    if pt != 0 {
        core::sync::atomic::fence(Ordering::SeqCst);
        crate::vm::wait_writer(pt);
    }
}

/// スケジューラがプロセスへ切りかえる (pt はその表のアドレス) / もどった (0)。大きなロックを持って呼ぶ
pub fn set_user_pt(pt: usize) {
    USER_PT[id()].store(pt, Ordering::Relaxed);
    own_user_pt();
}

/// 大きなロックを持っている CPU が、いまプロセスの中で使っている表 (vm::PageTable::write_enter が見る)
// [lockless]
pub fn bkl_pt() -> usize {
    BKL_PT.load(Ordering::SeqCst)
}

/// 取る。取った時刻 (ns) を返す (trap.rs が、持っていた時間を数えるのに時計を読みなおさないため)。測らない回は 0。
/// 時計を読むのはくじにあたったときだけ (SAMPLE。待ったものも)
pub fn lock() -> u64 {
    let me = id() + 1;
    debug_assert!(BKL.load(Ordering::Relaxed) != me, "BKL: cpu{} locks twice", me - 1);
    let c = &STATS.cpu[me - 1];
    let timed = draw(c);
    let ticket = NEXT.fetch_add(1, Ordering::Relaxed);
    let t0 = if SERVING.load(Ordering::Acquire) == ticket {
        None
    } else {
        let t0 = if timed { crate::timer::uptime_ns() } else { 0 };
        // 順番が来るまで wfe で休む (放す側が sev で起こす)。回りつづけると、QEMU (TCG) では待っている CPU が
        // 本当の CPU を取りあって、ロックを持っている CPU まで遅くなる
        while SERVING.load(Ordering::Acquire) != ticket {
            unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
        }
        Some(t0)
    };
    BKL.store(me, Ordering::Relaxed);
    add(&c.locks, 1);
    // くじにあたったものだけ、待った時間と持っていた時間を測って SAMPLE 倍。待ったものをいつも測ると、TCG では
    // 取ったあとに時計を読むのが重く (いくつもの CPU が同時に読むと 1 回 1 µs ほど: timer の粗い時計)、ロックを持つ
    // 時間が測るために伸びていた
    let (now, w) = match t0 {
        _ if !timed => (0, 0),
        Some(t0) => {
            let now = crate::timer::uptime_ns();
            add(&c.wait, (now - t0) * SAMPLE);
            (now, SAMPLE)
        }
        None => (crate::timer::uptime_ns(), SAMPLE),
    };
    c.since.store(now, Ordering::Relaxed);
    c.weight.store(w, Ordering::Relaxed);
    crate::heap::quar_check("lock");
    crate::vm::DROPPED_UNMAPPED[me - 1].store(0, Ordering::Relaxed);
    own_user_pt();
    own_user_files();
    now
}

/// 空いていれば取る (待たない)
fn try_lock() -> bool {
    let me = id() + 1;
    let s = SERVING.load(Ordering::Acquire);
    if NEXT.compare_exchange(s, s + 1, Ordering::Acquire, Ordering::Relaxed).is_err() {
        return false;
    }
    BKL.store(me, Ordering::Relaxed);
    let c = &STATS.cpu[me - 1];
    add(&c.locks, 1);
    let w = if draw(c) { SAMPLE } else { 0 };
    c.since.store(if w != 0 { crate::timer::uptime_ns() } else { 0 }, Ordering::Relaxed);
    c.weight.store(w, Ordering::Relaxed);
    own_user_pt();
    own_user_files();
    true
}

/// now (いまの時刻) までを持っていた時間に足して放す
fn release(now: u64) {
    let c = &STATS.cpu[id()];
    let w = c.weight.load(Ordering::Relaxed);
    if w != 0 {
        add(&c.hold, now.saturating_sub(c.since.load(Ordering::Relaxed)) * w);
    }
    BKL_PT.store(0, Ordering::SeqCst);
    BKL_FILES.store(0, Ordering::SeqCst);
    BKL.store(0, Ordering::Relaxed);
    // SERVING を書くのは持っているものだけなので、読んで書くだけでいい
    SERVING.store(SERVING.load(Ordering::Relaxed) + 1, Ordering::Release);
    // wfe で待っている CPU を起こす
    unsafe { core::arch::asm!("dsb ish", "sev", options(nostack)) };
}

/// 放す。そのまえとあとに、大きなロックなしの道が頼んでいった「起こす」を片づける (defer_wake)。
/// あとにも見るのは、放すまぎわに頼まれたもの (頼んだほうは取れなかったので、こちらに任せている) のため
pub fn unlock() {
    unlock_at(timed_now());
}

/// いまの時刻。この CPU のいまの hold も、いまの例外の区間 (SEG) も測っていなければ、時計を読まずに 0
pub fn timed_now() -> u64 {
    let i = id();
    if STATS.cpu[i].weight.load(Ordering::Relaxed) != 0 || SEG[i].2.load(Ordering::Relaxed) != 0 {
        crate::timer::uptime_ns()
    } else {
        0
    }
}

/// unlock と同じ。now は読んだばかりの時刻 (trap.rs: 時計をもう一度読まない)
pub fn unlock_at(now: u64) {
    crate::heap::quar_check("unlock");
    let mut now = Some(now);
    loop {
        let woke = drain_wakes();
        let t = match now.take() {
            Some(t) if !woke => t,
            _ => timed_now(),
        };
        release(t);
        // defer_wake と組 (あちらは PENDING_N を書いてから、こちらは放してから SeqCst の fence。どちらかが相手のものを見る)
        core::sync::atomic::fence(Ordering::SeqCst);
        if PENDING_N.load(Ordering::Relaxed) == 0 || !try_lock() {
            return;
        }
    }
}

// ---- ファイルシステムのロック (いまは ext4: extfs.rs が使う) ----
// 大きなロック (上) のほかに、ext4 の中 (ExtFs の Shared と ACell、ブロックのキャッシュ、ExtInode の数と Rc<ExtFs> の数) を守る。
// 読み書きロック: ext4 に入るところ (ExtInode の Inode の関数、ExtFs の外から呼ぶ関数) は、ひとりで持つ (fs_lock)。
// 大きなロックなしの作る・消す (fastdir.rs) と、ExtInode を落とすところ (数を減らすだけのとき) は、いっしょに持つ
// (fs_lock_shared): ちがうディレクトリなら同時に動く (Linux はディレクトリごとの i_rwsem と、グループごとのロック)。
// いっしょに持つものどうしは、ディレクトリごとのロック (DIR_LOCKS)、inode とブロックを取る・返すロック (ALLOC)、
// 帳簿のロック (BOOK、META_BOOK、NAME_BOOK: ExtFs の Shared をさわるあいだだけ) で分ける。ひとりで持とうとするものは、いっしょに持つものが
// みな出るのを待ち、待っているあいだは新しくいっしょには入れない (ひとりで持つ道が待ちつづけないように)。
// ひとりで持つのはスレッド (proc の番号。スケジューラの中は CPU) で、重ねて取れる。眠るとき (proc::sched) は手放し、
// 起きたらまた取る (大きなロックと同じく、眠っているあいだにほかが ext4 を変えてよい: with_block の wgen がそれを見る)。
// いっしょに持つのは CPU ごとに数える (カーネルは割り込みを止めて動き、いっしょに持ったまま眠らない) ので、重ねて取れる
// (ひとりで持っているものが、いっしょにと頼めば、重ねて取ったことに)。いっしょに持ったまま、ひとりで持とうとしては
// いけない (panic)。
static FS_OWNER: AtomicUsize = AtomicUsize::new(0);
/// 重ねて取った数 (持ち主だけが書く)
static FS_DEPTH: AtomicUsize = AtomicUsize::new(0);
/// いっしょに持っている CPU の数
static FS_READERS: AtomicUsize = AtomicUsize::new(0);
/// ひとりで持とうとして待っているものの数
static FS_WANT: AtomicUsize = AtomicUsize::new(0);
/// CPU ごとの、いっしょに持っている重ねた数 (その CPU だけが書く)
static FS_SHARED: [AtomicUsize; MAXCPU] = [const { AtomicUsize::new(0) }; MAXCPU];
/// 待った回数と時間 (ns。SAMPLE 回に 1 回測った見つもり) (/proc/bkl)
pub static FS_WAIT: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

// [lockless]
fn fs_me() -> usize {
    crate::proc::cur_index().unwrap_or(crate::proc::NPROC + id()) + 1
}

/// ext4 のロックを持っているしるし (落とすと放す)。true はいっしょに
pub struct FsGuard(bool);

/// ひとりで持つ
pub fn fs_lock() -> FsGuard {
    let me = fs_me();
    if FS_OWNER.load(Ordering::Relaxed) == me {
        FS_DEPTH.store(FS_DEPTH.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
        return FsGuard(false);
    }
    if FS_SHARED[id()].load(Ordering::Relaxed) != 0 {
        panic!("extfs: the ext4 lock is wanted alone while this CPU holds it shared");
    }
    fs_acquire(me);
    FS_DEPTH.store(1, Ordering::Relaxed);
    FsGuard(false)
}

/// いっしょに持つ (fastdir の作る・消すと、ExtInode を落とすところ)。ひとりで持っていれば、それを重ねて
// [lockless]
pub fn fs_lock_shared() -> FsGuard {
    if FS_OWNER.load(Ordering::Relaxed) == fs_me() {
        FS_DEPTH.store(FS_DEPTH.load(Ordering::Relaxed) + 1, Ordering::Relaxed);
        return FsGuard(false);
    }
    let c = &FS_SHARED[id()];
    let n = c.load(Ordering::Relaxed);
    if n == 0 {
        fs_acquire_shared();
    }
    c.store(n + 1, Ordering::Relaxed);
    FsGuard(true)
}

/// 待ちはじめ: 回数はいつも、時間は SAMPLE 回に 1 回だけ時計で測って SAMPLE 倍 (大きなロックと同じ: TCG では取ったあとに
/// 時計を読むのが重く、ロックを持つ時間が伸びる)
// [lockless]
fn wait_begin() -> u64 {
    let timed = FS_WAIT[0].fetch_add(1, Ordering::Relaxed) % SAMPLE == 0;
    if timed { crate::timer::uptime_ns().max(1) } else { 0 }
}

// [lockless]
fn wait_end(t0: u64) {
    if t0 != 0 {
        FS_WAIT[1].fetch_add((crate::timer::uptime_ns() - t0) * SAMPLE, Ordering::Relaxed);
    }
}

// [lockless]
pub fn wfe() {
    // 放す側が sev で起こす (大きなロックと同じ。回りつづけると TCG では持っている CPU まで遅くなる)
    unsafe { core::arch::asm!("wfe", options(nomem, nostack)) };
}

// [lockless]
pub fn sev() {
    unsafe { core::arch::asm!("dsb ish", "sev", options(nostack)) };
}

fn fs_acquire(me: usize) {
    let mut t0 = None;
    if FS_OWNER.compare_exchange(0, me, Ordering::SeqCst, Ordering::Relaxed).is_err() {
        t0 = Some(wait_begin());
        FS_WANT.fetch_add(1, Ordering::SeqCst);
        loop {
            wfe();
            if FS_OWNER.load(Ordering::Relaxed) == 0 && FS_OWNER.compare_exchange(0, me, Ordering::SeqCst, Ordering::Relaxed).is_ok() {
                break;
            }
        }
        FS_WANT.fetch_sub(1, Ordering::SeqCst);
    }
    // いっしょに持っているものが出るのを待つ (FS_OWNER を立てたので、新しくは入らない。入りかけたものは FS_OWNER を見て出る)
    while FS_READERS.load(Ordering::SeqCst) != 0 {
        if t0.is_none() {
            t0 = Some(wait_begin());
        }
        wfe();
    }
    if let Some(t) = t0 {
        wait_end(t);
    }
}

// [lockless]
fn fs_acquire_shared() {
    let mut t0 = None;
    loop {
        if FS_OWNER.load(Ordering::SeqCst) == 0 && FS_WANT.load(Ordering::SeqCst) == 0 {
            FS_READERS.fetch_add(1, Ordering::SeqCst);
            if FS_OWNER.load(Ordering::SeqCst) == 0 {
                break;
            }
            // ひとりで持とうとするものが先だった
            FS_READERS.fetch_sub(1, Ordering::SeqCst);
            sev();
        }
        if t0.is_none() {
            t0 = Some(wait_begin());
        }
        wfe();
    }
    if let Some(t) = t0 {
        wait_end(t);
    }
}

fn fs_release() {
    FS_DEPTH.store(0, Ordering::Relaxed);
    FS_OWNER.store(0, Ordering::Release);
    sev();
}

impl Drop for FsGuard {
    fn drop(&mut self) {
        if self.0 {
            let c = &FS_SHARED[id()];
            let n = c.load(Ordering::Relaxed);
            c.store(n - 1, Ordering::Relaxed);
            if n == 1 {
                FS_READERS.fetch_sub(1, Ordering::Release);
                sev();
            }
            return;
        }
        let d = FS_DEPTH.load(Ordering::Relaxed);
        if d > 1 {
            FS_DEPTH.store(d - 1, Ordering::Relaxed);
        } else {
            fs_release();
        }
    }
}

/// 眠る前 (proc::sched): いまのスレッドが ext4 のロックを持っていれば手放し、重ねた数を返す (0 なら持っていない)
pub fn fs_unlock_for_sleep() -> usize {
    if FS_SHARED[id()].load(Ordering::Relaxed) != 0 {
        panic!("extfs: sleeping while holding the ext4 lock shared");
    }
    if FS_OWNER.load(Ordering::Relaxed) != fs_me() {
        return 0;
    }
    let d = FS_DEPTH.load(Ordering::Relaxed);
    fs_release();
    d
}

/// 起きたあと: fs_unlock_for_sleep が手放したものを取りなおす
pub fn fs_relock_after_sleep(d: usize) {
    if d > 0 {
        fs_acquire(fs_me());
        FS_DEPTH.store(d, Ordering::Relaxed);
    }
}

/// システムコールや例外から戻るとき: ext4 のロックを持ったままなら、取ったところと放すところの組がこわれている
/// (そのままでは ext4 に入るものがみな止まる)
// [lockless]
pub fn fs_check_released() {
    if FS_OWNER.load(Ordering::Relaxed) == fs_me() {
        panic!("extfs: the ext4 lock is still held on the way out (depth {})", FS_DEPTH.load(Ordering::Relaxed));
    }
    if FS_SHARED[id()].load(Ordering::Relaxed) != 0 {
        panic!("extfs: the ext4 lock is still held shared on the way out");
    }
}

/// この CPU が、ファイルシステムのロックをいっしょに持っている
// [lockless]
pub fn fs_shared_here() -> bool {
    FS_SHARED[id()].load(Ordering::Relaxed) != 0
}

/// いまのスレッドが、ファイルシステムのロックをひとりで持っている
pub fn fs_owned_here() -> bool {
    FS_OWNER.load(Ordering::Relaxed) == fs_me()
}

// ---- 大きなロックなしの道からの「起こす」 ----
// パイプの読み書きの速い道 (file::Pipe::fast_rw) は、眠っている人や待っている人がいても進めて、起こすのは
// ここに頼む。大きなロックが空いていればその場で取って起こし、だれかが持っていれば、その CPU が放すときに起こす

const NPENDING: usize = 64;
static PENDING: crate::spinlock::SpinLock<([usize; NPENDING], usize)> = crate::spinlock::SpinLock::new(([0; NPENDING], 0));
/// PENDING の数 (PENDING を持って書く)。放すたびに見るので、PENDING を取らずに読めるように
static PENDING_N: AtomicUsize = AtomicUsize::new(0);

/// key (パイプ) で待っているものを起こしてもらう。大きなロックを持たずに呼ぶ
// [lockless]
pub fn defer_wake(key: usize) {
    let queued = {
        let mut p = PENDING.lock();
        let n = p.1;
        if p.0[..n].contains(&key) {
            true
        } else if n < NPENDING {
            p.0[n] = key;
            p.1 = n + 1;
            PENDING_N.store(n + 1, Ordering::Relaxed);
            true
        } else {
            false
        }
    };
    if !queued {
        // いっぱい: 待って取って、自分で
        // lockless-ok: ここで大きなロックを取る (起こすのはその中で)
        lock();
        // lockless-ok: 大きなロックの中
        crate::file::wake_key(key);
        // lockless-ok: 取ったものを放す
        unlock();
        return;
    }
    core::sync::atomic::fence(Ordering::SeqCst);
    // lockless-ok: 空いていれば大きなロックを取って放す (放すときに頼んだものを起こす: unlock_at)
    if try_lock() {
        // lockless-ok: 取ったものを放す
        unlock();
    }
}

/// 頼まれていたものを起こす (大きなロックを持って)。起こしたら true
fn drain_wakes() -> bool {
    let mut woke = false;
    // 見おとしても、放したあとに unlock_at が見なおす
    while PENDING_N.load(Ordering::Relaxed) != 0 {
        let (keys, n) = {
            let mut p = PENDING.lock();
            let n = p.1;
            p.1 = 0;
            PENDING_N.store(0, Ordering::Relaxed);
            (p.0, n)
        };
        for &k in &keys[..n] {
            crate::file::wake_key(k);
        }
        woke |= n > 0;
    }
    woke
}

// ---- 大きなロックの統計 (/proc/bkl。ロックを細かくするとき、どこから分けるかを決めるため) ----

const NSYS: usize = 512;

/// ロックを待った時間と持っていた時間は、SAMPLE 回に 1 回ほどだけ時計で測り、SAMPLE 倍して足す。QEMU の TCG では時計 (cntvct) を読むのが 150〜240 ns と、システムコールのほかのところぜんぶ (1 µs ほど)
/// にくらべて重く、出入りで 2 回読むと 4 割になっていた。Linux もプロセスの時間を出入りのたびには測らず、時計の割り込みの
/// たびに見る (tick で数える)。どれを測るかはくじ (xorshift) で選ぶ: 決まった順だと、交互に呼ぶもの (read と write など)
/// の片方だけが測られる
pub const SAMPLE: u64 = 8;

pub struct CpuStat {
    /// ロックを待っていた時間 (ns)
    pub wait: AtomicU64,
    /// ロックを持っていた時間 (ns。SAMPLE のぶん、見つもり)
    pub hold: AtomicU64,
    pub locks: AtomicU64,
    /// いま持ちはじめた時刻 (測らない回は 0)
    pub since: AtomicU64,
    /// いま持っている分を測っているなら、その重み (1 か SAMPLE)。0 は測っていない
    pub weight: AtomicU64,
    /// くじ (xorshift の種)
    pub dice: AtomicU64,
}

/// くじを引く: SAMPLE 回に 1 回ほどあたる (上の 3 ビットが 0)
/// この CPU のくじ (SAMPLE 回に 1 回ほど true): ほかの統計も、時計を読む回をこれで選ぶ
pub fn draw_here() -> bool {
    draw(&STATS.cpu[id()])
}

fn draw(c: &CpuStat) -> bool {
    let mut x = c.dice.load(Ordering::Relaxed);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    c.dice.store(x, Ordering::Relaxed);
    x >> (64 - SAMPLE.trailing_zeros()) == 0
}

pub struct Stats {
    pub cpu: [CpuStat; MAXCPU],
    /// システムコールの番号ごとの (回数, ロックを持っていた時間)。途中で眠ったものは、眠るまでと起きてからの分
    pub sys: [(AtomicU64, AtomicU64); NSYS],
    pub fault: (AtomicU64, AtomicU64),
    pub irq: (AtomicU64, AtomicU64),
    /// カーネルのスレッド (proc::kthread) が 1 回まわるあいだ
    pub kthread: (AtomicU64, AtomicU64),
    /// syscall::FD_SYS のシステムコールの、fd の開いたものの種類 (file::CLASSES) ごとの (回数, 時間)。sys には数えない
    pub fd: [[(AtomicU64, AtomicU64); NCLASS]; NFD],
    /// 途中で眠ったので数えなかったもの
    pub slept: AtomicU64,
    /// 数えはじめた時刻 (書くと 0 からやりなおす)
    pub start: AtomicU64,
}

const Z: AtomicU64 = AtomicU64::new(0);
/// STATS の数に足す。書くのは大きなロックを持っているものだけ (stats_reset も) なので、読んで書くだけでいい
/// (fetch_add の ldxr/stxr の輪より軽い)
// [lockless]
fn add(a: &AtomicU64, v: u64) {
    a.store(a.load(Ordering::Relaxed) + v, Ordering::Relaxed);
}

pub static STATS: Stats = Stats {
    cpu: [const { CpuStat { wait: Z, hold: Z, locks: Z, since: Z, weight: Z, dice: AtomicU64::new(0x9e37_79b9_7f4a_7c15) } }; MAXCPU],
    sys: [const { (Z, Z) }; NSYS],
    fault: (Z, Z),
    irq: (Z, Z),
    kthread: (Z, Z),
    fd: [const { [const { (Z, Z) }; NCLASS] }; NFD],
    slept: Z,
    start: Z,
};

/// スケジューラでほかのプロセスへ切りかえた回数 (ロックを持ったまま眠ったかを見る)
pub static SWITCHES: AtomicU64 = AtomicU64::new(0);

/// 起こされたものにゆずらせた数 (proc.rs preempt)
pub static PREEMPTS: AtomicUsize = AtomicUsize::new(0);
/// 大きなロックなしで片づけたページフォルト (vm.rs fast_fault)
pub static FAST_FAULTS: AtomicU64 = AtomicU64::new(0);
/// ページフォールトの数 (起きてからずっと。/proc/vmstat の pgfault)
pub static FAULTS: AtomicU64 = AtomicU64::new(0);

/// 例外 1 つぶんの、ロックを持っていた時間を数える (trap.rs)。sys はシステムコールの番号
pub enum Cause {
    Sys(u64),
    /// syscall::FD_SYS の何番めか、fd の開いたものの種類 (file::CLASSES の番号)
    SysFd(usize, usize),
    Fault,
    Irq,
    /// カーネルのスレッドの 1 回 (vfs::flusher)
    Kthread,
}

const NFD: usize = crate::syscall::FD_SYS.len();
const NCLASS: usize = crate::file::CLASSES.len();

impl Cause {
    /// システムコール nr (1 つめの引数 a0)。fd を取るもの (syscall::FD_SYS) は、その fd の開いたものの種類で分ける
    /// (大きなロックを持って呼ぶ: fd の表を見る)
    pub fn sys(nr: u64, a0: u64) -> Cause {
        match crate::syscall::FD_SYS.iter().position(|&n| n == nr) {
            Some(j) => Cause::SysFd(j, crate::file::class_of(a0)),
            None => Cause::Sys(nr),
        }
    }
}

fn slot_of(idx: u64) -> Option<&'static (AtomicU64, AtomicU64)> {
    match idx as usize {
        n if n < NSYS => Some(&STATS.sys[n]),
        n if n == NSYS => Some(&STATS.fault),
        n if n == NSYS + 1 => Some(&STATS.irq),
        n if n < NSYS + 2 + NFD * NCLASS => Some(&STATS.fd[(n - NSYS - 2) / NCLASS][(n - NSYS - 2) % NCLASS]),
        n if n == NSYS + 2 + NFD * NCLASS => Some(&STATS.kthread),
        _ => None,
    }
}

fn index_of(cause: &Cause) -> u64 {
    match *cause {
        Cause::Sys(n) if (n as usize) < NSYS => n,
        Cause::Sys(_) => u64::MAX,
        Cause::SysFd(j, k) => (NSYS + 2 + j * NCLASS + k) as u64,
        Cause::Fault => NSYS as u64,
        Cause::Irq => NSYS as u64 + 1,
        Cause::Kthread => (NSYS + 2 + NFD * NCLASS) as u64,
    }
}

/// CPU ごとの、いまロックを持って動いている区間 (始まった時刻, 原因の番号, 重み)。途中で眠るシステムコールも、
/// 眠るまでと起きてからの分を数えるため (sched が区切る)。重みはロックを取ったときのくじ (SAMPLE)、0 なら時間は測らない
static SEG: [(AtomicU64, AtomicU64, AtomicU64); MAXCPU] = [const { (AtomicU64::new(0), AtomicU64::new(u64::MAX), AtomicU64::new(0)) }; MAXCPU];

/// 例外の始まり (ロックを取ったあと。t0 は取った時刻、測らない回は 0)
pub fn seg_begin(cause: &Cause, t0: u64) {
    let i = id();
    let s = &SEG[i];
    s.0.store(t0, Ordering::Relaxed);
    s.1.store(index_of(cause), Ordering::Relaxed);
    s.2.store(STATS.cpu[i].weight.load(Ordering::Relaxed), Ordering::Relaxed);
}

/// 眠る (ほかへ切りかえる) まえ: ここまでの分を足す。返すのは (原因の番号, 重み) (起きたら seg_resume に)
pub fn seg_pause() -> (u64, u64) {
    let s = &SEG[id()];
    let idx = s.1.swap(u64::MAX, Ordering::Relaxed);
    let w = s.2.swap(0, Ordering::Relaxed);
    if w != 0
        && let Some(slot) = slot_of(idx)
    {
        add(&slot.1, crate::timer::uptime_ns().saturating_sub(s.0.load(Ordering::Relaxed)) * w);
    }
    (idx, w)
}

/// 起きた (切りかえから戻った): 続きの区間を始める
pub fn seg_resume((idx, w): (u64, u64)) {
    let s = &SEG[id()];
    s.0.store(if w != 0 { crate::timer::uptime_ns() } else { 0 }, Ordering::Relaxed);
    s.1.store(idx, Ordering::Relaxed);
    s.2.store(w, Ordering::Relaxed);
}

/// 例外 1 つぶんを数える (trap.rs)。ns はロックを取ってからの時間、now はいまの時刻 (timed_now。測らない回は 0)。
/// 回数はいつも、時間は区間の重みで
pub fn account(cause: Cause, ns: u64, slept: bool, now: u64) {
    let s = &SEG[id()];
    let w = s.2.swap(0, Ordering::Relaxed);
    s.1.store(u64::MAX, Ordering::Relaxed);
    let Some(slot) = slot_of(index_of(&cause)) else { return };
    add(&slot.0, 1);
    if slept {
        // 眠るまでの分は seg_pause が足した。起きてからの分を
        add(&STATS.slept, 1);
        if w != 0 {
            add(&slot.1, now.saturating_sub(s.0.load(Ordering::Relaxed)) * w);
        }
        return;
    }
    add(&slot.1, ns * w);
}

/// デバイスの終わりを回って待った回数と時間 (ns): 0 はディスク (virtio-blk)、1 は画面 (virtio-gpu)。
/// 2 はスケジューラが次を選んでページ表を載せるまで (ロックを持ったまま)。3 は起こされてから走りだすまで (待ち時間)。
/// 大きなロックを持ったまま待つものがどれだけあるかを /proc/bkl で見るため
pub static DEV_WAIT: [(AtomicU64, AtomicU64); 4] = [const { (AtomicU64::new(0), AtomicU64::new(0)) }; 4];
/// ディスク (virtio-blk) を眠って待った読みの数。数えるのは装置 (Linux の vm_event の数と同じく、数は core に置く)
pub static DISK_SLEPT: AtomicU64 = AtomicU64::new(0);
/// AF_UNIX の大きなロックなしの read / write (unix.rs の fast_rw) の数: 書いた、読んだ、ふつうの道へ
pub static UNIX_FAST: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// DEV_WAIT に 1 回分を足す
pub fn dev_wait(i: usize, t0: u64) {
    DEV_WAIT[i].0.fetch_add(1, Ordering::Relaxed);
    DEV_WAIT[i].1.fetch_add(crate::timer::uptime_ns().saturating_sub(t0), Ordering::Relaxed);
}

/// この CPU がロックを持っているか
// [lockless]
pub fn holding() -> bool {
    BKL.load(Ordering::Relaxed) == id() + 1
}

// ---- 2 つめからの CPU ----

core::arch::global_asm!(
    r#"
.equ S_MAIR, 0xff00
.equ S_TCR, (25 | (1 << 8) | (1 << 10) | (3 << 12) | (25 << 16) | (1 << 24) | (1 << 26) | (3 << 28) | (2 << 30) | (2 << 32))

.section .text
// 物理アドレスで、MMU なしで来る。x0 = CPU の番号
.global secondary_entry
secondary_entry:
    msr     daifset, #0xf
    mov     x21, x0
    mrs     x0, CurrentEL
    lsr     x0, x0, #2
    cmp     x0, #2
    b.ne    1f
    // EL2 なら EL1 へ (boot.rs と同じ)
    mov     x0, #(1 << 31)
    msr     hcr_el2, x0
    mov     x0, #3
    msr     cnthctl_el2, x0
    msr     cntvoff_el2, xzr
    mov     x0, #0x33ff
    msr     cptr_el2, x0
    msr     hstr_el2, xzr
    mov     x0, #0x3c5
    msr     spsr_el2, x0
    adr     x0, 1f
    msr     elr_el2, x0
    eret
1:  ldr     x0, =0x30d00800
    msr     sctlr_el1, x0
    isb
    // cpu0 が作ったページ表をそのまま使う
    adrp    x0, boot_l1_lo
    add     x0, x0, :lo12:boot_l1_lo
    msr     ttbr0_el1, x0
    adrp    x0, boot_l1_hi
    add     x0, x0, :lo12:boot_l1_hi
    msr     ttbr1_el1, x0
    ldr     x0, =S_MAIR
    msr     mair_el1, x0
    ldr     x0, =S_TCR
    msr     tcr_el1, x0
    isb
    tlbi    vmalle1
    dsb     nsh
    mrs     x0, sctlr_el1
    orr     x0, x0, #(1 << 0)
    orr     x0, x0, #(1 << 2)
    orr     x0, x0, #(1 << 12)
    // EL0 にも Linux と同じく許す: DC ZVA (14、memset)、CTR_EL0 を読む (15)、
    // キャッシュの掃除 DC CVAU / IC IVAU (26、JIT が書いた命令を流すのに使う)
    orr     x0, x0, #(1 << 14)
    orr     x0, x0, #(1 << 15)
    orr     x0, x0, #(1 << 26)
    // DSSBS (44) は 0: 例外でカーネルに入ると PSTATE.SSBS が落ちる (投機的なストアの追いこしを止める。cpuvuln.rs)
    bic     x0, x0, #(1 << 44)
    msr     sctlr_el1, x0
    isb
    ldr     x0, =2f
    br      x0
2:  // ここから上位アドレス
    ldr     x1, =secondary_stacks
    add     x2, x21, #1
    lsl     x2, x2, #14
    add     x1, x1, x2
    mov     sp, x1
    msr     tpidr_el1, x21
    mov     x0, #(3 << 20)
    msr     cpacr_el1, x0
    isb
    mov     x0, x21
    bl      secondary_main
3:  wfe
    b       3b
"#
);

unsafe extern "C" {
    fn secondary_entry();
}

#[unsafe(no_mangle)]
extern "C" fn secondary_main(cpu: usize) -> ! {
    crate::trap::init();
    crate::irq::init_cpu();
    crate::timer::init_cpu();
    lock();
    ONLINE.fetch_add(1, Ordering::AcqRel);
    println!("smp: cpu{} online", cpu);
    crate::proc::scheduler()
}

fn psci_cpu_on(mpidr: u64, entry: u64, ctx: u64) -> i64 {
    const CPU_ON: u64 = 0xc400_0003;
    let r: i64;
    unsafe {
        match crate::dtb::psci_method() {
            Some("smc") => core::arch::asm!("smc #0", inout("x0") CPU_ON => r, in("x1") mpidr, in("x2") entry, in("x3") ctx, options(nostack)),
            _ => core::arch::asm!("hvc #0", inout("x0") CPU_ON => r, in("x1") mpidr, in("x2") entry, in("x3") ctx, options(nostack)),
        }
    }
    r
}

/// spin-table: release addr に入口の物理アドレスを書いて起こす。x0 には番号が入らないので、
/// 入口を CPU ごとの小さな踏み台にする
fn spin_table_on(release: u64, cpu: usize) {
    let tramp = TRAMPOLINES[cpu] as *const () as usize;
    let va = p2v(release as usize) as *mut u64;
    unsafe {
        core::ptr::write_volatile(va, v2p(tramp) as u64);
        // MMU を入れる前の CPU が読むので、キャッシュから RAM へ
        core::arch::asm!("dc civac, {}", "dsb sy", "sev", in(reg) va);
    }
}

// spin-table の踏み台: x0 に番号を入れて secondary_entry へ
core::arch::global_asm!(
    r#"
.section .text
.global smp_tramp0
smp_tramp0: mov x0, #0
    b secondary_entry
.global smp_tramp1
smp_tramp1: mov x0, #1
    b secondary_entry
.global smp_tramp2
smp_tramp2: mov x0, #2
    b secondary_entry
.global smp_tramp3
smp_tramp3: mov x0, #3
    b secondary_entry
.global smp_tramp4
smp_tramp4: mov x0, #4
    b secondary_entry
.global smp_tramp5
smp_tramp5: mov x0, #5
    b secondary_entry
.global smp_tramp6
smp_tramp6: mov x0, #6
    b secondary_entry
.global smp_tramp7
smp_tramp7: mov x0, #7
    b secondary_entry
"#
);

unsafe extern "C" {
    fn smp_tramp0();
    fn smp_tramp1();
    fn smp_tramp2();
    fn smp_tramp3();
    fn smp_tramp4();
    fn smp_tramp5();
    fn smp_tramp6();
    fn smp_tramp7();
}

static TRAMPOLINES: [unsafe extern "C" fn(); MAXCPU] = [smp_tramp0, smp_tramp1, smp_tramp2, smp_tramp3, smp_tramp4, smp_tramp5, smp_tramp6, smp_tramp7];

/// DTB の CPU を起こす (cpu0 から、ロックを持って)。maxcpus=N / nosmp で減らせる
pub fn start() {
    let max = if crate::dtb::arg("nosmp").is_some() || crate::dtb::bootargs().is_some_and(|a| a.split_whitespace().any(|w| w == "nosmp")) {
        1
    } else {
        crate::dtb::arg("maxcpus").and_then(|n| n.parse().ok()).unwrap_or(MAXCPU).clamp(1, MAXCPU)
    };
    let me: u64;
    unsafe { core::arch::asm!("mrs {}, mpidr_el1", out(reg) me) };
    let me = me & 0xff_00ff_ffff;
    TARGET[0].store((me & 0xff) as usize, Ordering::Relaxed);
    let mut n = 1;
    let mut started = 0;
    crate::dtb::each_cpu(|mpidr, method, release| {
        if mpidr == me || n >= max {
            return;
        }
        let cpu = n;
        n += 1;
        TARGET[cpu].store((mpidr & 0xff) as usize, Ordering::Relaxed);
        match (method, release) {
            (Some("spin-table"), Some(rel)) => {
                spin_table_on(rel, cpu);
                started += 1;
            }
            _ => {
                let entry = v2p(secondary_entry as *const () as usize) as u64;
                let r = psci_cpu_on(mpidr, entry, cpu as u64);
                if r == 0 {
                    started += 1;
                } else {
                    println!("smp: cannot start cpu {:#x} ({})", mpidr, r);
                }
            }
        }
    });
    if started == 0 {
        return;
    }
    // 起きた CPU はロックを待っているので、いったん放して待つ
    let want = 1 + started;
    unlock();
    for _ in 0..50_000_000 {
        if online() >= want {
            break;
        }
        core::hint::spin_loop();
    }
    lock();
    println!("smp: {} cpus", online());
    crate::vm::calibrate_tlbi();
}

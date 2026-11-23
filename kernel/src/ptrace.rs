// ptrace (Linux の kernel/ptrace.c と arch/arm64/kernel/ptrace.c)。止まるところ (シグナルの知らせ、PTRACE_SYSCALL の入口と
// 出口、PTRACE_INTERRUPT) と tracee が眠るところは core (signal::ptrace_stop、proc::ptrace_sleep) にあり、ここは tracer の
// たのみ (つなぐ・離す・続けさせる・メモリとレジスタを読み書き) だけ。main.rs が syscall::register でつなぐ
use crate::cred;
use crate::hwdebug::{self, Attr, Ctrl, Ev};
use crate::proc::{self, Proc};
use crate::signal::{self, SigInfo, SIGKILL, SIGSTOP};
use crate::trap::TrapFrame;

type R = Result<i64, i64>;
const EPERM: i64 = 1;
const ESRCH: i64 = 3;
const EIO: i64 = 5;
const EFAULT: i64 = 14;
const EINVAL: i64 = 22;

const PTRACE_TRACEME: u64 = 0;
const PTRACE_PEEKTEXT: u64 = 1;
const PTRACE_PEEKDATA: u64 = 2;
const PTRACE_POKETEXT: u64 = 4;
const PTRACE_POKEDATA: u64 = 5;
const PTRACE_CONT: u64 = 7;
const PTRACE_KILL: u64 = 8;
const PTRACE_SINGLESTEP: u64 = 9;
const PTRACE_ATTACH: u64 = 16;
const PTRACE_DETACH: u64 = 17;
const PTRACE_SYSCALL: u64 = 24;
const PTRACE_SETOPTIONS: u64 = 0x4200;
const PTRACE_GETEVENTMSG: u64 = 0x4201;
const PTRACE_GET_SYSCALL_INFO: u64 = 0x420e;
const PTRACE_GETSIGINFO: u64 = 0x4202;
const PTRACE_GETREGSET: u64 = 0x4204;
const PTRACE_SETREGSET: u64 = 0x4205;
const PTRACE_SEIZE: u64 = 0x4206;
const PTRACE_INTERRUPT: u64 = 0x4207;
const PTRACE_SETSIGINFO: u64 = 0x4203;
const PTRACE_LISTEN: u64 = 0x4208;
const PTRACE_PEEKSIGINFO: u64 = 0x4209;
/// PEEKSIGINFO の flags: プロセス (スレッドグループ) に来たものを
const PTRACE_PEEKSIGINFO_SHARED: u32 = 1;
/// seccomp を止める (CRIU のため。CAP_SYS_ADMIN がいる: proc::PT_SUSPEND_SECCOMP)
const PTRACE_O_SUSPEND_SECCOMP: u64 = proc::PT_SUSPEND_SECCOMP as u64;
/// 知っている PTRACE_O_* (Linux の PTRACE_O_MASK)
const PTRACE_O_MASK: u64 = 0x0030_00ff;
/// GETREGSET の NT_PRSTATUS (struct user_pt_regs: x0〜x30、sp、pc、pstate の 272 バイト)
const NT_PRSTATUS: u64 = 1;
const PRSTATUS_SIZE: usize = 34 * 8;
/// NT_PRFPREG (struct user_fpsimd_state: v0〜v31、fpsr、fpcr、と 8 バイトの空き: 528 バイト)
const NT_PRFPREG: u64 = 2;
const FPREG_SIZE: usize = 32 * 16 + 16;
/// NT_ARM_SYSTEM_CALL (いまのシステムコールの番号: 4 バイト)
const NT_ARM_SYSTEM_CALL: u64 = 0x404;
/// NT_ARM_HW_BREAK と NT_ARM_HW_WATCH (struct user_hwdebug_state: dbg_info、pad、16 の (addr、ctrl、pad): 264 バイト)
const NT_ARM_HW_BREAK: u64 = 0x402;
const NT_ARM_HW_WATCH: u64 = 0x403;
const HW_SIZE: usize = 8 + 16 * hwdebug::MAX;
const ENOSPC: i64 = 28;
/// perf の bp_type (HW_BREAKPOINT_R、W、RW、X)
const HW_R: u8 = 1;
const HW_W: u8 = 2;
const HW_RW: u8 = 3;
const HW_X: u8 = 4;
/// AUDIT_ARCH_AARCH64 (PTRACE_GET_SYSCALL_INFO の arch)
const AUDIT_ARCH_AARCH64: u32 = 0xc000_00b7;

pub const SYSCALLS: &[u64] = {
    use crate::syscall::nr::*;
    &[PTRACE]
};

pub fn sys(n: u64, a: &[u64; 6]) -> R {
    match n {
        crate::syscall::nr::PTRACE => ptrace(a[0], a[1] as u32 as i32, a[2] as usize, a[3] as usize),
        _ => Err(-38),
    }
}

/// tracer がつなげてよいか (Linux の ptrace_may_access の PTRACE_MODE_ATTACH_REALCREDS): CAP_SYS_PTRACE か、uid と gid が
/// みな同じで dumpable なもの。LSM (landlock と AppArmor) も
fn may_attach(t: &Proc) -> bool {
    let (c, tc) = (cred::current(), &t.cred);
    let ids = c.uid == tc.uid && c.uid == tc.euid && c.uid == tc.suid && c.gid == tc.gid && c.gid == tc.egid && c.gid == tc.sgid;
    let cap = c.capable(cred::CAP_SYS_PTRACE);
    if !(ids && crate::pidfd::dumpable(t) == 1) && !cap {
        return false;
    }
    crate::landlock::check_ptrace(c.sandbox.as_ref(), tc.sandbox.as_ref()).is_ok() && crate::apparmor::may_ptrace(tc, false).is_ok()
}

/// 自分が tracer で、(state なら) 止まっている tracee (Linux の ptrace_check_attach)。でなければ ESRCH
fn traced(t: &'static mut Proc, stopped: bool) -> Result<&'static mut Proc, i64> {
    // 続けさせたら (pt_resume)、tracee がまだ起きていなくてももう止まっていない (Linux の wake_up_state で TASK_TRACED
    // でなくなる)
    if t.ptracer != proc::current().tgid || (stopped && (t.pt_status == 0 || t.pt_tf.is_null() || t.pt_resume.is_some())) {
        return Err(-ESRCH);
    }
    Ok(t)
}

/// 続けさせる (CONT、SYSCALL、SINGLESTEP、DETACH): シグナルは 0〜64 (でなければ EIO)。SINGLESTEP は 1 命令ずつに
/// (はじめてなら SPSR.SS を 1 に: Linux の user_enable_single_step。もう 1 命令ずつなら SS はそのまま)、ほかはやめる
fn resume(t: &mut Proc, sig: usize, syscall: bool, step: bool) -> R {
    if sig > 64 {
        return Err(-EIO);
    }
    if step && !t.pt_step {
        t.tf().spsr |= hwdebug::SPSR_SS;
    }
    t.pt_step = step;
    t.pt_syscall = syscall;
    t.pt_resume = Some(sig as i32);
    proc::wakeup(proc::pt_chan(t));
    Ok(0)
}

/// SEIZE と SETOPTIONS の印 (Linux の check_ptrace_options): 知らないものは EINVAL。SUSPEND_SECCOMP は CAP_SYS_ADMIN が
/// いり、呼ぶほうに seccomp がかかっているか、呼ぶほうも SUSPEND_SECCOMP で ptrace されていれば EPERM
fn check_options(data: u64) -> Result<(), i64> {
    if data & !PTRACE_O_MASK != 0 {
        return Err(crate::why::inval());
    }
    if data & PTRACE_O_SUSPEND_SECCOMP != 0 {
        let me = proc::current();
        if !me.cred.capable(crate::cred::CAP_SYS_ADMIN) || me.cred.seccomp.is_some() || me.seccomp_suspended() {
            return Err(-EPERM);
        }
    }
    Ok(())
}

fn attach(t: &'static mut Proc, seize: bool, addr: usize, data: usize) -> R {
    let me = proc::current();
    // SEIZE の知らない印は EIO (SETOPTIONS は EINVAL: Linux の ptrace_attach のむかしからのちがい)
    if seize && (addr != 0 || data as u64 & !PTRACE_O_MASK != 0) {
        return Err(-EIO);
    }
    if seize {
        check_options(data as u64)?;
    }
    // カーネルのスレッド、自分のスレッドグループ、もうつながっているものは EPERM
    if t.kthread || t.tgid == me.tgid || t.ptracer != 0 || !may_attach(t) {
        return Err(-EPERM);
    }
    t.ptracer = me.tgid;
    t.pt_seized = seize;
    t.pt_opts = if seize { data as u32 } else { 0 };
    t.sync_cred_work();
    // ATTACH は SIGSTOP を送る (止まったら tracer に知らせる)。SEIZE は止めない
    if !seize {
        let info = SigInfo { code: 0x80, ..SigInfo::ZERO };
        signal::send_thread(t, SIGSTOP, info);
    }
    Ok(0)
}

pub fn ptrace(req: u64, pid: i32, addr: usize, data: usize) -> R {
    let me = proc::current();
    if req == PTRACE_TRACEME {
        // 親が tracer になる。もうつながっていれば EPERM
        if me.ptracer != 0 {
            return Err(-EPERM);
        }
        let parent = proc::find_leader(me.ppid).ok_or(-EPERM)?;
        me.ptracer = parent.tgid;
        me.pt_seized = false;
        me.sync_cred_work();
        return Ok(0);
    }
    let tid = if pid > 0 { crate::ns::to_global(pid as u32) } else { None };
    let t = tid.and_then(proc::find_thread).ok_or(-ESRCH)?;
    match req {
        PTRACE_ATTACH | PTRACE_SEIZE => return attach(t, req == PTRACE_SEIZE, addr, data),
        // KILL と INTERRUPT は止まっていなくても
        PTRACE_KILL => {
            let t = traced(t, false)?;
            signal::send_thread(t, SIGKILL, SigInfo::ZERO);
            return Ok(0);
        }
        PTRACE_INTERRUPT => {
            let t = traced(t, false)?;
            if !t.pt_seized {
                return Err(-EIO);
            }
            t.pt_interrupt = true;
            proc::interrupt(t);
            return Ok(0);
        }
        _ => {}
    }
    let t = traced(t, true)?;
    match req {
        PTRACE_PEEKTEXT | PTRACE_PEEKDATA => {
            let mut b = [0u8; 8];
            if remote(t, addr, &mut b, false) != 8 {
                return Err(-EIO);
            }
            proc::put_user(data, &b)?;
            Ok(0)
        }
        PTRACE_POKETEXT | PTRACE_POKEDATA => {
            let b = (data as u64).to_le_bytes();
            if poke(t, addr, &b) != 8 {
                return Err(-EIO);
            }
            Ok(0)
        }
        PTRACE_CONT => resume(t, data, false, false),
        PTRACE_SYSCALL => resume(t, data, true, false),
        PTRACE_SINGLESTEP => resume(t, data, false, true),
        PTRACE_DETACH => {
            if data > 64 {
                return Err(-EIO);
            }
            t.ptracer = 0;
            t.pt_seized = false;
            t.pt_opts = 0;
            t.sync_cred_work();
            resume(t, data, false, false)
        }
        PTRACE_SETOPTIONS => {
            check_options(data as u64)?;
            t.pt_opts = data as u32;
            Ok(0)
        }
        PTRACE_GETEVENTMSG => {
            proc::put_user(data, &t.pt_msg.to_le_bytes())?;
            Ok(0)
        }
        PTRACE_GET_SYSCALL_INFO => syscall_info(t, addr, data),
        PTRACE_GETSIGINFO => {
            let (sig, info) = t.pt_info;
            proc::put_user(data, &signal::siginfo_of(sig, &info))?;
            Ok(0)
        }
        // 止まりの siginfo を書きかえる (シグナルの知らせなら、続けたときにそれが届く: signal.rs の deliver_now)
        PTRACE_SETSIGINFO => {
            let mut b = [0u8; 128];
            proc::get_user(data, &mut b)?;
            t.pt_info = signal::siginfo_from(&b);
            Ok(0)
        }
        PTRACE_PEEKSIGINFO => peek_siginfo(t, addr, data),
        // SEIZE したものの グループの止まり (PTRACE_EVENT_STOP) のときだけ: 止まったまま SIGCONT を待ち、来たらまた知らせる
        PTRACE_LISTEN => {
            if !t.pt_seized || t.pt_info.1.code >> 8 != signal::PTRACE_EVENT_STOP {
                return Err(-EIO);
            }
            t.pt_listen = true;
            Ok(0)
        }
        PTRACE_GETREGSET | PTRACE_SETREGSET => regset(t, req == PTRACE_SETREGSET, addr as u64, data),
        // PEEKUSR / POKEUSR (arm64 にはない) ほか、知らないもの
        _ => Err(-EIO),
    }
}

/// まだ届いていないシグナルの siginfo を off 番目から nr こ (Linux の ptrace_peek_siginfo)。args は
/// struct ptrace_peeksiginfo_args { u64 off; u32 flags; s32 nr; }。読めた数 (はじめの 1 つが書けなければ EFAULT)
fn peek_siginfo(t: &Proc, args: usize, out: usize) -> R {
    let mut b = [0u8; 16];
    proc::get_user(args, &mut b).map_err(|_| -EFAULT)?;
    let off = u64::from_le_bytes(b[0..8].try_into().unwrap());
    let flags = u32::from_le_bytes(b[8..12].try_into().unwrap());
    let nr = i32::from_le_bytes(b[12..16].try_into().unwrap());
    if flags & !PTRACE_PEEKSIGINFO_SHARED != 0 || nr < 0 {
        return Err(-EINVAL);
    }
    let list = signal::pending_list(t, flags & PTRACE_PEEKSIGINFO_SHARED != 0);
    let mut n = 0usize;
    for (sig, info) in list.iter().skip(off.min(usize::MAX as u64) as usize).take(nr as usize) {
        if proc::put_user(out + n * 128, &signal::siginfo_of(*sig, info)).is_err() {
            return if n > 0 { Ok(n as i64) } else { Err(-EFAULT) };
        }
        n += 1;
    }
    Ok(n as i64)
}

/// tracee のメモリを読む (Linux の ptrace_access_vm: /proc/PID/mem と同じ道)。読めたバイト数
fn remote(t: &Proc, addr: usize, buf: &mut [u8], _write: bool) -> usize {
    let Some(mm) = t.mm.as_ref().filter(|_| !t.kthread) else { return 0 };
    let pt = &mut mm.get().pt;
    let _h = pt.hold();
    pt.read_remote(addr, buf)
}

/// tracee のメモリに書く (FOLL_FORCE: コードにも。vm::PageTable::poke_remote)。書けたバイト数
fn poke(t: &Proc, addr: usize, buf: &[u8]) -> usize {
    let Some(mm) = t.mm.as_ref().filter(|_| !t.kthread) else { return 0 };
    let pt = &mut mm.get().pt;
    let _h = pt.hold();
    pt.poke_remote(addr, buf)
}

/// PTRACE_GET_SYSCALL_INFO (Linux の ptrace_get_syscall_info): struct ptrace_syscall_info を size まで写し、まるごとの
/// 大きさを返す。op はシステムコールの入口 (番号と引数 6 つ: 80)、出口 (戻り値と、まちがいか: 33)、ほか (24)
fn syscall_info(t: &mut Proc, size: usize, data: usize) -> R {
    let tf: &TrapFrame = unsafe { &*t.pt_tf };
    let mut b = [0u8; 80];
    b[0] = t.pt_kind;
    b[4..8].copy_from_slice(&AUDIT_ARCH_AARCH64.to_le_bytes());
    b[8..16].copy_from_slice(&tf.elr.to_le_bytes());
    b[16..24].copy_from_slice(&tf.sp_el0.to_le_bytes());
    let n = match t.pt_kind {
        1 => {
            b[24..32].copy_from_slice(&t.pt_scno.unwrap_or(tf.x[8]).to_le_bytes());
            // 引数の 1 つめは入口の x0 (orig_x0)
            b[32..40].copy_from_slice(&t.orig_x0.to_le_bytes());
            for i in 1..6 {
                b[32 + i * 8..40 + i * 8].copy_from_slice(&tf.x[i].to_le_bytes());
            }
            80
        }
        2 => {
            let rval = tf.x[0] as i64;
            b[24..32].copy_from_slice(&rval.to_le_bytes());
            b[32] = (-4095..0).contains(&rval) as u8;
            33
        }
        _ => 24,
    };
    proc::put_user(data, &b[..n.min(size)])?;
    Ok(n as i64)
}

/// GETREGSET / SETREGSET (iovec): NT_PRSTATUS だけ (ほかは EINVAL)。iov_len は写した長さに
fn regset(t: &mut Proc, set: bool, note: u64, iov: usize) -> R {
    if note == NT_ARM_HW_BREAK || note == NT_ARM_HW_WATCH {
        return hw_regset(t, set, note == NT_ARM_HW_WATCH, iov);
    }
    let size = match note {
        NT_PRSTATUS => PRSTATUS_SIZE,
        NT_PRFPREG => FPREG_SIZE,
        NT_ARM_SYSTEM_CALL => 4,
        _ => return Err(-EINVAL),
    };
    let mut v = [0u8; 16];
    proc::current().pt().copy_in(&mut v, iov).ok_or(-EFAULT)?;
    let base = u64::from_le_bytes(v[..8].try_into().unwrap()) as usize;
    let len = (u64::from_le_bytes(v[8..].try_into().unwrap()) as usize).min(size);
    if note == NT_PRFPREG {
        // 止まっている tracee の浮動小数のレジスタは、切りかえたときに p.fp へ (proc の sched)
        let mut b = [0u8; FPREG_SIZE];
        for (i, q) in t.fp.q.iter().enumerate() {
            b[i * 16..i * 16 + 16].copy_from_slice(&q.to_le_bytes());
        }
        b[512..516].copy_from_slice(&(t.fp.fpsr as u32).to_le_bytes());
        b[516..520].copy_from_slice(&(t.fp.fpcr as u32).to_le_bytes());
        if set {
            proc::current().pt().copy_in(&mut b[..len], base).ok_or(-EFAULT)?;
            for i in 0..32 {
                t.fp.q[i] = u128::from_le_bytes(b[i * 16..i * 16 + 16].try_into().unwrap());
            }
            t.fp.fpsr = u32::from_le_bytes(b[512..516].try_into().unwrap()) as u64;
            t.fp.fpcr = u32::from_le_bytes(b[516..520].try_into().unwrap()) as u64;
        } else {
            proc::put_user(base, &b[..len])?;
        }
        return iov_len_set(iov, len);
    }
    if note == NT_ARM_SYSTEM_CALL {
        let tf: &TrapFrame = unsafe { &*t.pt_tf };
        let mut b = (t.pt_scno.unwrap_or(tf.x[8]) as i32).to_le_bytes();
        if set {
            proc::current().pt().copy_in(&mut b[..len], base).ok_or(-EFAULT)?;
            t.pt_scno = Some(i32::from_le_bytes(b) as i64 as u64);
        } else {
            proc::put_user(base, &b[..len])?;
        }
        return iov_len_set(iov, len);
    }
    // 止まっているあいだだけ (traced で確かめた)。tracee のカーネルのスタックの上のレジスタ
    let tf: &mut TrapFrame = unsafe { &mut *t.pt_tf };
    let mut b = [0u8; PRSTATUS_SIZE];
    for (i, r) in tf.x.iter().enumerate() {
        b[i * 8..i * 8 + 8].copy_from_slice(&r.to_le_bytes());
    }
    b[248..256].copy_from_slice(&tf.sp_el0.to_le_bytes());
    b[256..264].copy_from_slice(&tf.elr.to_le_bytes());
    b[264..272].copy_from_slice(&tf.spsr.to_le_bytes());
    if set {
        proc::current().pt().copy_in(&mut b[..len], base).ok_or(-EFAULT)?;
        let w = |i: usize| u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
        for i in 0..31 {
            tf.x[i] = w(i);
        }
        tf.sp_el0 = w(31);
        tf.elr = w(32);
        // pstate は EL0 の印だけ (NZCV など)。モードは変えさせない (Linux の valid_user_regs)。SS は 1 命令ずつかで決める
        // (Linux の user_regs_reset_single_step)
        let ss = if t.pt_step { hwdebug::SPSR_SS } else { 0 };
        tf.spsr = (tf.spsr & !(0xf000_0000 | hwdebug::SPSR_SS)) | (w(33) & 0xf000_0000) | ss;
    } else {
        proc::put_user(base, &b[..len])?;
    }
    iov_len_set(iov, len)
}

/// iovec の iov_len を写した長さに
fn iov_len_set(iov: usize, len: usize) -> R {
    proc::put_user(iov + 8, &(len as u64).to_le_bytes())?;
    Ok(0)
}

// ---- ハードウェアのブレークポイントとウォッチポイント (Linux の arch/arm64/kernel/ptrace.c の ptrace_hbp_* と
// hw_break_get / hw_break_set、hw_breakpoint.c の hw_breakpoint_arch_parse)。CPU に載せるのと例外は hwdebug.rs ----

/// attr から CPU の値に (hwdebug.rs の parse: Linux の hw_breakpoint_arch_parse)
fn hw_parse(a: &Attr) -> Result<(u64, Ctrl), i64> {
    hwdebug::parse(a)
}

/// ptrace のスロット idx (なければ作る: Linux の ptrace_hbp_create。はじめは番地 0、4 バイト、実行か読み書き、無効)。
/// 無効なものも数えて、CPU のスロットの数をこえて作ろうとすると ENOSPC (perf の reserve_bp_slot)
fn hw_event(t: &mut Proc, watch: bool, idx: usize) -> Result<&mut Ev, i64> {
    let st = t.hwdbg.get_or_insert_with(Default::default);
    let used = st.used(watch);
    let arr = if watch { &mut st.wch } else { &mut st.brk };
    if arr[idx].is_none() {
        if used >= hwdebug::slots(watch) {
            return Err(-ENOSPC);
        }
        let attr = Attr { addr: 0, len: 4, ty: if watch { HW_RW } else { HW_X }, disabled: true };
        let (addr, ctrl) = hw_parse(&attr)?;
        arr[idx] = Some(Ev { attr, addr, ctrl });
    }
    Ok(arr[idx].as_mut().unwrap())
}

/// attr を入れかえる (modify_user_hw_breakpoint)。まちがっていれば、なにも変えずに
fn hw_modify(e: &mut Ev, attr: Attr) -> Result<(), i64> {
    let (addr, ctrl) = hw_parse(&attr)?;
    *e = Ev { attr, addr, ctrl };
    Ok(())
}

/// ctrl をスロットに (ptrace_hbp_set_ctrl と ptrace_hbp_fill_attr_ctrl): 無効なら無効にするだけ。有効なら BAS の
/// 1 のならび (1〜8 バイト。下の 0 の数だけ番地を進める) と種類 (ブレークポイントは実行、ウォッチポイントは読み書き)。
/// EL のビットは見ない
fn hw_set_ctrl(e: &mut Ev, watch: bool, uctrl: u32) -> Result<(), i64> {
    let mut attr = e.attr;
    attr.disabled = uctrl & 1 == 0;
    if !attr.disabled {
        let bas = (uctrl >> 5) & 0xff;
        let ty = match (uctrl >> 3) & 3 {
            0 => HW_X,
            1 => HW_R,
            2 => HW_W,
            _ => HW_RW,
        };
        if bas == 0 {
            return Err(-EINVAL);
        }
        let off = bas.trailing_zeros();
        let run = bas >> off;
        if run & (run + 1) != 0 {
            return Err(-EINVAL);
        }
        if (ty == HW_X) == watch {
            return Err(-EINVAL);
        }
        attr.len = run.count_ones() as u8;
        attr.ty = ty;
        attr.addr = attr.addr.wrapping_add(off as u64);
    }
    hw_modify(e, attr)
}

/// GETREGSET / SETREGSET の NT_ARM_HW_BREAK と NT_ARM_HW_WATCH。iov_len は 4 の倍 (でなければ EINVAL) で 264 まで。
/// 読みは dbg_info (デバッグの版 << 8 | スロットの数) と 16 のスロット (ないものは 0)。書きは dbg_info を飛ばし、
/// スロットごとに番地、それから ctrl (どちらも入れたところまで。番地の途中で終われば EINVAL、まちがいのところで
/// やめる。前のスロットは書いたまま)
fn hw_regset(t: &mut Proc, set: bool, watch: bool, iov: usize) -> R {
    let mut v = [0u8; 16];
    proc::current().pt().copy_in(&mut v, iov).ok_or(-EFAULT)?;
    let base = u64::from_le_bytes(v[..8].try_into().unwrap()) as usize;
    let len = u64::from_le_bytes(v[8..].try_into().unwrap()) as usize;
    if len % 4 != 0 {
        return Err(-EINVAL);
    }
    let len = len.min(HW_SIZE);
    let mut b = [0u8; HW_SIZE];
    if !set {
        b[0..4].copy_from_slice(&(hwdebug::arch() << 8 | hwdebug::slots(watch) as u32).to_le_bytes());
        if let Some(st) = t.hwdbg.as_deref() {
            for (i, e) in (if watch { &st.wch } else { &st.brk }).iter().enumerate() {
                if let Some(e) = e {
                    b[8 + i * 16..16 + i * 16].copy_from_slice(&e.addr.to_le_bytes());
                    b[16 + i * 16..20 + i * 16].copy_from_slice(&e.ctrl.encode().to_le_bytes());
                }
            }
        }
        proc::put_user(base, &b[..len])?;
        return iov_len_set(iov, len);
    }
    proc::current().pt().copy_in(&mut b[..len], base).ok_or(-EFAULT)?;
    let mut off = 8;
    let mut idx = 0;
    while off < len {
        if len - off < 8 {
            return Err(-EINVAL);
        }
        let addr = u64::from_le_bytes(b[off..off + 8].try_into().unwrap());
        let e = hw_event(t, watch, idx)?;
        let mut attr = e.attr;
        attr.addr = addr;
        hw_modify(e, attr)?;
        off += 8;
        if off >= len {
            break;
        }
        let ctrl = u32::from_le_bytes(b[off..off + 4].try_into().unwrap());
        hw_set_ctrl(hw_event(t, watch, idx)?, watch, ctrl)?;
        off += 8;
        idx += 1;
    }
    iov_len_set(iov, len)
}

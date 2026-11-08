// pidfd (Linux の pidfs): pidfd が指すスレッド・プロセスのことを答える口と、pidfd を使うシステムコール (Linux 7.4)
//
//   /proc/PID/fdinfo/N の Pid: と NSpid: (fdinfo)。片づけられたものは -1、読む人の PID の namespace から見えなければ 0
//   ioctl の PIDFD_GET_INFO (struct pidfd_info): 番号 (pid、tgid、ppid)、資格情報、終わって片づけられたときの exit_code、
//       coredump の印 (PR_SET_DUMPABLE)、知っている印 (supported_mask)。大きさは版ごと (64、72、80、88) で、入らない欄の
//       印は落とす。systemd の pidfd_get_pid などはこれを使い、なければ fdinfo を読む
//   pidfd_getfd (438): 相手の fd を写して受けとる (close-on-exec。ptrace で ATTACH できる相手だけ)
//   pidfd_send_signal (424): flags (THREAD、THREAD_GROUP、PROCESS_GROUP のどれか 1 つ) と siginfo (ほかのプロセスへは
//       si_code が負のものだけ)、pidfd のかわりの PIDFD_SELF_THREAD (-10000) と PIDFD_SELF_THREAD_GROUP (-10001)
//
// aios の pid は使いまわさない (proc の NEXT_PID がふえるだけ) ので、pidfd は番号だけを持つ (file::Kind::PidFd)。Linux の
// struct pid の attr にあたる「片づけられたときの exit_code」は、pidfd を開いたことのある番号だけ ATTRS に置き (Linux も
// pidfd がなかった pid には残さない)、その番号の pidfd がみな閉じたら消す
use crate::spinlock::SpinLock;
use alloc::collections::BTreeMap;
use alloc::string::String;

const EPERM: i64 = 1;
const ESRCH: i64 = 3;
const EBADF: i64 = 9;
const EFAULT: i64 = 14;
const ENOTTY: i64 = 25;
const EREMOTE: i64 = 66;

pub const SELF_THREAD: i32 = -10000;
pub const SELF_THREAD_GROUP: i32 = -10001;

const INFO_PID: u64 = 1 << 0;
const INFO_CREDS: u64 = 1 << 1;
const INFO_EXIT: u64 = 1 << 3;
const INFO_COREDUMP: u64 = 1 << 4;
const INFO_SUPPORTED_MASK: u64 = 1 << 5;
const INFO_COREDUMP_SIGNAL: u64 = 1 << 6;
const INFO_COREDUMP_CODE: u64 = 1 << 7;
/// このカーネルが知っている印 (CGROUPID も: cgroup のない Linux と同じく、答えには入らない)
const INFO_SUPPORTED: u64 = 0xff;
const SIZE_VER0: usize = 64;
const SIZE_VER1: usize = 72;
const SIZE_VER2: usize = 80;
const SIZE_VER3: usize = 88;

/// coredump_mask: 落ちたら coredump を作らない / 作る人の権利で / root の権利で (Linux の pidfs_coredump_mask)
const COREDUMP_SKIP: u32 = 1 << 1;
const COREDUMP_USER: u32 = 1 << 2;
const COREDUMP_ROOT: u32 = 1 << 3;

struct Attr {
    /// この番号を指す pidfd (開いたファイル) の数
    refs: u32,
    /// 片づけられたときの wait の status (Linux の pidfs_exit)
    exit: Option<i32>,
}

static ATTRS: SpinLock<BTreeMap<u32, Attr>> = SpinLock::new(BTreeMap::new());

/// pid を指す pidfd を作った (syscall::pidfd_install)
pub fn opened(pid: u32) {
    ATTRS.lock().entry(pid).or_insert(Attr { refs: 0, exit: None }).refs += 1;
}

/// pid を指す pidfd を閉じた (OpenFile を捨てるとき)
pub fn closed(pid: u32) {
    let mut a = ATTRS.lock();
    if let Some(e) = a.get_mut(&pid) {
        e.refs -= 1;
        if e.refs == 0 {
            a.remove(&pid);
        }
    }
}

/// pid のスレッドが片づけられた (proc の free_slot。status は wait の形): pidfd があれば覚える
pub fn reaped(pid: u32, status: i32) {
    if let Some(e) = ATTRS.lock().get_mut(&pid) {
        e.exit = Some(status);
    }
}

fn exit_of(pid: u32) -> Option<i32> {
    ATTRS.lock().get(&pid).and_then(|e| e.exit)
}

/// pid の Proc (回収されるまでのゾンビも。Linux の get_pid_task(pid, PIDTYPE_PID))
fn task(pid: u32) -> Option<&'static mut crate::proc::Proc> {
    crate::proc::find_any(pid)
}

/// fd が pidfd なら (番号、PIDFD_THREAD か)。fd がなければ、pidfd でなければ EBADF (Linux の pidfd_pid)
pub fn of_fd(fd: i64) -> Result<(u32, bool), i64> {
    let f = crate::proc::current().files().get(fd as u64).cloned().ok_or(-EBADF)?;
    let b = f.borrow();
    match b.kind {
        crate::file::Kind::PidFd(pid) => Ok((pid, b.flags & crate::file::PIDFD_THREAD != 0)),
        _ => Err(-EBADF),
    }
}

/// /proc/PID/fdinfo/N の pidfd の行 (Linux の pidfd_show_fdinfo)
pub fn fdinfo(pid: u32, thread: bool) -> String {
    let (_, reaped) = crate::proc::pidfd_state(pid, thread);
    if reaped {
        return "Pid:\t-1\nNSpid:\t-1\n".into();
    }
    let nr = crate::ns::to_local(pid).unwrap_or(0);
    let mut s = alloc::format!("Pid:\t{}\nNSpid:\t{}", nr, nr);
    // 読む人より内の PID の namespace にいれば、そこでの番号も
    if nr > 0
        && crate::proc::current().pid_ns.is_none()
        && let Some(t) = task(pid)
        && t.pid_ns.is_some()
    {
        s.push_str(&alloc::format!("\t{}", t.vpid));
    }
    s.push('\n');
    s
}

/// 持ちもの (mm) の dumpable (PR_GET_DUMPABLE: 0 作らない、1 作る、2 root の権利で)
pub fn dumpable(t: &crate::proc::Proc) -> u8 {
    t.mm.as_ref().map_or(1, |m| m.get().dumpable)
}

fn copy_in(va: usize, b: &mut [u8]) -> Result<(), i64> {
    crate::proc::current().pt().copy_in(b, va).ok_or(-EFAULT)
}

/// pidfd の ioctl。PIDFD_GET_INFO (_IOWR(0xFF, 11, ...) で大きさは 64 から) だけ。ほかは ENOTTY (namespace の fd を
/// 返すもの (PIDFD_GET_*_NAMESPACE) と FS_IOC_GETVERSION は、まだない)
pub fn ioctl(pid: u32, thread: bool, req: u64, arg: usize) -> Result<i64, i64> {
    let req = req as u32;
    let (dir, size, ty, nr) = (req >> 30, (req >> 16) & 0x3fff, (req >> 8) & 0xff, req & 0xff);
    if ty != 0xff || nr != 11 || dir != 3 || (size as usize) < SIZE_VER0 {
        return Err(-ENOTTY);
    }
    info(pid, thread, size as usize, arg)
}

/// PIDFD_GET_INFO (Linux の pidfd_info)。usize は呼んだ人の struct の大きさ
fn info(pid: u32, _thread: bool, usize: usize, arg: usize) -> Result<i64, i64> {
    if arg == 0 {
        return Err(crate::why::inval());
    }
    let mut m = [0u8; 8];
    copy_in(arg, &mut m)?;
    let mask = u64::from_le_bytes(m);
    // 呼んだ人の PID の namespace から見えるものだけ
    if crate::ns::to_local(pid).is_none() && task(pid).is_some() {
        return Err(-EREMOTE);
    }
    let mut k = [0u8; SIZE_VER3];
    let put = |k: &mut [u8; SIZE_VER3], o: usize, v: u32| k[o..o + 4].copy_from_slice(&v.to_le_bytes());
    let mut got = 0u64;
    if mask & INFO_EXIT != 0
        && let Some(code) = exit_of(pid)
    {
        got |= INFO_EXIT;
        put(&mut k, 60, code as u32);
    }
    match task(pid) {
        None => {
            // 片づけられたあとは、終わったときのことだけ
            if mask & INFO_EXIT == 0 {
                return Err(-ESRCH);
            }
        }
        Some(t) => {
            if mask & INFO_COREDUMP != 0 {
                let cm = match dumpable(t) {
                    0 => COREDUMP_SKIP,
                    2 => COREDUMP_ROOT,
                    _ => COREDUMP_USER,
                };
                put(&mut k, 64, cm);
                got |= INFO_COREDUMP;
            }
            // 番号と資格情報はいつも
            let c = &t.cred;
            let uid = |u: u32| crate::ns::show_uid(u);
            let gid = |g: u32| crate::ns::show_gid(g);
            for (o, v) in [(28, uid(c.uid)), (32, gid(c.gid)), (36, uid(c.euid)), (40, gid(c.egid)), (44, uid(c.suid)), (48, gid(c.sgid)), (52, uid(c.fsuid)), (56, gid(c.fsgid))] {
                put(&mut k, o, v);
            }
            got |= INFO_CREDS;
            put(&mut k, 16, crate::ns::local_or_0(pid));
            put(&mut k, 20, crate::ns::local_or_0(t.tgid));
            put(&mut k, 24, crate::ns::local_or_0(t.ppid));
            got |= INFO_PID;
        }
    }
    if mask & INFO_SUPPORTED_MASK != 0 {
        got |= INFO_SUPPORTED_MASK;
        k[80..88].copy_from_slice(&INFO_SUPPORTED.to_le_bytes());
    }
    // 呼んだ人の struct に入らない欄の印は落とす
    if usize < SIZE_VER1 {
        got &= !(INFO_COREDUMP | INFO_COREDUMP_SIGNAL);
    }
    if usize < SIZE_VER2 {
        got &= !INFO_COREDUMP_CODE;
    }
    if usize < SIZE_VER3 {
        got &= !INFO_SUPPORTED_MASK;
    }
    k[0..8].copy_from_slice(&got.to_le_bytes());
    // 小さければ入るところまで、大きければ知らない続きを 0 に (Linux の copy_struct_to_user)
    let pt = crate::proc::current().pt();
    let n = usize.min(SIZE_VER3);
    pt.copy_out(arg, &k[..n]).ok_or(-EFAULT)?;
    if usize > SIZE_VER3 {
        pt.copy_out(arg + SIZE_VER3, &alloc::vec![0u8; usize - SIZE_VER3]).ok_or(-EFAULT)?;
    }
    Ok(0)
}

/// いまのプロセスが t を ptrace で ATTACH できるか (Linux の __ptrace_may_access の PTRACE_MODE_ATTACH_REALCREDS):
/// 同じスレッドグループか、実の uid と gid が相手の 3 つの id とみな同じで、相手が dumpable (でなければ CAP_SYS_PTRACE)。
/// それから landlock と AppArmor の ptrace の決まり
fn may_attach(t: &crate::proc::Proc) -> Result<(), i64> {
    let me = crate::proc::current();
    if me.tgid == t.tgid {
        return Ok(());
    }
    let (c, tc) = (&me.cred, &t.cred);
    let ids = c.uid == tc.uid && c.uid == tc.euid && c.uid == tc.suid && c.gid == tc.gid && c.gid == tc.egid && c.gid == tc.sgid;
    if !ids && !c.capable(crate::cred::CAP_SYS_PTRACE) {
        return Err(-EPERM);
    }
    if dumpable(t) != 1 && !c.capable(crate::cred::CAP_SYS_PTRACE) {
        return Err(-EPERM);
    }
    crate::landlock::check_ptrace(c.landlock.as_ref(), tc.landlock.as_ref())?;
    crate::apparmor::may_ptrace(tc, false).map_err(|_| -EPERM)
}

/// pidfd_getfd(pidfd, fd, flags): pidfd の指すものの fd を写して、いまのプロセスの fd (close-on-exec) に
pub fn getfd(pidfd: i64, fd: i64, flags: u64) -> Result<i64, i64> {
    if flags != 0 {
        return Err(crate::why::inval());
    }
    let (pid, _) = of_fd(pidfd)?;
    let t = task(pid).ok_or(-ESRCH)?;
    may_attach(t)?;
    // 終わりかけ (ゾンビ) は fd の表がもうない
    if t.state == crate::proc::State::Zombie || t.files.is_none() {
        return Err(-ESRCH);
    }
    let f = if fd < 0 { None } else { t.files().get(fd as u64).cloned() };
    let f = f.ok_or(-EBADF)?;
    let n = crate::proc::current().files().add(f, true, 0).ok_or(-24)?;
    Ok(n as i64)
}

/// pidfd_send_signal(pidfd, sig, info, flags)
pub fn send_signal(pidfd: i64, sig: i32, info: usize, flags: u64) -> Result<i64, i64> {
    use crate::signal::{SI_TKILL, SI_USER, SigInfo};
    const THREAD: u64 = 1;
    const THREAD_GROUP: u64 = 2;
    const PROCESS_GROUP: u64 = 4;
    const ALL: u64 = THREAD | THREAD_GROUP | PROCESS_GROUP;
    #[derive(PartialEq, PartialOrd)]
    enum Ty {
        Pid,
        Tgid,
        Pgid,
    }
    if flags & !ALL != 0 || (flags & ALL).count_ones() > 1 {
        return Err(crate::why::inval());
    }
    let me = crate::proc::current();
    let (pid, thread) = match pidfd as i32 {
        SELF_THREAD => (me.pid, true),
        SELF_THREAD_GROUP => (me.tgid, false),
        _ => {
            let (pid, thread) = of_fd(pidfd)?;
            // 見えない PID の namespace のもの
            if crate::ns::to_local(pid).is_none() && task(pid).is_some() {
                return Err(crate::why::inval());
            }
            (pid, thread)
        }
    };
    let ty = match flags {
        THREAD => Ty::Pid,
        THREAD_GROUP => Ty::Tgid,
        PROCESS_GROUP => Ty::Pgid,
        _ if thread => Ty::Pid,
        _ => Ty::Tgid,
    };
    let si = if info != 0 {
        // siginfo_t (128 バイト): si_signo、si_errno、si_code、そして si_pid、si_uid、si_value
        let mut b = [0u8; 128];
        copy_in(info, &mut b)?;
        let w = |o: usize| i32::from_le_bytes(b[o..o + 4].try_into().unwrap());
        if w(0) != sig {
            return Err(crate::why::inval());
        }
        // カーネルの印 (0 以上と SI_TKILL) を名のれるのは、自分 (のスレッドグループ) へだけ
        let code = w(8);
        if (me.pid != pid || ty > Ty::Tgid) && (code >= 0 || code == SI_TKILL) {
            return Err(-EPERM);
        }
        let spid = w(16) as u32;
        SigInfo { code, pid: crate::ns::to_global(spid).unwrap_or(spid), uid: w(20) as u32, value: u64::from_le_bytes(b[24..32].try_into().unwrap()), ..SigInfo::ZERO }
    } else {
        SigInfo::from(if ty == Ty::Pid { SI_TKILL } else { SI_USER })
    };
    match ty {
        // pidfd の指すプロセスの番号のプロセスグループ (Linux の kill_pgrp_info)
        Ty::Pgid => crate::signal::send_pgrp_info(pid, sig, si),
        Ty::Pid => crate::signal::send_pid_info(pid, true, sig, si),
        Ty::Tgid => crate::signal::send_pid_info(pid, false, sig, si),
    }
}

// kcmp: 2 つのプロセス (スレッド) が持ちものを分けているか (Linux の CONFIG_KCMP: kernel/kcmp.c。CHECKPOINT_RESTORE が
// 選ぶ)。CRIU がプロセスの木を写すときと、Mesa が DRM の fd が同じ開いたものか (os_same_file_description) を見るときに使う。
// 持ちもの (開いたファイル、アドレス空間、fd の表、cwd と root、シグナルのハンドラ) の番地を、起動ごとの乱数でまぜて
// くらべる (Linux の kcmp_ptr: 番地そのものは見せない)。aios は fd の表と cwd・root を 1 つ (proc::Files) に持つので、
// KCMP_FILES と KCMP_FS は同じ答え (CLONE_FILES と CLONE_FS をいっしょに使うときは Linux と同じ)。KCMP_IO は Linux の
// io_context がないときと同じくいつも同じ。main.rs が syscall::register でつなぐ
use crate::proc::{self, Proc};
use alloc::rc::Rc;

const EPERM: i64 = 1;
const ESRCH: i64 = 3;
const EBADF: i64 = 9;
const EFAULT: i64 = 14;

const KCMP_FILE: u64 = 0;
const KCMP_VM: u64 = 1;
const KCMP_FILES: u64 = 2;
const KCMP_FS: u64 = 3;
const KCMP_SIGHAND: u64 = 4;
const KCMP_IO: u64 = 5;
const KCMP_SYSVSEM: u64 = 6;
const KCMP_EPOLL_TFD: u64 = 7;

pub const SYSCALLS: &[u64] = &[crate::syscall::nr::KCMP];

pub fn sys(n: u64, a: &[u64; 6]) -> Result<i64, i64> {
    match n {
        crate::syscall::nr::KCMP => kcmp(a[0] as u32 as i32, a[1] as u32 as i32, a[2], a[3], a[4]),
        _ => Err(-38),
    }
}

/// 型ごとの乱数 (はじめて使うときに決める: Linux の cookies。かける数は奇数)
static COOKIES: crate::cell::Bkl<Option<[(u64, u64); 8]>> = crate::cell::Bkl::new(None);

/// a と b の番地をまぜてくらべる: 同じなら 0、小さければ 1、大きければ 2 (Linux の kcmp_ptr)
fn cmp_ptr(a: usize, b: usize, ty: u64) -> i64 {
    let (x, m) = COOKIES.get().get_or_insert_with(|| core::array::from_fn(|_| (crate::rand::u64(), crate::rand::u64() | 1)))[ty as usize];
    let f = |v: usize| (v as u64 ^ x).wrapping_mul(m) as i64;
    let (t1, t2) = (f(a), f(b));
    (t1 < t2) as i64 | ((t1 > t2) as i64) << 1
}

/// いまのプロセスから見た番号のスレッド (Linux の find_task_by_vpid)
fn task(pid: i32) -> Result<&'static mut Proc, i64> {
    if pid <= 0 {
        return Err(-ESRCH);
    }
    let g = crate::ns::to_global(pid as u32).ok_or(-ESRCH)?;
    proc::find_thread(g).ok_or(-ESRCH)
}

/// 見てよいか (Linux の ptrace_may_access の PTRACE_MODE_READ_REALCREDS): 同じプロセスか、uid と gid がみな同じで
/// dumpable なもの、でなければ CAP_SYS_PTRACE。LSM (landlock と AppArmor の read) も
fn may_read(t: &Proc) -> bool {
    if t.tgid == proc::current().tgid {
        return true;
    }
    let (c, tc) = (crate::cred::current(), &t.cred);
    let ids = c.uid == tc.uid && c.uid == tc.euid && c.uid == tc.suid && c.gid == tc.gid && c.gid == tc.egid && c.gid == tc.sgid;
    if !(ids && crate::pidfd::dumpable(t) == 1) && !c.capable(crate::cred::CAP_SYS_PTRACE) {
        return false;
    }
    crate::landlock::check_ptrace(c.sandbox.as_ref(), tc.sandbox.as_ref()).is_ok() && crate::apparmor::may_ptrace(tc, true).is_ok()
}

/// t の fd の開いたもの
fn file(t: &Proc, fd: u64) -> Option<crate::file::FileRef> {
    t.files.as_ref()?.get().get(fd).cloned()
}

fn kcmp(pid1: i32, pid2: i32, ty: u64, idx1: u64, idx2: u64) -> Result<i64, i64> {
    let t1: &Proc = task(pid1)?;
    let t2: &Proc = task(pid2)?;
    if !may_read(t1) || !may_read(t2) {
        return Err(-EPERM);
    }
    let files = |t: &Proc| t.files.as_ref().map_or(0, |f| f.id());
    let sighand = |t: &Proc| t.sigacts.as_ref().map_or(0, |s| s.id());
    Ok(match ty {
        KCMP_FILE => match (file(t1, idx1), file(t2, idx2)) {
            (Some(a), Some(b)) => cmp_ptr(Rc::as_ptr(&a) as usize, Rc::as_ptr(&b) as usize, ty),
            _ => return Err(-EBADF),
        },
        KCMP_VM => cmp_ptr(t1.mm_id(), t2.mm_id(), ty),
        KCMP_FILES | KCMP_FS => cmp_ptr(files(t1), files(t2), ty),
        KCMP_SIGHAND => cmp_ptr(sighand(t1), sighand(t2), ty),
        KCMP_IO => cmp_ptr(0, 0, ty),
        KCMP_SYSVSEM => cmp_ptr(crate::sem::undo_id(t1.tgid), crate::sem::undo_id(t2.tgid), ty),
        KCMP_EPOLL_TFD => epoll_tfd(t1, t2, idx1, idx2 as usize)?,
        _ => return Err(crate::why::inval()),
    })
}

/// KCMP_EPOLL_TFD: t1 の fd idx1 と、t2 の epoll (struct kcmp_epoll_slot { efd, tfd, toff }) に入れたもの
/// (Linux の kcmp_epoll_target: epoll でなければ EINVAL、入っていなければ ENOENT)
fn epoll_tfd(t1: &Proc, t2: &Proc, idx1: u64, slot: usize) -> Result<i64, i64> {
    let mut b = [0u8; 12];
    proc::current().pt().copy_in(&mut b, slot).ok_or(-EFAULT)?;
    let w = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let a = file(t1, idx1).ok_or(-EBADF)?;
    let ep = file(t2, w(0) as u64).ok_or(-EBADF)?;
    let target = crate::epoll::target_of(&ep, w(4), w(8))?;
    Ok(cmp_ptr(Rc::as_ptr(&a) as usize, Rc::as_ptr(&target) as usize, KCMP_FILE))
}

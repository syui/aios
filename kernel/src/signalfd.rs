// signalfd (signalfd4)。ブロックしたシグナルを fd から読む (Linux の fs/signalfd.c)。read で、読むスレッドに来ている
// mask のシグナルを 1 つずつとり出し、struct signalfd_siginfo (128 バイト) にする (ハンドラは動かない)。なければ待つ
// (O_NONBLOCK なら EAGAIN)。poll / epoll は、待つスレッドに mask のシグナルが来ていれば読める。ブロックしたまま
// シグナルが来たら、signal.rs の send_thread がそのプロセスのチャンネル (signal::fd_chan) を起こす。
// PostgreSQL 17 / 18 は、ラッチ (WaitEventSet: epoll) で SIGURG をこれで待つ (なければ initdb が「signalfd() failed」で止まる)
use crate::file::{self, Kind};
use crate::proc;
use alloc::rc::Rc;
use core::cell::Cell;

const EAGAIN: i64 = 11;
const EBADF: i64 = 9;
const EFAULT: i64 = 14;
const SFD_CLOEXEC: u64 = 0o2000000;
const SFD_NONBLOCK: u64 = 0o4000;
/// struct signalfd_siginfo の大きさ
const INFO: usize = 128;

/// signalfd の fd の中身 (file::Kind::Ops。Linux の signalfd_fops)。mask は signalfd4 で変えられる
pub struct SignalFile {
    mask: Cell<u64>,
}

fn read(s: &SignalFile, dst: &mut [u8], nonblock: bool) -> Result<usize, i64> {
    if dst.len() < INFO {
        return Err(crate::why::inval());
    }
    loop {
        let mut n = 0;
        while n + INFO <= dst.len() {
            let Some((sig, info)) = crate::signal::take(s.mask.get()) else { break };
            dst[n..n + INFO].copy_from_slice(&crate::signal::signalfd_info(sig, &info));
            n += INFO;
        }
        if n > 0 {
            return Ok(n);
        }
        if nonblock {
            return Err(-EAGAIN);
        }
        // ブロックしていないシグナルが来れば EINTR で戻る (proc::interrupt)
        proc::sleep_until(crate::signal::fd_chan(proc::current().tgid), 0)?;
    }
}

impl file::FileOps for SignalFile {
    fn describe(&self) -> alloc::string::String {
        "anon_inode:[signalfd]".into()
    }
    fn class(&self) -> usize {
        9
    }
    fn stream(&self) -> bool {
        true
    }
    fn read(&self, dst: &mut [u8], nonblock: bool) -> Result<usize, i64> {
        read(self, dst, nonblock)
    }
    fn write(&self, _src: &[u8], _nonblock: bool) -> Result<usize, i64> {
        Err(crate::why::inval())
    }
    fn fdinfo(&self) -> alloc::string::String {
        // Linux と同じ: 待っているシグナルの組 (16 進)
        alloc::format!("sigmask:\t{:016x}\n", self.mask.get())
    }
    fn event_gen(&self) -> u64 {
        crate::signal::GEN.load(core::sync::atomic::Ordering::Relaxed)
    }
    fn readiness(&self) -> (bool, bool, bool) {
        (crate::signal::pending_in(self.mask.get()), false, false)
    }
    fn poll_keys(&self) -> Option<alloc::vec::Vec<usize>> {
        Some(alloc::vec![crate::signal::fd_chan(proc::current().tgid)])
    }
}

/// signalfd4(fd, mask, sizemask, flags): fd が -1 なら新しく作り、signalfd の fd ならその mask を変える (flags は
/// 見ない)。SIGKILL と SIGSTOP は mask から落とす (Linux と同じ)。読むのは、読むスレッドに来ているものだけ (Linux は
/// プロセスに来たもの (shared_pending) も読むが、aios はプロセスに送ったものもどれかのスレッドに来る)
fn signalfd4(fd: i64, mask: usize, size: usize, flags: u64) -> Result<i64, i64> {
    // Linux と同じ順: 大きさ、mask を読む、flags
    if size != 8 {
        return Err(crate::why::inval());
    }
    let mut b = [0u8; 8];
    proc::current().pt().copy_in(&mut b, mask).ok_or(-EFAULT)?;
    if flags & !(SFD_CLOEXEC | SFD_NONBLOCK) != 0 {
        return Err(crate::why::inval());
    }
    let m = u64::from_le_bytes(b) & !(1 << (crate::signal::SIGKILL - 1) | 1 << (crate::signal::SIGSTOP - 1));
    if fd != -1 {
        let f = proc::current().files().get(fd as u64).cloned().ok_or(-EBADF)?;
        let k = f.borrow();
        let s = k.kind.ops::<SignalFile>().ok_or_else(|| crate::why::inval())?;
        s.mask.set(m);
        return Ok(fd);
    }
    // Linux と同じく O_RDWR で開く (anon_inode_getfile)。書けば EINVAL
    let fl = file::O_RDWR | if flags & SFD_NONBLOCK != 0 { file::O_NONBLOCK } else { 0 };
    let fd = proc::current().files().add(file::new(Kind::Ops(Rc::new(SignalFile { mask: Cell::new(m) })), fl), flags & SFD_CLOEXEC != 0, 0).ok_or(-24)?;
    Ok(fd as i64)
}

/// システムコール (main.rs が syscall::register で置く。Linux の fs/signalfd.c)
pub const SYSCALLS: &[u64] = {
    use crate::syscall::nr::*;
    &[SIGNALFD4]
};

pub fn sys(n: u64, a: &[u64; 6]) -> Result<i64, i64> {
    match n {
        crate::syscall::nr::SIGNALFD4 => signalfd4(a[0] as i32 as i64, a[1] as usize, a[2] as usize, a[3]),
        _ => Err(-38),
    }
}

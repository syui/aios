// seccomp: プロセスが使えるシステムコールをしぼる (Linux と同じ番号と形)
//   seccomp(SECCOMP_SET_MODE_FILTER, flags, &sock_fprog) / prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &sock_fprog)
//     フィルタ (classic BPF。seccomp_data を読んで答えを返す) を足す。足したものは外せず、fork と exec で引き継ぐ
//   seccomp(SECCOMP_SET_MODE_STRICT) / prctl(PR_SET_SECCOMP, SECCOMP_MODE_STRICT)
//     read / write / exit / rt_sigreturn だけ (ほかは SIGKILL)
//   フィルタをかけるには prctl(PR_SET_NO_NEW_PRIVS) か root が要る (setuid のプログラムをだませないように。landlock と同じ)
// システムコールのたびに、かかっているフィルタをぜんぶ動かし、いちばんきびしい答えにしたがう:
//   KILL_PROCESS > KILL_THREAD > TRAP (SIGSYS) > ERRNO > USER_NOTIF > TRACE > LOG > ALLOW
// USER_NOTIF は、そのフィルタを NEW_LISTENER でかけたときにできた listener (fd) を持つ人に知らせて答えを待つ
// (user notification: 下の「user notification」)。listener がなければ ENOSYS。TRACE は受け手 (ptrace) がないので ENOSYS
// (Linux も受け手がなければそう)
use crate::cred::Cred;
use crate::proc;
use crate::syscall::AUDIT_ARCH_AARCH64;
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;

const EFAULT: i64 = 14;
const EACCES: i64 = 13;
const ENOMEM: i64 = 12;
const ENOSYS: i64 = 38;
const EOPNOTSUPP: i64 = 95;

pub const MODE_DISABLED: u64 = 0;
pub const MODE_STRICT: u64 = 1;
pub const MODE_FILTER: u64 = 2;

const SET_MODE_STRICT: u64 = 0;
const SET_MODE_FILTER: u64 = 1;
const GET_ACTION_AVAIL: u64 = 2;
const GET_NOTIF_SIZES: u64 = 3;
const FLAG_TSYNC: u64 = 1;
const FLAG_LOG: u64 = 2;
const FLAG_SPEC_ALLOW: u64 = 4;
const FLAG_NEW_LISTENER: u64 = 8;
const FLAG_TSYNC_ESRCH: u64 = 16;
const FLAG_WAIT_KILLABLE_RECV: u64 = 32;
/// Linux 7.4: 受け手が受ける前にシグナルで割りこまれたら、ハンドラのあとでいつもやりなおす (ERESTARTNOINTR)
const FLAG_RESTART_BEFORE_RECV: u64 = 64;

const RET_KILL_PROCESS: u32 = 0x8000_0000;
const RET_KILL_THREAD: u32 = 0x0000_0000;
const RET_TRAP: u32 = 0x0003_0000;
const RET_ERRNO: u32 = 0x0005_0000;
const RET_USER_NOTIF: u32 = 0x7fc0_0000;
const RET_TRACE: u32 = 0x7ff0_0000;
const RET_LOG: u32 = 0x7ffc_0000;
const RET_ALLOW: u32 = 0x7fff_0000;
const RET_ACTION_FULL: u32 = 0xffff_0000;
const RET_DATA: u32 = 0x0000_ffff;

/// seccomp_data の大きさ (nr 4、arch 4、instruction_pointer 8、args 8 × 6)
const DATA_LEN: u32 = 64;
const MAX_INSNS: usize = 4096;
/// 重ねたフィルタの命令の合計の上限 (Linux と同じ)
const MAX_TOTAL: usize = 32768;

/// BPF の命令 (struct sock_filter)
#[derive(Clone, Copy)]
pub struct Insn {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

/// かかっているもの: strict か、フィルタの重なり (新しいものが後ろ)
pub struct State {
    pub strict: bool,
    pub filters: Vec<Rc<Filter>>,
}

/// フィルタ 1 つ (Linux の seccomp_filter): プログラムと、NEW_LISTENER でかけたなら user notification の受け口
pub struct Filter {
    prog: Vec<Insn>,
    /// listener があるあいだの、待っている知らせ (大きなロックの中で)
    notif: RefCell<Option<Notif>>,
    wait_killable: bool,
    restart_before_recv: bool,
}

type Seccomp = Rc<State>;

/// c にかかっているもの (Cred の seccomp の中。core は中を知らない: security::Blob)
fn state(c: &Cred) -> Option<&State> {
    c.seccomp.as_ref().map(|b| b.downcast_ref::<State>().expect("seccomp: state"))
}

/// /proc/PID/status の Seccomp: (0 なし、1 strict、2 filter) と Seccomp_filters:
pub fn mode(c: &Cred) -> (u64, usize) {
    match state(c) {
        None => (MODE_DISABLED, 0),
        Some(s) if s.strict => (MODE_STRICT, s.filters.len()),
        Some(s) => (MODE_FILTER, s.filters.len()),
    }
}

// ---- BPF ----

const LD: u16 = 0x00;
const LDX: u16 = 0x01;
const ST: u16 = 0x02;
const STX: u16 = 0x03;
const ALU: u16 = 0x04;
const JMP: u16 = 0x05;
const RET: u16 = 0x06;
const MISC: u16 = 0x07;
const W: u16 = 0x00;
const IMM: u16 = 0x00;
const ABS: u16 = 0x20;
const MEM: u16 = 0x60;
const LEN: u16 = 0x80;
const K: u16 = 0x00;
const X: u16 = 0x08;
const A: u16 = 0x10;

// ---- user notification ----
// フィルタが RET_USER_NOTIF と答えたシステムコールは、知らせ (Knotif: id、スレッド、seccomp_data) を並べ、listener の
// 持ち主が NOTIF_RECV で受けて NOTIF_SEND で答えるまで眠る (Linux の seccomp_do_user_notification)。答えは値かエラー、
// CONTINUE ならそのままシステムコールをする。NOTIF_ADDFD は答えの前に相手の fd の表に fd を入れる。listener を閉じると、
// 待っているものはみな ENOSYS、それからの知らせも ENOSYS。眠りはシグナルで起き (知らせは消える)、RESTART_BEFORE_RECV
// なら受ける前のものはいつもやりなおす (ERESTARTNOINTR)、WAIT_KILLABLE_RECV なら受けたあとは SIGKILL だけで起きる

const ENOENT: i64 = 2;
const EBADF: i64 = 9;
const EBUSY: i64 = 16;
const EINPROGRESS: i64 = 115;
const E2BIG: i64 = 7;
const NOTIFY_INIT: u8 = 0;
const NOTIFY_SENT: u8 = 1;
const NOTIFY_REPLIED: u8 = 2;
const USER_NOTIF_FLAG_CONTINUE: u32 = 1;
const USER_NOTIF_FD_SYNC_WAKE_UP: u64 = 1;
const ADDFD_FLAG_SETFD: u32 = 1;
const ADDFD_FLAG_SEND: u32 = 2;
const O_CLOEXEC: u32 = 0o2000000;
/// struct seccomp_notif (id 8、pid 4、flags 4、seccomp_data 64)、seccomp_notif_resp (id 8、val 8、error 4、flags 4)、
/// seccomp_notif_addfd (id 8、flags、srcfd、newfd、newfd_flags 4 ずつ)
const NOTIF_SIZE: usize = 80;
const RESP_SIZE: usize = 24;
const ADDFD_SIZE: usize = 24;
/// ioctl の番号 ('!' の _IOWR / _IOW。ID_VALID は昔のまちがえた向きのものも)
const IOCTL_NOTIF_RECV: u64 = 0xc050_2100;
const IOCTL_NOTIF_SEND: u64 = 0xc018_2101;
const IOCTL_NOTIF_ID_VALID: u64 = 0x4008_2102;
const IOCTL_NOTIF_ID_VALID_WRONG_DIR: u64 = 0x8008_2102;
const IOCTL_NOTIF_ADDFD: u64 = 0x4018_2103;
const IOCTL_NOTIF_SET_FLAGS: u64 = 0x4008_2104;

/// 待っている知らせ
struct Knotif {
    id: u64,
    /// 知らせたスレッド (本当の番号)
    tid: u32,
    state: u8,
    data: [u8; DATA_LEN as usize],
    error: i64,
    val: i64,
    flags: u32,
}

/// listener の受け口 (Linux の notification)
struct Notif {
    next_id: u64,
    list: Vec<Knotif>,
    /// listener を閉じた (待っていたものは ENOSYS で起こした。新しい知らせは作らない)
    closed: bool,
}

/// listener の fd (anon_inode:seccomp notify)。さいごの参照を閉じると、待っているものはみな ENOSYS
pub struct Listener(Rc<Filter>);

impl Drop for Listener {
    fn drop(&mut self) {
        let f = &self.0;
        if let Some(n) = f.notif.borrow_mut().as_mut() {
            n.closed = true;
            for k in n.list.iter_mut().filter(|k| k.state != NOTIFY_REPLIED) {
                k.state = NOTIFY_REPLIED;
                k.error = -ENOSYS;
                k.val = 0;
                k.flags = 0;
            }
        }
        proc::wakeup(notify_chan(f));
    }
}

/// 知らせたものが眠る chan (答え、ADDFD、listener を閉じたとき)
fn notify_chan(f: &Rc<Filter>) -> usize {
    Rc::as_ptr(f) as usize + 1
}

/// listener の NOTIF_RECV が眠り、poll が見る chan
pub fn listener_chan(l: &Listener) -> usize {
    Rc::as_ptr(&l.0) as usize
}

fn wake_listener(f: &Rc<Filter>) {
    proc::wakeup(Rc::as_ptr(f) as usize);
    proc::poll_wake(Rc::as_ptr(f) as usize);
}

/// そのフィルタをかけているスレッドの数 (Linux の users: 回収されるまでのゾンビもふくむ)。0 なら listener は POLLHUP
fn users(f: &Rc<Filter>) -> usize {
    proc::count_live(|c| state(c).is_some_and(|s| s.filters.iter().any(|x| Rc::ptr_eq(x, f))))
}

/// スレッドの Proc が片づけられる (proc::free_slot): そのフィルタの listener を起こす (使う人がいなくなったかも)
pub fn released(c: &Cred) {
    if let Some(s) = state(c) {
        for f in s.filters.iter().filter(|f| f.notif.borrow().is_some()) {
            wake_listener(f);
        }
    }
}

/// 知らせて答えを待った結果
pub enum Notified {
    /// この値で返す (システムコールはしない)
    Return(i64),
    /// CONTINUE: システムコールをする
    Continue,
    /// シグナルで割りこまれた (always ならハンドラのあとでもいつもやりなおす)
    Interrupted { always: bool },
}

/// RET_USER_NOTIF: f の listener に知らせて、答えを待つ
pub fn notify(f: &Rc<Filter>, data: [u8; DATA_LEN as usize]) -> Notified {
    let id = {
        let mut n = f.notif.borrow_mut();
        let Some(n) = n.as_mut().filter(|n| !n.closed) else { return Notified::Return(-ENOSYS) };
        let id = n.next_id;
        n.next_id = n.next_id.wrapping_add(1);
        n.list.push(Knotif { id, tid: proc::current().pid, state: NOTIFY_INIT, data, error: 0, val: 0, flags: 0 });
        id
    };
    wake_listener(f);
    let state = |f: &Rc<Filter>| f.notif.borrow().as_ref().and_then(|n| n.list.iter().find(|k| k.id == id).map(|k| k.state)).unwrap_or(NOTIFY_REPLIED);
    loop {
        let st = state(f);
        if st == NOTIFY_REPLIED {
            break;
        }
        // 受けられたあとは、WAIT_KILLABLE_RECV なら SIGKILL でだけ起きる
        let killable = f.wait_killable && st >= NOTIFY_SENT;
        let r = if killable { proc::sleep_killable(notify_chan(f)) } else { proc::sleep(notify_chan(f)) };
        if r.is_err() {
            let st = state(f);
            if st == NOTIFY_REPLIED {
                break;
            }
            // 眠っているあいだに受けられた: WAIT_KILLABLE_RECV なら、ふつうのシグナルではやめずに待ちなおす
            if !killable && f.wait_killable && st >= NOTIFY_SENT && !proc::current().killed {
                continue;
            }
            let always = f.restart_before_recv && st == NOTIFY_INIT;
            remove(f, id);
            return Notified::Interrupted { always };
        }
    }
    let k = remove(f, id);
    match k {
        Some(k) if k.flags & USER_NOTIF_FLAG_CONTINUE != 0 => Notified::Continue,
        Some(k) => Notified::Return(if k.error != 0 { k.error } else { k.val }),
        None => Notified::Return(-ENOSYS),
    }
}

fn remove(f: &Rc<Filter>, id: u64) -> Option<Knotif> {
    let mut n = f.notif.borrow_mut();
    let n = n.as_mut()?;
    let i = n.list.iter().position(|k| k.id == id)?;
    Some(n.list.remove(i))
}

/// listener の poll: (読める: まだ受けていない知らせ, 書ける: 受けて答えていない知らせ, POLLHUP: 使う人がいない)
pub fn readiness(l: &Listener) -> (bool, bool, bool) {
    let f = &l.0;
    let (mut i, mut o) = (false, false);
    if let Some(n) = f.notif.borrow().as_ref() {
        i = n.list.iter().any(|k| k.state == NOTIFY_INIT);
        o = n.list.iter().any(|k| k.state == NOTIFY_SENT);
    }
    (i, o, users(f) == 0)
}

/// listener の ioctl (Linux の seccomp_notify_ioctl)。l は fd の OpenFile を借りずに写したもの (RECV は眠る)
pub fn ioctl(l: &Listener, req: u64, arg: usize) -> Result<i64, i64> {
    let f = &l.0;
    match req {
        IOCTL_NOTIF_RECV => recv(f, arg),
        IOCTL_NOTIF_SEND => send(f, arg),
        IOCTL_NOTIF_ID_VALID | IOCTL_NOTIF_ID_VALID_WRONG_DIR => {
            let id = u64::from_le_bytes(read_user::<8>(arg)?);
            let ok = f.notif.borrow().as_ref().is_some_and(|n| n.list.iter().any(|k| k.id == id && k.state == NOTIFY_SENT));
            if ok { Ok(0) } else { Err(-ENOENT) }
        }
        IOCTL_NOTIF_SET_FLAGS => {
            // FD_SYNC_WAKE_UP: 答えたら同じ CPU で起こす (aios はいつも同じ起こしかた)
            if arg as u64 & !USER_NOTIF_FD_SYNC_WAKE_UP != 0 {
                return Err(crate::why::inval());
            }
            Ok(0)
        }
        // 大きさの入る形 (Linux の EA_IOCTL: 向きと大きさをのぞいて番号で)
        r if r & !(0xc000_0000 | 0x3fff_0000) == IOCTL_NOTIF_ADDFD & 0xffff => addfd(f, arg, ((r >> 16) & 0x3fff) as usize),
        _ => Err(crate::why::inval()),
    }
}

fn read_user<const N: usize>(va: usize) -> Result<[u8; N], i64> {
    let mut b = [0u8; N];
    proc::current().pt().copy_in(&mut b, va).ok_or(-EFAULT)?;
    Ok(b)
}

/// NOTIF_RECV: まだ受けていない知らせを待って (使う人がいなくなれば ENOENT)、受けたことにして渡す。入れ物は 0 でなければ EINVAL
fn recv(f: &Rc<Filter>, arg: usize) -> Result<i64, i64> {
    if read_user::<NOTIF_SIZE>(arg)?.iter().any(|&b| b != 0) {
        return Err(crate::why::inval());
    }
    let chan = Rc::as_ptr(f) as usize;
    let (id, tid, data) = loop {
        let found = f.notif.borrow().as_ref().and_then(|n| n.list.iter().find(|k| k.state == NOTIFY_INIT).map(|k| (k.id, k.tid, k.data)));
        if let Some(x) = found {
            break x;
        }
        if users(f) == 0 {
            return Err(-ENOENT);
        }
        proc::sleep(chan)?;
    };
    let mut b = [0u8; NOTIF_SIZE];
    b[0..8].copy_from_slice(&id.to_le_bytes());
    b[8..12].copy_from_slice(&crate::ns::local_or_0(tid).to_le_bytes());
    b[16..80].copy_from_slice(&data);
    // 写せなければ受けなかったことに (Linux と同じく EFAULT)
    proc::current().pt().copy_out(arg, &b).ok_or(-EFAULT)?;
    if let Some(n) = f.notif.borrow_mut().as_mut()
        && let Some(k) = n.list.iter_mut().find(|k| k.id == id)
    {
        k.state = NOTIFY_SENT;
    }
    proc::poll_wake(chan);
    Ok(0)
}

/// NOTIF_SEND: 受けた知らせに答える (1 度だけ)
fn send(f: &Rc<Filter>, arg: usize) -> Result<i64, i64> {
    let b = read_user::<RESP_SIZE>(arg)?;
    let id = u64::from_le_bytes(b[0..8].try_into().unwrap());
    let val = i64::from_le_bytes(b[8..16].try_into().unwrap());
    let error = i32::from_le_bytes(b[16..20].try_into().unwrap()) as i64;
    let flags = u32::from_le_bytes(b[20..24].try_into().unwrap());
    if flags & !USER_NOTIF_FLAG_CONTINUE != 0 || (flags & USER_NOTIF_FLAG_CONTINUE != 0 && (error != 0 || val != 0)) {
        return Err(crate::why::inval());
    }
    let mut n = f.notif.borrow_mut();
    let k = n.as_mut().and_then(|n| n.list.iter_mut().find(|k| k.id == id)).ok_or(-ENOENT)?;
    if k.state != NOTIFY_SENT {
        return Err(-EINPROGRESS);
    }
    k.state = NOTIFY_REPLIED;
    k.error = error;
    k.val = val;
    k.flags = flags;
    drop(n);
    proc::wakeup(notify_chan(f));
    Ok(0)
}

/// NOTIF_ADDFD: 受けた知らせのスレッドの fd の表に、こちらの srcfd のファイルを入れる (SETFD なら newfd の番号に、
/// SEND ならそれを答えにする)。入れた番号を返す
fn addfd(f: &Rc<Filter>, arg: usize, size: usize) -> Result<i64, i64> {
    if !(ADDFD_SIZE..4096).contains(&size) {
        return Err(crate::why::inval());
    }
    let mut raw = alloc::vec![0u8; size];
    proc::current().pt().copy_in(&mut raw, arg).ok_or(-EFAULT)?;
    // 知らない後ろのところは 0 (Linux の copy_struct_from_user)
    if raw[ADDFD_SIZE..].iter().any(|&b| b != 0) {
        return Err(-E2BIG);
    }
    let w = |o: usize| u32::from_le_bytes(raw[o..o + 4].try_into().unwrap());
    let id = u64::from_le_bytes(raw[0..8].try_into().unwrap());
    let (flags, srcfd, newfd, newfd_flags) = (w(8), w(12), w(16), w(20));
    if newfd_flags & !O_CLOEXEC != 0 || flags & !(ADDFD_FLAG_SETFD | ADDFD_FLAG_SEND) != 0 || (newfd != 0 && flags & ADDFD_FLAG_SETFD == 0) {
        return Err(crate::why::inval());
    }
    // O_PATH のものも (SCM_RIGHTS と同じ)
    let file = proc::current().files().get(srcfd as u64).cloned().ok_or(-EBADF)?;
    let tid = {
        let n = f.notif.borrow();
        let k = n.as_ref().and_then(|n| n.list.iter().find(|k| k.id == id)).ok_or(-ENOENT)?;
        if k.state != NOTIFY_SENT {
            return Err(-EINPROGRESS);
        }
        k.tid
    };
    let t = proc::find_thread(tid).ok_or(-ENOENT)?;
    let cloexec = newfd_flags & O_CLOEXEC != 0;
    let files = t.files();
    let fd = if flags & ADDFD_FLAG_SETFD != 0 { files.put_at(newfd as usize, file, cloexec)? } else { files.add(file, cloexec, 0).ok_or(-24)? };
    if flags & ADDFD_FLAG_SEND != 0 {
        if let Some(n) = f.notif.borrow_mut().as_mut()
            && let Some(k) = n.list.iter_mut().find(|k| k.id == id)
        {
            k.state = NOTIFY_REPLIED;
            k.error = 0;
            k.val = fd as i64;
            k.flags = 0;
        }
        proc::wakeup(notify_chan(f));
    }
    Ok(fd as i64)
}

/// 読みこんだプログラムを確かめる (使える命令だけ、跳び先は中、読む場所は seccomp_data の中、終わりは RET)
fn validate(p: &[Insn]) -> Result<(), i64> {
    if p.is_empty() || p.len() > MAX_INSNS {
        return Err(crate::why::inval());
    }
    for (i, n) in p.iter().enumerate() {
        let left = (p.len() - 1 - i) as u32;
        let ok = match n.code & 0x07 {
            LD => match n.code {
                c if c == LD | W | ABS => n.k % 4 == 0 && n.k < DATA_LEN,
                c if c == LD | W | LEN || c == LD | IMM => true,
                c if c == LD | MEM => n.k < 16,
                _ => false,
            },
            LDX => match n.code {
                c if c == LDX | W | IMM || c == LDX | W | LEN => true,
                c if c == LDX | MEM => n.k < 16,
                _ => false,
            },
            ST | STX => n.code & !0x07 == 0 && n.k < 16,
            ALU => {
                let op = n.code & 0xf0;
                let src_ok = n.code & !0xf8 == ALU;
                // DIV と MOD の 0 割り (K のとき) は読むときに断る
                src_ok && op <= 0xa0 && !(matches!(op, 0x30 | 0x90) && n.code & X == 0 && n.k == 0)
            }
            JMP => match n.code & 0xf0 {
                0x00 => n.code == JMP && n.k < left,
                0x10..=0x40 => n.code & !0xf8 == JMP && (n.jt as u32) < left && (n.jf as u32) < left,
                _ => false,
            },
            RET => n.code == RET | K || n.code == RET | A,
            MISC => n.code == MISC || n.code == MISC | 0x80,
            _ => false,
        };
        if !ok {
            return Err(crate::why::inval());
        }
    }
    // 終わりまで流れ落ちないように、最後は RET
    if p[p.len() - 1].code & 0x07 != RET {
        return Err(crate::why::inval());
    }
    Ok(())
}

/// プログラムを動かして答え (SECCOMP_RET_*) を出す
fn run(p: &[Insn], data: &[u8; DATA_LEN as usize]) -> u32 {
    let (mut a, mut x): (u32, u32) = (0, 0);
    let mut m = [0u32; 16];
    let mut pc = 0usize;
    let word = |k: u32| u32::from_le_bytes([data[k as usize], data[k as usize + 1], data[k as usize + 2], data[k as usize + 3]]);
    while pc < p.len() {
        let n = p[pc];
        pc += 1;
        match n.code & 0x07 {
            LD => {
                a = match n.code & 0xe0 {
                    ABS => word(n.k),
                    LEN => DATA_LEN,
                    MEM => m[n.k as usize],
                    _ => n.k,
                }
            }
            LDX => {
                x = match n.code & 0xe0 {
                    LEN => DATA_LEN,
                    MEM => m[n.k as usize],
                    _ => n.k,
                }
            }
            ST => m[n.k as usize] = a,
            STX => m[n.k as usize] = x,
            ALU => {
                let v = if n.code & X != 0 { x } else { n.k };
                a = match n.code & 0xf0 {
                    0x00 => a.wrapping_add(v),
                    0x10 => a.wrapping_sub(v),
                    0x20 => a.wrapping_mul(v),
                    0x30 => a.checked_div(v).unwrap_or(0),
                    0x40 => a | v,
                    0x50 => a & v,
                    0x60 => a.checked_shl(v).unwrap_or(0),
                    0x70 => a.checked_shr(v).unwrap_or(0),
                    0x80 => a.wrapping_neg(),
                    0x90 => a.checked_rem(v).unwrap_or(0),
                    _ => a ^ v,
                }
            }
            JMP => {
                let v = if n.code & X != 0 { x } else { n.k };
                let t = match n.code & 0xf0 {
                    0x00 => {
                        pc += n.k as usize;
                        continue;
                    }
                    0x10 => a == v,
                    0x20 => a > v,
                    0x30 => a >= v,
                    _ => a & v != 0,
                };
                pc += if t { n.jt } else { n.jf } as usize;
            }
            RET => return if n.code & A != 0 { a } else { n.k },
            _ => {
                if n.code & 0x80 != 0 {
                    a = x;
                } else {
                    x = a;
                }
            }
        }
    }
    RET_KILL_THREAD
}

/// strict: read 63、write 64、exit 93、rt_sigreturn 139 だけ
fn strict_allows(nr: u64) -> bool {
    matches!(nr, 63 | 64 | 93 | 139)
}

// ---- システムコールのたびに ----

/// したがうこと
pub enum Verdict {
    Allow,
    /// listener に知らせて答えを待つ (notify)
    Notify(Rc<Filter>, [u8; DATA_LEN as usize]),
    /// このエラーで返す (システムコールはしない)
    Errno(i64),
    /// SIGSYS を送る (付帯情報の errno)
    Trap(u16),
    /// SIGSYS で終わる
    Kill,
}

/// システムコール nr (引数 args、呼んだ場所 pc) をしてよいか
pub fn check(s: &State, nr: u64, args: &[u64; 6], pc: u64) -> Verdict {
    if s.strict && !strict_allows(nr) {
        return Verdict::Kill;
    }
    if s.filters.is_empty() {
        return Verdict::Allow;
    }
    let mut d = [0u8; DATA_LEN as usize];
    d[0..4].copy_from_slice(&(nr as u32).to_le_bytes());
    d[4..8].copy_from_slice(&AUDIT_ARCH_AARCH64.to_le_bytes());
    d[8..16].copy_from_slice(&pc.to_le_bytes());
    for (i, v) in args.iter().enumerate() {
        d[16 + i * 8..24 + i * 8].copy_from_slice(&v.to_le_bytes());
    }
    // いちばんきびしいもの (答えの上 16 ビットを符号つきでくらべて小さいほう。同じなら新しいフィルタ)
    let mut best = RET_ALLOW;
    let mut matched: Option<&Rc<Filter>> = None;
    for f in s.filters.iter().rev() {
        let r = run(&f.prog, &d);
        if ((r & RET_ACTION_FULL) as i32) < ((best & RET_ACTION_FULL) as i32) {
            best = r;
            matched = Some(f);
        }
    }
    let data = (best & RET_DATA) as u16;
    match best & RET_ACTION_FULL {
        RET_ALLOW => Verdict::Allow,
        RET_LOG => {
            println!("seccomp: pid {} syscall {} (log)", proc::current().pid, nr);
            Verdict::Allow
        }
        RET_ERRNO => Verdict::Errno(-(data.min(4095) as i64)),
        RET_TRAP => Verdict::Trap(data),
        RET_USER_NOTIF => match matched {
            Some(f) => Verdict::Notify(f.clone(), d),
            None => Verdict::Errno(-ENOSYS),
        },
        RET_TRACE => Verdict::Errno(-ENOSYS),
        // KILL_THREAD、KILL_PROCESS、知らない答え (Linux は KILL_PROCESS とみなす)
        _ => Verdict::Kill,
    }
}

// ---- かける ----

fn may_set(c: &Cred) -> Result<(), i64> {
    if !c.no_new_privs && !c.capable_noaudit(crate::cred::CAP_SYS_ADMIN) {
        return Err(-EACCES);
    }
    Ok(())
}

/// ユーザーの sock_fprog (len u16、間 6 バイト、filter のアドレス u64) を読む
fn read_prog(addr: usize) -> Result<Vec<Insn>, i64> {
    let p = proc::current();
    let mut h = [0u8; 16];
    p.pt().copy_in(&mut h, addr).ok_or(-EFAULT)?;
    let len = u16::from_le_bytes([h[0], h[1]]) as usize;
    let ptr = u64::from_le_bytes(h[8..16].try_into().unwrap()) as usize;
    if len == 0 || len > MAX_INSNS {
        return Err(crate::why::inval());
    }
    let mut raw = alloc::vec![0u8; len * 8];
    p.pt().copy_in(&mut raw, ptr).ok_or(-EFAULT)?;
    let prog: Vec<Insn> = raw
        .chunks(8)
        .map(|c| Insn { code: u16::from_le_bytes([c[0], c[1]]), jt: c[2], jf: c[3], k: u32::from_le_bytes([c[4], c[5], c[6], c[7]]) })
        .collect();
    validate(&prog)?;
    Ok(prog)
}

/// 今のもの (なければ空) に足した新しい State
fn add(old: Option<&State>, strict: bool, prog: Option<Rc<Filter>>) -> Result<Seccomp, i64> {
    let (s0, mut f) = match old {
        Some(s) => (s.strict, s.filters.clone()),
        None => (false, Vec::new()),
    };
    if let Some(p) = prog {
        if f.iter().map(|x| x.prog.len()).sum::<usize>() + p.prog.len() > MAX_TOTAL {
            return Err(-ENOMEM);
        }
        f.push(p);
    }
    Ok(Rc::new(State { strict: s0 || strict, filters: f }))
}

/// 自分 (TSYNC ならスレッドグループのみな) にかける。listener なら、その fd (close-on-exec) を返す
fn install(strict: bool, prog: Option<Vec<Insn>>, flags: u64) -> Result<i64, i64> {
    let tsync = flags & FLAG_TSYNC != 0;
    let p = proc::current();
    // フィルタは no_new_privs か root が要る。strict は要らない (できることが減るだけなので。Linux と同じ)
    if prog.is_some() {
        may_set(&p.cred)?;
    }
    let listener = flags & FLAG_NEW_LISTENER != 0;
    let prog = prog.map(|prog| {
        let notif = listener.then(|| Notif { next_id: (crate::rand::u32() as u64) << 32 | crate::rand::u32() as u64, list: Vec::new(), closed: false });
        Rc::new(Filter { prog, notif: RefCell::new(notif), wait_killable: flags & FLAG_WAIT_KILLABLE_RECV != 0, restart_before_recv: flags & FLAG_RESTART_BEFORE_RECV != 0 })
    });
    // 上のフィルタにもう listener があれば EBUSY (Linux の has_duplicate_listener)
    if listener && state(&p.cred).is_some_and(|s| s.filters.iter().any(|x| x.notif.borrow().as_ref().is_some_and(|n| !n.closed))) {
        return Err(-EBUSY);
    }
    let new = add(state(&p.cred), strict, prog.clone())?;
    let mut fd = 0;
    if let Some(f) = prog.as_ref().filter(|_| listener) {
        let file = crate::file::new(crate::file::Kind::SeccompNotify(Rc::new(Listener(f.clone()))), crate::file::O_RDWR);
        fd = p.files().add(file, true, 0).ok_or(-24)? as i64;
    }
    let p = proc::current();
    p.cred.seccomp = Some(new as crate::security::Blob);
    if tsync {
        let s = p.cred.seccomp.clone();
        for t in p.siblings() {
            t.cred.seccomp = s.clone();
        }
    }
    Ok(fd)
}

/// seccomp(2)
pub fn sys_seccomp(op: u64, flags: u64, args: usize) -> Result<i64, i64> {
    match op {
        SET_MODE_STRICT => {
            if flags != 0 || args != 0 {
                return Err(crate::why::inval());
            }
            install(true, None, 0)
        }
        SET_MODE_FILTER => {
            const ALL: u64 = FLAG_TSYNC | FLAG_LOG | FLAG_SPEC_ALLOW | FLAG_NEW_LISTENER | FLAG_TSYNC_ESRCH | FLAG_WAIT_KILLABLE_RECV | FLAG_RESTART_BEFORE_RECV;
            if flags & !ALL != 0 {
                return Err(crate::why::inval());
            }
            // TSYNC がしくじったときの答え (スレッドの番号) と listener の fd が見わけられないので、TSYNC_ESRCH がいる
            if flags & FLAG_TSYNC != 0 && flags & FLAG_NEW_LISTENER != 0 && flags & FLAG_TSYNC_ESRCH == 0 {
                return Err(crate::why::inval());
            }
            // 待ちかたの印は listener があるときだけ
            if flags & (FLAG_WAIT_KILLABLE_RECV | FLAG_RESTART_BEFORE_RECV) != 0 && flags & FLAG_NEW_LISTENER == 0 {
                return Err(crate::why::inval());
            }
            let prog = read_prog(args)?;
            install(false, Some(prog), flags)
        }
        GET_ACTION_AVAIL => {
            let mut b = [0u8; 4];
            proc::current().pt().copy_in(&mut b, args).ok_or(-EFAULT)?;
            match u32::from_le_bytes(b) {
                RET_KILL_PROCESS | RET_KILL_THREAD | RET_TRAP | RET_ERRNO | RET_USER_NOTIF | RET_TRACE | RET_LOG | RET_ALLOW => Ok(0),
                _ => Err(-EOPNOTSUPP),
            }
        }
        // struct seccomp_notif_sizes: seccomp_notif、seccomp_notif_resp、seccomp_data の大きさ (u16 ずつ)
        GET_NOTIF_SIZES => {
            let mut b = [0u8; 6];
            b[0..2].copy_from_slice(&(NOTIF_SIZE as u16).to_le_bytes());
            b[2..4].copy_from_slice(&(RESP_SIZE as u16).to_le_bytes());
            b[4..6].copy_from_slice(&(DATA_LEN as u16).to_le_bytes());
            proc::current().pt().copy_out(args, &b).ok_or(-EFAULT)?;
            Ok(0)
        }
        _ => Err(crate::why::inval()),
    }
}

/// prctl(PR_SET_SECCOMP, mode, prog)
pub fn prctl_set(mode: u64, prog: usize) -> Result<i64, i64> {
    match mode {
        MODE_STRICT => install(true, None, 0),
        MODE_FILTER => install(false, Some(read_prog(prog)?), 0),
        _ => Err(crate::why::inval()),
    }
}

// ---- seccomp の入り口 (security.rs: Linux の include/linux/seccomp.h。LSM ではない) ----

/// listener の fd の中身 (security::Listener)
impl crate::security::Listener for Listener {
    fn readiness(&self) -> (bool, bool, bool) {
        readiness(self)
    }

    fn chan(&self) -> usize {
        listener_chan(self)
    }

    fn ioctl(&self, req: u64, arg: usize) -> Result<i64, i64> {
        ioctl(self, req, arg)
    }
}

/// seccomp (main.rs が register_seccomp でつなぐ)
pub struct Ops;
pub static OPS: Ops = Ops;

impl crate::security::Seccomp for Ops {
    /// フィルタの答え。user notification なら listener に知らせて答えを待つ (CONTINUE ならシステムコールをする)
    fn permit(&self, s: &crate::security::Blob, nr: u64, a: &[u64; 6], pc: u64) -> crate::security::Permit {
        use crate::security::Permit;
        match check(s.downcast_ref::<State>().expect("seccomp: state"), nr, a, pc) {
            Verdict::Allow => Permit::Allow,
            Verdict::Errno(e) => Permit::Errno(e),
            Verdict::Trap(errno) => Permit::Trap(errno),
            Verdict::Kill => Permit::Kill,
            Verdict::Notify(f, data) => match notify(&f, data) {
                Notified::Continue => Permit::Allow,
                Notified::Return(v) => Permit::Return(v),
                Notified::Interrupted { always } => Permit::Interrupted { always },
            },
        }
    }

    fn release(&self, c: &Cred) {
        released(c);
    }

    fn mode(&self, c: &Cred) -> i64 {
        mode(c).0 as i64
    }

    fn prctl_set(&self, mode: u64, prog: usize) -> Result<i64, i64> {
        prctl_set(mode, prog)
    }

    fn syscall(&self, op: u64, flags: u64, args: usize) -> Result<i64, i64> {
        sys_seccomp(op, flags, args)
    }
}

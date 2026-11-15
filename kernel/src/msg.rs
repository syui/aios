// System V のメッセージの列 (msgget、msgsnd、msgrcv、msgctl)。Linux の ipc/msg.c と同じ形と答え (CONFIG_SYSVIPC)。
// 列は IPC の namespace ごとの表 (shm.rs と同じ: CLONE_NEWIPC で番号も 0 から、namespace がなくなれば表ごと消える)。
// 送るほうは列がいっぱい (msg_qbytes をこえる) なら待ち (IPC_NOWAIT なら EAGAIN)、受けるほうは合うものがなければ待つ
// (IPC_NOWAIT なら ENOMSG)。待つのはどちらも列ごとのチャンネルで、列が変わればみな起こす (起きたらもう一度見る)。
// IPC_RMID は待っているものを EIDRM で起こす。/proc/sysvipc/msg は ipcs が読む
use crate::cell::Bkl;
use crate::proc;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::rc::Weak;
use alloc::vec::Vec;

const EPERM: i64 = 1;
const ENOENT: i64 = 2;
const EINTR: i64 = 4;
const E2BIG: i64 = 7;
const EAGAIN: i64 = 11;
const EFAULT: i64 = 14;
const EACCES: i64 = 13;
const EEXIST: i64 = 17;
const ENOSPC: i64 = 28;
const ENOMSG: i64 = 42;
const EIDRM: i64 = 43;

const IPC_PRIVATE: i32 = 0;
const IPC_CREAT: u64 = 0o1000;
const IPC_EXCL: u64 = 0o2000;
const IPC_NOWAIT: u64 = 0o4000;
const IPC_RMID: i64 = 0;
const IPC_SET: i64 = 1;
const IPC_STAT: i64 = 2;
const IPC_INFO: i64 = 3;
const MSG_STAT: i64 = 11;
const MSG_INFO: i64 = 12;
const MSG_STAT_ANY: i64 = 13;
const IPC_64: i64 = 0x100;
const MSG_NOERROR: u64 = 0o10000;
const MSG_EXCEPT: u64 = 0o20000;
const MSG_COPY: u64 = 0o40000;

const CAP_IPC_OWNER: u32 = 15;
const CAP_SYS_RESOURCE: u32 = 24;

/// Linux の既定 (msg_ctlmax、msg_ctlmnb、msg_ctlmni と MSGPOOL、MSGMAP、MSGTQL、MSGSSZ、MSGSEG)
const MSGMAX: usize = 8192;
const MSGMNB: usize = 16384;
const MSGMNI: usize = 32000;
const SEQ_SHIFT: u32 = 15;
const MIN_CYCLE: usize = 64;

struct Msg {
    mtype: i64,
    data: Vec<u8>,
}

struct Queue {
    key: i32,
    seq: u16,
    uid: u32,
    gid: u32,
    cuid: u32,
    cgid: u32,
    mode: u32,
    msgs: VecDeque<Msg>,
    cbytes: usize,
    qbytes: usize,
    stime: i64,
    rtime: i64,
    ctime: i64,
    /// さいごに送った / 受けたプロセス (本当の番号)
    lspid: u32,
    lrpid: u32,
    /// 待つチャンネル (列ごとにちがう番号: IPC_RMID のあとに同じ位置の新しい列ができても、古い列を待つものは起きて
    /// EIDRM を見る)
    chan: usize,
}

impl Queue {
    fn id(&self, idx: usize) -> i32 {
        ((self.seq as i32) << SEQ_SHIFT) + idx as i32
    }
}

struct Table {
    queues: BTreeMap<usize, Queue>,
    last_idx: i64,
    seq: u16,
    ns: Option<Weak<crate::ns::Ipc>>,
}

static TABLES: Bkl<BTreeMap<u64, Table>> = Bkl::new(BTreeMap::new());
/// 待つチャンネルの番号 (消した列のものは使いまわさない)
static NEXT_CHAN: Bkl<usize> = Bkl::new(1);

fn table() -> &'static mut Table {
    let all = TABLES.get();
    all.retain(|_, t| t.ns.as_ref().is_none_or(|w| w.strong_count() > 0));
    let (id, ns) = crate::ns::ipc();
    all.entry(id).or_insert_with(|| Table { queues: BTreeMap::new(), last_idx: -1, seq: 0, ns })
}

fn chan_of(c: usize) -> usize {
    &NEXT_CHAN as *const _ as usize + c * 8
}

fn find(t: &Table, id: i64) -> Result<usize, i64> {
    if id < 0 {
        return Err(crate::why::inval());
    }
    let idx = (id as usize) & ((1 << SEQ_SHIFT) - 1);
    match t.queues.get(&idx) {
        Some(q) if q.id(idx) as i64 == id => Ok(idx),
        _ => Err(crate::why::inval()),
    }
}

/// Linux の ipcperms (shm.rs と同じ)
fn perms(q: &Queue, flag: u32) -> bool {
    let c = crate::cred::current();
    let want = ((flag >> 6) | (flag >> 3) | flag) & 7;
    let mut have = q.mode;
    if c.euid == q.cuid || c.euid == q.uid {
        have >>= 6;
    } else if c.in_group(q.cgid) || c.in_group(q.gid) {
        have >>= 3;
    }
    want & !have & 7 == 0 || c.capable(CAP_IPC_OWNER)
}

fn owner(q: &Queue) -> bool {
    let c = crate::cred::current();
    c.euid == q.cuid || c.euid == q.uid || c.capable(crate::cred::CAP_SYS_ADMIN)
}

fn now() -> i64 {
    (crate::timer::stamp_ns() / 1_000_000_000) as i64
}

/// msgget(key, flags) (Linux の ipcget: shmget と同じ順)
fn msgget(key: i32, flags: u64) -> Result<i64, i64> {
    let t = table();
    if key != IPC_PRIVATE {
        if let Some((&idx, q)) = t.queues.iter().find(|(_, q)| q.key == key) {
            if flags & IPC_CREAT != 0 && flags & IPC_EXCL != 0 {
                return Err(-EEXIST);
            }
            if !perms(q, (flags & 0o777) as u32) {
                return Err(-EACCES);
            }
            return Ok(q.id(idx) as i64);
        }
        if flags & IPC_CREAT == 0 {
            return Err(-ENOENT);
        }
    }
    if t.queues.len() >= MSGMNI {
        return Err(-ENOSPC);
    }
    let span = (t.queues.len() * 3 / 2).max(MIN_CYCLE).min(MSGMNI);
    let start = ((t.last_idx + 1) as usize) % span;
    let idx = (0..span).map(|i| (start + i) % span).find(|i| !t.queues.contains_key(i)).ok_or(-ENOSPC)?;
    if idx as i64 <= t.last_idx {
        t.seq = if (t.seq as u32 + 1) >= (1 << (31 - SEQ_SHIFT)) { 0 } else { t.seq + 1 };
    }
    t.last_idx = idx as i64;
    let c = crate::cred::current();
    let n = NEXT_CHAN.get();
    *n += 1;
    t.queues.insert(idx, Queue {
        key,
        seq: t.seq,
        uid: c.euid,
        gid: c.egid,
        cuid: c.euid,
        cgid: c.egid,
        mode: (flags & 0o777) as u32,
        msgs: VecDeque::new(),
        cbytes: 0,
        qbytes: MSGMNB,
        stime: 0,
        rtime: 0,
        ctime: now(),
        lspid: 0,
        lrpid: 0,
        chan: *n,
    });
    Ok(t.queues[&idx].id(idx) as i64)
}

/// 待つ。起きたら、列がまだあるか (なければ EIDRM) を呼ぶほうが見る
fn wait(chan: usize) -> Result<(), i64> {
    proc::sleep(chan_of(chan)).map_err(|_| -EINTR)
}

/// msgsnd(id, msgp, size, flags) (Linux の do_msgsnd: 大きさが MSGMAX をこえるか負なら EINVAL、mtype が 1 より小さければ
/// EINVAL、許しがなければ EACCES、入らなければ待つ (IPC_NOWAIT なら EAGAIN)、待つあいだに消されれば EIDRM)
fn msgsnd(id: i64, msgp: usize, size: i64, flags: u64) -> Result<i64, i64> {
    let pt = proc::current().pt();
    let mut mt = [0u8; 8];
    pt.copy_in(&mut mt, msgp).ok_or(-EFAULT)?;
    let mtype = i64::from_le_bytes(mt);
    if size < 0 || size as usize > MSGMAX || id < 0 {
        return Err(crate::why::inval());
    }
    if mtype < 1 {
        return Err(crate::why::inval());
    }
    let mut data = alloc::vec![0u8; size as usize];
    pt.copy_in(&mut data, msgp + 8).ok_or(-EFAULT)?;
    let size = size as usize;
    let t = table();
    let mut idx = find(t, id)?;
    if !perms(&t.queues[&idx], 0o222) {
        return Err(-EACCES);
    }
    loop {
        let q = t.queues.get_mut(&idx).unwrap();
        if size + q.cbytes <= q.qbytes && 1 + q.msgs.len() <= q.qbytes {
            q.msgs.push_back(Msg { mtype, data });
            q.cbytes += size;
            q.stime = now();
            q.lspid = proc::current().tgid;
            proc::wakeup(chan_of(q.chan));
            return Ok(0);
        }
        if flags & IPC_NOWAIT != 0 {
            return Err(-EAGAIN);
        }
        let ch = q.chan;
        wait(ch)?;
        // 消されていないか (同じ番号の列が残っているか)
        idx = match find(table(), id) {
            Ok(i) if table().queues[&i].chan == ch => i,
            _ => return Err(-EIDRM),
        };
    }
}

/// 受けるものを選ぶ (Linux の find_msg): 0 ははじめのもの、正はその型 (MSG_EXCEPT ならその型でないもの)、負はその絶対値
/// 以下でいちばん小さい型のもの (同じ型ならはじめのもの)
fn pick(q: &Queue, mtype: i64, flags: u64) -> Option<usize> {
    if mtype == 0 {
        return (!q.msgs.is_empty()).then_some(0);
    }
    if mtype < 0 {
        let lim = mtype.unsigned_abs();
        let mut best: Option<(i64, usize)> = None;
        for (i, m) in q.msgs.iter().enumerate() {
            if (m.mtype as u64) <= lim && best.is_none_or(|(t, _)| m.mtype < t) {
                best = Some((m.mtype, i));
            }
        }
        return best.map(|(_, i)| i);
    }
    q.msgs.iter().position(|m| if flags & MSG_EXCEPT != 0 { m.mtype != mtype } else { m.mtype == mtype })
}

/// msgrcv(id, msgp, size, mtype, flags) (Linux の do_msgrcv: id が負か大きさが負なら EINVAL、MSG_COPY はない (手本の
/// 設定に CHECKPOINT_RESTORE がない: ENOSYS)、許しがなければ EACCES、合うものが大きさより大きければ E2BIG (MSG_NOERROR なら
/// 切って渡す)、なければ待つ (IPC_NOWAIT なら ENOMSG))。答えは写した本文の長さ
fn msgrcv(id: i64, msgp: usize, size: i64, mtype: i64, flags: u64) -> Result<i64, i64> {
    if id < 0 || size < 0 {
        return Err(crate::why::inval());
    }
    if flags & MSG_COPY != 0 {
        if flags & MSG_EXCEPT != 0 || flags & IPC_NOWAIT == 0 {
            return Err(crate::why::inval());
        }
        return Err(-38);
    }
    let t = table();
    let mut idx = find(t, id)?;
    if !perms(&t.queues[&idx], 0o444) {
        return Err(-EACCES);
    }
    loop {
        let q = t.queues.get_mut(&idx).unwrap();
        if let Some(i) = pick(q, mtype, flags) {
            if q.msgs[i].data.len() > size as usize && flags & MSG_NOERROR == 0 {
                return Err(-E2BIG);
            }
            let m = q.msgs.remove(i).unwrap();
            q.cbytes -= m.data.len();
            q.rtime = now();
            q.lrpid = proc::current().tgid;
            let n = m.data.len().min(size as usize);
            proc::wakeup(chan_of(q.chan));
            let pt = proc::current().pt();
            pt.copy_out(msgp, &m.mtype.to_le_bytes()).ok_or(-EFAULT)?;
            pt.copy_out(msgp + 8, &m.data[..n]).ok_or(-EFAULT)?;
            return Ok(n as i64);
        }
        if flags & IPC_NOWAIT != 0 {
            return Err(-ENOMSG);
        }
        let ch = q.chan;
        wait(ch)?;
        idx = match find(table(), id) {
            Ok(i) if table().queues[&i].chan == ch => i,
            _ => return Err(-EIDRM),
        };
    }
}

/// struct msqid64_ds (120 バイト: ipc64_perm 48、stime、rtime、ctime、cbytes、qnum、qbytes、lspid、lrpid)
fn stat_bytes(q: &Queue) -> [u8; 120] {
    let mut b = [0u8; 120];
    let mut put = |o: usize, v: &[u8]| b[o..o + v.len()].copy_from_slice(v);
    put(0, &q.key.to_le_bytes());
    put(4, &q.uid.to_le_bytes());
    put(8, &q.gid.to_le_bytes());
    put(12, &q.cuid.to_le_bytes());
    put(16, &q.cgid.to_le_bytes());
    put(20, &q.mode.to_le_bytes());
    put(24, &q.seq.to_le_bytes());
    put(48, &q.stime.to_le_bytes());
    put(56, &q.rtime.to_le_bytes());
    put(64, &q.ctime.to_le_bytes());
    put(72, &(q.cbytes as u64).to_le_bytes());
    put(80, &(q.msgs.len() as u64).to_le_bytes());
    put(88, &(q.qbytes as u64).to_le_bytes());
    put(96, &crate::ns::local_or_0(q.lspid).to_le_bytes());
    put(100, &crate::ns::local_or_0(q.lrpid).to_le_bytes());
    b
}

fn max_idx(t: &Table) -> i64 {
    t.queues.keys().next_back().map_or(0, |&i| i as i64)
}

/// msgctl(id, cmd, buf) (Linux の ksys_msgctl)
fn msgctl(id: i64, cmd: i64, buf: usize) -> Result<i64, i64> {
    let cmd = cmd & !IPC_64;
    if cmd < 0 || id < 0 {
        return Err(crate::why::inval());
    }
    let t = table();
    match cmd {
        IPC_INFO | MSG_INFO => {
            // struct msginfo: msgpool、msgmap、msgmax、msgmnb、msgmni、msgssz、msgtql、msgseg (MSG_INFO は使っている列の数、
            // メッセージの数、バイト数)
            let (pool, map, tql) = if cmd == MSG_INFO {
                (t.queues.len() as i32, t.queues.values().map(|q| q.msgs.len()).sum::<usize>() as i32, t.queues.values().map(|q| q.cbytes).sum::<usize>() as i32)
            } else {
                ((MSGMNI * MSGMNB / 1024) as i32, MSGMNB as i32, MSGMNB as i32)
            };
            let mut b = [0u8; 32];
            for (i, v) in [pool, map, MSGMAX as i32, MSGMNB as i32, MSGMNI as i32, 16, tql, 0xffff].iter().enumerate() {
                b[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
            }
            proc::put_user(buf, &b)?;
            Ok(max_idx(t))
        }
        MSG_STAT | MSG_STAT_ANY => {
            let q = t.queues.get(&(id as usize)).ok_or_else(crate::why::inval)?;
            if cmd == MSG_STAT && !perms(q, 0o444) {
                return Err(-EACCES);
            }
            proc::put_user(buf, &stat_bytes(q))?;
            Ok(q.id(id as usize) as i64)
        }
        IPC_STAT => {
            let idx = find(t, id)?;
            let q = &t.queues[&idx];
            if !perms(q, 0o444) {
                return Err(-EACCES);
            }
            proc::put_user(buf, &stat_bytes(q))?;
            Ok(0)
        }
        IPC_SET => {
            let mut b = [0u8; 120];
            proc::current().pt().copy_in(&mut b, buf).ok_or(-EFAULT)?;
            let idx = find(t, id)?;
            let q = t.queues.get_mut(&idx).unwrap();
            if !owner(q) {
                return Err(-EPERM);
            }
            let w = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
            let qbytes = u64::from_le_bytes(b[88..96].try_into().unwrap()) as usize;
            if qbytes > MSGMNB && !crate::cred::current().capable(CAP_SYS_RESOURCE) {
                return Err(-EPERM);
            }
            if w(4) == u32::MAX || w(8) == u32::MAX {
                return Err(crate::why::inval());
            }
            q.qbytes = qbytes;
            q.uid = w(4);
            q.gid = w(8);
            q.mode = (q.mode & !0o777) | (w(20) & 0o777);
            q.ctime = now();
            // 入るようになったかもしれない
            proc::wakeup(chan_of(q.chan));
            Ok(0)
        }
        IPC_RMID => {
            let idx = find(t, id)?;
            if !owner(&t.queues[&idx]) {
                return Err(-EPERM);
            }
            let q = t.queues.remove(&idx).unwrap();
            proc::wakeup(chan_of(q.chan));
            Ok(0)
        }
        _ => Err(crate::why::inval()),
    }
}

/// /proc/sysvipc/msg (Linux の sysvipc_msg_proc_show の形)
pub fn proc_text() -> alloc::string::String {
    use core::fmt::Write;
    let t = table();
    let mut s = alloc::string::String::from("       key      msqid perms      cbytes       qnum lspid lrpid   uid   gid  cuid  cgid      stime      rtime      ctime\n");
    for (&idx, q) in &t.queues {
        let _ = writeln!(
            s,
            "{:>10} {:>10}  {:>4o}  {:>10} {:>10} {:>5} {:>5} {:>5} {:>5} {:>5} {:>5} {:>10} {:>10} {:>10}",
            q.key,
            q.id(idx),
            q.mode,
            q.cbytes,
            q.msgs.len(),
            crate::ns::local_or_0(q.lspid),
            crate::ns::local_or_0(q.lrpid),
            q.uid,
            q.gid,
            q.cuid,
            q.cgid,
            q.stime,
            q.rtime,
            q.ctime
        );
    }
    s
}

/// システムコール (main.rs が syscall::register で置く。Linux の ipc/msg.c、CONFIG_SYSVIPC)
pub const SYSCALLS: &[u64] = {
    use crate::syscall::nr::*;
    &[MSGGET, MSGCTL, MSGRCV, MSGSND]
};

pub fn sys(n: u64, a: &[u64; 6]) -> Result<i64, i64> {
    use crate::syscall::nr::*;
    match n {
        MSGGET => msgget(a[0] as i32, a[1] as u32 as u64),
        MSGCTL => msgctl(a[0] as i32 as i64, a[1] as i32 as i64, a[2] as usize),
        MSGSND => msgsnd(a[0] as i32 as i64, a[1] as usize, a[2] as i64, a[3] as u32 as u64),
        MSGRCV => msgrcv(a[0] as i32 as i64, a[1] as usize, a[2] as i64, a[3] as i64, a[4] as u32 as u64),
        _ => Err(-38),
    }
}

// System V のセマフォ (semget、semctl、semop、semtimedop)。Linux の ipc/sem.c と同じ形と答え (CONFIG_SYSVIPC)。
// 組 (set) は IPC の namespace ごとの表 (shm.rs、msg.rs と同じ)。semop はみなできるときだけみなする (できなければ何も
// せずに待つ: IPC_NOWAIT なら EAGAIN、semtimedop の時間切れも EAGAIN)。待つのは組ごとのチャンネルで、値が変われば
// みな起こす (起きたらもう一度ためす)。GETNCNT と GETZCNT は、待っているものを止めた操作 (Linux の q->blocking) で数える。
// SEM_UNDO の戻し (semadj) はプロセス (スレッドグループ) ごとに持ち、終わるとき (proc の exit のフック) に足す。SETVAL と
// SETALL はその値の戻しを消す。fork した子は持たない。/proc/sysvipc/sem は ipcs が読む
use crate::cell::Bkl;
use crate::proc;
use alloc::collections::BTreeMap;
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
const EFBIG: i64 = 27;
const ENOSPC: i64 = 28;
const ERANGE: i64 = 34;
const EIDRM: i64 = 43;

const IPC_PRIVATE: i32 = 0;
const IPC_CREAT: u64 = 0o1000;
const IPC_EXCL: u64 = 0o2000;
const IPC_NOWAIT: i16 = 0o4000;
const SEM_UNDO: i16 = 0x1000;
const IPC_RMID: i64 = 0;
const IPC_SET: i64 = 1;
const IPC_STAT: i64 = 2;
const IPC_INFO: i64 = 3;
const GETPID: i64 = 11;
const GETVAL: i64 = 12;
const GETALL: i64 = 13;
const GETNCNT: i64 = 14;
const GETZCNT: i64 = 15;
const SETVAL: i64 = 16;
const SETALL: i64 = 17;
const SEM_STAT: i64 = 18;
const SEM_INFO: i64 = 19;
const SEM_STAT_ANY: i64 = 20;
const IPC_64: i64 = 0x100;

const CAP_IPC_OWNER: u32 = 15;

/// Linux の既定 (sem_ctls: SEMMSL、SEMMNS、SEMOPM、SEMMNI と SEMVMX、SEMAEM、SEMUME、SEMMNU、SEMMAP、SEMUSZ)
const SEMMSL: i64 = 32000;
const SEMMNI: usize = 32000;
const SEMMNS: i64 = SEMMNI as i64 * SEMMSL;
const SEMOPM: usize = 500;
const SEMVMX: i32 = 32767;
const SEMAEM: i32 = SEMVMX;
const SEQ_SHIFT: u32 = 15;
const MIN_CYCLE: usize = 64;

#[derive(Clone, Copy, Default)]
struct Sem {
    val: i32,
    /// さいごに変えたプロセス (本当の番号: GETPID)
    pid: u32,
}

struct Set {
    key: i32,
    seq: u16,
    uid: u32,
    gid: u32,
    cuid: u32,
    cgid: u32,
    mode: u32,
    sems: Vec<Sem>,
    otime: i64,
    ctime: i64,
    /// 待つチャンネル (組ごとにちがう番号)
    chan: usize,
    /// 待っているもの: (止めた操作のセマフォ, 0 を待つか)
    waiters: Vec<(u16, bool)>,
}

impl Set {
    fn id(&self, idx: usize) -> i32 {
        ((self.seq as i32) << SEQ_SHIFT) + idx as i32
    }
}

struct Table {
    sets: BTreeMap<usize, Set>,
    last_idx: i64,
    seq: u16,
    ns: Option<Weak<crate::ns::Ipc>>,
}

static TABLES: Bkl<BTreeMap<u64, Table>> = Bkl::new(BTreeMap::new());
static NEXT_CHAN: Bkl<usize> = Bkl::new(1);
/// SEM_UNDO の戻し: (IPC の namespace, スレッドグループ) ごとに、組のチャンネルとセマフォごとの足す値
static UNDO: Bkl<BTreeMap<(u64, u32), Vec<(usize, Vec<i32>)>>> = Bkl::new(BTreeMap::new());

fn table() -> &'static mut Table {
    let all = TABLES.get();
    all.retain(|_, t| t.ns.as_ref().is_none_or(|w| w.strong_count() > 0));
    let (id, ns) = crate::ns::ipc();
    all.entry(id).or_insert_with(|| Table { sets: BTreeMap::new(), last_idx: -1, seq: 0, ns })
}

fn chan_of(c: usize) -> usize {
    &NEXT_CHAN as *const _ as usize + c * 8
}

fn find(t: &Table, id: i64) -> Result<usize, i64> {
    if id < 0 {
        return Err(crate::why::inval());
    }
    let idx = (id as usize) & ((1 << SEQ_SHIFT) - 1);
    match t.sets.get(&idx) {
        Some(s) if s.id(idx) as i64 == id => Ok(idx),
        _ => Err(crate::why::inval()),
    }
}

fn perms(s: &Set, flag: u32) -> bool {
    let c = crate::cred::current();
    let want = ((flag >> 6) | (flag >> 3) | flag) & 7;
    let mut have = s.mode;
    if c.euid == s.cuid || c.euid == s.uid {
        have >>= 6;
    } else if c.in_group(s.cgid) || c.in_group(s.gid) {
        have >>= 3;
    }
    want & !have & 7 == 0 || c.capable(CAP_IPC_OWNER)
}

fn owner(s: &Set) -> bool {
    let c = crate::cred::current();
    c.euid == s.cuid || c.euid == s.uid || c.capable(crate::cred::CAP_SYS_ADMIN)
}

fn now() -> i64 {
    (crate::timer::stamp_ns() / 1_000_000_000) as i64
}

/// その組のセマフォ num の戻しを、みなのプロセスから消す (num が None なら組のものみな: SETALL と IPC_RMID)
fn clear_undo(chan: usize, num: Option<usize>) {
    for list in UNDO.get().values_mut() {
        for (c, adj) in list.iter_mut() {
            if *c == chan {
                match num {
                    Some(n) => adj[n] = 0,
                    None => adj.iter_mut().for_each(|a| *a = 0),
                }
            }
        }
        if num.is_none() {
            list.retain(|(c, _)| *c != chan);
        }
    }
}

/// semget(key, nsems, flags) (Linux の ksys_semget: nsems が 0..SEMMSL の外なら EINVAL。あるものは IPC_CREAT | IPC_EXCL で
/// EEXIST、nsems があるものより多ければ EINVAL、それから許し。作るのは nsems が 0 なら EINVAL)
fn semget(key: i32, nsems: i64, flags: u64) -> Result<i64, i64> {
    if !(0..=SEMMSL).contains(&nsems) {
        return Err(crate::why::inval());
    }
    let t = table();
    if key != IPC_PRIVATE {
        if let Some((&idx, s)) = t.sets.iter().find(|(_, s)| s.key == key) {
            if flags & IPC_CREAT != 0 && flags & IPC_EXCL != 0 {
                return Err(-EEXIST);
            }
            if nsems as usize > s.sems.len() {
                return Err(crate::why::inval());
            }
            if !perms(s, (flags & 0o777) as u32) {
                return Err(-EACCES);
            }
            return Ok(s.id(idx) as i64);
        }
        if flags & IPC_CREAT == 0 {
            return Err(-ENOENT);
        }
    }
    if nsems == 0 {
        return Err(crate::why::inval());
    }
    let total: i64 = t.sets.values().map(|s| s.sems.len() as i64).sum();
    if total + nsems > SEMMNS || t.sets.len() >= SEMMNI {
        return Err(-ENOSPC);
    }
    let span = (t.sets.len() * 3 / 2).max(MIN_CYCLE).min(SEMMNI);
    let start = ((t.last_idx + 1) as usize) % span;
    let idx = (0..span).map(|i| (start + i) % span).find(|i| !t.sets.contains_key(i)).ok_or(-ENOSPC)?;
    if idx as i64 <= t.last_idx {
        t.seq = if (t.seq as u32 + 1) >= (1 << (31 - SEQ_SHIFT)) { 0 } else { t.seq + 1 };
    }
    t.last_idx = idx as i64;
    let c = crate::cred::current();
    let n = NEXT_CHAN.get();
    *n += 1;
    t.sets.insert(idx, Set {
        key,
        seq: t.seq,
        uid: c.euid,
        gid: c.egid,
        cuid: c.euid,
        cgid: c.egid,
        mode: (flags & 0o777) as u32,
        sems: alloc::vec![Sem::default(); nsems as usize],
        otime: 0,
        ctime: now(),
        chan: *n,
        waiters: Vec::new(),
    });
    Ok(t.sets[&idx].id(idx) as i64)
}

/// struct semid64_ds (88 バイト: ipc64_perm 48、otime、ctime、nsems)
fn stat_bytes(s: &Set) -> [u8; 88] {
    let mut b = [0u8; 88];
    let mut put = |o: usize, v: &[u8]| b[o..o + v.len()].copy_from_slice(v);
    put(0, &s.key.to_le_bytes());
    put(4, &s.uid.to_le_bytes());
    put(8, &s.gid.to_le_bytes());
    put(12, &s.cuid.to_le_bytes());
    put(16, &s.cgid.to_le_bytes());
    put(20, &s.mode.to_le_bytes());
    put(24, &s.seq.to_le_bytes());
    put(48, &s.otime.to_le_bytes());
    put(56, &s.ctime.to_le_bytes());
    put(64, &(s.sems.len() as u64).to_le_bytes());
    b
}

fn max_idx(t: &Table) -> i64 {
    t.sets.keys().next_back().map_or(0, |&i| i as i64)
}

/// semctl(id, num, cmd, arg) (Linux の ksys_semctl)
fn semctl(id: i64, num: i64, cmd: i64, arg: u64) -> Result<i64, i64> {
    let cmd = cmd & !IPC_64;
    if id < 0 {
        return Err(crate::why::inval());
    }
    let t = table();
    let me = proc::current().tgid;
    let buf = arg as usize;
    match cmd {
        IPC_INFO | SEM_INFO => {
            // struct seminfo: semmap、semmni、semmns、semmnu、semmsl、semopm、semume、semusz、semvmx、semaem (SEM_INFO は
            // semusz が使っている組の数、semaem がセマフォの数)
            let (usz, aem) = if cmd == SEM_INFO { (t.sets.len() as i32, t.sets.values().map(|s| s.sems.len()).sum::<usize>() as i32) } else { (20, SEMAEM) };
            let mut b = [0u8; 40];
            for (i, v) in [SEMMNS as i32, SEMMNI as i32, SEMMNS as i32, SEMMNS as i32, SEMMSL as i32, SEMOPM as i32, SEMOPM as i32, usz, SEMVMX, aem].iter().enumerate() {
                b[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
            }
            proc::put_user(buf, &b)?;
            Ok(max_idx(t))
        }
        SEM_STAT | SEM_STAT_ANY => {
            let s = t.sets.get(&(id as usize)).ok_or_else(crate::why::inval)?;
            if cmd == SEM_STAT && !perms(s, 0o444) {
                return Err(-EACCES);
            }
            proc::put_user(buf, &stat_bytes(s))?;
            Ok(s.id(id as usize) as i64)
        }
        IPC_STAT => {
            let idx = find(t, id)?;
            let s = &t.sets[&idx];
            if !perms(s, 0o444) {
                return Err(-EACCES);
            }
            proc::put_user(buf, &stat_bytes(s))?;
            Ok(0)
        }
        SETVAL => {
            let v = arg as i32;
            if !(0..=SEMVMX).contains(&v) {
                return Err(-ERANGE);
            }
            let idx = find(t, id)?;
            let s = t.sets.get_mut(&idx).unwrap();
            if num < 0 || num as usize >= s.sems.len() {
                return Err(crate::why::inval());
            }
            if !perms(s, 0o222) {
                return Err(-EACCES);
            }
            clear_undo(s.chan, Some(num as usize));
            s.sems[num as usize] = Sem { val: v, pid: me };
            s.ctime = now();
            proc::wakeup(chan_of(s.chan));
            Ok(0)
        }
        GETALL | SETALL | GETVAL | GETPID | GETNCNT | GETZCNT => {
            let idx = find(t, id)?;
            let s = t.sets.get_mut(&idx).unwrap();
            let n = s.sems.len();
            if !perms(s, if cmd == SETALL { 0o222 } else { 0o444 }) {
                return Err(-EACCES);
            }
            match cmd {
                GETALL => {
                    let b: Vec<u8> = s.sems.iter().flat_map(|x| (x.val as u16).to_le_bytes()).collect();
                    proc::put_user(buf, &b)?;
                    return Ok(0);
                }
                SETALL => {
                    let mut b = alloc::vec![0u8; n * 2];
                    proc::current().pt().copy_in(&mut b, buf).ok_or(-EFAULT)?;
                    let vals: Vec<i32> = b.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]]) as i32).collect();
                    if vals.iter().any(|&v| v > SEMVMX) {
                        return Err(-ERANGE);
                    }
                    clear_undo(s.chan, None);
                    for (x, v) in s.sems.iter_mut().zip(vals) {
                        *x = Sem { val: v, pid: me };
                    }
                    s.ctime = now();
                    proc::wakeup(chan_of(s.chan));
                    return Ok(0);
                }
                _ => {}
            }
            if num < 0 || num as usize >= n {
                return Err(crate::why::inval());
            }
            let k = num as usize;
            Ok(match cmd {
                GETVAL => s.sems[k].val as i64,
                GETPID => crate::ns::local_or_0(s.sems[k].pid) as i64,
                GETNCNT => s.waiters.iter().filter(|&&(w, z)| w as usize == k && !z).count() as i64,
                _ => s.waiters.iter().filter(|&&(w, z)| w as usize == k && z).count() as i64,
            })
        }
        IPC_SET => {
            let mut b = [0u8; 88];
            proc::current().pt().copy_in(&mut b, buf).ok_or(-EFAULT)?;
            let idx = find(t, id)?;
            let s = t.sets.get_mut(&idx).unwrap();
            if !owner(s) {
                return Err(-EPERM);
            }
            let w = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
            if w(4) == u32::MAX || w(8) == u32::MAX {
                return Err(crate::why::inval());
            }
            s.uid = w(4);
            s.gid = w(8);
            s.mode = (s.mode & !0o777) | (w(20) & 0o777);
            s.ctime = now();
            Ok(0)
        }
        IPC_RMID => {
            let idx = find(t, id)?;
            if !owner(&t.sets[&idx]) {
                return Err(-EPERM);
            }
            let s = t.sets.remove(&idx).unwrap();
            clear_undo(s.chan, None);
            proc::wakeup(chan_of(s.chan));
            Ok(0)
        }
        _ => Err(crate::why::inval()),
    }
}

/// 操作をみなためす (Linux の perform_atomic_semop): できればして Ok(None)、待つなら Ok(Some(止めた操作))、IPC_NOWAIT
/// なら EAGAIN、値が SEMVMX をこえれば ERANGE、戻しが SEMAEM をこえれば ERANGE
fn try_ops(s: &mut Set, ops: &[(u16, i16, i16)], undo: &mut Option<&mut Vec<i32>>) -> Result<Option<(u16, bool)>, i64> {
    let mut vals: Vec<i32> = s.sems.iter().map(|x| x.val).collect();
    for &(num, op, flg) in ops {
        let cur = vals[num as usize];
        if (op == 0 && cur != 0) || cur + (op as i32) < 0 {
            if flg & IPC_NOWAIT != 0 {
                return Err(-EAGAIN);
            }
            return Ok(Some((num, op == 0)));
        }
        let r = cur + op as i32;
        if r > SEMVMX {
            return Err(-ERANGE);
        }
        vals[num as usize] = r;
    }
    // 戻しの範囲
    if let Some(adj) = undo.as_deref_mut() {
        let mut a = adj.clone();
        for &(num, op, flg) in ops {
            if flg & SEM_UNDO != 0 {
                let v = a[num as usize] - op as i32;
                if !(-SEMAEM - 1..=SEMAEM).contains(&v) {
                    return Err(-ERANGE);
                }
                a[num as usize] = v;
            }
        }
        *adj = a;
    }
    let me = proc::current().tgid;
    for &(num, _, _) in ops {
        s.sems[num as usize] = Sem { val: vals[num as usize], pid: me };
    }
    s.otime = now();
    Ok(None)
}

/// semop と semtimedop (Linux の do_semtimedop: nsops が 1 より小さいか id が負なら EINVAL、SEMOPM をこえれば E2BIG、
/// 時間がおかしければ EINVAL、セマフォの番号が組の外なら EFBIG、許しがなければ EACCES)
fn semtimedop(id: i64, sops: usize, nsops: i64, timeout: usize) -> Result<i64, i64> {
    if nsops < 1 || id < 0 {
        return Err(crate::why::inval());
    }
    if nsops as usize > SEMOPM {
        return Err(-E2BIG);
    }
    let pt = proc::current().pt();
    let mut b = alloc::vec![0u8; nsops as usize * 6];
    pt.copy_in(&mut b, sops).ok_or(-EFAULT)?;
    let ops: Vec<(u16, i16, i16)> =
        b.chunks(6).map(|c| (u16::from_le_bytes([c[0], c[1]]), i16::from_le_bytes([c[2], c[3]]), i16::from_le_bytes([c[4], c[5]]))).collect();
    let deadline = if timeout != 0 {
        let mut tb = [0u8; 16];
        pt.copy_in(&mut tb, timeout).ok_or(-EFAULT)?;
        let (sec, ns) = (i64::from_le_bytes(tb[..8].try_into().unwrap()), i64::from_le_bytes(tb[8..].try_into().unwrap()));
        if sec < 0 || !(0..1_000_000_000).contains(&ns) {
            return Err(crate::why::inval());
        }
        let tick = 1_000_000_000 / crate::timer::HZ;
        Some(crate::timer::ticks() + ((sec as u64 * 1_000_000_000 + ns as u64).div_ceil(tick)).max(1))
    } else {
        None
    };
    let max = ops.iter().map(|o| o.0).max().unwrap_or(0);
    let undos = ops.iter().any(|o| o.2 & SEM_UNDO != 0);
    let alter = ops.iter().any(|o| o.1 != 0);
    let t = table();
    let mut idx = find(t, id)?;
    let s = &t.sets[&idx];
    if max as usize >= s.sems.len() {
        return Err(-EFBIG);
    }
    if !perms(s, if alter { 0o222 } else { 0o444 }) {
        return Err(-EACCES);
    }
    let ch = s.chan;
    let n = s.sems.len();
    let key = (crate::ns::ipc().0, proc::current().tgid);
    loop {
        let s = table().sets.get_mut(&idx).unwrap();
        let mut adj = if undos {
            let list = UNDO.get().entry(key).or_default();
            if !list.iter().any(|(c, _)| *c == ch) {
                list.push((ch, alloc::vec![0; n]));
            }
            list.iter_mut().find(|(c, _)| *c == ch).map(|(_, a)| a)
        } else {
            None
        };
        match try_ops(s, &ops, &mut adj)? {
            None => {
                proc::wakeup(chan_of(ch));
                return Ok(0);
            }
            Some(block) => {
                s.waiters.push(block);
                // sleep_until は起こされたら Ok(true)、時間切れは Ok(false)
                let r = match deadline {
                    Some(d) => proc::sleep_until(chan_of(ch), d),
                    None => proc::sleep(chan_of(ch)).map(|_| true),
                };
                // 待っていた印を外す (組がまだあれば)
                if let Some(s) = table().sets.get_mut(&idx).filter(|s| s.chan == ch) {
                    if let Some(i) = s.waiters.iter().position(|w| *w == block) {
                        s.waiters.remove(i);
                    }
                }
                match r {
                    Err(_) => return Err(-EINTR),
                    Ok(false) => return Err(-EAGAIN),
                    Ok(true) => {}
                }
                idx = match find(table(), id) {
                    Ok(i) if table().sets[&i].chan == ch => i,
                    _ => return Err(-EIDRM),
                };
            }
        }
    }
}

/// プロセス (スレッドグループ) が終わる: SEM_UNDO の戻しを足す (Linux の exit_sem: 0 より小さくなるなら 0、SEMVMX で止める)
pub fn exit(tgid: u32) {
    let keys: Vec<(u64, u32)> = UNDO.get().keys().filter(|k| k.1 == tgid).copied().collect();
    for k in keys {
        let Some(list) = UNDO.get().remove(&k) else { continue };
        let Some(t) = TABLES.get().get_mut(&k.0) else { continue };
        for (ch, adj) in list {
            if let Some(s) = t.sets.values_mut().find(|s| s.chan == ch) {
                for (x, a) in s.sems.iter_mut().zip(adj.iter()) {
                    if *a != 0 {
                        x.val = (x.val + a).clamp(0, SEMVMX);
                        x.pid = tgid;
                    }
                }
                s.otime = now();
                proc::wakeup(chan_of(ch));
            }
        }
    }
}

/// /proc/sysvipc/sem (Linux の sysvipc_sem_proc_show の形)
pub fn proc_text() -> alloc::string::String {
    use core::fmt::Write;
    let t = table();
    let mut s = alloc::string::String::from("       key      semid perms      nsems   uid   gid  cuid  cgid      otime      ctime\n");
    for (&idx, g) in &t.sets {
        let _ = writeln!(s, "{:>10} {:>10}  {:>4o} {:>10} {:>5} {:>5} {:>5} {:>5} {:>10} {:>10}", g.key, g.id(idx), g.mode, g.sems.len(), g.uid, g.gid, g.cuid, g.cgid, g.otime, g.ctime);
    }
    s
}

/// システムコール (main.rs が syscall::register で置く。Linux の ipc/sem.c、CONFIG_SYSVIPC)
pub const SYSCALLS: &[u64] = {
    use crate::syscall::nr::*;
    &[SEMGET, SEMCTL, SEMTIMEDOP, SEMOP]
};

pub fn sys(n: u64, a: &[u64; 6]) -> Result<i64, i64> {
    use crate::syscall::nr::*;
    match n {
        SEMGET => semget(a[0] as i32, a[1] as i32 as i64, a[2] as u32 as u64),
        SEMCTL => semctl(a[0] as i32 as i64, a[1] as i32 as i64, a[2] as i32 as i64, a[3]),
        SEMOP => semtimedop(a[0] as i32 as i64, a[1] as usize, a[2] as i64, 0),
        SEMTIMEDOP => semtimedop(a[0] as i32 as i64, a[1] as usize, a[2] as i64, a[3] as usize),
        _ => Err(-38),
    }
}

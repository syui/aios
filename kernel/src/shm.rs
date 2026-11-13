// System V の共有メモリ (shmget、shmat、shmdt、shmctl)。Linux の ipc/shm.c と同じ形と答え (CONFIG_SYSVIPC)。
// かたまり (segment) の中身は、名前のない tmpfs のファイル (memfd と同じ: tmpfs::anon_file)。shmat はそれを MAP_SHARED で
// 写す (/proc/PID/maps では /SYSV%08x (deleted))。つないでいる数 (shm_nattch) は、そのファイルを写した領域 (vm::Vma) の
// 数を、みなのアドレス空間で数える: fork で増え、shmdt・munmap・exec・終わりで減り、munmap で領域が分かれれば増える
// (Linux の shm_open / shm_close と同じ)。ちがい: Linux は fork と終わりでも shm_lpid と shm_dtime を変えるが、aios は
// shmat と shmdt でだけ変える。IPC_RMID のときにつないでいるものがあれば、SHM_DEST の印をつけて鍵を IPC_PRIVATE に
// (もう shmget で見つからない)、表は弱い参照だけを持ち、さいごの写しが外れたときに中身が消える (表の行は次に表を見る
// ときに消す)。
// PostgreSQL は、いつも小さなかたまりをとる (データのディレクトリの印: 前の postmaster がまだつないでいるかを shm_nattch で
// 見る)。表は IPC の namespace ごと (CLONE_NEWIPC: 番号も 0 から。namespace がなくなれば、その表のかたまりも消える:
// つないでいる写しは外れるまで残る。Linux の shm_exit_ns と同じ)
use crate::cell::Bkl;
use crate::proc;
use crate::vfs::{Inode, InodeRef};
use alloc::collections::BTreeMap;
use alloc::rc::{Rc, Weak};

const EPERM: i64 = 1;
const ENOENT: i64 = 2;
const EFAULT: i64 = 14;
const EACCES: i64 = 13;
const EEXIST: i64 = 17;
const ENOMEM: i64 = 12;
const EIDRM: i64 = 43;
const ENOSPC: i64 = 28;

const IPC_PRIVATE: i32 = 0;
const IPC_CREAT: u64 = 0o1000;
const IPC_EXCL: u64 = 0o2000;
const IPC_RMID: i64 = 0;
const IPC_SET: i64 = 1;
const IPC_STAT: i64 = 2;
const IPC_INFO: i64 = 3;
const SHM_LOCK: i64 = 11;
const SHM_UNLOCK: i64 = 12;
const SHM_STAT: i64 = 13;
const SHM_INFO: i64 = 14;
const SHM_STAT_ANY: i64 = 15;
/// 古い形と分ける印 (musl や glibc が足すことがある。aarch64 はいつも 64 の形)
const IPC_64: i64 = 0x100;

const SHM_RDONLY: u64 = 0o10000;
const SHM_RND: u64 = 0o20000;
const SHM_REMAP: u64 = 0o40000;
const SHM_EXEC: u64 = 0o100000;
/// shm_perm.mode の印: IPC_RMID したが、まだつないでいるものがある / SHM_LOCK した
const SHM_DEST: u32 = 0o1000;
const SHM_LOCKED: u32 = 0o2000;

const CAP_IPC_LOCK: u32 = 14;
const CAP_IPC_OWNER: u32 = 15;

/// Linux の既定 (ipc_namespace の shm_ctlmax、shm_ctlall、shm_ctlmni と SHMMIN)
const SHMMAX: u64 = u64::MAX - (1 << 24);
const SHMALL: u64 = u64::MAX - (1 << 24);
const SHMMNI: usize = 4096;
const SHMMIN: usize = 1;
/// arm64 の SHMLBA (asm-generic: ページの大きさ)
const SHMLBA: usize = 4096;
const PGSIZE: usize = 4096;
/// 番号 = seq << 15 | 表の位置 (Linux の IPCMNI_SHIFT)。位置は使ったものの次から回して配る (ipc_idr_alloc の
/// idr_alloc_cyclic。回る幅は max(使っている数 * 3 / 2, 64))
const SEQ_SHIFT: u32 = 15;
const MIN_CYCLE: usize = 64;

enum Hold {
    Live(InodeRef),
    /// IPC_RMID したあと: 写しだけが中身を持つ
    Dest(Weak<dyn Inode>),
}

struct Seg {
    key: i32,
    seq: u16,
    size: usize,
    ino: Hold,
    uid: u32,
    gid: u32,
    cuid: u32,
    cgid: u32,
    mode: u32,
    atime: i64,
    dtime: i64,
    ctime: i64,
    /// 作った / さいごに shmat・shmdt したプロセス (本当の番号)
    cpid: u32,
    lpid: u32,
}

impl Seg {
    fn inode(&self) -> Option<InodeRef> {
        match &self.ino {
            Hold::Live(i) => Some(i.clone()),
            Hold::Dest(w) => w.upgrade(),
        }
    }

    /// つないでいる写しの数
    fn nattch(&self) -> usize {
        self.inode().map_or(0, |i| attaches(&i))
    }

    fn id(&self, idx: usize) -> i32 {
        ((self.seq as i32) << SEQ_SHIFT) + idx as i32
    }
}

struct Table {
    segs: BTreeMap<usize, Seg>,
    last_idx: i64,
    seq: u16,
    /// この表の IPC の namespace (はじめからあるものは None)。なくなったら表ごと捨てる
    ns: Option<Weak<crate::ns::Ipc>>,
}

/// IPC の namespace の番号ごとの表
static TABLES: Bkl<BTreeMap<u64, Table>> = Bkl::new(BTreeMap::new());

/// いまのプロセスの IPC の namespace の表
fn table() -> &'static mut Table {
    let all = TABLES.get();
    // なくなった namespace の表 (かたまりの中身は、つないでいる写しが外れたときに消える)
    all.retain(|_, t| t.ns.as_ref().is_none_or(|w| w.strong_count() > 0));
    let (id, ns) = crate::ns::ipc();
    let t = all.entry(id).or_insert_with(|| Table { segs: BTreeMap::new(), last_idx: -1, seq: 0, ns });
    // IPC_RMID したもので、もうだれもつないでいないもの
    t.segs.retain(|_, s| !matches!(&s.ino, Hold::Dest(_)) || s.nattch() > 0);
    t
}

/// ino を写した領域の数 (アドレス空間ごとに 1 度: スレッドは同じ空間を持つ)。ファイルの参照の数ではない (触れた
/// ページもファイルを持つ)
fn attaches(ino: &InodeRef) -> usize {
    let mut seen: alloc::vec::Vec<*const crate::vm::PageTable> = alloc::vec::Vec::new();
    let mut n = 0;
    for p in proc::live() {
        if p.mm.is_none() {
            continue;
        }
        let pt = &p.mm().pt as *const crate::vm::PageTable;
        if seen.contains(&pt) {
            continue;
        }
        seen.push(pt);
        // SAFETY: 大きなロックの中で、生きているプロセスのアドレス空間を読むだけ (/proc/PID/maps と同じ)
        n += unsafe { &*pt }.vmas_iter().filter(|(_, v)| matches!(&v.back, crate::vm::Backing::File { ino: i, .. } if same(i, ino))).count();
    }
    n
}

fn now() -> i64 {
    (crate::timer::stamp_ns() / 1_000_000_000) as i64
}

fn same(a: &InodeRef, b: &InodeRef) -> bool {
    Rc::as_ptr(a) as *const u8 == Rc::as_ptr(b) as *const u8
}

/// 番号からかたまりの位置 (なければ、または古い番号なら EINVAL)
fn find(t: &Table, id: i64) -> Result<usize, i64> {
    if id < 0 {
        return Err(crate::why::inval());
    }
    let idx = (id as usize) & ((1 << SEQ_SHIFT) - 1);
    match t.segs.get(&idx) {
        Some(s) if s.id(idx) as i64 == id => Ok(idx),
        _ => Err(crate::why::inval()),
    }
}

/// Linux の ipcperms: flag の rwx (持ち主・組・ほかのどれでも) を、持ち主 (cuid か uid) なら上の 3 つ、組なら中の 3 つ、
/// ほかは下の 3 つでくらべる。足りなければ CAP_IPC_OWNER
fn perms(s: &Seg, flag: u32) -> bool {
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

/// IPC_SET、IPC_RMID、SHM_LOCK を許すか (Linux の ipcctl_obtain_check: 持ち主か作った人か CAP_SYS_ADMIN)
fn owner(s: &Seg) -> bool {
    let c = crate::cred::current();
    c.euid == s.cuid || c.euid == s.uid || c.capable(crate::cred::CAP_SYS_ADMIN)
}

/// shmget(key, size, flags)。Linux の ipcget の順: IPC_PRIVATE はいつも新しく、鍵がなければ IPC_CREAT で作る
/// (なければ ENOENT)、あれば IPC_CREAT | IPC_EXCL で EEXIST、大きさが足りなければ EINVAL、許しがなければ EACCES
fn shmget(key: i32, size: usize, flags: u64) -> Result<i64, i64> {
    let t = table();
    if key != IPC_PRIVATE {
        if let Some((&idx, s)) = t.segs.iter().find(|(_, s)| s.key == key && matches!(s.ino, Hold::Live(_))) {
            if flags & IPC_CREAT != 0 && flags & IPC_EXCL != 0 {
                return Err(-EEXIST);
            }
            if s.size < size {
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
    // newseg
    if size < SHMMIN || size as u64 > SHMMAX {
        return Err(crate::why::inval());
    }
    let pages = size.div_ceil(PGSIZE) as u64;
    let total: u64 = t.segs.values().map(|s| s.size.div_ceil(PGSIZE) as u64).sum();
    if total.saturating_add(pages) > SHMALL {
        return Err(-ENOSPC);
    }
    if t.segs.len() >= SHMMNI {
        return Err(-ENOSPC);
    }
    // 位置: 前に配ったものの次から、空いているもの (回る幅の終わりまで行ったら 0 から。0 に戻ったら seq を進める)
    let span = (t.segs.len() * 3 / 2).max(MIN_CYCLE).min(SHMMNI);
    let start = ((t.last_idx + 1) as usize) % span;
    let idx = (0..span).map(|i| (start + i) % span).find(|i| !t.segs.contains_key(i)).ok_or(-ENOSPC)?;
    if idx as i64 <= t.last_idx {
        t.seq = if (t.seq as u32 + 1) >= (1 << (31 - SEQ_SHIFT)) { 0 } else { t.seq + 1 };
    }
    t.last_idx = idx as i64;
    let c = crate::cred::current();
    let ino = crate::tmpfs::anon_file(c.euid, c.egid, 0);
    ino.truncate(size).map_err(|_| -ENOMEM)?;
    let me = proc::current().tgid;
    t.segs.insert(idx, Seg {
        key,
        seq: t.seq,
        size,
        ino: Hold::Live(ino),
        uid: c.euid,
        gid: c.egid,
        cuid: c.euid,
        cgid: c.egid,
        mode: (flags & 0o777) as u32,
        atime: 0,
        dtime: 0,
        ctime: now(),
        cpid: me,
        lpid: 0,
    });
    Ok(t.segs[&idx].id(idx) as i64)
}

/// shmat(id, addr, flags): かたまりを写して、その番地を返す (Linux の do_shmat)
fn shmat(id: i64, addr: usize, flags: u64) -> Result<i64, i64> {
    let mut addr = addr;
    if id < 0 {
        return Err(crate::why::inval());
    }
    if addr != 0 {
        if addr & (SHMLBA - 1) != 0 {
            if flags & SHM_RND != 0 {
                addr &= !(SHMLBA - 1);
                if addr == 0 && flags & SHM_REMAP != 0 {
                    return Err(crate::why::inval());
                }
            } else {
                return Err(crate::why::inval());
            }
        }
    } else if flags & SHM_REMAP != 0 {
        return Err(crate::why::inval());
    }
    let (mut prot, mut acc) = if flags & SHM_RDONLY != 0 { (1u8, 0o444) } else { (3u8, 0o666) };
    if flags & SHM_EXEC != 0 {
        prot |= 4;
        acc |= 0o111;
    }
    let t = table();
    let idx = find(t, id)?;
    let s = t.segs.get_mut(&idx).unwrap();
    if !perms(s, acc) {
        return Err(-EACCES);
    }
    let ino = s.inode().ok_or(-EIDRM)?;
    let len = s.size.div_ceil(PGSIZE) * PGSIZE;
    let name = alloc::format!("/SYSV{:08x} (deleted)", s.key as u32);
    let m = proc::current().mm();
    let va = if addr != 0 {
        if addr.checked_add(len).is_none() || (flags & SHM_REMAP == 0 && m.pt.overlaps(addr, addr + len)) {
            return Err(crate::why::inval());
        }
        addr
    } else {
        let va = m.pt.free_area(m.mmap_next, len);
        m.mmap_next = va + len;
        va
    };
    let back = crate::vm::Backing::File { ver: crate::vm::file_ver(&ino), ino, off: 0, fend: usize::MAX };
    m.pt.map(va, va + len, prot, true, back).ok_or(-ENOMEM)?;
    m.pt.set_name(va, &name);
    s.atime = now();
    s.lpid = proc::current().tgid;
    Ok(va as i64)
}

/// shmdt(addr): addr から写したかたまりを外す (Linux の ksys_shmdt: addr で始まるかたまりの写しを見つけ、その大きさの
/// うちの同じかたまりの写し (mprotect や munmap で分かれたもの) をみな外す)。なければ EINVAL
fn shmdt(addr: usize) -> Result<i64, i64> {
    if addr & (PGSIZE - 1) != 0 {
        return Err(crate::why::inval());
    }
    let t = table();
    let m = proc::current().mm();
    // addr から始まり、ファイルの頭 (off 0) を写したもので、かたまりのファイルのもの
    let first = match m.pt.find(addr) {
        Some((start, v)) if start == addr => match &v.back {
            crate::vm::Backing::File { ino, off: 0, .. } => t.segs.iter().find(|(_, s)| s.inode().is_some_and(|i| same(&i, ino))).map(|(&idx, s)| (idx, ino.clone(), s.size)),
            _ => None,
        },
        _ => None,
    };
    let Some((idx, ino, size)) = first else { return Err(crate::why::inval()) };
    let end = addr + size.div_ceil(PGSIZE) * PGSIZE;
    let mut va = addr;
    let mut spans = alloc::vec::Vec::new();
    while va < end {
        match m.pt.find(va) {
            Some((start, v)) => {
                if v.end > end {
                    break;
                }
                if let crate::vm::Backing::File { ino: i, off, .. } = &v.back {
                    if same(i, &ino) && *off == start - addr {
                        spans.push((start, v.end));
                    }
                }
                va = v.end;
            }
            None => va += PGSIZE,
        }
    }
    drop(ino);
    for (s, e) in spans {
        m.pt.unmap(s, e);
    }
    if let Some(s) = t.segs.get_mut(&idx) {
        s.dtime = now();
        s.lpid = proc::current().tgid;
    }
    Ok(0)
}

/// struct shmid64_ds (asm-generic の 64 ビット、112 バイト): ipc64_perm (key、uid、gid、cuid、cgid、mode、seq) と
/// shm_segsz、atime、dtime、ctime、cpid、lpid、nattch
fn stat_bytes(s: &Seg) -> [u8; 112] {
    let mut b = [0u8; 112];
    let mut put = |o: usize, v: &[u8]| b[o..o + v.len()].copy_from_slice(v);
    put(0, &s.key.to_le_bytes());
    put(4, &s.uid.to_le_bytes());
    put(8, &s.gid.to_le_bytes());
    put(12, &s.cuid.to_le_bytes());
    put(16, &s.cgid.to_le_bytes());
    put(20, &s.mode.to_le_bytes());
    put(24, &s.seq.to_le_bytes());
    put(48, &(s.size as u64).to_le_bytes());
    put(56, &s.atime.to_le_bytes());
    put(64, &s.dtime.to_le_bytes());
    put(72, &s.ctime.to_le_bytes());
    put(80, &crate::ns::local_or_0(s.cpid).to_le_bytes());
    put(84, &crate::ns::local_or_0(s.lpid).to_le_bytes());
    put(88, &(s.nattch() as u64).to_le_bytes());
    b
}


fn max_idx(t: &Table) -> i64 {
    t.segs.keys().next_back().map_or(0, |&i| i as i64)
}

/// shmctl(id, cmd, buf) (Linux の ksys_shmctl)
fn shmctl(id: i64, cmd: i64, buf: usize) -> Result<i64, i64> {
    let cmd = cmd & !IPC_64;
    if cmd < 0 || id < 0 {
        return Err(crate::why::inval());
    }
    let t = table();
    match cmd {
        IPC_INFO => {
            // struct shminfo64: shmmax、shmmin、shmmni、shmseg、shmall (と 4 つの空き)
            let mut b = [0u8; 72];
            for (i, v) in [SHMMAX, SHMMIN as u64, SHMMNI as u64, SHMMNI as u64, SHMALL].iter().enumerate() {
                b[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
            }
            proc::put_user(buf, &b)?;
            Ok(max_idx(t))
        }
        SHM_INFO => {
            // struct shm_info: used_ids、shm_tot、shm_rss、shm_swp (ページ)、swap_attempts、swap_successes
            let mut b = [0u8; 48];
            let tot: u64 = t.segs.values().map(|s| s.size.div_ceil(PGSIZE) as u64).sum();
            let rss: u64 = t.segs.values().filter_map(|s| s.inode()).map(|i| i.meta().blocks * 512 / PGSIZE as u64).sum();
            b[0..4].copy_from_slice(&(t.segs.len() as i32).to_le_bytes());
            b[8..16].copy_from_slice(&tot.to_le_bytes());
            b[16..24].copy_from_slice(&rss.to_le_bytes());
            proc::put_user(buf, &b)?;
            Ok(max_idx(t))
        }
        // 位置で見る (ipcs)。答えは番号
        SHM_STAT | SHM_STAT_ANY => {
            let s = t.segs.get(&(id as usize)).ok_or_else(crate::why::inval)?;
            if cmd == SHM_STAT && !perms(s, 0o444) {
                return Err(-EACCES);
            }
            proc::put_user(buf, &stat_bytes(s))?;
            Ok(s.id(id as usize) as i64)
        }
        IPC_STAT => {
            let idx = find(t, id)?;
            let s = &t.segs[&idx];
            if !perms(s, 0o444) {
                return Err(-EACCES);
            }
            proc::put_user(buf, &stat_bytes(s))?;
            Ok(0)
        }
        IPC_SET => {
            let mut b = [0u8; 112];
            proc::current().pt().copy_in(&mut b, buf).ok_or(-EFAULT)?;
            let idx = find(t, id)?;
            let s = t.segs.get_mut(&idx).unwrap();
            if !owner(s) {
                return Err(-EPERM);
            }
            let w = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
            // 持ち主と組 (-1 は使えない: Linux の uid_valid)
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
            let s = t.segs.get_mut(&idx).unwrap();
            if !owner(s) {
                return Err(-EPERM);
            }
            if s.nattch() == 0 {
                t.segs.remove(&idx);
            } else if let Hold::Live(i) = &s.ino {
                s.ino = Hold::Dest(Rc::downgrade(i));
                s.mode |= SHM_DEST;
                s.key = IPC_PRIVATE;
            }
            Ok(0)
        }
        SHM_LOCK | SHM_UNLOCK => {
            let idx = find(t, id)?;
            let s = t.segs.get_mut(&idx).unwrap();
            // Linux: CAP_IPC_LOCK がなければ、持ち主で、RLIMIT_MEMLOCK があること (aios は数えないので持ち主なら)
            let c = crate::cred::current();
            if !c.capable_noaudit(CAP_IPC_LOCK) && c.euid != s.cuid && c.euid != s.uid {
                return Err(-EPERM);
            }
            if cmd == SHM_LOCK {
                s.mode |= SHM_LOCKED;
            } else {
                s.mode &= !SHM_LOCKED;
            }
            Ok(0)
        }
        _ => Err(crate::why::inval()),
    }
}

/// /proc/sysvipc/shm (Linux の sysvipc_shm_proc_show の形。ipcs が読む)
pub fn proc_text() -> alloc::string::String {
    use core::fmt::Write;
    let t = table();
    let mut s = alloc::string::String::from(
        "       key      shmid perms                  size  cpid  lpid nattch   uid   gid  cuid  cgid      atime      dtime      ctime                   rss                  swap\n",
    );
    for (&idx, g) in &t.segs {
        let rss = g.inode().map_or(0, |i| i.meta().blocks * 512);
        let _ = writeln!(
            s,
            "{:>10} {:>10}  {:>4o} {:>21} {:>5} {:>5}  {:>5} {:>5} {:>5} {:>5} {:>5} {:>10} {:>10} {:>10} {:>21} {:>21}",
            g.key,
            g.id(idx),
            g.mode,
            g.size,
            crate::ns::local_or_0(g.cpid),
            crate::ns::local_or_0(g.lpid),
            g.nattch(),
            g.uid,
            g.gid,
            g.cuid,
            g.cgid,
            g.atime,
            g.dtime,
            g.ctime,
            rss,
            0
        );
    }
    s
}

/// システムコール (main.rs が syscall::register で置く。Linux の ipc/shm.c、CONFIG_SYSVIPC)
pub const SYSCALLS: &[u64] = {
    use crate::syscall::nr::*;
    &[SHMGET, SHMCTL, SHMAT, SHMDT]
};

pub fn sys(n: u64, a: &[u64; 6]) -> Result<i64, i64> {
    use crate::syscall::nr::*;
    match n {
        SHMGET => shmget(a[0] as i32, a[1] as usize, a[2] as u32 as u64),
        SHMCTL => shmctl(a[0] as i32 as i64, a[1] as i32 as i64, a[2] as usize),
        SHMAT => shmat(a[0] as i32 as i64, a[1] as usize, a[2] as u32 as u64),
        SHMDT => shmdt(a[0] as usize),
        _ => Err(-38),
    }
}

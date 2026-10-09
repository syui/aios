// ユーザーとグループ (資格情報) と、ファイルのパーミッションの判定
use crate::proc;
use crate::vfs::{Meta, S_IFDIR, S_IFMT};
use alloc::vec::Vec;

pub const EPERM: i64 = 1;
pub const EACCES: i64 = 13;
pub const EFAULT: i64 = 14;
pub const EINVAL: i64 = 22;

pub const R: u32 = 4;
pub const W: u32 = 2;
pub const X: u32 = 1;

pub const S_ISUID: u32 = 0o4000;
pub const S_ISGID: u32 = 0o2000;
pub const S_ISVTX: u32 = 0o1000;

#[derive(Clone)]
pub struct Cred {
    pub uid: u32,
    pub euid: u32,
    pub suid: u32,
    pub gid: u32,
    pub egid: u32,
    pub sgid: u32,
    pub groups: Vec<u32>,
    /// prctl(PR_SET_NO_NEW_PRIVS): exec で setuid / setgid のビットを見ない。外せない
    pub no_new_privs: bool,
    /// landlock の砂場 (かかっている層)
    pub landlock: Option<crate::landlock::Domain>,
    /// seccomp のフィルタ (かかっているもの)
    pub seccomp: Option<crate::seccomp::Seccomp>,
    /// namespace (ns.rs)
    pub ns: crate::ns::Ns,
}

impl Cred {
    pub const ROOT: Cred = Cred { uid: 0, euid: 0, suid: 0, gid: 0, egid: 0, sgid: 0, groups: Vec::new(), no_new_privs: false, landlock: None, seccomp: None, ns: crate::ns::Ns::INIT };

    pub fn in_group(&self, gid: u32) -> bool {
        self.egid == gid || self.groups.contains(&gid)
    }

    /// meta に want (R/W/X の組み合わせ) ができるか。real なら実 uid/gid で (access(2) 用)
    pub fn may(&self, m: &Meta, want: u32, real: bool) -> bool {
        let (uid, gid) = if real { (self.uid, self.gid) } else { (self.euid, self.egid) };
        if uid == 0 {
            // root でも、実行はどこかに x がなければできない (ディレクトリは別)
            return want & X == 0 || m.mode & S_IFMT == S_IFDIR || m.mode & 0o111 != 0;
        }
        let bits = if m.uid == uid {
            (m.mode >> 6) & 7
        } else if gid == m.gid || self.groups.contains(&m.gid) {
            (m.mode >> 3) & 7
        } else {
            m.mode & 7
        };
        bits & want == want
    }

    pub fn check(&self, m: &Meta, want: u32) -> Result<(), i64> {
        if self.may(m, want, false) { Ok(()) } else { Err(-EACCES) }
    }

    /// 持ち主か root か
    pub fn owns(&self, m: &Meta) -> bool {
        self.euid == 0 || self.euid == m.uid
    }
}

/// いまのプロセスの資格情報 (起動中でまだプロセスがなければ root)
pub fn current() -> Cred {
    proc::current_cred()
}

// ---- システムコール ----

type Res = Result<i64, i64>;

fn set(f: impl FnOnce(&mut Cred) -> Result<(), i64>) -> Res {
    let p = proc::current();
    let mut c = p.cred.clone();
    f(&mut c)?;
    p.cred = c;
    Ok(0)
}

/// -1 は「変えない」
fn arg(v: u64) -> Option<u32> {
    (v as u32 != u32::MAX).then_some(v as u32)
}

pub fn setuid(uid: u64) -> Res {
    let u = crate::ns::take_uid(uid as u32)?;
    set(|c| {
        if c.euid == 0 {
            (c.uid, c.euid, c.suid) = (u, u, u);
        } else if u == c.uid || u == c.suid {
            c.euid = u;
        } else {
            return Err(-EPERM);
        }
        Ok(())
    })
}

pub fn setgid(gid: u64) -> Res {
    let g = crate::ns::take_gid(gid as u32)?;
    set(|c| {
        if c.euid == 0 {
            (c.gid, c.egid, c.sgid) = (g, g, g);
        } else if g == c.gid || g == c.sgid {
            c.egid = g;
        } else {
            return Err(-EPERM);
        }
        Ok(())
    })
}

/// ユーザーの namespace の中の番号を外の番号に (-1 はそのまま)
fn take_u(v: u64) -> Result<u64, i64> {
    Ok(if v as u32 == u32::MAX { v } else { crate::ns::take_uid(v as u32)? as u64 })
}

fn take_g(v: u64) -> Result<u64, i64> {
    Ok(if v as u32 == u32::MAX { v } else { crate::ns::take_gid(v as u32)? as u64 })
}

pub fn setresuid(r: u64, e: u64, s: u64) -> Res {
    setresuid_out(take_u(r)?, take_u(e)?, take_u(s)?)
}

pub fn setresgid(r: u64, e: u64, s: u64) -> Res {
    setresgid_out(take_g(r)?, take_g(e)?, take_g(s)?)
}

/// 外の番号で
fn setresuid_out(r: u64, e: u64, s: u64) -> Res {
    set(|c| {
        let ok = |v: u32| c.euid == 0 || v == c.uid || v == c.euid || v == c.suid;
        let (r, e, s) = (arg(r), arg(e), arg(s));
        if ![r, e, s].iter().flatten().all(|&v| ok(v)) {
            return Err(-EPERM);
        }
        if let Some(v) = r {
            c.uid = v;
        }
        if let Some(v) = e {
            c.euid = v;
        }
        if let Some(v) = s {
            c.suid = v;
        }
        Ok(())
    })
}

fn setresgid_out(r: u64, e: u64, s: u64) -> Res {
    set(|c| {
        let ok = |v: u32| c.euid == 0 || v == c.gid || v == c.egid || v == c.sgid;
        let (r, e, s) = (arg(r), arg(e), arg(s));
        if ![r, e, s].iter().flatten().all(|&v| ok(v)) {
            return Err(-EPERM);
        }
        if let Some(v) = r {
            c.gid = v;
        }
        if let Some(v) = e {
            c.egid = v;
        }
        if let Some(v) = s {
            c.sgid = v;
        }
        Ok(())
    })
}

/// setreuid(r, e): 実 uid を変えたとき (または e が実 uid と違うとき) は suid = 新しい euid
pub fn setreuid(r: u64, e: u64) -> Res {
    let (r, e) = (take_u(r)?, take_u(e)?);
    let old = proc::current().cred.clone();
    setresuid_out(r, e, u64::MAX)?;
    let c = &mut proc::current().cred;
    if arg(r).is_some() || arg(e).is_some_and(|v| v != old.uid) {
        c.suid = c.euid;
    }
    Ok(0)
}

pub fn setregid(r: u64, e: u64) -> Res {
    let (r, e) = (take_g(r)?, take_g(e)?);
    let old = proc::current().cred.clone();
    setresgid_out(r, e, u64::MAX)?;
    let c = &mut proc::current().cred;
    if arg(r).is_some() || arg(e).is_some_and(|v| v != old.gid) {
        c.sgid = c.egid;
    }
    Ok(0)
}

fn out(va: usize, b: &[u8]) -> Result<(), i64> {
    proc::current().pt().copy_out(va, b).ok_or(-EFAULT)
}

pub fn getresuid(r: usize, e: usize, s: usize) -> Res {
    let c = proc::current().cred.clone();
    let show = crate::ns::show_uid;
    out(r, &show(c.uid).to_le_bytes())?;
    out(e, &show(c.euid).to_le_bytes())?;
    out(s, &show(c.suid).to_le_bytes())?;
    Ok(0)
}

pub fn getresgid(r: usize, e: usize, s: usize) -> Res {
    let c = proc::current().cred.clone();
    let show = crate::ns::show_gid;
    out(r, &show(c.gid).to_le_bytes())?;
    out(e, &show(c.egid).to_le_bytes())?;
    out(s, &show(c.sgid).to_le_bytes())?;
    Ok(0)
}

pub fn getgroups(size: usize, list: usize) -> Res {
    let g = proc::current().cred.groups.clone();
    if size == 0 {
        return Ok(g.len() as i64);
    }
    if size < g.len() {
        return Err(-EINVAL);
    }
    for (i, v) in g.iter().enumerate() {
        out(list + i * 4, &crate::ns::show_gid(*v).to_le_bytes())?;
    }
    Ok(g.len() as i64)
}

pub fn setgroups(size: usize, list: usize) -> Res {
    if proc::current().cred.euid != 0 || proc::current().cred.ns.user.is_some() {
        return Err(-EPERM);
    }
    if size > 65536 {
        return Err(-EINVAL);
    }
    let mut g = Vec::with_capacity(size);
    for i in 0..size {
        let mut b = [0u8; 4];
        proc::current().pt().copy_in(&mut b, list + i * 4).ok_or(-EFAULT)?;
        g.push(u32::from_le_bytes(b));
    }
    proc::current().cred.groups = g;
    Ok(0)
}

/// setfsuid/setfsgid: fs 用の id は euid と同じとして、前の値を返す
pub fn setfsuid(_uid: u64) -> Res {
    Ok(proc::current().cred.euid as i64)
}

pub fn setfsgid(_gid: u64) -> Res {
    Ok(proc::current().cred.egid as i64)
}

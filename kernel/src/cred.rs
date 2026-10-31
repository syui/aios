// ユーザーとグループ (資格情報) と、ファイルのパーミッションの判定
use crate::proc;
use crate::vfs::{Meta, S_IFDIR, S_IFMT};
use alloc::vec::Vec;

pub const EPERM: i64 = 1;
pub const EACCES: i64 = 13;
pub const EFAULT: i64 = 14;

pub const R: u32 = 4;
pub const W: u32 = 2;
pub const X: u32 = 1;

pub const S_ISUID: u32 = 0o4000;
pub const S_ISGID: u32 = 0o2000;
pub const S_ISVTX: u32 = 0o1000;

// capability の番号 (Linux の include/uapi/linux/capability.h。aios では root がみな持つ: capable)
pub const CAP_CHOWN: u32 = 0;
pub const CAP_DAC_OVERRIDE: u32 = 1;
pub const CAP_DAC_READ_SEARCH: u32 = 2;
pub const CAP_FOWNER: u32 = 3;
pub const CAP_FSETID: u32 = 4;
pub const CAP_KILL: u32 = 5;
pub const CAP_SETGID: u32 = 6;
pub const CAP_SETUID: u32 = 7;
pub const CAP_NET_BIND_SERVICE: u32 = 10;
pub const CAP_NET_ADMIN: u32 = 12;
pub const CAP_NET_RAW: u32 = 13;
pub const CAP_SYS_MODULE: u32 = 16;
pub const CAP_SYS_CHROOT: u32 = 18;
pub const CAP_SYS_PTRACE: u32 = 19;
pub const CAP_SYS_ADMIN: u32 = 21;
pub const CAP_SYS_BOOT: u32 = 22;
pub const CAP_MKNOD: u32 = 27;
pub const CAP_MAC_ADMIN: u32 = 33;
/// capability の名前 (番号の順。AppArmor の記録の capname と features/caps/mask)
pub const CAP_NAMES: [&str; 41] = [
    "chown", "dac_override", "dac_read_search", "fowner", "fsetid", "kill", "setgid", "setuid", "setpcap", "linux_immutable", "net_bind_service", "net_broadcast", "net_admin", "net_raw",
    "ipc_lock", "ipc_owner", "sys_module", "sys_rawio", "sys_chroot", "sys_ptrace", "sys_pacct", "sys_admin", "sys_boot", "sys_nice", "sys_resource", "sys_time", "sys_tty_config",
    "mknod", "lease", "audit_write", "audit_control", "setfcap", "mac_override", "mac_admin", "syslog", "wake_alarm", "block_suspend", "audit_read", "perfmon", "bpf", "checkpoint_restore",
];

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
    /// AppArmor のプロファイル (なければ unconfined) と、次の exec でつけるもの (/proc/self/attr/exec。apparmor.rs)
    pub apparmor: Option<crate::apparmor::Label>,
    pub aa_onexec: Option<crate::apparmor::Label>,
}

impl Cred {
    pub const ROOT: Cred = Cred { uid: 0, euid: 0, suid: 0, gid: 0, egid: 0, sgid: 0, groups: Vec::new(), no_new_privs: false, landlock: None, seccomp: None, ns: crate::ns::Ns::INIT, apparmor: None, aa_onexec: None };

    pub fn in_group(&self, gid: u32) -> bool {
        self.egid == gid || self.groups.contains(&gid)
    }

    /// cap を持つか (Linux の capable)。aios では root (euid 0) がみな持つ。持っていても AppArmor のプロファイルが
    /// 許さなければ持たない (記録する: apparmor::capable)。要るときだけ呼ぶ (持ち主などで足りるなら先にそちらを: Linux と同じ順)
    pub fn capable(&self, cap: u32) -> bool {
        self.euid == 0 && crate::apparmor::capable(self, cap, true)
    }

    /// capable と同じだが記録しない (ns_capable_noaudit: できなければほかのやりかたに回るもの)
    pub fn capable_noaudit(&self, cap: u32) -> bool {
        self.euid == 0 && crate::apparmor::capable(self, cap, false)
    }

    /// meta に want (R/W/X の組み合わせ) ができるか。real なら実 uid/gid で (access(2) 用)。Linux の
    /// generic_permission と同じ順: まず mode のビットで、足りなければ root は CAP_DAC_READ_SEARCH (読む、ディレクトリを
    /// たどる) か CAP_DAC_OVERRIDE (実行はどこかに x がなければできない。ディレクトリは別)
    pub fn may(&self, m: &Meta, want: u32, real: bool) -> bool {
        let (uid, gid) = if real { (self.uid, self.gid) } else { (self.euid, self.egid) };
        let bits = if m.uid == uid {
            (m.mode >> 6) & 7
        } else if gid == m.gid || self.groups.contains(&m.gid) {
            (m.mode >> 3) & 7
        } else {
            m.mode & 7
        };
        if bits & want == want {
            return true;
        }
        if uid != 0 {
            return false;
        }
        let cap = |c: u32| crate::apparmor::capable(self, c, true);
        if m.mode & S_IFMT == S_IFDIR {
            return (want & W == 0 && cap(CAP_DAC_READ_SEARCH)) || cap(CAP_DAC_OVERRIDE);
        }
        if want == R && cap(CAP_DAC_READ_SEARCH) {
            return true;
        }
        (want & X == 0 || m.mode & 0o111 != 0) && cap(CAP_DAC_OVERRIDE)
    }

    pub fn check(&self, m: &Meta, want: u32) -> Result<(), i64> {
        if self.may(m, want, false) {
            Ok(())
        } else {
            crate::why::perm(m, want, self, false);
            Err(-EACCES)
        }
    }

    /// may の答えを決めた組: 0 持ち主、1 グループ、2 ほか、3 root (why.rs)
    pub fn class(&self, m: &Meta, real: bool) -> u8 {
        let (uid, gid) = if real { (self.uid, self.gid) } else { (self.euid, self.egid) };
        if uid == 0 {
            3
        } else if m.uid == uid {
            0
        } else if gid == m.gid || self.groups.contains(&m.gid) {
            1
        } else {
            2
        }
    }

    /// 持ち主か CAP_FOWNER か (inode_owner_or_capable)
    pub fn owns(&self, m: &Meta) -> bool {
        self.euid == m.uid || self.capable(CAP_FOWNER)
    }
}

/// SO_PRIORITY を 7 より上に、SO_MARK をつける: CAP_NET_RAW か CAP_NET_ADMIN (Linux の sk_set_prio_allowed)
pub fn net_prio_ok(c: &Cred) -> bool {
    c.capable(CAP_NET_RAW) || c.capable(CAP_NET_ADMIN)
}

/// いまのプロセスの資格情報 (起動中でまだプロセスがなければ root)
/// いまのスレッドの資格情報 (写さずに: groups や namespace を、引くたびに写さない)
pub fn current() -> &'static Cred {
    proc::current_cred_ref()
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
        if c.capable(CAP_SETUID) {
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
        if c.capable(CAP_SETGID) {
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
        // いまの番号のどれかへなら capability は問わない (Linux の __sys_setresuid)
        let ok = |v: u32| v == c.uid || v == c.euid || v == c.suid;
        let (r, e, s) = (arg(r), arg(e), arg(s));
        if ![r, e, s].iter().flatten().all(|&v| ok(v)) && !c.capable(CAP_SETUID) {
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
        // いまの番号のどれかへなら capability は問わない (Linux の __sys_setresgid)
        let ok = |v: u32| v == c.gid || v == c.egid || v == c.sgid;
        let (r, e, s) = (arg(r), arg(e), arg(s));
        if ![r, e, s].iter().flatten().all(|&v| ok(v)) && !c.capable(CAP_SETGID) {
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
        return Err(crate::why::inval());
    }
    for (i, v) in g.iter().enumerate() {
        out(list + i * 4, &crate::ns::show_gid(*v).to_le_bytes())?;
    }
    Ok(g.len() as i64)
}

pub fn setgroups(size: usize, list: usize) -> Res {
    if !proc::current().cred.capable(CAP_SETGID) || proc::current().cred.ns.user.is_some() {
        return Err(-EPERM);
    }
    if size > 65536 {
        return Err(crate::why::inval());
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

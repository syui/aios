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
pub const CAP_LINUX_IMMUTABLE: u32 = 9;
pub const CAP_NET_BIND_SERVICE: u32 = 10;
pub const CAP_NET_ADMIN: u32 = 12;
pub const CAP_NET_RAW: u32 = 13;
pub const CAP_SYS_MODULE: u32 = 16;
pub const CAP_SYS_CHROOT: u32 = 18;
pub const CAP_SYS_PTRACE: u32 = 19;
pub const CAP_SYS_ADMIN: u32 = 21;
pub const CAP_SYS_BOOT: u32 = 22;
pub const CAP_MKNOD: u32 = 27;
pub const CAP_AUDIT_WRITE: u32 = 29;
pub const CAP_AUDIT_CONTROL: u32 = 30;
pub const CAP_MAC_ADMIN: u32 = 33;
pub const CAP_AUDIT_READ: u32 = 37;
pub const CAP_PERFMON: u32 = 38;
/// ファイルの capability (Linux の CAP_FS_SET: fsuid が 0 でなくなると effective から外れるもの)
pub const FS_CAPS: [u32; 8] = [CAP_CHOWN, CAP_DAC_OVERRIDE, CAP_DAC_READ_SEARCH, CAP_FOWNER, CAP_FSETID, CAP_LINUX_IMMUTABLE, CAP_MKNOD, 32];
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
    /// ファイルの許しと作るものの持ち主に使う id (Linux の fsuid / fsgid)。set*uid / set*gid で euid / egid に、setfsuid /
    /// setfsgid で変わる
    pub fsuid: u32,
    pub fsgid: u32,
    /// capability (aios では root がみな持つ) のうち、permitted があるか (uid・euid・suid のどれかが 0 のあいだ: Linux の
    /// cap_emulate_setxuid)、ファイルのもの (FS_CAPS) が effective にあるか (fsuid を 0 からほかにすると外れ、0 にもどすと
    /// permitted から入る。euid が 0 からほかになると外れ、0 にもどると入る。exec で euid が 0 なら入る)
    pub cap_perm: bool,
    pub fs_caps: bool,
    pub groups: Vec<u32>,
    /// prctl(PR_SET_NO_NEW_PRIVS): exec で setuid / setgid のビットを見ない。外せない
    pub no_new_privs: bool,
    /// 砂場の LSM (landlock) の層。中はその LSM だけが知る。fork で受けつぐ。is_some なら、file::fast_open と fastdir
    /// (大きなロックのない open と作る・消す) を使わない
    pub sandbox: Option<crate::security::Blob>,
    /// seccomp のフィルタ (かかっているもの。中は seccomp だけが知る: security::Blob)。syscall::fast は is_some を見る
    pub seccomp: Option<crate::security::Blob>,
    /// namespace (ns.rs)
    pub ns: crate::ns::Ns,
    /// LSM のラベル (AppArmor のプロファイルの名前。なければ unconfined)。fork で受けつぐ。syscall::fast は is_some を見る
    pub label: Option<crate::security::Label>,
    /// ラベルを持つ LSM のもの (AppArmor の onexec、change_hat の前のプロファイルと token: Linux の aa_task_ctx)。中は
    /// その LSM だけが知る。exec で消える (security::bprm_committed_creds)
    pub label_ctx: Option<crate::security::Blob>,
    /// AUDIT_TTY_SET の設定 (Linux は signal_struct の audit_tty: fork で受けつぎ、exec でも消えない。audit.rs)
    pub audit_tty: u32,
    /// ログインした人と、そのセッションの番号 (/proc/PID/loginuid と sessionid。audit の記録の auid= ses=)。
    /// u32::MAX はまだない。fork で受けつぎ、exec でも setuid でも変わらない (Linux は task_struct)
    pub loginuid: u32,
    pub sessionid: u32,
}

impl Cred {
    pub const ROOT: Cred = Cred { uid: 0, euid: 0, suid: 0, gid: 0, egid: 0, sgid: 0, fsuid: 0, fsgid: 0, cap_perm: true, fs_caps: true, groups: Vec::new(), no_new_privs: false, sandbox: None, seccomp: None, ns: crate::ns::Ns::INIT, label: None, label_ctx: None, audit_tty: 0, loginuid: u32::MAX, sessionid: u32::MAX };

    /// gid の組にいるか (Linux の in_group_p: fsgid と、ほかの組)
    pub fn in_group(&self, gid: u32) -> bool {
        self.fsgid == gid || self.groups.contains(&gid)
    }

    /// その capability が effective にあるか: ファイルのもの (FS_CAPS) は fs_caps、ほかは euid が 0 (aios では root が
    /// みな持つ)
    fn has_cap(&self, cap: u32) -> bool {
        if FS_CAPS.contains(&cap) { self.fs_caps } else { self.euid == 0 }
    }

    /// cap を持つか (Linux の capable)。aios では root (euid 0) がみな持つ。持っていても LSM (AppArmor のプロファイル
    /// など) が許さなければ持たない (記録する: security::capable)。要るときだけ呼ぶ (持ち主などで足りるなら先にそちらを:
    /// Linux と同じ順)
    pub fn capable(&self, cap: u32) -> bool {
        self.has_cap(cap) && self.lsm_capable(cap, true)
    }

    /// capable と同じだが記録しない (ns_capable_noaudit: できなければほかのやりかたに回るもの)
    pub fn capable_noaudit(&self, cap: u32) -> bool {
        self.has_cap(cap) && self.lsm_capable(cap, false)
    }

    /// commoncap が許したあとの LSM (security_capable: landlock の CAPABILITY_USE、それから AppArmor。CONFIG_LSM の順)
    fn lsm_capable(&self, cap: u32, audit: bool) -> bool {
        crate::security::capable(self, cap, audit)
    }

    /// meta に want (R/W/X の組み合わせ) ができるか。real なら実 uid/gid で (access(2) 用)。Linux の
    /// generic_permission と同じ順: まず mode のビットで、足りなければ root は CAP_DAC_READ_SEARCH (読む、ディレクトリを
    /// たどる) か CAP_DAC_OVERRIDE (実行はどこかに x がなければできない。ディレクトリは別)
    pub fn may(&self, m: &Meta, want: u32, real: bool) -> bool {
        let (uid, gid) = if real { (self.uid, self.gid) } else { (self.fsuid, self.fsgid) };
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
        // access(2) は実の id で、実の uid が 0 なら capability がある (Linux の access_override_creds)。ふつうは effective の
        // ファイルの capability (fsuid を 0 でなくすと外れる)
        if if real { uid != 0 } else { !self.fs_caps } {
            return false;
        }
        let cap = |c: u32| self.lsm_capable(c, true);
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
        let (uid, gid) = if real { (self.uid, self.gid) } else { (self.fsuid, self.fsgid) };
        if if real { uid == 0 } else { self.fs_caps } {
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
        self.fsuid == m.uid || self.capable(CAP_FOWNER)
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

/// set*uid (uid が true) / set*gid の資格を f で変える (Linux の __sys_set*uid と commit_creds): uid のものは fsuid を euid
/// に、capability を uid にあわせる (cap_emulate_setxuid)。gid のものは fsgid を egid に
fn set_ids(uid: bool, f: impl FnOnce(&mut Cred) -> Result<(), i64>) -> Res {
    let p = proc::current();
    let mut c = p.cred.clone();
    f(&mut c)?;
    if uid {
        let o = &p.cred;
        c.fsuid = c.euid;
        let (had, has) = (o.uid == 0 || o.euid == 0 || o.suid == 0, c.uid == 0 || c.euid == 0 || c.suid == 0);
        // uid・euid・suid がみな 0 でなくなった: permitted も effective も空に
        if had && !has {
            (c.cap_perm, c.fs_caps) = (false, false);
        }
        // euid が 0 からほかへ: effective を空に。ほかから 0 へ: effective を permitted に
        if o.euid == 0 && c.euid != 0 {
            c.fs_caps = false;
        }
        if o.euid != 0 && c.euid == 0 {
            c.fs_caps = c.cap_perm;
        }
    } else {
        c.fsgid = c.egid;
    }
    changed(p, &c);
    p.cred = c;
    Ok(0)
}

/// 資格を new に変える前に (Linux の commit_creds): euid、egid、fsuid、fsgid のどれかが変わるなら (aios では capability も
/// euid で決まる)、dumpable を fs.suid_dumpable (既定 0) に。もとの人が /proc/PID の中や ptrace で、権利の変わったプロセスを
/// 見られないように (持ちもの (mm) のないカーネルのスレッドはそのまま)
pub fn changed(p: &mut proc::Proc, new: &Cred) {
    let o = &p.cred;
    if (new.euid != o.euid || new.egid != o.egid || new.fsuid != o.fsuid || new.fsgid != o.fsgid) && p.mm.is_some() {
        p.mm().dumpable = crate::sysctl::SUID_DUMPABLE.load(core::sync::atomic::Ordering::Relaxed) as u8;
    }
}

/// -1 は「変えない」
fn arg(v: u64) -> Option<u32> {
    (v as u32 != u32::MAX).then_some(v as u32)
}

pub fn setuid(uid: u64) -> Res {
    let u = crate::ns::take_uid(uid as u32)?;
    set_ids(true, |c| {
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
    set_ids(false, |c| {
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

/// 外の番号で。何も変わらなければ (euid は fsuid とも同じなら) 何もしない (Linux の __sys_setresuid)
fn setresuid_out(r: u64, e: u64, s: u64) -> Res {
    let (r, e, s) = (arg(r), arg(e), arg(s));
    let o = &proc::current().cred;
    if r.is_none_or(|v| v == o.uid) && e.is_none_or(|v| v == o.euid && v == o.fsuid) && s.is_none_or(|v| v == o.suid) {
        return Ok(0);
    }
    set_ids(true, |c| {
        // いまの番号のどれかへなら capability は問わない (Linux の __sys_setresuid)
        let ok = |v: u32| v == c.uid || v == c.euid || v == c.suid;
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
    let (r, e, s) = (arg(r), arg(e), arg(s));
    let o = &proc::current().cred;
    if r.is_none_or(|v| v == o.gid) && e.is_none_or(|v| v == o.egid && v == o.fsgid) && s.is_none_or(|v| v == o.sgid) {
        return Ok(0);
    }
    set_ids(false, |c| {
        // いまの番号のどれかへなら capability は問わない (Linux の __sys_setresgid)
        let ok = |v: u32| v == c.gid || v == c.egid || v == c.sgid;
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

/// setreuid(r, e) (Linux の __sys_setreuid): 実 uid はいまの uid か euid へ、euid は 3 つのどれかへ (でなければ
/// CAP_SETUID)。実 uid を変えたとき (または e が前の実 uid と違うとき) は suid = 新しい euid
pub fn setreuid(r: u64, e: u64) -> Res {
    let (r, e) = (arg(take_u(r)?), arg(take_u(e)?));
    set_ids(true, |c| {
        let old = c.clone();
        let cap = || old.capable(CAP_SETUID);
        if let Some(v) = r {
            if v != old.uid && v != old.euid && !cap() {
                return Err(-EPERM);
            }
            c.uid = v;
        }
        if let Some(v) = e {
            if v != old.uid && v != old.euid && v != old.suid && !cap() {
                return Err(-EPERM);
            }
            c.euid = v;
        }
        if r.is_some() || e.is_some_and(|v| v != old.uid) {
            c.suid = c.euid;
        }
        Ok(())
    })
}

pub fn setregid(r: u64, e: u64) -> Res {
    let (r, e) = (arg(take_g(r)?), arg(take_g(e)?));
    set_ids(false, |c| {
        let old = c.clone();
        let cap = || old.capable(CAP_SETGID);
        if let Some(v) = r {
            if v != old.gid && v != old.egid && !cap() {
                return Err(-EPERM);
            }
            c.gid = v;
        }
        if let Some(v) = e {
            if v != old.gid && v != old.egid && v != old.sgid && !cap() {
                return Err(-EPERM);
            }
            c.egid = v;
        }
        if r.is_some() || e.is_some_and(|v| v != old.gid) {
            c.sgid = c.egid;
        }
        Ok(())
    })
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

/// setfsuid (Linux の __sys_setfsuid): いつも前の fsuid を返す。-1 (と namespace で見えない番号) は変えずに問うだけ。
/// uid・euid・suid・fsuid のどれかへか、CAP_SETUID なら変える (ちがえば dumpable も)。fsuid を 0 からほかにすると
/// ファイルの capability が外れ、0 にもどすと permitted から入る (cap_task_fix_setuid の LSM_SETID_FS)
pub fn setfsuid(uid: u64) -> Res {
    let p = proc::current();
    let old = p.cred.fsuid;
    let ret = crate::ns::show_uid(old) as i64;
    if uid as u32 == u32::MAX {
        return Ok(ret);
    }
    let Ok(u) = crate::ns::take_uid(uid as u32) else { return Ok(ret) };
    let o = &p.cred;
    if u == old || !(u == o.uid || u == o.euid || u == o.suid || o.capable(CAP_SETUID)) {
        return Ok(ret);
    }
    let mut c = p.cred.clone();
    c.fsuid = u;
    if old == 0 && u != 0 {
        c.fs_caps = false;
    }
    if old != 0 && u == 0 {
        c.fs_caps = c.cap_perm;
    }
    changed(p, &c);
    p.cred = c;
    Ok(ret)
}

/// setfsgid (Linux の __sys_setfsgid): setfsuid と同じく、gid・egid・sgid・fsgid のどれかへか CAP_SETGID なら
pub fn setfsgid(gid: u64) -> Res {
    let p = proc::current();
    let old = p.cred.fsgid;
    let ret = crate::ns::show_gid(old) as i64;
    if gid as u32 == u32::MAX {
        return Ok(ret);
    }
    let Ok(g) = crate::ns::take_gid(gid as u32) else { return Ok(ret) };
    let o = &p.cred;
    if g == old || !(g == o.gid || g == o.egid || g == o.sgid || o.capable(CAP_SETGID)) {
        return Ok(ret);
    }
    let mut c = p.cred.clone();
    c.fsgid = g;
    changed(p, &c);
    p.cred = c;
    Ok(ret)
}

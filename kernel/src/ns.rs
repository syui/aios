// namespace: プロセスから見える世界を分ける (Linux と同じ CLONE_NEW* と unshare)
//   UTS  ホスト名 (uname の nodename、sethostname)
//   NET  ネットワーク: 中から見えるのは自分の 127.0.0.1 だけ。TCP は中どうしでだけつながる (unix.rs の inet:
//        smoltcp はひとつなので、中の TCP は unix ソケットのしくみで「namespace の番号とポート」の名前につなぐ)。
//        UDP と ICMP はどこへも送れない (ENETUNREACH)。抽象名前空間の unix ソケット (@...) も分かれる (Linux と同じ)
//   PID  プロセスの番号: 中では 1 から数え、中のプロセスだけが見える (getpid、kill、wait、/proc ...)。
//        unshare(CLONE_NEWPID) はそのあとに作る子から (Linux と同じ)。中の 1 番が終わると中のみなが終わる。
//        入れ子 (中でさらに作る) はできない (EINVAL)。外 (はじめの namespace) からは、みな本当の番号で見える
// clone(CLONE_NEW*) か unshare(CLONE_NEW*) で新しいものを作る。fork と exec で引き継ぐ (cred の中に持つ)
// 作るには root か no_new_privs が要る (aios には user namespace がないので、Linux のように root 以外を
// 止めるのではなく、できることが減るだけの namespace は、setuid のプログラムをだませない no_new_privs で許す)。
// 新しい UTS の中ではホスト名を変えられる (外には見えないので、root でなくても)
// /proc/PID/ns/uts は「uts:[番号]」へのリンク (同じものか見分けるため)
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;

pub const CLONE_NEWUTS: u64 = 0x0400_0000;
pub const CLONE_NEWNET: u64 = 0x4000_0000;
pub const CLONE_NEWPID: u64 = 0x2000_0000;
pub const CLONE_NEWNS: u64 = 0x0002_0000;
pub const CLONE_NEWUSER: u64 = 0x1000_0000;

const EPERM: i64 = 1;

/// はじめからある namespace の番号 (Linux と同じ)
pub const INIT_UTS: u64 = 4026531838;
pub const INIT_NET: u64 = 4026531840;
pub const INIT_PID: u64 = 4026531836;
pub const INIT_MNT: u64 = 4026531841;
pub const INIT_USER: u64 = 4026531837;
const EINVAL: i64 = 22;

static mut NEXT_ID: u64 = 4026532000;

fn next_id() -> u64 {
    unsafe {
        NEXT_ID += 1;
        NEXT_ID
    }
}

/// UTS: ホスト名
pub struct Uts {
    pub id: u64,
    pub name: RefCell<([u8; 64], usize)>,
}

/// NET: 番号だけ (中のソケットは番号で分ける)
pub struct Net {
    pub id: u64,
}

/// PID: 中の番号を配る
pub struct Pid {
    pub id: u64,
    next: core::cell::Cell<u32>,
}

impl Pid {
    pub fn alloc(&self) -> u32 {
        let n = self.next.get() + 1;
        self.next.set(n);
        n
    }
}

/// プロセスの namespace (None ははじめからあるもの)。pid はこれから作る子の PID (自分のは Proc の pid_ns)
#[derive(Clone)]
pub struct Ns {
    pub uts: Option<Rc<Uts>>,
    pub net: Option<Rc<Net>>,
    pub pid: Option<Rc<Pid>>,
    /// マウントの表 (vfs.rs)
    pub mnt: Option<Rc<crate::vfs::MountNs>>,
    /// ユーザー (下の「ユーザーの namespace」)
    pub user: Option<Rc<User>>,
}

impl Ns {
    pub const INIT: Ns = Ns { uts: None, net: None, pid: None, mnt: None, user: None };

    pub fn user_id(&self) -> u64 {
        self.user.as_ref().map_or(INIT_USER, |u| u.id)
    }

    pub fn mnt_id(&self) -> u64 {
        self.mnt.as_ref().map_or(INIT_MNT, |m| m.id)
    }

    pub fn pid_children_id(&self) -> u64 {
        self.pid.as_ref().map_or(INIT_PID, |p| p.id)
    }

    /// 番号 (/proc/PID/ns と lsns)
    pub fn uts_id(&self) -> u64 {
        self.uts.as_ref().map_or(INIT_UTS, |u| u.id)
    }

    pub fn net_id(&self) -> u64 {
        self.net.as_ref().map_or(INIT_NET, |n| n.id)
    }
}

/// いまのプロセスが分けた NET の中なら、その番号
pub fn net() -> Option<u64> {
    crate::proc::current().cred.ns.net.as_ref().map(|n| n.id)
}

/// flags の CLONE_NEW* のうち、扱えるもの
pub const SUPPORTED: u64 = CLONE_NEWUTS | CLONE_NEWNET | CLONE_NEWPID | CLONE_NEWNS | CLONE_NEWUSER;

/// cred の namespace を flags のぶん新しくする (clone の子と unshare)
pub fn renew(c: &mut crate::cred::Cred, flags: u64) -> Result<(), i64> {
    if flags & SUPPORTED == 0 {
        return Ok(());
    }
    // ユーザーの namespace はだれでも作れる (先に作る。中ではほかの namespace も作れる)。入れ子と chroot の中はだめ
    if flags & CLONE_NEWUSER != 0 {
        if c.ns.user.is_some() {
            return Err(-EINVAL);
        }
        if !crate::proc::current_root().is_empty() {
            return Err(-EPERM);
        }
        c.ns.user = Some(Rc::new(User { id: next_id(), owner: c.euid, owner_gid: c.egid, maps: RefCell::new(Maps::default()) }));
    }
    if flags & !CLONE_NEWUSER & SUPPORTED != 0 && c.euid != 0 && !c.no_new_privs && c.ns.user.is_none() {
        return Err(-EPERM);
    }
    if flags & CLONE_NEWUTS != 0 {
        let name = current_name(c);
        c.ns.uts = Some(Rc::new(Uts { id: next_id(), name: RefCell::new(name) }));
    }
    if flags & CLONE_NEWNET != 0 {
        c.ns.net = Some(Rc::new(Net { id: next_id() }));
    }
    if flags & CLONE_NEWNS != 0 {
        c.ns.mnt = Some(crate::vfs::new_mnt_ns(next_id(), c.ns.user_id()));
    }
    if flags & CLONE_NEWPID != 0 {
        // 入れ子はできない
        if c.ns.pid.is_some() || crate::proc::current().pid_ns.is_some() {
            return Err(-EINVAL);
        }
        c.ns.pid = Some(Rc::new(Pid { id: next_id(), next: core::cell::Cell::new(0) }));
    }
    Ok(())
}

// ---- ホスト名 ----

static mut HOSTNAME: ([u8; 64], usize) = {
    let mut b = [0u8; 64];
    b[0] = b'a';
    b[1] = b'i';
    b[2] = b'o';
    b[3] = b's';
    (b, 4)
};

fn current_name(c: &crate::cred::Cred) -> ([u8; 64], usize) {
    match &c.ns.uts {
        Some(u) => *u.name.borrow(),
        None => unsafe { *(&raw const HOSTNAME) },
    }
}

/// いまのプロセスから見たホスト名
pub fn hostname() -> ([u8; 64], usize) {
    current_name(&crate::proc::current().cred)
}

/// ホスト名を変える。はじめの UTS は root だけ、新しく作った UTS の中ならだれでも
pub fn set_hostname(name: &[u8], check: bool) -> Result<(), i64> {
    if name.len() > 64 {
        return Err(-22);
    }
    let mut b = [0u8; 64];
    b[..name.len()].copy_from_slice(name);
    let c = &crate::proc::current().cred;
    match &c.ns.uts {
        Some(u) => *u.name.borrow_mut() = (b, name.len()),
        None => {
            if check && c.euid != 0 {
                return Err(-EPERM);
            }
            unsafe { *(&raw mut HOSTNAME) = (b, name.len()) };
        }
    }
    Ok(())
}

// ---- PID の番号の読みかえ ----

fn same(a: &Option<Rc<Pid>>, b: &Rc<Pid>) -> bool {
    a.as_ref().is_some_and(|x| Rc::ptr_eq(x, b))
}

/// 本当の番号 g のプロセス (スレッド) は、いまのプロセスから見ると何番か (見えなければ None)
pub fn to_local(g: u32) -> Option<u32> {
    let Some(v) = crate::proc::current().pid_ns.clone() else { return Some(g) };
    let t = crate::proc::find_any(g)?;
    same(&t.pid_ns, &v).then_some(t.vpid)
}

/// 見えなければ 0 (getppid など)
pub fn local_or_0(g: u32) -> u32 {
    to_local(g).unwrap_or(0)
}

/// いまのプロセスから見た番号 l の、本当の番号 (なければ None)
pub fn to_global(l: u32) -> Option<u32> {
    let Some(v) = crate::proc::current().pid_ns.clone() else { return Some(l) };
    crate::proc::find_where(|p| same(&p.pid_ns, &v) && p.vpid == l).map(|p| p.pid)
}

/// システムコールの引数の pid (0 と -1 はそのまま、負は pgid)。見えなければ ESRCH
pub fn arg_pid(pid: i64) -> Result<i64, i64> {
    const ESRCH: i64 = 3;
    match pid {
        0 | -1 => Ok(pid),
        p if p > 0 => to_global(p as u32).map(|g| g as i64).ok_or(-ESRCH),
        p => to_global((-p) as u32).map(|g| -(g as i64)).ok_or(-ESRCH),
    }
}

/// 中の 1 番が終わる: 中のほかのプロセスを終わらせる
pub fn init_exited(p: &crate::proc::Proc) {
    let Some(v) = p.pid_ns.clone() else { return };
    if p.vpid != 1 || p.pid != p.tgid {
        return;
    }
    for tgid in crate::proc::all_leaders() {
        if tgid != p.tgid && crate::proc::find_leader(tgid).is_some_and(|q| same(&q.pid_ns, &v)) {
            crate::proc::kill_group(tgid, 9);
        }
    }
}

// ---- ユーザーの namespace ----
// 中では、外の uid / gid を地図 (uid_map / gid_map) で読みかえて見せる (地図にない番号は 65534)。
// カーネルの中の資格情報 (cred) はいつも外の本当の番号のままなので、ファイルのパーミッションは外の番号で
// 判定する: 中の root (地図で 0 に見える人) でも、外の自分にできないことはできない。中の root にできるのは、
// その中だけのこと: ほかの namespace を作る (no_new_privs なしで)、その中で作ったマウントの namespace でのマウント。
// setuid / setgid のプログラムは、中では効かない (no_new_privs と同じ)。
// 地図は /proc/PID/uid_map と gid_map に一度だけ書く。root でない人は、作った人の uid (gid) 1 つだけを、
// gid_map は先に /proc/PID/setgroups に deny を書いてから (Linux と同じ)。入れ子はない

/// 地図にない番号 (Linux の overflowuid)
pub const OVERFLOW: u32 = 65534;

pub struct User {
    pub id: u64,
    /// 作った人 (外の euid / egid)
    pub owner: u32,
    pub owner_gid: u32,
    pub maps: RefCell<Maps>,
}

/// (中の番号, 外の番号, いくつ) の並び。None はまだ書いていない
#[derive(Default)]
pub struct Maps {
    pub uid: Option<Vec<(u32, u32, u32)>>,
    pub gid: Option<Vec<(u32, u32, u32)>>,
    pub setgroups_deny: bool,
}

fn inside(map: &Option<Vec<(u32, u32, u32)>>, out: u32) -> u32 {
    map.iter().flatten().find(|&&(_, o, n)| out >= o && out - o < n).map_or(OVERFLOW, |&(i, o, _)| i + (out - o))
}

fn outside(map: &Option<Vec<(u32, u32, u32)>>, ins: u32) -> Option<u32> {
    map.iter().flatten().find(|&&(i, _, n)| ins >= i && ins - i < n).map(|&(i, o, _)| o + (ins - i))
}

/// いまのプロセスから見た uid (外の番号 u を中の番号に)
pub fn show_uid(u: u32) -> u32 {
    match &crate::proc::current_cred_ref().ns.user {
        Some(n) => inside(&n.maps.borrow().uid, u),
        None => u,
    }
}

pub fn show_gid(g: u32) -> u32 {
    match &crate::proc::current_cred_ref().ns.user {
        Some(n) => inside(&n.maps.borrow().gid, g),
        None => g,
    }
}

/// システムコールの引数の uid (中の番号) を外の番号に。-1 (変えない) はそのまま、地図になければ EINVAL
pub fn take_uid(u: u32) -> Result<u32, i64> {
    match &crate::proc::current_cred_ref().ns.user {
        Some(n) if u != u32::MAX => outside(&n.maps.borrow().uid, u).ok_or(-EINVAL),
        _ => Ok(u),
    }
}

pub fn take_gid(g: u32) -> Result<u32, i64> {
    match &crate::proc::current_cred_ref().ns.user {
        Some(n) if g != u32::MAX => outside(&n.maps.borrow().gid, g).ok_or(-EINVAL),
        _ => Ok(g),
    }
}

/// /proc/PID/uid_map と gid_map の中身 (はじめの namespace は 0 0 4294967295)
pub fn map_text(u: &Option<Rc<User>>, gid: bool) -> alloc::string::String {
    let Some(u) = u else { return "         0          0 4294967295\n".into() };
    let m = u.maps.borrow();
    let map = if gid { &m.gid } else { &m.uid };
    map.iter().flatten().map(|(i, o, n)| alloc::format!("{:>10} {:>10} {:>10}\n", i, o, n)).collect()
}

/// /proc/PID/uid_map (gid_map) に書く。u は書かれるプロセスの namespace
pub fn write_map(u: &Option<Rc<User>>, gid: bool, text: &[u8]) -> Result<(), i64> {
    let u = u.as_ref().ok_or(-EPERM)?;
    let me = crate::proc::current_cred_ref();
    // 書けるのは、作った人 (外にいるか、同じ namespace の中) か、外の root
    let root = me.euid == 0 && me.ns.user.is_none();
    if !root && (me.euid != u.owner || me.ns.user.as_ref().is_some_and(|n| !Rc::ptr_eq(n, u))) {
        return Err(-EPERM);
    }
    let text = core::str::from_utf8(text).map_err(|_| -EINVAL)?;
    let mut lines = Vec::new();
    for l in text.lines().filter(|l| !l.trim().is_empty()) {
        let f: Vec<u32> = l.split_whitespace().map(|x| x.parse().map_err(|_| -EINVAL)).collect::<Result<_, i64>>()?;
        if f.len() != 3 || f[2] == 0 || f[0].checked_add(f[2]).is_none() || f[1].checked_add(f[2]).is_none() {
            return Err(-EINVAL);
        }
        lines.push((f[0], f[1], f[2]));
    }
    if lines.is_empty() || lines.len() > 5 {
        return Err(-EINVAL);
    }
    let mut m = u.maps.borrow_mut();
    if !root {
        // root でない人は、自分の番号 1 つだけ
        let own = if gid { u.owner_gid } else { u.owner };
        if lines.len() != 1 || lines[0].1 != own || lines[0].2 != 1 || (gid && !m.setgroups_deny) {
            return Err(-EPERM);
        }
    }
    let slot = if gid { &mut m.gid } else { &mut m.uid };
    if slot.is_some() {
        return Err(-EPERM);
    }
    *slot = Some(lines);
    Ok(())
}

/// /proc/PID/setgroups: "allow" か "deny" (gid_map を書く前だけ変えられる)
pub fn setgroups_text(u: &Option<Rc<User>>) -> &'static str {
    match u {
        Some(u) if u.maps.borrow().setgroups_deny => "deny\n",
        _ => "allow\n",
    }
}

pub fn write_setgroups(u: &Option<Rc<User>>, text: &[u8]) -> Result<(), i64> {
    let u = u.as_ref().ok_or(-EPERM)?;
    let me = crate::proc::current_cred_ref();
    if !(me.euid == 0 && me.ns.user.is_none()) && me.euid != u.owner {
        return Err(-EPERM);
    }
    let mut m = u.maps.borrow_mut();
    if m.gid.is_some() {
        return Err(-EPERM);
    }
    match core::str::from_utf8(text).map(|t| t.trim()) {
        Ok("deny") => m.setgroups_deny = true,
        Ok("allow") if !m.setgroups_deny => {}
        _ => return Err(-EINVAL),
    }
    Ok(())
}

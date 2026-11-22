// namespace: プロセスから見える世界を分ける (Linux と同じ CLONE_NEW* と unshare)
//   UTS  ホスト名 (uname の nodename、sethostname)
//   NET  ネットワーク: 中にあるのは lo だけ (Linux の新しい netns と同じ)。lo ははじめ DOWN で番地も道もなく
//        (127.0.0.1 へは ENETUNREACH、bind は EADDRNOTAVAIL)、SIOCSIFFLAGS か RTM_NEWLINK で UP にすると 127.0.0.1/8 と
//        ::1 ができる (DOWN にもどすと ::1 は消え、127.0.0.1 は残る)。netlink、ioctl、/proc/net に見えるのもそれだけ。
//        TCP と UDP は中どうしでだけ。外へはどこへも送れない (ENETUNREACH)。抽象名前空間の unix ソケット (@...) も分かれる
//   PID  プロセスの番号: 中では 1 から数え、中のプロセスだけが見える (getpid、kill、wait、/proc ...)。
//        unshare(CLONE_NEWPID) はそのあとに作る子から (Linux と同じ)。中の 1 番が終わると中のみなが終わる。
//        入れ子 (中でさらに作る) はできない (EINVAL)。外 (はじめの namespace) からは、みな本当の番号で見える
// clone(CLONE_NEW*) か unshare(CLONE_NEW*) で新しいものを作る。fork と exec で引き継ぐ (cred の中に持つ)
// 作るには root か no_new_privs が要る (aios には user namespace がないので、Linux のように root 以外を
// 止めるのではなく、できることが減るだけの namespace は、setuid のプログラムをだませない no_new_privs で許す)。
// 新しい UTS の中ではホスト名を変えられる (外には見えないので、root でなくても)
// /proc/PID/ns/uts は「uts:[番号]」へのリンク (同じものか見分けるため)。開くと nsfs の口 (NsInode: その namespace を
// 持っておく) になり、setns でそこへ入れる (下の「setns と nsfs」)
use crate::cell::Bkl;
use alloc::rc::Rc;
use alloc::vec::Vec;
use core::cell::RefCell;

pub const CLONE_NEWUTS: u64 = 0x0400_0000;
pub const CLONE_NEWNET: u64 = 0x4000_0000;
pub const CLONE_NEWPID: u64 = 0x2000_0000;
pub const CLONE_NEWNS: u64 = 0x0002_0000;
pub const CLONE_NEWUSER: u64 = 0x1000_0000;
pub const CLONE_NEWIPC: u64 = 0x0800_0000;

const EPERM: i64 = 1;

/// はじめからある namespace の番号 (Linux と同じ)
pub const INIT_UTS: u64 = 4026531838;
pub const INIT_NET: u64 = 4026531840;
pub const INIT_PID: u64 = 4026531836;
pub const INIT_MNT: u64 = 4026531841;
pub const INIT_USER: u64 = 4026531837;
pub const INIT_IPC: u64 = 4026531839;

static NEXT_ID: Bkl<u64> = Bkl::new(4026532000);

fn next_id() -> u64 {
    *NEXT_ID.get() += 1;
    *NEXT_ID.get()
}

/// UTS: ホスト名と、uname の形 (Uname: aios か linux)
pub struct Uts {
    pub id: u64,
    /// 持ち主のユーザーの namespace の番号 (作ったときのもの。はじめのものは INIT_USER: Linux の ns->user_ns)
    pub owner: u64,
    pub name: RefCell<([u8; 64], usize)>,
    /// NIS のドメイン名 (uname の domainname、setdomainname)
    pub domain: RefCell<([u8; 64], usize)>,
    pub uname: core::cell::Cell<Uname>,
}

/// IPC: 番号だけ (System V の共有メモリ (shm.rs) の表は番号ごと。なくなったら、そこのかたまりも消える)
pub struct Ipc {
    pub id: u64,
    pub owner: u64,
}

/// NET: 番号 (中のソケットは番号で分ける) と lo の様子 (lo_state)
pub struct Net {
    pub id: u64,
    pub owner: u64,
}

impl Drop for Net {
    fn drop(&mut self) {
        LO.get().remove(&self.id);
    }
}

/// 分けた NET の lo: New ははじめ (DOWN、番地なし)、Up、Down は UP のあと DOWN (127.0.0.1/8 と local の道は残る)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lo {
    New,
    Up,
    Down,
}

impl Lo {
    /// 127.0.0.1/8 があるか (Linux の inetdev は DOWN にしても番地を消さない)
    pub fn has_v4(self) -> bool {
        self != Lo::New
    }
}

static LO: Bkl<alloc::collections::BTreeMap<u64, Lo>> = Bkl::new(alloc::collections::BTreeMap::new());

/// NET の番号 netns (0 ははじめのもの: lo はいつも UP) の lo の様子
pub fn lo_state(netns: u64) -> Lo {
    if netns == 0 { Lo::Up } else { LO.get().get(&netns).copied().unwrap_or(Lo::New) }
}

/// いまのプロセスが分けた NET の中なら、その lo の様子
pub fn net_lo() -> Option<Lo> {
    net().map(lo_state)
}

/// 分けた NET の中の口を変えてよいか (Linux の ns_capable(net->user_ns, CAP_NET_ADMIN)。aios は、その NET を作れた人:
/// ユーザーの namespace の中か no_new_privs。LSM は聞く)。はじめの NET は root (CAP_NET_ADMIN)
pub fn net_admin() -> bool {
    let c = &crate::proc::current().cred;
    match &c.ns.net {
        Some(_) => (c.ns.user.is_some() || c.no_new_privs) && crate::security::capable(c, crate::cred::CAP_NET_ADMIN, true),
        None => c.capable(crate::cred::CAP_NET_ADMIN),
    }
}

/// いまのプロセスの NET の lo を UP / DOWN に (分けた NET の中だけ。はじめの NET の lo は変えられない)
pub fn set_lo(up: bool) -> Result<(), i64> {
    let id = net().ok_or(-95)?; // EOPNOTSUPP
    if !net_admin() {
        return Err(-EPERM);
    }
    let now = lo_state(id);
    let next = match (up, now) {
        (true, _) => Lo::Up,
        (false, Lo::New) => Lo::New,
        (false, _) => Lo::Down,
    };
    LO.get().insert(id, next);
    Ok(())
}

/// PID: 中の番号を配る
pub struct Pid {
    pub id: u64,
    pub owner: u64,
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
    pub ipc: Option<Rc<Ipc>>,
}

impl Ns {
    pub const INIT: Ns = Ns { uts: None, net: None, pid: None, mnt: None, user: None, ipc: None };

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

    pub fn ipc_id(&self) -> u64 {
        self.ipc.as_ref().map_or(INIT_IPC, |n| n.id)
    }
}

/// いまのプロセスの IPC の namespace (番号と、なくなったかを見るための弱い参照。はじめからあるものは None)
pub fn ipc() -> (u64, Option<alloc::rc::Weak<Ipc>>) {
    let n = &crate::proc::current().cred.ns;
    (n.ipc_id(), n.ipc.as_ref().map(Rc::downgrade))
}

/// いまのプロセスが分けた NET の中なら、その番号
pub fn net() -> Option<u64> {
    crate::proc::current().cred.ns.net.as_ref().map(|n| n.id)
}

/// flags の CLONE_NEW* のうち、扱えるもの
pub const SUPPORTED: u64 = CLONE_NEWUTS | CLONE_NEWNET | CLONE_NEWPID | CLONE_NEWNS | CLONE_NEWUSER | CLONE_NEWIPC;

/// cred の namespace を flags のぶん新しくする (clone の子と unshare)
pub fn renew(c: &mut crate::cred::Cred, flags: u64) -> Result<(), i64> {
    if flags & SUPPORTED == 0 {
        return Ok(());
    }
    // namespace を切っていれば (aios.off=ns)、入れずに作った Linux と同じ EINVAL
    if crate::parts::off(crate::parts::Part::Ns) {
        return Err(crate::why::inval());
    }
    // ユーザーの namespace はだれでも作れる (先に作る。中ではほかの namespace も作れる)。chroot の中と入れ子はだめ
    // (chroot が先: Linux の create_user_ns は入れ子を許すので、chroot の中なら EPERM)
    if flags & CLONE_NEWUSER != 0 {
        if !crate::proc::current_root().is_empty() {
            return Err(-EPERM);
        }
        if c.ns.user.is_some() {
            return Err(crate::why::inval());
        }
        // LSM (landlock の NAMESPACE_USE: Linux の security_namespace_init)
        crate::security::namespace_init(c, CLONE_NEWUSER)?;
        let u = Rc::new(User { id: next_id(), owner: c.euid, owner_gid: c.egid, maps: RefCell::new(Maps::default()) });
        USERS.get().insert(u.id, Rc::downgrade(&u));
        c.ns.user = Some(u);
    }
    if flags & !CLONE_NEWUSER & SUPPORTED != 0 {
        // Linux の ns_capable(CAP_SYS_ADMIN): capability と LSM みな (landlock の CAPABILITY_USE、AppArmor の capability)。
        // ユーザーの namespace の中 (CAP_SYS_ADMIN を持つ) と no_new_privs のときも、LSM は聞く
        if c.ns.user.is_none() && !c.no_new_privs {
            if !c.capable(crate::cred::CAP_SYS_ADMIN) {
                return Err(-EPERM);
            }
        } else if !crate::security::capable(c, crate::cred::CAP_SYS_ADMIN, true) {
            return Err(-EPERM);
        }
        crate::security::namespace_init(c, flags & !CLONE_NEWUSER)?;
    }
    // 持ち主は、いっしょに作ったユーザーの namespace (あれば)。作る順 (番号の順: listns の並び) は Linux の
    // create_new_namespaces と同じ mnt、uts、ipc、pid、net
    let owner = c.ns.user_id();
    if flags & CLONE_NEWNS != 0 {
        c.ns.mnt = Some(crate::vfs::new_mnt_ns(next_id(), c.ns.user_id()));
    }
    if flags & CLONE_NEWUTS != 0 {
        let name = current_name(c);
        let domain = current_domain(c);
        c.ns.uts = Some(Rc::new(Uts { id: next_id(), owner, name: RefCell::new(name), domain: RefCell::new(domain), uname: core::cell::Cell::new(current_uname(c)) }));
    }
    if flags & CLONE_NEWIPC != 0 {
        c.ns.ipc = Some(Rc::new(Ipc { id: next_id(), owner }));
    }
    if flags & CLONE_NEWPID != 0 {
        // 入れ子はできない
        if c.ns.pid.is_some() || crate::proc::current().pid_ns.is_some() {
            return Err(crate::why::inval());
        }
        c.ns.pid = Some(Rc::new(Pid { id: next_id(), owner, next: core::cell::Cell::new(0) }));
    }
    if flags & CLONE_NEWNET != 0 {
        c.ns.net = Some(Rc::new(Net { id: next_id(), owner }));
    }
    Ok(())
}

// ---- ホスト名 ----

static HOSTNAME: Bkl<([u8; 64], usize)> = Bkl::new({
    let mut b = [0u8; 64];
    b[0] = b'a';
    b[1] = b'i';
    b[2] = b'o';
    b[3] = b's';
    (b, 4)
});

fn current_name(c: &crate::cred::Cred) -> ([u8; 64], usize) {
    match &c.ns.uts {
        Some(u) => *u.name.borrow(),
        None => *HOSTNAME.get(),
    }
}

/// いまのプロセスから見たホスト名
pub fn hostname() -> ([u8; 64], usize) {
    current_name(&crate::proc::current().cred)
}

/// ドメイン名 (Linux の UTS_DOMAINNAME の既定は "(none)")
static DOMAIN: Bkl<([u8; 64], usize)> = Bkl::new({
    let mut b = [0u8; 64];
    let s = b"(none)";
    let mut i = 0;
    while i < s.len() {
        b[i] = s[i];
        i += 1;
    }
    (b, s.len())
});

fn current_domain(c: &crate::cred::Cred) -> ([u8; 64], usize) {
    match &c.ns.uts {
        Some(u) => *u.domain.borrow(),
        None => *DOMAIN.get(),
    }
}

/// いまのプロセスから見たドメイン名
pub fn domainname() -> ([u8; 64], usize) {
    current_domain(&crate::proc::current().cred)
}

/// sethostname と setdomainname がしてよいか (Linux の ns_capable(uts_ns->user_ns, CAP_SYS_ADMIN))。はじめの UTS は
/// root だけ、新しく作った UTS の中ならだれでも。自分の UTS でも LSM はみな聞く (landlock の CAPABILITY_USE、AppArmor)
pub fn uts_may_set() -> bool {
    let c = &crate::proc::current().cred;
    match &c.ns.uts {
        Some(_) => crate::security::capable(c, crate::cred::CAP_SYS_ADMIN, true),
        None => c.capable(crate::cred::CAP_SYS_ADMIN),
    }
}

/// ホスト名 (domain なら ドメイン名) を変える。check なら uts_may_set を見る (sysctl の書き手は呼ぶほうで見る)
pub fn set_uts_name(name: &[u8], domain: bool, check: bool) -> Result<(), i64> {
    if check && !uts_may_set() {
        return Err(-EPERM);
    }
    if name.len() > 64 {
        return Err(-22);
    }
    let mut b = [0u8; 64];
    b[..name.len()].copy_from_slice(name);
    let c = &crate::proc::current().cred;
    match (&c.ns.uts, domain) {
        (Some(u), false) => *u.name.borrow_mut() = (b, name.len()),
        (Some(u), true) => *u.domain.borrow_mut() = (b, name.len()),
        (None, false) => *HOSTNAME.get() = (b, name.len()),
        (None, true) => *DOMAIN.get() = (b, name.len()),
    }
    Ok(())
}

/// ホスト名を変える
pub fn set_hostname(name: &[u8], check: bool) -> Result<(), i64> {
    set_uts_name(name, false, check)
}

// ---- uname の形 ----

/// aios が手本にしている Linux の版 (test/linux-vm.sh の mainline、7.4)。linux の形の uname の release の頭と、vDSO の
/// 「Linux」の注釈 (glibc が起きるときに読む) に出す。手本を上げたらここも
pub const LINUX_VERSION: (u32, u32, u32) = (7, 4, 0);

/// uname (と /proc/sys/kernel/ostype、osrelease、/proc/version) の形。UTS の namespace ごとに持ち、新しい UTS は親のものを
/// 受けつぐ。/proc/sys/kernel/uname で切りかえる (書けるのはホスト名と同じ人)。
///   Aios (既定): sysname は aios、release は aios の版 (AIOS_RELEASE)
///   Linux: sysname は Linux、release は 7.4.0-aios-AIOS_RELEASE (手本の Linux の版が頭)。Linux のためのプログラムが
///     Linux だと思って動くように: glibc を静的につないだもの (公式の protoc など) は release の版を見て、3.7.0 より古いと
///     「FATAL: kernel too old」で止まる。config.guess などは uname -s で Linux を見る。aibox --image は中をこれにする
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Uname {
    Aios,
    Linux,
}

static UNAME: Bkl<Uname> = Bkl::new(Uname::Aios);

fn current_uname(c: &crate::cred::Cred) -> Uname {
    match &c.ns.uts {
        Some(u) => u.uname.get(),
        None => *UNAME.get(),
    }
}

/// いまのプロセスから見た uname の形
pub fn uname() -> Uname {
    current_uname(&crate::proc::current().cred)
}

/// uname の形を変える (いまのプロセスの UTS の)。許しは呼ぶほう (sysctl の書き手) で見る (ホスト名と同じ)
pub fn set_uname(u: Uname) {
    match &crate::proc::current().cred.ns.uts {
        Some(x) => x.uname.set(u),
        None => *UNAME.get() = u,
    }
}

/// いまのプロセスが自分の UTS の namespace の中で、それを変えてよいか (set_hostname の自分の UTS と同じ: CAP_SYS_ADMIN を
/// 持つものとして、LSM に聞く)
pub fn uts_admin() -> bool {
    let c = &crate::proc::current().cred;
    c.ns.uts.is_some() && crate::security::capable(c, crate::cred::CAP_SYS_ADMIN, true)
}

/// UTS の namespace ごとの /proc/sys の値 (kernel/uname) の持ち主に見せるもの: 自分の UTS の中なら、いまのユーザーの
/// namespace の root (の外の番号。地図になければ 0)、ほかは root
pub fn uts_owner() -> (u32, u32) {
    if crate::proc::current_cred_ref().ns.uts.is_none() {
        return (0, 0);
    }
    (take_uid(0).unwrap_or(0), take_gid(0).unwrap_or(0))
}

/// uname の sysname (/proc/sys/kernel/ostype)
pub fn sysname() -> &'static str {
    match uname() {
        Uname::Aios => "aios",
        Uname::Linux => "Linux",
    }
}

/// uname の release (/proc/sys/kernel/osrelease)
pub fn release() -> alloc::string::String {
    match uname() {
        Uname::Aios => env!("AIOS_RELEASE").into(),
        Uname::Linux => {
            let (a, b, c) = LINUX_VERSION;
            alloc::format!("{}.{}.{}-aios-{}", a, b, c, env!("AIOS_RELEASE"))
        }
    }
}

/// uname の version
pub fn version() -> &'static str {
    match uname() {
        Uname::Aios => "#1 aios",
        Uname::Linux => "#1 SMP aios",
    }
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

/// 親 (いまのプロセス) から見た、作ったばかりの子の番号 (まだ走らせていないので to_local では見つからない)
pub fn child_seen(child: &crate::proc::Proc) -> u32 {
    match crate::proc::current().pid_ns.clone() {
        Some(v) if same(&child.pid_ns, &v) => child.vpid,
        _ => child.pid,
    }
}

/// 見えなければ 0 (getppid など)
pub fn local_or_0(g: u32) -> u32 {
    to_local(g).unwrap_or(0)
}

/// いまのプロセスから見た番号 l の、本当の番号 (なければ None)。終わって回収されていないもの (ゾンビ) も: Linux の
/// struct pid は回収されるまで残る (waitid(WNOWAIT) で待ったあとの wait4(pid)、kill(ゾンビ, 0)、/proc/PID)。
/// 前はゾンビを見ず、Go の os/exec (pidfd がないとき: waitid(P_PID, WNOWAIT) → wait4(pid)) が PID の namespace の中で
/// ECHILD になった (aibox --image の gitea が git を待てなかった)。中の番号は使いまわさないので、同じ番号はひとつだけ
pub fn to_global(l: u32) -> Option<u32> {
    let Some(v) = crate::proc::current().pid_ns.clone() else { return Some(l) };
    crate::proc::live().find(|p| p.state != crate::proc::State::Unused && same(&p.pid_ns, &v) && p.vpid == l).map(|p| p.pid)
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

/// ユーザーの namespace を番号から (ns_capable が、持ち主の namespace を作った人を見る)
static USERS: Bkl<alloc::collections::BTreeMap<u64, alloc::rc::Weak<User>>> = Bkl::new(alloc::collections::BTreeMap::new());

impl Drop for User {
    fn drop(&mut self) {
        USERS.get().remove(&self.id);
    }
}

/// (中の番号, 外の番号, いくつ) の並び。None はまだ書いていない
#[derive(Default)]
pub struct Maps {
    pub uid: Option<Vec<(u32, u32, u32)>>,
    pub gid: Option<Vec<(u32, u32, u32)>>,
    /// projid_map (ディスクの割りあての project id: aios は使わない。覚えて見せるだけ)
    pub projid: Option<Vec<(u32, u32, u32)>>,
    pub setgroups_deny: bool,
}

// [lockless]
fn inside(map: &Option<Vec<(u32, u32, u32)>>, out: u32) -> u32 {
    map.iter().flatten().find(|&&(_, o, n)| out >= o && out - o < n).map_or(OVERFLOW, |&(i, o, _)| i + (out - o))
}

fn outside(map: &Option<Vec<(u32, u32, u32)>>, ins: u32) -> Option<u32> {
    map.iter().flatten().find(|&&(i, _, n)| ins >= i && ins - i < n).map(|&(i, o, _)| o + (ins - i))
}

/// いまのプロセスから見た uid (外の番号 u を中の番号に)
// [lockless]
pub fn show_uid(u: u32) -> u32 {
    match &crate::proc::current_cred_ref().ns.user {
        Some(n) => inside(&n.maps.borrow().uid, u),
        None => u,
    }
}

/// show_uid と同じだが、地図にないものは -1 (from_kuid: /proc/PID/loginuid)
pub fn show_uid_raw(u: u32) -> u32 {
    match &crate::proc::current_cred_ref().ns.user {
        Some(n) if u != u32::MAX => n.maps.borrow().uid.iter().flatten().find(|&&(_, o, c)| u >= o && u - o < c).map_or(u32::MAX, |&(i, o, _)| i + (u - o)),
        _ => u,
    }
}

// [lockless]
pub fn show_gid(g: u32) -> u32 {
    match &crate::proc::current_cred_ref().ns.user {
        Some(n) => inside(&n.maps.borrow().gid, g),
        None => g,
    }
}

/// システムコールの引数の uid (中の番号) を外の番号に。-1 (変えない) はそのまま、地図になければ EINVAL
pub fn take_uid(u: u32) -> Result<u32, i64> {
    match &crate::proc::current_cred_ref().ns.user {
        Some(n) if u != u32::MAX => outside(&n.maps.borrow().uid, u).ok_or_else(|| crate::why::inval()),
        _ => Ok(u),
    }
}

pub fn take_gid(g: u32) -> Result<u32, i64> {
    match &crate::proc::current_cred_ref().ns.user {
        Some(n) if g != u32::MAX => outside(&n.maps.borrow().gid, g).ok_or_else(|| crate::why::inval()),
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
    // 書けるのは、作った人 (外にいるか、同じ namespace の中) か、外で CAP_SETUID (gid_map は CAP_SETGID) を持つ人
    let own = me.euid == u.owner && !me.ns.user.as_ref().is_some_and(|n| !Rc::ptr_eq(n, u));
    let root = || me.ns.user.is_none() && me.capable(if gid { crate::cred::CAP_SETGID } else { crate::cred::CAP_SETUID });
    if !own && !root() {
        return Err(-EPERM);
    }
    let text = core::str::from_utf8(text).map_err(|_| crate::why::inval())?;
    let mut lines = Vec::new();
    for l in text.lines().filter(|l| !l.trim().is_empty()) {
        let f: Vec<u32> = l.split_whitespace().map(|x| x.parse().map_err(|_| crate::why::inval())).collect::<Result<_, i64>>()?;
        if f.len() != 3 || f[2] == 0 || f[0].checked_add(f[2]).is_none() || f[1].checked_add(f[2]).is_none() {
            return Err(crate::why::inval());
        }
        lines.push((f[0], f[1], f[2]));
    }
    if lines.is_empty() || lines.len() > 5 {
        return Err(crate::why::inval());
    }
    let mut m = u.maps.borrow_mut();
    // 自分の番号 1 つだけなら、作った人はケーパビリティなしで (Linux の new_idmap_permitted)
    let id = if gid { u.owner_gid } else { u.owner };
    let simple = own && lines.len() == 1 && lines[0].1 == id && lines[0].2 == 1 && (!gid || m.setgroups_deny);
    if !simple && !root() {
        return Err(-EPERM);
    }
    let slot = if gid { &mut m.gid } else { &mut m.uid };
    if slot.is_some() {
        return Err(-EPERM);
    }
    *slot = Some(lines);
    Ok(())
}

/// /proc/PID/projid_map: init のものは 0 0 4294967295、作った namespace のものは書くまで空
pub fn projid_text(u: &Option<Rc<User>>) -> alloc::string::String {
    let Some(u) = u else { return "         0          0 4294967295\n".into() };
    u.maps.borrow().projid.iter().flatten().map(|(i, o, n)| alloc::format!("{:>10} {:>10} {:>10}\n", i, o, n)).collect()
}

/// projid_map を書く (Linux の proc_projid_map_write と map_write: ケーパビリティは要らない。書く人はその namespace か
/// 親の中。1 度だけ (init のものはもうあるので EPERM))
pub fn write_projid(u: &Option<Rc<User>>, text: &[u8]) -> Result<(), i64> {
    let u = u.as_ref().ok_or(-EPERM)?;
    let me = crate::proc::current_cred_ref();
    if !me.ns.user.as_ref().is_none_or(|n| Rc::ptr_eq(n, u)) {
        return Err(-EPERM);
    }
    let text = core::str::from_utf8(text).map_err(|_| crate::why::inval())?;
    let mut lines = Vec::new();
    for l in text.lines().filter(|l| !l.trim().is_empty()) {
        let f: Vec<u32> = l.split_whitespace().map(|x| x.parse().map_err(|_| crate::why::inval())).collect::<Result<_, i64>>()?;
        if f.len() != 3 || f[2] == 0 || f[0].checked_add(f[2]).is_none() || f[1].checked_add(f[2]).is_none() {
            return Err(crate::why::inval());
        }
        lines.push((f[0], f[1], f[2]));
    }
    if lines.is_empty() || lines.len() > 5 {
        return Err(crate::why::inval());
    }
    let mut m = u.maps.borrow_mut();
    if m.projid.is_some() {
        return Err(-EPERM);
    }
    m.projid = Some(lines);
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
    if me.euid != u.owner && !(me.ns.user.is_none() && me.capable(crate::cred::CAP_SYS_ADMIN)) {
        return Err(-EPERM);
    }
    let mut m = u.maps.borrow_mut();
    if m.gid.is_some() {
        return Err(-EPERM);
    }
    match core::str::from_utf8(text).map(|t| t.trim()) {
        Ok("deny") => m.setgroups_deny = true,
        Ok("allow") if !m.setgroups_deny => {}
        _ => return Err(crate::why::inval()),
    }
    Ok(())
}

// ---- setns と nsfs (Linux の kernel/nsproxy.c と fs/nsfs.c) ----

/// namespace そのもの (None ははじめからあるもの)。nsfs の口が持ち、setns がそれへ入れる
#[derive(Clone)]
pub enum Which {
    Uts(Option<Rc<Uts>>),
    Ipc(Option<Rc<Ipc>>),
    Net(Option<Rc<Net>>),
    Mnt(Option<Rc<crate::vfs::MountNs>>),
    Pid(Option<Rc<Pid>>),
    User(Option<Rc<User>>),
}

impl Which {
    /// CLONE_NEW* (Linux の ns_type)
    pub fn flag(&self) -> u64 {
        match self {
            Which::Uts(_) => CLONE_NEWUTS,
            Which::Ipc(_) => CLONE_NEWIPC,
            Which::Net(_) => CLONE_NEWNET,
            Which::Mnt(_) => CLONE_NEWNS,
            Which::Pid(_) => CLONE_NEWPID,
            Which::User(_) => CLONE_NEWUSER,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Which::Uts(_) => "uts",
            Which::Ipc(_) => "ipc",
            Which::Net(_) => "net",
            Which::Mnt(_) => "mnt",
            Which::Pid(_) => "pid",
            Which::User(_) => "user",
        }
    }

    /// 番号 (/proc/PID/ns のリンクの [N] と、nsfs の inode の番号)
    pub fn id(&self) -> u64 {
        match self {
            Which::Uts(u) => u.as_ref().map_or(INIT_UTS, |u| u.id),
            Which::Ipc(i) => i.as_ref().map_or(INIT_IPC, |i| i.id),
            Which::Net(n) => n.as_ref().map_or(INIT_NET, |n| n.id),
            Which::Mnt(m) => m.as_ref().map_or(INIT_MNT, |m| m.id),
            Which::Pid(p) => p.as_ref().map_or(INIT_PID, |p| p.id),
            Which::User(u) => u.as_ref().map_or(INIT_USER, |u| u.id),
        }
    }

    /// 持ち主のユーザーの namespace の番号 (ユーザーの namespace はそれ自身: 入るには、その中の CAP_SYS_ADMIN)
    fn owner(&self) -> u64 {
        match self {
            Which::Uts(u) => u.as_ref().map_or(INIT_USER, |u| u.owner),
            Which::Ipc(i) => i.as_ref().map_or(INIT_USER, |i| i.owner),
            Which::Net(n) => n.as_ref().map_or(INIT_USER, |n| n.owner),
            Which::Mnt(m) => m.as_ref().map_or(INIT_USER, |m| m.owner),
            Which::Pid(p) => p.as_ref().map_or(INIT_USER, |p| p.owner),
            Which::User(_) => self.id(),
        }
    }
}

/// c が、番号 owner のユーザーの namespace の持ち物に cap の力を持つか (Linux の ns_capable と cap_capable)。ユーザーの
/// namespace の中からは、その namespace のものだけ (aios に入れ子はない。中ではみな持つ)。はじめのところからは、はじめの
/// ものは capability で、ユーザーの namespace のものは、それを作った人 (euid が同じ) か capability。LSM は聞く
pub fn ns_capable(c: &crate::cred::Cred, owner: u64, cap: u32) -> bool {
    match &c.ns.user {
        Some(u) => u.id == owner && crate::security::capable(c, cap, true),
        None if owner == INIT_USER => c.capable(cap),
        None => {
            let made = USERS.get().get(&owner).and_then(|w| w.upgrade()).is_some_and(|u| u.owner == c.euid);
            (made && crate::security::capable(c, cap, true)) || c.capable(cap)
        }
    }
}

/// nsfs の口 (/proc/PID/ns/X を開いたもの): namespace を持っておく (閉じるまでなくならない)。0444 のふつうのファイルで、
/// inode の番号が namespace の番号。読み書きはできない (EINVAL)
pub struct NsInode {
    pub which: Which,
}

static NSFS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

fn nsfs_id() -> usize {
    use core::sync::atomic::Ordering::Relaxed;
    match NSFS.load(Relaxed) {
        0 => {
            let id = crate::vfs::new_fs_id();
            NSFS.store(id, Relaxed);
            id
        }
        id => id,
    }
}

impl crate::vfs::Inode for NsInode {
    fn id(&self) -> (usize, u64) {
        (nsfs_id(), self.which.id())
    }

    fn meta(&self) -> crate::vfs::Meta {
        crate::vfs::Meta { ino: self.which.id(), mode: crate::vfs::S_IFREG | 0o444, nlink: 1, uid: 0, gid: 0, size: 0, rdev: 0, blocks: 0, mtime: 0, ctime: 0, atime: 0 }
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn read_at(&self, _off: usize, _buf: &mut [u8]) -> Result<usize, i64> {
        Err(crate::why::inval())
    }

    fn write_at(&self, _off: usize, _buf: &[u8]) -> Result<usize, i64> {
        Err(crate::why::inval())
    }

    fn truncate(&self, _len: usize) -> Result<(), i64> {
        Err(crate::why::inval())
    }

    fn readlink(&self) -> Result<alloc::string::String, i64> {
        Err(crate::why::inval())
    }

    fn lookup(&self, _name: &str) -> Result<crate::vfs::InodeRef, i64> {
        Err(-20)
    }

    fn readdir(&self) -> Result<Vec<crate::vfs::DirEntry>, i64> {
        Err(-20)
    }

    fn create(&self, _name: &str, _mode: u32, _node: crate::vfs::NewNode) -> Result<crate::vfs::InodeRef, i64> {
        Err(-20)
    }

    fn link(&self, _name: &str, _target: &crate::vfs::InodeRef) -> Result<(), i64> {
        Err(-20)
    }

    fn unlink(&self, _name: &str, _rmdir: bool) -> Result<(), i64> {
        Err(-20)
    }

    fn rename(&self, _old: &str, _newdir: &crate::vfs::InodeRef, _new: &str) -> Result<(), i64> {
        Err(-20)
    }

    fn set_mode(&self, _mode: u32) -> Result<(), i64> {
        Err(-EPERM)
    }

    fn set_owner(&self, _uid: Option<u32>, _gid: Option<u32>) -> Result<(), i64> {
        Err(-EPERM)
    }

    fn set_mtime(&self, _ns: u64) -> Result<(), i64> {
        Err(-EPERM)
    }

    /// NSFS_MAGIC
    fn statfs(&self) -> [u8; 120] {
        crate::tmpfs::statfs_bytes(0x6e73_6673, 4096, 0, 0, 0, 0)
    }
}

/// /proc/PID/ns/KIND を開いたもの: (名前 "uts:[N]", nsfs の口)。pid はそのプロセスのもの、pid_for_children はこれから
/// 作る子のもの
/// nsfs の口のマウントの namespace (statmount と listmount の mnt_ns_fd)。nsfs でないか、ほかの種類なら None (EINVAL)
pub fn mnt_ns_of(ino: &crate::vfs::InodeRef) -> Option<Rc<crate::vfs::MountNs>> {
    match &ino.as_any().downcast_ref::<NsInode>()?.which {
        Which::Mnt(m) => Some(m.clone().unwrap_or_else(crate::vfs::init_mnt_ns)),
        _ => None,
    }
}

pub fn ns_file(kind: &str, p: &crate::proc::Proc) -> (alloc::string::String, crate::vfs::InodeRef) {
    let n = &p.cred.ns;
    let w = match kind {
        "uts" => Which::Uts(n.uts.clone()),
        "ipc" => Which::Ipc(n.ipc.clone()),
        "net" => Which::Net(n.net.clone()),
        "mnt" => Which::Mnt(n.mnt.clone()),
        "pid" => Which::Pid(p.pid_ns.clone()),
        "pid_for_children" => Which::Pid(n.pid.clone()),
        _ => Which::User(n.user.clone()),
    };
    (alloc::format!("{}:[{}]", w.name(), w.id()), Rc::new(NsInode { which: w }))
}

/// ひとつの namespace へ入れる (Linux の ops->install)。c は入ったあとの資格情報 (pidfd でいくつも入れるときは、先に
/// 入れたユーザーの namespace の中から次のものを見る)。mnt なら true (cwd と root をその / に)
fn install(c: &mut crate::cred::Cred, w: &Which) -> Result<bool, i64> {
    use crate::cred::{CAP_SYS_ADMIN, CAP_SYS_CHROOT};
    let me = crate::proc::current();
    let mine = c.ns.user_id();
    // cwd と root (fs) をほかのプロセスと分けていない (Linux の fs->users == 1)
    let fs_private = me.files.as_ref().is_some_and(|f| f.private());
    if let Which::User(u) = w {
        // 同じところへもう一度入って力を得ることはできない。スレッドがいても、fs を分けていてもだめ
        if w.id() == mine || crate::proc::threads_of(me.tgid).len() > 1 || !fs_private {
            return Err(crate::why::inval());
        }
        if !ns_capable(c, w.id(), CAP_SYS_ADMIN) {
            return Err(-EPERM);
        }
        c.ns.user = u.clone();
        return Ok(false);
    }
    if !ns_capable(c, w.owner(), CAP_SYS_ADMIN) {
        return Err(-EPERM);
    }
    if matches!(w, Which::Mnt(_)) && !ns_capable(c, mine, CAP_SYS_CHROOT) {
        return Err(-EPERM);
    }
    if !ns_capable(c, mine, CAP_SYS_ADMIN) {
        return Err(-EPERM);
    }
    match w {
        Which::Uts(u) => c.ns.uts = u.clone(),
        Which::Ipc(i) => c.ns.ipc = i.clone(),
        Which::Net(n) => c.ns.net = n.clone(),
        Which::Mnt(m) => {
            if !fs_private {
                return Err(crate::why::inval());
            }
            c.ns.mnt = m.clone();
            return Ok(true);
        }
        Which::Pid(p) => {
            // いまの (自分の) PID の namespace か、その下のものだけ (aios に入れ子はないので、はじめのところからはどれでも、
            // 中からはそこだけ)
            let active = me.pid_ns.as_ref().map(|p| p.id);
            if active.is_some() && active != p.as_ref().map(|p| p.id) {
                return Err(crate::why::inval());
            }
            c.ns.pid = p.clone();
        }
        Which::User(_) => {}
    }
    Ok(false)
}

/// setns(fd, flags) (Linux の kernel/nsproxy.c): fd は nsfs の口 (flags は 0 かその種類: ちがえば EINVAL) か pidfd (flags は
/// 入る種類の組: 0 や知らないものは EINVAL。相手を ptrace で読めなければ EPERM、終わっていれば ESRCH。ユーザー、mnt、uts、
/// ipc、pid、net の順に入れる)。ほかの fd は EINVAL。みな入れられたときだけ変える (Linux の nsset)。mnt に入ると cwd と
/// root はその /
pub fn setns(fd: i64, flags: u64) -> Result<i64, i64> {
    const EBADF: i64 = 9;
    const ESRCH: i64 = 3;
    let me = crate::proc::current();
    let f = me.files().get(fd as u64).cloned().ok_or(-EBADF)?;
    let mut c = me.cred.clone();
    let mut mnt = false;
    let b = f.borrow();
    match &b.kind {
        crate::file::Kind::Inode(ino, _) if ino.as_any().is::<NsInode>() => {
            let w = &ino.as_any().downcast_ref::<NsInode>().unwrap().which;
            if flags != 0 && flags != w.flag() {
                return Err(crate::why::inval());
            }
            mnt = install(&mut c, w)?;
        }
        crate::file::Kind::PidFd(pid) => {
            if flags == 0 || flags & !SUPPORTED != 0 {
                return Err(crate::why::inval());
            }
            let t = crate::proc::find_any(*pid).ok_or(-ESRCH)?;
            // PTRACE_MODE_READ_REALCREDS (本当の uid と gid が相手の 3 つにみな同じで dumpable、でなければ CAP_SYS_PTRACE。
            // それから LSM)
            if t.tgid != me.tgid {
                let (mc, tc) = (&me.cred, &t.cred);
                let ids = [tc.uid, tc.euid, tc.suid].iter().all(|&u| u == mc.uid) && [tc.gid, tc.egid, tc.sgid].iter().all(|&g| g == mc.gid);
                let dump = t.mm.is_none() || crate::pidfd::dumpable(t) == 1;
                if !((ids && dump) || mc.capable(crate::cred::CAP_SYS_PTRACE)) || crate::security::ptrace_access_check(tc, crate::security::PTRACE_MODE_READ).is_err() {
                    return Err(-EPERM);
                }
            }
            if t.state == crate::proc::State::Zombie {
                return Err(-ESRCH);
            }
            let n = &t.cred.ns;
            let all = [
                Which::User(n.user.clone()),
                Which::Mnt(n.mnt.clone()),
                Which::Uts(n.uts.clone()),
                Which::Ipc(n.ipc.clone()),
                Which::Pid(t.pid_ns.clone()),
                Which::Net(n.net.clone()),
            ];
            for w in all.iter().filter(|w| flags & w.flag() != 0) {
                mnt |= install(&mut c, w)?;
            }
        }
        _ => return Err(crate::why::inval()),
    }
    drop(b);
    me.cred = c;
    me.sync_cred_work();
    if mnt {
        let files = me.files();
        files.set_root(alloc::string::String::new());
        files.set_cwd(alloc::string::String::new());
    }
    Ok(0)
}

// ---- listns と nsfs の ioctl (Linux 6.19、7.4 の形: kernel/nstree.c と fs/nsfs.c) ----

/// Linux の ns_id (listns、nsfs の NS_GET_ID、statmount の mnt_ns_id): はじめのものは Linux と同じ 1〜8 (ipc 1、uts 2、
/// user 3、pid 4、net 7、mnt 8。cgroup 5 と time 6 は aios にない)、新しいものは作った順 (aios の番号の順。種類によらず
/// 1 つの数) に 10 から (Linux の ns_tree_gen_id)
pub fn ns_id(inum: u64) -> u64 {
    match inum {
        INIT_IPC => 1,
        INIT_UTS => 2,
        INIT_USER => 3,
        INIT_PID => 4,
        INIT_NET => 7,
        INIT_MNT => 8,
        n => n.saturating_sub(4026532000) + 9,
    }
}

/// 使われている (Linux の active な) namespace: 番号 (inum) → (種類 CLONE_NEW*, 持ち主のユーザーの namespace の番号。
/// はじめのユーザーの namespace は 0)。はじめのものはいつも。プロセスが入っているもの (cred の ns、pid は入っているものと
/// 子のためのもの)、nsfs の口 (開いた fd) のもの、それらの持ち主のユーザーの namespace (Linux の __ns_ref_active_get が
/// 持ち主をたどる。aios のユーザーの namespace は入れ子にならないので、持ち主はいつもはじめのもの)
fn active() -> alloc::collections::BTreeMap<u64, (u64, u64)> {
    let mut m = alloc::collections::BTreeMap::new();
    for (id, kind) in [(INIT_IPC, CLONE_NEWIPC), (INIT_UTS, CLONE_NEWUTS), (INIT_PID, CLONE_NEWPID), (INIT_NET, CLONE_NEWNET), (INIT_MNT, CLONE_NEWNS)] {
        m.insert(id, (kind, INIT_USER));
    }
    m.insert(INIT_USER, (CLONE_NEWUSER, 0));
    let add = |w: &Which, m: &mut alloc::collections::BTreeMap<u64, (u64, u64)>| {
        let owner = match w {
            Which::User(u) => u.as_ref().map_or(0, |_| INIT_USER),
            _ => w.owner(),
        };
        m.insert(w.id(), (w.flag(), owner));
    };
    for p in crate::proc::live() {
        if p.state == crate::proc::State::Zombie {
            continue;
        }
        let n = &p.cred.ns;
        for w in [
            Which::Uts(n.uts.clone()),
            Which::Ipc(n.ipc.clone()),
            Which::Net(n.net.clone()),
            Which::Mnt(n.mnt.clone()),
            Which::Pid(n.pid.clone()),
            Which::Pid(p.pid_ns.clone()),
            Which::User(n.user.clone()),
        ] {
            add(&w, &mut m);
        }
    }
    let mut open = Vec::new();
    crate::proc::each_open_file(|f| {
        if let crate::file::Kind::Inode(ino, _) = &f.kind
            && let Some(n) = ino.as_any().downcast_ref::<NsInode>()
        {
            open.push(n.which.clone());
        }
    });
    for w in open {
        add(&w, &mut m);
    }
    // 持ち主のユーザーの namespace も (子の namespace が使われていれば)
    let owners: Vec<u64> = m.values().map(|&(_, o)| o).filter(|&o| o != 0 && o != INIT_USER).collect();
    for o in owners {
        if USERS.get().get(&o).and_then(|w| w.upgrade()).is_some() {
            m.insert(o, (CLONE_NEWUSER, INIT_USER));
        }
    }
    m
}

/// いまのプロセスの、その種類の namespace の番号 (Linux の is_current_namespace: pid は入っているもの)
fn current_of(kind: u64) -> u64 {
    let p = crate::proc::current();
    let n = &p.cred.ns;
    match kind {
        CLONE_NEWUTS => Which::Uts(n.uts.clone()).id(),
        CLONE_NEWIPC => Which::Ipc(n.ipc.clone()).id(),
        CLONE_NEWNET => Which::Net(n.net.clone()).id(),
        CLONE_NEWNS => Which::Mnt(n.mnt.clone()).id(),
        CLONE_NEWPID => Which::Pid(p.pid_ns.clone()).id(),
        _ => n.user_id(),
    }
}

/// みな見てよいか (Linux の may_see_all_namespaces: はじめの pid の namespace の中で、はじめのユーザーの namespace の
/// CAP_SYS_ADMIN)
fn may_see_all() -> bool {
    let p = crate::proc::current();
    p.pid_ns.is_none() && p.cred.ns.user.is_none() && p.cred.capable(crate::cred::CAP_SYS_ADMIN)
}

/// listns(req, ns_ids, nr_ns_ids, flags) (Linux の順: flags EINVAL、多すぎる EOVERFLOW、入れ物 EFAULT、ns_id_req (大きさ
/// EFAULT / E2BIG / EINVAL、知らないところ E2BIG、spare EINVAL、知らない種類 EOPNOTSUPP))。使われている namespace を番号の
/// 順に、ns_type でしぼって (1 つの種類なら、その種類の中で)、ns_id の次から (なければ ENOENT)、nr こまで。見えるのは
/// 自分のいまのものか、みな見てよい (may_see_all) とき。user_ns_id があれば、そのユーザーの namespace (LISTNS_CURRENT_USER
/// は自分の。なければ EINVAL) が持ち主のものだけ (みな見てよければみな)
pub fn listns(reqp: usize, idsp: usize, nr: usize, flags: u64) -> Result<i64, i64> {
    const EFAULT: i64 = 14;
    const E2BIG: i64 = 7;
    const ENOENT: i64 = 2;
    const EOVERFLOW: i64 = 75;
    const EOPNOTSUPP: i64 = 95;
    const CLONE_NEWCGROUP: u64 = 0x0200_0000;
    const CLONE_NEWTIME: u64 = 0x80;
    const NS_ALL: u64 = CLONE_NEWPID | CLONE_NEWUSER | CLONE_NEWNS | CLONE_NEWUTS | CLONE_NEWIPC | CLONE_NEWNET | CLONE_NEWCGROUP | CLONE_NEWTIME;
    const CURRENT_USER: u64 = u64::MAX;
    if flags != 0 {
        return Err(crate::why::inval());
    }
    if nr > 1_000_000 {
        return Err(-EOVERFLOW);
    }
    if idsp.checked_add(nr * 8).is_none_or(|e| e > 1 << 48) {
        return Err(-EFAULT);
    }
    let pt = crate::proc::current().pt();
    let mut sz = [0u8; 4];
    pt.copy_in(&mut sz, reqp).ok_or(-EFAULT)?;
    let size = u32::from_le_bytes(sz) as usize;
    if size > 4096 {
        return Err(-E2BIG);
    }
    if size < 32 {
        return Err(crate::why::inval());
    }
    let mut b = alloc::vec![0u8; size];
    crate::proc::current().pt().copy_in(&mut b, reqp).ok_or(-EFAULT)?;
    if b[32..].iter().any(|&x| x != 0) {
        return Err(-E2BIG);
    }
    let w64 = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
    let w32 = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap()) as u64;
    let (spare, last, ns_type, user) = (w32(4), w64(8), w32(16), w64(24));
    if spare != 0 {
        return Err(crate::why::inval());
    }
    if ns_type & !NS_ALL != 0 {
        return Err(-EOPNOTSUPP);
    }
    let act = active();
    // (ns_id, 種類, 持ち主, いまのものか)
    let mut all: Vec<(u64, u64, u64, bool)> = act.iter().map(|(&id, &(kind, owner))| (ns_id(id), kind, owner, current_of(kind) == id)).collect();
    all.sort_by_key(|e| e.0);
    let see_all = may_see_all();
    let requested = |k: u64| ns_type == 0 || ns_type & k != 0;
    let list: Vec<&(u64, u64, u64, bool)> = if user != 0 {
        let u = if user == CURRENT_USER {
            crate::proc::current().cred.ns.user_id()
        } else {
            act.iter().find(|&(&id, &(kind, _))| kind == CLONE_NEWUSER && ns_id(id) == user).map(|(&id, _)| id).ok_or_else(crate::why::inval)?
        };
        let owned: Vec<&(u64, u64, u64, bool)> = all.iter().filter(|e| e.2 == u).collect();
        let start = if last != 0 { owned.iter().position(|e| e.0 > last).ok_or(-ENOENT)? } else { 0 };
        owned[start..].iter().copied().filter(|e| requested(e.1) && (see_all || e.3)).collect()
    } else {
        let tree: Vec<&(u64, u64, u64, bool)> = if ns_type.count_ones() == 1 {
            // その種類の木 (aios にない cgroup と time は Linux の「木がない」と同じく EINVAL)
            if ns_type & (CLONE_NEWCGROUP | CLONE_NEWTIME) != 0 {
                return Err(crate::why::inval());
            }
            all.iter().filter(|e| e.1 == ns_type).collect()
        } else {
            all.iter().collect()
        };
        let start = if last != 0 { tree.iter().position(|e| e.0 > last).ok_or(-ENOENT)? } else { 0 };
        tree[start..].iter().copied().filter(|e| requested(e.1) && (see_all || e.3)).collect()
    };
    let mut out = Vec::new();
    for e in list.iter().take(nr) {
        out.extend_from_slice(&e.0.to_le_bytes());
    }
    if !out.is_empty() {
        crate::proc::current().pt().copy_out(idsp, &out).ok_or(-EFAULT)?;
    }
    Ok((out.len() / 8) as i64)
}

/// nsfs の口の ioctl (Linux の ns_ioctl): NS_GET_NSTYPE (種類 CLONE_NEW*)、NS_GET_ID (ns_id)、NS_GET_MNTNS_ID (マウントの
/// namespace でなければ EINVAL)、NS_GET_OWNER_UID (ユーザーの namespace を作った人。ほかは EINVAL)。nsfs でなければ None。
/// NS_GET_USERNS と NS_GET_PARENT (口を返すもの) はまだない
pub fn nsfs_ioctl(ino: &crate::vfs::InodeRef, req: u64, arg: usize) -> Option<Result<i64, i64>> {
    const NS_GET_NSTYPE: u64 = 0xb703;
    const NS_GET_OWNER_UID: u64 = 0xb704;
    const NS_GET_MNTNS_ID: u64 = 0x8008_b705;
    const NS_GET_ID: u64 = 0x8008_b70d;
    const EFAULT: i64 = 14;
    let w = &ino.as_any().downcast_ref::<NsInode>()?.which;
    let put = |b: &[u8]| crate::proc::current().pt().copy_out(arg, b).map(|_| 0).ok_or(-EFAULT);
    Some(match req {
        NS_GET_NSTYPE => Ok(w.flag() as i64),
        NS_GET_ID => put(&ns_id(w.id()).to_le_bytes()),
        NS_GET_MNTNS_ID if w.flag() == CLONE_NEWNS => put(&ns_id(w.id()).to_le_bytes()),
        NS_GET_MNTNS_ID => Err(crate::why::inval()),
        NS_GET_OWNER_UID => match w {
            Which::User(u) => put(&u.as_ref().map_or(0u32, |u| u.owner).to_le_bytes()),
            _ => Err(crate::why::inval()),
        },
        _ => return None,
    })
}

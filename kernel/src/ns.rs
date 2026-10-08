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
use core::cell::RefCell;

pub const CLONE_NEWUTS: u64 = 0x0400_0000;
pub const CLONE_NEWNET: u64 = 0x4000_0000;
pub const CLONE_NEWPID: u64 = 0x2000_0000;

const EPERM: i64 = 1;

/// はじめからある namespace の番号 (Linux と同じ)
pub const INIT_UTS: u64 = 4026531838;
pub const INIT_NET: u64 = 4026531840;
pub const INIT_PID: u64 = 4026531836;
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
}

impl Ns {
    pub const INIT: Ns = Ns { uts: None, net: None, pid: None };

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
pub const SUPPORTED: u64 = CLONE_NEWUTS | CLONE_NEWNET | CLONE_NEWPID;

/// cred の namespace を flags のぶん新しくする (clone の子と unshare)
pub fn renew(c: &mut crate::cred::Cred, flags: u64) -> Result<(), i64> {
    if flags & SUPPORTED == 0 {
        return Ok(());
    }
    if c.euid != 0 && !c.no_new_privs {
        return Err(-EPERM);
    }
    if flags & CLONE_NEWUTS != 0 {
        let name = current_name(c);
        c.ns.uts = Some(Rc::new(Uts { id: next_id(), name: RefCell::new(name) }));
    }
    if flags & CLONE_NEWNET != 0 {
        c.ns.net = Some(Rc::new(Net { id: next_id() }));
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

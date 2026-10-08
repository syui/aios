// namespace: プロセスから見える世界を分ける (Linux と同じ CLONE_NEW* と unshare)
//   UTS  ホスト名 (uname の nodename、sethostname)
//   NET  ネットワーク: 中から見えるのは自分の 127.0.0.1 だけ。TCP は中どうしでだけつながる (unix.rs の inet:
//        smoltcp はひとつなので、中の TCP は unix ソケットのしくみで「namespace の番号とポート」の名前につなぐ)。
//        UDP と ICMP はどこへも送れない (ENETUNREACH)。抽象名前空間の unix ソケット (@...) も分かれる (Linux と同じ)
// clone(CLONE_NEW*) か unshare(CLONE_NEW*) で新しいものを作る。fork と exec で引き継ぐ (cred の中に持つ)
// 作るには root か no_new_privs が要る (aios には user namespace がないので、Linux のように root 以外を
// 止めるのではなく、できることが減るだけの namespace は、setuid のプログラムをだませない no_new_privs で許す)。
// 新しい UTS の中ではホスト名を変えられる (外には見えないので、root でなくても)
// /proc/PID/ns/uts は「uts:[番号]」へのリンク (同じものか見分けるため)
use alloc::rc::Rc;
use core::cell::RefCell;

pub const CLONE_NEWUTS: u64 = 0x0400_0000;
pub const CLONE_NEWNET: u64 = 0x4000_0000;

const EPERM: i64 = 1;

/// はじめからある namespace の番号 (Linux と同じ)
pub const INIT_UTS: u64 = 4026531838;
pub const INIT_NET: u64 = 4026531840;

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

/// プロセスの namespace (None ははじめからあるもの)
#[derive(Clone)]
pub struct Ns {
    pub uts: Option<Rc<Uts>>,
    pub net: Option<Rc<Net>>,
}

impl Ns {
    pub const INIT: Ns = Ns { uts: None, net: None };

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
pub const SUPPORTED: u64 = CLONE_NEWUTS | CLONE_NEWNET;

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

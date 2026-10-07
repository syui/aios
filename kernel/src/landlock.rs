// Landlock: プロセスが自分で「ここから先しか触らない」と決める砂場 (Linux の landlock と同じ形と番号)
//
//   landlock_create_ruleset (444)  決まりの束を作る (fd)。何を見張るか (handled_access_fs / net) を決める
//   landlock_add_rule (445)        束に足す: このディレクトリ (の下) ではこれができる / この TCP の口ではこれができる
//   landlock_restrict_self (446)   束を自分にかける。子にも引き継がれ、exec しても外れない。もう外せない
//
// かけるには prctl(PR_SET_NO_NEW_PRIVS) か root が要る (setuid のプログラムで外へ出られないように)。
// かけるたびに層が増え、どの層も許したものだけができる。見張っていない操作はその層では止めない。
//
// aios のファイルの場所は、リンクをたどったあとの絶対パス (vfs::lookup) なので、決まりはパスの前の部分で
// くらべる (ディレクトリ "a/b" の決まりは "a/b" と "a/b/..." に効く)。作る・消すは親ディレクトリでくらべる。
// Linux とちがい、stat や chdir、chmod は止めない (Linux の landlock も止めない)
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

const EINVAL: i64 = 22;
const EACCES: i64 = 13;
const EPERM: i64 = 1;
const EBADF: i64 = 9;
const EFAULT: i64 = 14;
const EBADFD: i64 = 77;
const ENOMSG: i64 = 42;
const EOPNOTSUPP: i64 = 95;

/// 答える ABI の版 (4: TCP の bind / connect まで)
pub const ABI: i64 = 4;

pub const EXECUTE: u64 = 1 << 0;
pub const WRITE_FILE: u64 = 1 << 1;
pub const READ_FILE: u64 = 1 << 2;
pub const READ_DIR: u64 = 1 << 3;
pub const REMOVE_DIR: u64 = 1 << 4;
pub const REMOVE_FILE: u64 = 1 << 5;
pub const MAKE_CHAR: u64 = 1 << 6;
pub const MAKE_DIR: u64 = 1 << 7;
pub const MAKE_REG: u64 = 1 << 8;
pub const MAKE_SOCK: u64 = 1 << 9;
pub const MAKE_FIFO: u64 = 1 << 10;
pub const MAKE_BLOCK: u64 = 1 << 11;
pub const MAKE_SYM: u64 = 1 << 12;
pub const REFER: u64 = 1 << 13;
pub const TRUNCATE: u64 = 1 << 14;
pub const IOCTL_DEV: u64 = 1 << 15;
/// ABI 4 までのファイルの権利ぜんぶ (ABI 5 の IOCTL_DEV も受けるが止めない)
const FS_ALL: u64 = (1 << 16) - 1;
/// ファイル (ディレクトリでないもの) の決まりに書ける権利
const FS_FILE: u64 = EXECUTE | WRITE_FILE | READ_FILE | TRUNCATE | IOCTL_DEV;

pub const BIND_TCP: u64 = 1 << 0;
pub const CONNECT_TCP: u64 = 1 << 1;
const NET_ALL: u64 = BIND_TCP | CONNECT_TCP;

/// 決まりの束 (fd で持つ。restrict_self で写して層にする)
#[derive(Clone, Default)]
pub struct Ruleset {
    pub fs: u64,
    pub net: u64,
    /// (パス (先頭 / なし、"" はルート), 許すもの)
    paths: Vec<(String, u64)>,
    /// (TCP の口, 許すもの)
    ports: Vec<(u16, u64)>,
}

pub type RulesetRef = Rc<RefCell<Ruleset>>;

/// かかっている層 (古いものから)。Cred に入り、fork で引き継ぐ
pub type Domain = Rc<Vec<Rc<Ruleset>>>;

/// path が base の下か (base 自身も)
fn beneath(path: &str, base: &str) -> bool {
    base.is_empty() || path == base || (path.starts_with(base) && path.as_bytes().get(base.len()) == Some(&b'/'))
}

impl Ruleset {
    /// この層で、path に access ができるか
    fn allows_fs(&self, path: &str, access: u64) -> bool {
        let need = access & self.fs;
        if need == 0 {
            return true;
        }
        let got = self.paths.iter().filter(|(b, _)| beneath(path, b)).fold(0, |m, (_, a)| m | a);
        need & !got == 0
    }

    fn allows_net(&self, port: u16, access: u64) -> bool {
        let need = access & self.net;
        need == 0 || self.ports.iter().any(|&(p, a)| p == port && a & need == need)
    }
}

fn domain() -> Option<Domain> {
    crate::cred::current().landlock
}

/// path (先頭 / なし、リンクをたどったあと) に access ができるか。できなければ -EACCES
pub fn check_fs(path: &str, access: u64) -> Result<(), i64> {
    match domain() {
        Some(d) if !d.iter().all(|r| r.allows_fs(path, access)) => Err(-EACCES),
        _ => Ok(()),
    }
}

/// path に作る・消す (access は MAKE_* / REMOVE_*): 親ディレクトリでくらべる
pub fn check_parent(path: &str, access: u64) -> Result<(), i64> {
    check_fs(path.rsplit_once('/').map_or("", |(d, _)| d), access)
}

/// 作るものの種類 (mode の S_IFMT) の権利
pub fn make_right(mode: u32) -> u64 {
    match mode & crate::vfs::S_IFMT {
        crate::vfs::S_IFDIR => MAKE_DIR,
        crate::vfs::S_IFCHR => MAKE_CHAR,
        crate::vfs::S_IFBLK => MAKE_BLOCK,
        crate::vfs::S_IFIFO => MAKE_FIFO,
        crate::vfs::S_IFLNK => MAKE_SYM,
        0o140000 => MAKE_SOCK,
        _ => MAKE_REG,
    }
}

/// TCP の口 (bind / connect) に access ができるか。できなければ -EACCES
pub fn check_net(port: u16, access: u64) -> Result<(), i64> {
    match domain() {
        Some(d) if !d.iter().all(|r| r.allows_net(port, access)) => Err(-EACCES),
        _ => Ok(()),
    }
}

// ---- システムコール ----

type R = Result<i64, i64>;

fn copy_in(va: usize, b: &mut [u8]) -> Result<(), i64> {
    crate::proc::current().pt().copy_in(b, va).ok_or(-EFAULT)
}

fn ruleset_of(fd: u64) -> Result<RulesetRef, i64> {
    let f = crate::proc::current().files().get(fd).cloned().ok_or(-EBADF)?;
    let f = f.borrow();
    match &f.kind {
        crate::file::Kind::Landlock(r) => Ok(r.clone()),
        _ => Err(-EBADFD),
    }
}

/// landlock_create_ruleset(attr, size, flags)。flags = VERSION (1) なら ABI の版を返す
pub fn create_ruleset(attr: usize, size: usize, flags: u64) -> R {
    const VERSION: u64 = 1;
    if flags == VERSION {
        return if attr == 0 && size == 0 { Ok(ABI) } else { Err(-EINVAL) };
    }
    if flags != 0 {
        return Err(-EINVAL);
    }
    // struct landlock_ruleset_attr { handled_access_fs, handled_access_net, scoped }: 8 バイトずつ
    if size < 8 {
        return Err(-EINVAL);
    }
    let mut b = [0u8; 24];
    let n = size.min(24);
    copy_in(attr, &mut b[..n])?;
    if size > 24 {
        // 知らない続き (新しい ABI) は 0 でなければ断る
        let mut rest = alloc::vec![0u8; size - 24];
        copy_in(attr + 24, &mut rest)?;
        if rest.iter().any(|&x| x != 0) {
            return Err(-7); // E2BIG
        }
    }
    let q = |o: usize| u64::from_le_bytes(b[o..o + 8].try_into().unwrap());
    let (fs, net, scoped) = (q(0), q(8), q(16));
    if fs & !FS_ALL != 0 || net & !NET_ALL != 0 || scoped != 0 {
        return Err(-EINVAL);
    }
    if fs == 0 && net == 0 {
        return Err(-ENOMSG);
    }
    let r = Rc::new(RefCell::new(Ruleset { fs, net, ..Default::default() }));
    let f = crate::file::new(crate::file::Kind::Landlock(r), crate::file::O_RDWR);
    let fd = crate::proc::current().files().add(f, true, 0).ok_or(-24)?;
    Ok(fd as i64)
}

/// landlock_add_rule(ruleset_fd, rule_type, rule_attr, flags)
pub fn add_rule(fd: u64, ty: u64, attr: usize, flags: u64) -> R {
    const PATH_BENEATH: u64 = 1;
    const NET_PORT: u64 = 2;
    if flags != 0 {
        return Err(-EINVAL);
    }
    let r = ruleset_of(fd)?;
    match ty {
        PATH_BENEATH => {
            // struct landlock_path_beneath_attr { allowed_access: u64, parent_fd: i32 } (packed、12 バイト)
            let mut b = [0u8; 12];
            copy_in(attr, &mut b)?;
            let allowed = u64::from_le_bytes(b[0..8].try_into().unwrap());
            let pfd = i32::from_le_bytes(b[8..12].try_into().unwrap());
            let mut rs = r.borrow_mut();
            if allowed == 0 {
                return Err(-ENOMSG);
            }
            if allowed & !rs.fs != 0 {
                return Err(-EINVAL);
            }
            let f = crate::proc::current().files().get(pfd as u64).cloned().ok_or(-EBADF)?;
            let f = f.borrow();
            let crate::file::Kind::Inode(ino, path) = &f.kind else { return Err(-EBADFD) };
            if !ino.meta().is_dir() && allowed & !FS_FILE != 0 {
                return Err(-EINVAL);
            }
            rs.paths.push((path.trim_start_matches('/').into(), allowed));
            Ok(0)
        }
        NET_PORT => {
            // struct landlock_net_port_attr { allowed_access: u64, port: u64 }
            let mut b = [0u8; 16];
            copy_in(attr, &mut b)?;
            let allowed = u64::from_le_bytes(b[0..8].try_into().unwrap());
            let port = u64::from_le_bytes(b[8..16].try_into().unwrap());
            let mut rs = r.borrow_mut();
            if rs.net == 0 {
                return Err(-EOPNOTSUPP);
            }
            if allowed == 0 {
                return Err(-ENOMSG);
            }
            if allowed & !rs.net != 0 || port > 0xffff {
                return Err(-EINVAL);
            }
            rs.ports.push((port as u16, allowed));
            Ok(0)
        }
        _ => Err(-EINVAL),
    }
}

/// landlock_restrict_self(ruleset_fd, flags): 自分に層を 1 つ足す
pub fn restrict_self(fd: u64, flags: u64) -> R {
    if flags != 0 {
        return Err(-EINVAL);
    }
    let p = crate::proc::current();
    if !p.cred.no_new_privs && p.cred.euid != 0 {
        return Err(-EPERM);
    }
    let r = ruleset_of(fd)?;
    let mut layers: Vec<Rc<Ruleset>> = p.cred.landlock.as_ref().map(|d| d.as_ref().clone()).unwrap_or_default();
    if layers.len() >= 16 {
        return Err(-7); // E2BIG (Linux と同じく 16 層まで)
    }
    layers.push(Rc::new(r.borrow().clone()));
    p.cred.landlock = Some(Rc::new(layers));
    Ok(0)
}

/// /proc/PID/status の Landlock: かかっている層の数
pub fn layers(c: &crate::cred::Cred) -> usize {
    c.landlock.as_ref().map_or(0, |d| d.len())
}

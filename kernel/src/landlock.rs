// Landlock: プロセスが自分で「ここから先しか触らない」と決める砂場 (Linux 7.4 の landlock、ABI 12 と同じ形と番号)
//
//   landlock_create_ruleset (444)  決まりの束を作る (fd)。何を見張るか (struct landlock_ruleset_attr の 7 つの欄:
//                                  handled_access_fs / net、scoped、quiet_* の 3 つ、handled_permissions) を決める
//   landlock_add_rule (445)        束に足す: このディレクトリ (の下) ではこれができる (PATH_BENEATH)、この TCP / UDP の
//                                  口ではこれができる (NET_PORT)、この namespace を作れる (NAMESPACE)、この capability を
//                                  使える (CAPABILITY)
//   landlock_restrict_self (446)   束を自分にかける。子にも引き継がれ、exec しても外れない。もう外せない。TSYNC なら
//                                  同じプロセスのほかのスレッドにも、NO_NEW_PRIVS なら no_new_privs も立てる
//
// かけるには prctl(PR_SET_NO_NEW_PRIVS) (か restrict_self の NO_NEW_PRIVS) か root が要る (setuid のプログラムで外へ
// 出られないように)。かけるたびに層が増え、どの層も許したものだけができる。見張っていない操作はその層では止めない。
//
// 止めるもの:
//   ファイル    開く・作る・消す・つなぐ・切りつめる (パスで)。開いたときに TRUNCATE と IOCTL_DEV (デバイスだけ) の
//               許しを OpenFile に覚え (lsm_deny)、あとの ftruncate と ioctl はそれで決める (渡された fd でも同じ)
//   名前のある unix ソケットにつなぐ・送る (RESOLVE_UNIX)。相手のソケットを作ったプロセスが同じ層の中なら許す
//   ネットワーク TCP の bind / connect、UDP の bind / connect / sendto (名前のないソケットは autobind なので、ポート 0 の
//               BIND_UDP も要る)
//   scope       抽象名前空間の unix ソケット (作ったプロセスがその層の外なら EPERM)、シグナル (相手がその層の外なら
//               EPERM。同じスレッドグループはよい)
//   permission  namespace を作る (unshare、clone の CLONE_NEW*) と capability を使う (Cred::capable から)。EPERM
//
// 層の「同じ」は Linux の hierarchy とおなじく、その段の層 (Rc) が同じものか: restrict_self は新しい層を 1 つ作って
// 古い層の Rc をそのまま並べるので、ある段が同じなら、その下の段もみな同じ。
//
// aios のファイルの場所は、リンクをたどったあとの絶対パス (vfs::lookup) なので、決まりはパスの前の部分で
// くらべる (ディレクトリ "a/b" の決まりは "a/b" と "a/b/..." に効く)。作る・消すは親ディレクトリでくらべる。
// Linux とちがい、stat や chdir、chmod は止めない (Linux の landlock も止めない)。記録 (audit) はないので、quiet の
// 欄と印は形だけたしかめて覚える
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

const EPERM: i64 = 1;
const E2BIG: i64 = 7;
const EBADF: i64 = 9;
const EFAULT: i64 = 14;
const ENOMSG: i64 = 42;
const EBADFD: i64 = 77;
const EAFNOSUPPORT: i64 = 97;

/// 答える ABI の版 (12: Linux 7.4 の namespace と capability の permission まで)
pub const ABI: i64 = 12;
/// 直したもの (LANDLOCK_CREATE_RULESET_ERRATA): 1 TCP は IPPROTO_TCP だけ、2 scope のシグナルは同じスレッドグループを
/// 許す、3 つながっていないディレクトリ、4 whiteout は MAKE_REG
pub const ERRATA: i64 = 0b1111;

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
pub const RESOLVE_UNIX: u64 = 1 << 16;
const FS_ALL: u64 = (1 << 17) - 1;
/// ファイル (ディレクトリでないもの) の決まりに書ける権利
const FS_FILE: u64 = EXECUTE | WRITE_FILE | READ_FILE | TRUNCATE | IOCTL_DEV | RESOLVE_UNIX;

pub const BIND_TCP: u64 = 1 << 0;
pub const CONNECT_TCP: u64 = 1 << 1;
pub const BIND_UDP: u64 = 1 << 2;
pub const SEND_UDP: u64 = 1 << 3;
const NET_ALL: u64 = (1 << 4) - 1;

pub const SCOPE_UNIX: u64 = 1 << 0;
pub const SCOPE_SIGNAL: u64 = 1 << 1;
const SCOPE_ALL: u64 = (1 << 2) - 1;

const PERM_NS: u64 = 1 << 0;
const PERM_CAP: u64 = 1 << 1;
const PERM_ALL: u64 = (1 << 2) - 1;

/// namespace の種類 (CLONE_NS_ALL: CGROUP UTS IPC USER PID NET NS TIME)
const CLONE_NS_ALL: u64 = 0x0200_0000 | 0x0400_0000 | 0x0800_0000 | 0x1000_0000 | 0x2000_0000 | 0x4000_0000 | 0x0002_0000 | 0x80;
/// capability の番号 (0 から CAP_LAST_CAP = 40)
const CAP_VALID: u64 = (1 << 41) - 1;

/// 決まりの束 (fd で持つ。restrict_self で写して層にする)
#[derive(Clone, Default)]
pub struct Ruleset {
    pub fs: u64,
    pub net: u64,
    pub scoped: u64,
    pub perms: u64,
    quiet_fs: u64,
    quiet_net: u64,
    /// (パス (先頭 / なし、"" はルート), 許すもの)
    paths: Vec<(String, u64)>,
    /// (TCP / UDP の口, 許すもの)
    ports: Vec<(u16, u64)>,
    /// 作ってよい namespace (CLONE_NEW*) と、使ってよい capability (1 << CAP_*)
    ns: u64,
    caps: u64,
}

pub type RulesetRef = Rc<RefCell<Ruleset>>;

/// かかっている層 (古いものから。中は Layers)。Cred の sandbox に入り、fork で引き継ぐ。core は中を知らない
/// (security::Blob)
pub type Domain = crate::security::Blob;

/// 層の並び (Domain の中)
type Layers = Vec<Rc<Ruleset>>;

/// Domain の中の層 (ほかの型のものは入らない)
fn view(d: &Domain) -> &Layers {
    d.downcast_ref::<Layers>().expect("landlock: sandbox")
}

/// path が base の下か (base 自身も)
fn beneath(path: &str, base: &str) -> bool {
    base.is_empty() || path == base || (path.starts_with(base) && path.as_bytes().get(base.len()) == Some(&b'/'))
}

impl Ruleset {
    /// この層が path に許すもの
    fn granted_fs(&self, path: &str) -> u64 {
        self.paths.iter().filter(|(b, _)| beneath(path, b)).fold(0, |m, (_, a)| m | a)
    }

    /// この層で、path の access のうち止めるもの
    fn denied_fs(&self, path: &str, access: u64) -> u64 {
        let need = access & self.fs;
        if need == 0 { 0 } else { need & !self.granted_fs(path) }
    }

    fn allows_net(&self, port: u16, access: u64) -> bool {
        let need = access & self.net;
        need == 0 || self.ports.iter().any(|&(p, a)| p == port && a & need == need)
    }
}

fn domain() -> Option<Domain> {
    crate::cred::current().sandbox.clone()
}

/// path (先頭 / なし、リンクをたどったあと) に access ができるか。できなければ -EACCES
pub fn check_fs(path: &str, access: u64) -> Result<(), i64> {
    match domain() {
        Some(d) if view(&d).iter().any(|r| r.denied_fs(path, access) != 0) => Err(crate::why::landlock(access, false, path)),
        _ => Ok(()),
    }
}

/// path の access のうち、どれかの層が止めるもの (開いたときに覚える TRUNCATE と IOCTL_DEV: Linux の hook_file_open)
pub fn denied_fs(path: &str, access: u64) -> u64 {
    domain().map_or(0, |d| view(&d).iter().fold(0, |m, r| m | r.denied_fs(path, access)))
}

/// path に作る・消す (access は MAKE_* / REMOVE_*): 親ディレクトリでくらべる
pub fn check_parent(path: &str, access: u64) -> Result<(), i64> {
    check_fs(path.rsplit_once('/').map_or("", |(d, _)| d), access)
}

/// 作るものの種類 (mode の S_IFMT) の権利。whiteout (S_IFCHR の 0:0) は MAKE_REG (erratum 4)
pub fn make_right(mode: u32) -> u64 {
    make_right_dev(mode, u64::MAX)
}

pub fn make_right_dev(mode: u32, dev: u64) -> u64 {
    match mode & crate::vfs::S_IFMT {
        crate::vfs::S_IFDIR => MAKE_DIR,
        crate::vfs::S_IFCHR if dev == 0 => MAKE_REG,
        crate::vfs::S_IFCHR => MAKE_CHAR,
        crate::vfs::S_IFBLK => MAKE_BLOCK,
        crate::vfs::S_IFIFO => MAKE_FIFO,
        crate::vfs::S_IFLNK => MAKE_SYM,
        0o140000 => MAKE_SOCK,
        _ => MAKE_REG,
    }
}

/// デバイスの ioctl のうち、IOCTL_DEV がなくてもよいもの (Linux の is_masked_device_ioctl: fd の印、ファイルシステムの
/// ことを聞くもの)。FIONREAD などデバイスに回るものはだめ
pub fn masked_ioctl(req: u64) -> bool {
    matches!(
        req as u32,
        0x5451 // FIOCLEX
        | 0x5450 // FIONCLEX
        | 0x5421 // FIONBIO
        | 0x5452 // FIOASYNC
        | 0x5460 // FIOQSIZE
        | 0xc004_5877 // FIFREEZE
        | 0xc004_5878 // FITHAW
        | 0xc020_660b // FS_IOC_FIEMAP
        | 0x2 // FIGETBSZ
        | 0x4004_9409 // FICLONE
        | 0x4020_940d // FICLONERANGE
        | 0xc018_9436 // FIDEDUPERANGE
        | 0x8011_1500 // FS_IOC_GETFSUUID
        | 0x8081_1501 // FS_IOC_GETFSSYSFSPATH
    )
}

/// 開いたときに覚えた許し (OpenFile の lsm_deny) で、ftruncate ができるか
pub fn check_file_truncate(deny: u32) -> Result<(), i64> {
    if deny as u64 & TRUNCATE != 0 { Err(crate::why::landlock(TRUNCATE, false, "an opened file")) } else { Ok(()) }
}

/// 開いたときに覚えた許し (OpenFile の lsm_deny) で、デバイスに ioctl ができるか
pub fn check_file_ioctl(deny: u32, req: u64) -> Result<(), i64> {
    if deny as u64 & IOCTL_DEV != 0 && !masked_ioctl(req) { Err(crate::why::landlock(IOCTL_DEV, false, "an opened device")) } else { Ok(()) }
}

// ---- 層をくらべる (Linux の hierarchy) ----

/// client の層のうち scope を見張るものが、server の外を指すか (Linux の domain_is_scoped)。server の層より上の段で
/// 見張っていれば外、下の段は同じ層かどうか
fn scoped(client: &Domain, server: Option<&Domain>, scope: u64) -> bool {
    let (client, server) = (view(client), server.map(view));
    let sn = server.map_or(0, |s| s.len());
    for i in (0..client.len()).rev() {
        if client[i].scoped & scope == 0 {
            continue;
        }
        return i >= sn || !Rc::ptr_eq(&client[i], &server.unwrap()[i]);
    }
    false
}

/// client の i 段めが server と同じ層か
fn same_layer(client: &Domain, server: Option<&Domain>, i: usize) -> bool {
    server.map(view).is_some_and(|s| i < s.len() && Rc::ptr_eq(&view(client)[i], &s[i]))
}

/// 抽象名前空間の unix ソケット (作ったプロセスの層が other) につなぐ・送る (Linux の hook_unix_stream_connect と
/// hook_unix_may_send)。だめなら -EPERM
pub fn check_scope_unix(other: Option<&Domain>) -> Result<(), i64> {
    match domain() {
        Some(d) if scoped(&d, other, SCOPE_UNIX) => Err(crate::why::landlock_scope("connecting to an abstract unix socket outside its domain")),
        _ => Ok(()),
    }
}

/// target にシグナルを送る (Linux の hook_task_kill。同じスレッドグループは呼ぶほうで外す)。だめなら -EPERM
pub fn check_scope_signal(target: Option<&Domain>) -> Result<(), i64> {
    match domain() {
        Some(d) if scoped(&d, target, SCOPE_SIGNAL) => Err(crate::why::landlock_scope("signaling a process outside its domain")),
        _ => Ok(()),
    }
}

/// tracee を ptrace する (Linux の domain_ptrace: 追う人の層が、相手の層の下になければ EPERM)
pub fn check_ptrace(tracer: Option<&Domain>, tracee: Option<&Domain>) -> Result<(), i64> {
    let Some(p) = tracer.map(view) else { return Ok(()) };
    let top = p.len() - 1;
    if tracee.map(view).is_some_and(|c| c.len() > top && Rc::ptr_eq(&c[top], &p[top])) {
        return Ok(());
    }
    Err(crate::why::landlock_scope("tracing a process outside its domain"))
}

/// 名前のある unix ソケット path (先頭 / なし) に、つなぐ・送る (Linux の hook_unix_find)。相手 (作ったプロセスの層が
/// other) と同じ層は見ない。ほかの層はパスの決まりの RESOLVE_UNIX で
pub fn check_resolve_unix(path: &str, other: Option<&Domain>) -> Result<(), i64> {
    let Some(d) = domain() else { return Ok(()) };
    for (i, r) in view(&d).iter().enumerate() {
        if r.fs & RESOLVE_UNIX != 0 && !same_layer(&d, other, i) && r.denied_fs(path, RESOLVE_UNIX) != 0 {
            return Err(crate::why::landlock(RESOLVE_UNIX, false, path));
        }
    }
    Ok(())
}

// ---- ネットワーク ----

const AF_UNSPEC: u16 = 0;
const AF_INET: u16 = 2;
const AF_INET6: u16 = 10;

/// fam (AF_INET / AF_INET6) のソケットの addr (bind / connect / sendto の名前) に access (BIND_* / CONNECT_TCP /
/// SEND_UDP) ができるか (Linux の current_check_access_socket)。connecting は connect (AF_UNSPEC はつながりを切るだけ)
pub fn check_socket(fam: u16, addr: &[u8], access: u64, connecting: bool) -> Result<(), i64> {
    let Some(d) = domain() else { return Ok(()) };
    let d = view(&d);
    if d.iter().all(|r| r.net & access == 0) {
        return Ok(());
    }
    if addr.len() < 2 {
        return Err(crate::why::inval());
    }
    let af = u16::from_le_bytes([addr[0], addr[1]]);
    let v4 = |a: &[u8]| if a.len() < 16 { Err(crate::why::inval()) } else { Ok(u16::from_be_bytes([a[2], a[3]])) };
    let port = match af {
        AF_UNSPEC => {
            if access == CONNECT_TCP || (access == SEND_UDP && connecting) {
                return Ok(());
            }
            if access == SEND_UDP {
                if fam == AF_INET6 {
                    return Err(crate::why::landlock(access, true, "AF_UNSPEC"));
                }
            } else if fam == AF_INET {
                // bind の AF_UNSPEC は INADDR_ANY だけ (AF_INET として)
                if addr.len() < 16 {
                    return Err(crate::why::inval());
                }
                if addr[4..8] != [0; 4] {
                    return Err(-EAFNOSUPPORT);
                }
            } else {
                return Err(if addr.len() < 24 { crate::why::inval() } else { -EAFNOSUPPORT });
            }
            v4(addr)?
        }
        AF_INET => v4(addr)?,
        AF_INET6 if addr.len() < 24 => return Err(crate::why::inval()),
        AF_INET6 => u16::from_be_bytes([addr[2], addr[3]]),
        _ => return Ok(()),
    };
    if af != fam && af != AF_UNSPEC {
        return Err(crate::why::inval());
    }
    if d.iter().all(|r| r.allows_net(port, access)) {
        Ok(())
    } else {
        Err(crate::why::landlock(access, true, &alloc::format!("port {}", port)))
    }
}

/// まだ名前のない UDP ソケットは、送る・つなぐときに autobind する: ポート 0 の BIND_UDP (Linux の
/// current_check_autobind_udp_socket)
pub fn check_autobind_udp(fam: u16, bound: bool) -> Result<(), i64> {
    if bound {
        return Ok(());
    }
    let mut port0 = [0u8; 28];
    port0[..2].copy_from_slice(&fam.to_le_bytes());
    check_socket(fam, &port0, BIND_UDP, false)
}

// ---- permission ----

/// c (の層) が capability cap を使えるか (Linux の hook_capable: commoncap が許したあとで、だめなら止めるだけ)
pub fn cap_ok(c: &crate::cred::Cred, cap: u32) -> bool {
    c.sandbox.as_ref().is_none_or(|d| view(d).iter().all(|r| r.perms & PERM_CAP == 0 || (cap < 64 && r.caps & (1 << cap) != 0)))
}

/// c (の層) が flags (CLONE_NEW*) の namespace をみな作れるか (Linux の hook_namespace_init)。だめなら -EPERM
pub fn check_ns(c: &crate::cred::Cred, flags: u64) -> Result<(), i64> {
    let want = flags & CLONE_NS_ALL;
    match &c.sandbox {
        Some(d) if want != 0 && view(d).iter().any(|r| r.perms & PERM_NS != 0 && want & !r.ns != 0) => Err(crate::why::landlock_scope("creating a namespace")),
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
        crate::file::Kind::Landlock(r) => r.clone().downcast::<RefCell<Ruleset>>().map_err(|_| -EBADFD),
        _ => Err(-EBADFD),
    }
}

fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

/// landlock_create_ruleset(attr, size, flags)。flags = VERSION (1) なら ABI の版、ERRATA (2) なら直したもの
pub fn create_ruleset(attr: usize, size: usize, flags: u64) -> R {
    const VERSION: u64 = 1;
    const ERRATA_FLAG: u64 = 2;
    const ATTR: usize = 56;
    if flags != 0 {
        if attr != 0 || size != 0 {
            return Err(crate::why::inval());
        }
        return match flags {
            VERSION => Ok(ABI),
            ERRATA_FLAG => Ok(ERRATA),
            _ => Err(crate::why::inval()),
        };
    }
    // struct landlock_ruleset_attr (7 つの u64)。古い大きさ (8 バイトから) も受け、知らない続きは 0 でなければ E2BIG
    // (Linux の copy_min_struct_from_user)
    if attr == 0 {
        return Err(-EFAULT);
    }
    if size < 8 {
        return Err(crate::why::inval());
    }
    if size > 4096 {
        return Err(-E2BIG);
    }
    let mut b = alloc::vec![0u8; size.max(ATTR)];
    copy_in(attr, &mut b[..size])?;
    if b[ATTR..].iter().any(|&x| x != 0) {
        return Err(-E2BIG);
    }
    let (fs, net, scoped, qfs, qnet, qscoped, perms) = (u64_at(&b, 0), u64_at(&b, 8), u64_at(&b, 16), u64_at(&b, 24), u64_at(&b, 32), u64_at(&b, 40), u64_at(&b, 48));
    if fs & !FS_ALL != 0 || net & !NET_ALL != 0 || scoped & !SCOPE_ALL != 0 {
        return Err(crate::why::inval());
    }
    // quiet はそれぞれ見張るものの中だけ
    if qfs & !fs != 0 || qnet & !net != 0 || qscoped & !scoped != 0 {
        return Err(crate::why::inval());
    }
    if perms & !PERM_ALL != 0 {
        return Err(crate::why::inval());
    }
    if fs == 0 && net == 0 && scoped == 0 && perms == 0 {
        return Err(-ENOMSG);
    }
    let r = Rc::new(RefCell::new(Ruleset { fs, net, scoped, perms, quiet_fs: qfs, quiet_net: qnet, ..Default::default() }));
    let f = crate::file::new(crate::file::Kind::Landlock(r), crate::file::O_RDWR);
    let fd = crate::proc::current().files().add(f, true, 0).ok_or(-24)?;
    Ok(fd as i64)
}

/// landlock_add_rule(ruleset_fd, rule_type, rule_attr, flags)。flags は QUIET (1) だけ (PATH_BENEATH と NET_PORT)
pub fn add_rule(fd: u64, ty: u64, attr: usize, flags: u64) -> R {
    const PATH_BENEATH: u64 = 1;
    const NET_PORT: u64 = 2;
    const NAMESPACE: u64 = 3;
    const CAPABILITY: u64 = 4;
    const QUIET: u64 = 1;
    if flags & !QUIET != 0 {
        return Err(crate::why::inval());
    }
    let r = ruleset_of(fd)?;
    match ty {
        PATH_BENEATH => {
            // struct landlock_path_beneath_attr { allowed_access: u64, parent_fd: i32 } (packed、12 バイト)
            let mut b = [0u8; 12];
            copy_in(attr, &mut b)?;
            let allowed = u64_at(&b, 0);
            let pfd = i32::from_le_bytes(b[8..12].try_into().unwrap());
            let mut rs = r.borrow_mut();
            // 許すものがなければ役に立たない (quiet の印をつけるためならよい)
            if flags == 0 && allowed == 0 {
                return Err(-ENOMSG);
            }
            if allowed & !rs.fs != 0 {
                return Err(crate::why::inval());
            }
            if flags & QUIET != 0 && rs.quiet_fs == 0 {
                return Err(crate::why::inval());
            }
            let f = crate::proc::current().files().get(pfd as u64).cloned().ok_or(-EBADF)?;
            let f = f.borrow();
            // 決まりの束、パイプやソケットなど名前のないものはだめ。デバイスを開いたもの (/dev/null など) はその場所
            let path = f.describe();
            if !path.starts_with('/') {
                return Err(-EBADFD);
            }
            let is_dir = matches!(&f.kind, crate::file::Kind::Inode(ino, _) if ino.meta().is_dir());
            if !is_dir && allowed & !FS_FILE != 0 {
                return Err(crate::why::inval());
            }
            rs.paths.push((path.trim_start_matches('/').into(), allowed));
            Ok(0)
        }
        NET_PORT => {
            // struct landlock_net_port_attr { allowed_access: u64, port: u64 }
            let mut b = [0u8; 16];
            copy_in(attr, &mut b)?;
            let (allowed, port) = (u64_at(&b, 0), u64_at(&b, 8));
            let mut rs = r.borrow_mut();
            if flags == 0 && allowed == 0 {
                return Err(-ENOMSG);
            }
            if allowed & !rs.net != 0 {
                return Err(crate::why::inval());
            }
            if flags & QUIET != 0 && rs.quiet_net == 0 {
                return Err(crate::why::inval());
            }
            if port > 0xffff {
                return Err(crate::why::inval());
            }
            rs.ports.push((port as u16, allowed));
            Ok(0)
        }
        NAMESPACE | CAPABILITY => {
            // struct landlock_namespace_attr / landlock_capability_attr { permissions, allowed, quiet: u64 }。QUIET の
            // 印は使えない (quiet は欄で)
            if flags != 0 {
                return Err(crate::why::inval());
            }
            let mut b = [0u8; 24];
            copy_in(attr, &mut b)?;
            let (perm, allowed, quiet) = (u64_at(&b, 0), u64_at(&b, 8), u64_at(&b, 16));
            if perm == 0 {
                return Err(-ENOMSG);
            }
            let want = if ty == NAMESPACE { PERM_NS } else { PERM_CAP };
            let mut rs = r.borrow_mut();
            if perm != want || rs.perms & want == 0 {
                return Err(crate::why::inval());
            }
            if allowed == 0 && quiet == 0 {
                return Err(-ENOMSG);
            }
            // 知らないビットは黙って受けて捨てる (新しいヘッダで作ったものが古いカーネルでも動くように)
            if ty == NAMESPACE {
                rs.ns |= allowed & CLONE_NS_ALL;
            } else {
                rs.caps |= allowed & CAP_VALID;
            }
            Ok(0)
        }
        _ => Err(crate::why::inval()),
    }
}

/// landlock_restrict_self(ruleset_fd, flags): 自分に層を 1 つ足す。flags は LOG_SAME_EXEC_OFF (1)、LOG_NEW_EXEC_ON (2)、
/// LOG_SUBDOMAINS_OFF (4)、TSYNC (8: 同じプロセスのほかのスレッドにも)、NO_NEW_PRIVS (16: うまくいけば no_new_privs も)。
/// fd が -1 で LOG_SUBDOMAINS_OFF (と TSYNC) だけなら、層は足さない (記録の印だけ。aios に記録はない)
pub fn restrict_self(fd: u64, flags: u64) -> R {
    const LOG_SUBDOMAINS_OFF: u64 = 4;
    const TSYNC: u64 = 8;
    const NO_NEW_PRIVS: u64 = 16;
    const ALL: u64 = (1 << 5) - 1;
    if flags & !ALL != 0 {
        return Err(crate::why::inval());
    }
    let p = crate::proc::current();
    if flags & NO_NEW_PRIVS == 0 && !p.cred.no_new_privs && !p.cred.capable_noaudit(crate::cred::CAP_SYS_ADMIN) {
        return Err(-EPERM);
    }
    let only_log = fd as i32 == -1 && flags & !TSYNC == LOG_SUBDOMAINS_OFF;
    let new = if only_log {
        p.cred.sandbox.clone()
    } else {
        let r = ruleset_of(fd)?;
        let mut layers: Layers = p.cred.sandbox.as_ref().map(|d| view(d).clone()).unwrap_or_default();
        if layers.len() >= 16 {
            return Err(-E2BIG); // Linux と同じく 16 層まで
        }
        layers.push(Rc::new(r.borrow().clone()));
        Some(Rc::new(layers) as Domain)
    };
    // TSYNC: ほかのスレッドにも同じ層 (と、呼んだスレッドが no_new_privs なら no_new_privs も: Linux の tsync)
    let nnp = flags & NO_NEW_PRIVS != 0 || p.cred.no_new_privs;
    if flags & TSYNC != 0 {
        for t in p.siblings() {
            t.cred.sandbox = new.clone();
            if nnp {
                t.cred.no_new_privs = true;
            }
        }
    }
    p.cred.sandbox = new;
    if flags & NO_NEW_PRIVS != 0 {
        p.cred.no_new_privs = true;
    }
    Ok(0)
}

/// /proc/PID/status の Landlock: かかっている層の数
pub fn layers(c: &crate::cred::Cred) -> usize {
    c.sandbox.as_ref().map_or(0, |d| view(d).len())
}

/// LSM としての landlock (security.rs: Linux の DEFINE_LSM(landlock) と security/landlock の hook_*)。重ねる LSM
/// (ラベルは持たない)。切ってあってもつないだまま: 切るのはシステムコールだけ (core の part_of)、かかっている層は
/// いつも効く
pub struct Landlock;
pub static LSM: Landlock = Landlock;

/// path の親ディレクトリ (先頭 / なし)
fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(d, _)| d)
}

impl crate::security::Lsm for Landlock {
    fn id(&self) -> (&'static str, u64) {
        ("landlock", crate::security::LSM_ID_LANDLOCK)
    }

    fn enabled(&self) -> bool {
        !crate::parts::off(crate::parts::Part::Landlock)
    }

    fn capable(&self, c: &crate::cred::Cred, cap: u32, _audit: bool) -> bool {
        cap_ok(c, cap)
    }

    /// hook_task_kill: 層のある送り主が、ほかのスレッドグループの層の外の相手へ送るのは EPERM
    fn task_kill(&self, target: &crate::proc::Proc, _sig: i32) -> Result<(), i64> {
        let me = crate::proc::current();
        if me.cred.sandbox.is_some() && me.tgid != target.tgid {
            check_scope_signal(target.cred.sandbox.as_ref())?;
        }
        Ok(())
    }

    fn ptrace_access_check(&self, child: &crate::cred::Cred, _mode: u32) -> Result<(), i64> {
        check_ptrace(crate::cred::current().sandbox.as_ref(), child.sandbox.as_ref())
    }

    /// hook_namespace_init: NAMESPACE_USE
    fn namespace_init(&self, c: &crate::cred::Cred, flags: u64) -> Result<(), i64> {
        check_ns(c, flags)
    }

    /// hook_file_open: 開く道ごとの権利。返すのは開いたファイルに覚える、許されなかった TRUNCATE と IOCTL_DEV
    fn file_open(&self, o: &crate::security::FileOpen) -> Result<u32, i64> {
        use crate::file::{O_ACCMODE, O_RDONLY, O_WRONLY};
        use crate::security::How;
        const O_TRUNC: u32 = 0o1000;
        let acc = o.flags & O_ACCMODE;
        match o.how {
            How::Exec => {
                check_fs(o.path, EXECUTE)?;
                Ok(0)
            }
            // そのディレクトリの下にふつうのファイルを作り、読み書きする
            How::Tmpfile => {
                let rw = if acc == O_WRONLY { WRITE_FILE } else { READ_FILE | WRITE_FILE };
                check_fs(o.path, MAKE_REG | rw)?;
                Ok(denied_fs(o.path, TRUNCATE) as u32)
            }
            // 作ったばかりのものは、mode に関係なく開ける
            How::Created => Ok(denied_fs(o.path, TRUNCATE) as u32),
            // ディレクトリは READ_DIR、ほか (デバイスも) は READ_FILE / WRITE_FILE、O_TRUNC は TRUNCATE
            How::Open => {
                let a = if o.dir {
                    READ_DIR
                } else {
                    (if acc != O_WRONLY { READ_FILE } else { 0 }) | (if acc != O_RDONLY { WRITE_FILE } else { 0 }) | (if o.flags & O_TRUNC != 0 { TRUNCATE } else { 0 })
                };
                check_fs(o.path, a)?;
                Ok(denied_fs(o.path, TRUNCATE | if o.dev { IOCTL_DEV } else { 0 }) as u32)
            }
        }
    }

    /// O_CREAT で作る前: 親に MAKE_REG、書く口なら WRITE_FILE も
    fn may_o_create(&self, full: &str, flags: u32) -> Result<(), i64> {
        check_parent(full, MAKE_REG)?;
        if flags & crate::file::O_ACCMODE != crate::file::O_RDONLY {
            check_fs(full, WRITE_FILE)?;
        }
        Ok(())
    }

    fn path_mkdir(&self, full: &str) -> Result<(), i64> {
        check_parent(full, MAKE_DIR)
    }

    fn path_mknod(&self, full: &str, mode: u32, dev: u64) -> Result<(), i64> {
        check_parent(full, make_right_dev(mode, dev))
    }

    fn path_symlink(&self, full: &str) -> Result<(), i64> {
        check_parent(full, MAKE_SYM)
    }

    fn path_unlink(&self, full: &str, _dir: &crate::vfs::InodeRef, _name: &str, rmdir: bool) -> Result<(), i64> {
        check_parent(full, if rmdir { REMOVE_DIR } else { REMOVE_FILE })
    }

    /// link: 作るところの MAKE_* (つなぐ元を引く前)
    fn path_link_make(&self, full: &str, mode: u32) -> Result<(), i64> {
        check_parent(full, make_right(mode))
    }

    /// link: ほかのディレクトリのものをつなぐなら、両方に REFER も
    fn path_link(&self, src: &str, full: &str, _ino: &crate::vfs::InodeRef) -> Result<(), i64> {
        if dir_of(src) != dir_of(full) {
            check_parent(src, REFER)?;
            check_parent(full, REFER)?;
        }
        Ok(())
    }

    /// rename: 元を消して、先に作る (先にあるものは消す)。入れかえなら、それも元のところへ作る。ディレクトリを
    /// またぐなら両方に REFER も
    fn path_rename(&self, r: &crate::security::Rename) -> Result<(), i64> {
        let kind = |m: u32| if m & crate::vfs::S_IFMT == crate::vfs::S_IFDIR { REMOVE_DIR } else { REMOVE_FILE };
        let mode = r.moving.meta().mode;
        check_parent(r.old, kind(mode))?;
        check_parent(r.new, make_right(mode))?;
        if let Some(t) = r.target {
            check_parent(r.new, kind(t.meta().mode))?;
            if r.exchange {
                check_parent(r.old, make_right(t.meta().mode))?;
            }
        }
        if dir_of(r.old) != dir_of(r.new) {
            check_parent(r.old, REFER)?;
            check_parent(r.new, REFER)?;
        }
        Ok(())
    }

    fn path_truncate(&self, full: &str, _ouid: u32) -> Result<(), i64> {
        check_fs(full, TRUNCATE)
    }

    /// hook_file_truncate: 開いたときの TRUNCATE の許しで
    fn file_truncate(&self, f: &crate::file::FileRef) -> Result<(), i64> {
        check_file_truncate(f.borrow().lsm_deny)
    }

    /// hook_file_ioctl: デバイスの ioctl は、開いたときの IOCTL_DEV の許しで (fd の印などはいつもよい)
    fn file_ioctl(&self, f: &crate::file::FileRef, req: u64) -> Result<(), i64> {
        check_file_ioctl(f.borrow().lsm_deny, req)
    }

    /// landlock_create_ruleset (444)、landlock_add_rule (445)、landlock_restrict_self (446)
    fn syscall(&self, nr: u64, a: &[u64; 6]) -> Option<Result<i64, i64>> {
        match nr {
            444 => Some(create_ruleset(a[0] as usize, a[1] as usize, a[2])),
            445 => Some(add_rule(a[0], a[1], a[2] as usize, a[3])),
            446 => Some(restrict_self(a[0], a[1])),
            _ => None,
        }
    }
}

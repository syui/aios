// ファイルまわりのシステムコール
use crate::file::{self, FileRef, Kind, Pipe, Stat, EBADF, EINVAL};
use crate::cred::{self, Cred, R as PR, W as PW, X as PX};
use crate::fs;
use crate::inotify;
use crate::proc::{self, Fd};
use crate::vfs::{self, InodeRef, NewNode, S_IFMT};
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

const ENOENT: i64 = 2;
const EFAULT: i64 = 14;
const EBUSY: i64 = 16;
const EEXIST: i64 = 17;
const ENOTDIR: i64 = 20;
const EISDIR: i64 = 21;
const EMFILE: i64 = 24;
const ENOTTY: i64 = 25;
const ERANGE: i64 = 34;
const ENOTEMPTY: i64 = 39;

const AT_FDCWD: i64 = -100;
const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
const AT_REMOVEDIR: u64 = 0x200;
const AT_SYMLINK_FOLLOW: u64 = 0x400;
const AT_EMPTY_PATH: u64 = 0x1000;

const O_CREAT: u64 = 0o100;
const O_EXCL: u64 = 0o200;
const O_TRUNC: u64 = 0o1000;
const ETXTBSY: i64 = 26;
// arm64 の値 (x86 とは違う)
const O_DIRECTORY: u64 = 0o40000;
const O_NOFOLLOW: u64 = 0o100000;
const O_CLOEXEC: u64 = 0o2000000;

const F_DUPFD: u64 = 0;
const F_GETFD: u64 = 1;
const F_SETFD: u64 = 2;
const F_GETFL: u64 = 3;
const F_SETFL: u64 = 4;
const F_DUPFD_CLOEXEC: u64 = 1030;
const F_SETPIPE_SZ: u64 = 1031;
const F_GETPIPE_SZ: u64 = 1032;
const F_ADD_SEALS: u64 = 1033;
const F_GET_SEALS: u64 = 1034;
const FD_CLOEXEC: u64 = 1;

const O_NONBLOCK: u32 = 0o4000;
const FIONBIO: u64 = 0x5421;

/// いまのプロセスの umask (作るファイルの mode から外すもの)
fn umask() -> u32 {
    proc::current().files().umask
}

type R = Result<i64, i64>;

fn user_str(va: usize) -> Result<String, i64> {
    let b = proc::current().pt().copy_in_str(va, 4096).ok_or(-EFAULT)?;
    String::from_utf8(b).map_err(|_| -ENOENT)
}

fn out(va: usize, b: &[u8]) -> Result<(), i64> {
    proc::current().pt().copy_out(va, b).ok_or(-EFAULT)
}

fn file_of(fd: u64) -> Result<FileRef, i64> {
    proc::current().files().get(fd).cloned().ok_or_else(|| crate::why::bad_fd(fd))
}

fn inode_of(fd: u64) -> Result<InodeRef, i64> {
    let f = file_of(fd)?;
    let f = f.borrow();
    match &f.kind {
        Kind::Inode(i, _) => Ok(i.clone()),
        _ => Err(crate::why::inval()),
    }
}

/// dirfd と path から、探索の基準にするディレクトリ (先頭 / なし)
/// dirfd が開いているディレクトリ (inode とパス)。AT_FDCWD か絶対パスなら None
fn base_ino(dirfd: i64, path: &str) -> Result<Option<(InodeRef, String)>, i64> {
    if path.starts_with('/') || dirfd == AT_FDCWD {
        return Ok(None);
    }
    let f = file_of(dirfd as u64)?;
    let f = f.borrow();
    match &f.kind {
        Kind::Inode(i, p) if i.meta().is_dir() => Ok(Some((i.clone(), p.clone()))),
        _ => Err(-ENOTDIR),
    }
}

fn base_dir(dirfd: i64, path: &str) -> Result<String, i64> {
    if path.starts_with('/') || dirfd == AT_FDCWD {
        return Ok(proc::current().files().cwd.clone());
    }
    let f = file_of(dirfd as u64)?;
    let f = f.borrow();
    match &f.kind {
        Kind::Inode(i, p) if i.meta().is_dir() => Ok(p.clone()),
        _ => Err(-ENOTDIR),
    }
}

/// dirfd + path の inode。path が空で AT_EMPTY_PATH なら dirfd 自身
fn at(dirfd: i64, pathp: usize, flags: u64) -> Result<InodeRef, i64> {
    let path = user_str(pathp)?;
    at_path(dirfd, &path, flags)
}

/// at と同じ (パスはもうユーザーから写したもの: newfstatat などが 2 度写さないように)
fn at_path(dirfd: i64, path: &str, flags: u64) -> Result<InodeRef, i64> {
    if path.is_empty() {
        if flags & AT_EMPTY_PATH == 0 {
            return Err(-ENOENT);
        }
        // AT_FDCWD なら、いまのディレクトリ
        if dirfd == AT_FDCWD {
            return vfs::resolve("", &proc::current().files().cwd.clone(), true);
        }
        return inode_of(dirfd as u64);
    }
    let follow = flags & AT_SYMLINK_NOFOLLOW == 0;
    // 開いているディレクトリからで、パスの覚えにあるもの: 開いたファイルを借りたまま引く (ディレクトリのパスを写さない。
    // 覚えになければ、たどるのは借りを返してから)。まちがい (EBADF、ENOTDIR) の順は base_ino と同じ
    if !path.starts_with('/') && dirfd != AT_FDCWD {
        let f = file_of(dirfd as u64)?;
        let hit = match &f.borrow().kind {
            Kind::Inode(i, p) if i.meta().is_dir() => vfs::cached_ino_at(i, p, path, follow),
            _ => return Err(-ENOTDIR),
        };
        if let Some(r) = hit {
            return r;
        }
    }
    match base_ino(dirfd, path)? {
        // 開いているディレクトリから (ルートからたどりなおさない)
        Some((dir, dir_path)) => vfs::lookup_at(&dir, &dir_path, path, follow).map(|(_, i)| i),
        None => vfs::resolve(&proc::current().files().cwd.clone(), path, follow),
    }
}

/// at と同じものを探し、そのパス (先頭 / なし) を返す
fn old_path(dirfd: i64, pathp: usize, flags: u64) -> Result<String, i64> {
    let path = user_str(pathp)?;
    // 空のパスは、AT_EMPTY_PATH でなければ dirfd を見る前に ENOENT (Linux の getname)
    if path.is_empty() && flags & AT_EMPTY_PATH == 0 {
        return Err(-ENOENT);
    }
    if path.is_empty() {
        if dirfd == AT_FDCWD {
            return Ok(proc::current().files().cwd.trim_start_matches('/').into());
        }
        let f = file_of(dirfd as u64)?;
        let f = f.borrow();
        return match &f.kind {
            Kind::Inode(_, p) => Ok(p.trim_start_matches('/').into()),
            _ => Err(crate::why::inval()),
        };
    }
    let follow = flags & AT_SYMLINK_NOFOLLOW == 0;
    match base_ino(dirfd, &path)? {
        Some((dir, dir_path)) => vfs::lookup_at(&dir, &dir_path, &path, follow).map(|(p, _)| p),
        None => vfs::lookup(&proc::current().files().cwd.clone(), &path, follow).map(|(p, _)| p),
    }
}

/// 作る・消す・名前を変えるところ (vfs::parent_last)。空のパスは dirfd を見る前に ENOENT (Linux の getname)
fn last_at(dirfd: i64, path: &str) -> Result<vfs::Parent, i64> {
    let p = last_at_dead(dirfd, path)?;
    p.alive()?;
    Ok(p)
}

/// last_at と同じだが、親が消したディレクトリでも (dead) 答える: rename は . .. / の EBUSY と EXDEV を、名前を引く
/// (ENOENT) より先に見る
fn last_at_dead(dirfd: i64, path: &str) -> Result<vfs::Parent, i64> {
    if path.is_empty() {
        return Err(-ENOENT);
    }
    let start = base_ino(dirfd, path)?;
    vfs::parent_last(start, &proc::current().files().cwd.clone(), path)
}

/// 作る・消すところの親のディレクトリを statc の表に覚えさせる: 次からは、大きなロックなしの作る・消す (fastdir.rs) が
/// それで親を引く
fn learn_parent(p: &vfs::Parent) {
    crate::statc::learn_dir(&alloc::format!("/{}", dir_of(&p.full)), &p.dir, mgtime(&p.dir));
}

/// 作るもの (mknod、symlink、link の新しい名前) の最後の名前: . と .. と / はもうあるもの (EEXIST)。name/ は
/// ディレクトリでないものには使えない (Linux の filename_create: あれば EEXIST、なければ ENOENT)。あれば EEXIST
/// (書けなくても、砂場でも: mkdir -p や create_dir_all がそれで「ある」とわかる)
fn new_name(p: &vfs::Parent) -> Result<(), i64> {
    if p.last != vfs::Last::Norm {
        return Err(-EEXIST);
    }
    p.name_ok()?;
    let exists = p.dir.lookup(&p.name).is_ok();
    if exists {
        return Err(-EEXIST);
    }
    if p.slash {
        return Err(-ENOENT);
    }
    Ok(())
}

fn parent_at(dirfd: i64, pathp: usize) -> Result<(InodeRef, String), i64> {
    parent_full(dirfd, pathp).map(|(p, n, _)| (p, n))
}

/// parent_at と、作る・消すもののパス (先頭 / なし。landlock でくらべる)
fn parent_full(dirfd: i64, pathp: usize) -> Result<(InodeRef, String, String), i64> {
    let path = user_str(pathp)?;
    let base = base_dir(dirfd, &path)?;
    let r = vfs::parent_path(&base, &path)?;
    // 作る・消す・名前を変えるところ: 読むだけのマウントの下なら EROFS
    vfs::check_writable(&r.2)?;
    Ok(r)
}

/// dirfd + path が読むだけのマウントの下なら EROFS (chmod、chown、utimensat、truncate)
fn writable_at(dirfd: i64, pathp: usize, flags: u64) -> Result<(), i64> {
    vfs::check_writable(&old_path(dirfd, pathp, flags)?)
}

/// fd の開いているファイルが読むだけのマウントの下なら EROFS
fn writable_fd(fd: u64) -> Result<(), i64> {
    match &file_of(fd)?.borrow().kind {
        Kind::Inode(_, p) => vfs::check_writable(p.trim_start_matches('/')),
        _ => Ok(()),
    }
}

/// 親ディレクトリのパス (先頭 / なし)
fn dir_of(full: &str) -> &str {
    full.rsplit_once('/').map_or("", |(d, _)| d)
}

/// 親ディレクトリに書ける (w と x) か
fn parent_writable(c: &Cred, parent: &InodeRef) -> Result<(), i64> {
    c.check(&parent.meta(), PW | PX)
}

/// sticky なディレクトリでは、消す/動かすのは持ち主 (ファイルかディレクトリ) か root だけ
fn sticky_ok(c: &Cred, parent: &InodeRef, name: &str) -> Result<(), i64> {
    let pm = parent.meta();
    if pm.mode & cred::S_ISVTX == 0 || c.euid == 0 || c.euid == pm.uid {
        return Ok(());
    }
    let child = parent.lookup(name)?;
    if child.meta().uid == c.euid { Ok(()) } else { Err(-cred::EPERM) }
}

/// 作ったものの持ち主を決める (setgid のディレクトリの下ならそのグループ)。ext4 は作るときに決めている
/// (Linux の inode_init_owner) ので、もう同じなら inode を書きなおさない
fn own_new(c: &Cred, parent: &InodeRef, ino: &InodeRef) -> Result<(), i64> {
    let pm = parent.meta();
    let gid = if pm.mode & cred::S_ISGID != 0 { pm.gid } else { c.egid };
    let m = ino.meta();
    if m.uid != c.euid || m.gid != gid {
        ino.set_owner(Some(c.euid), Some(gid))?;
    }
    if pm.mode & cred::S_ISGID != 0 && m.is_dir() && m.mode & cred::S_ISGID == 0 {
        ino.set_mode(m.mode | cred::S_ISGID)?;
    }
    Ok(())
}

/// 一度に運ぶ大きさ
const CHUNK: usize = 64 * 1024;

/// 中身を 0 にしない n バイト (すぐ全部書きこむか、書いた分だけ使うところ用)。ヒープが取れなければ ENOMEM
/// (panic しない: heap::try_vec)
fn uninit(n: usize) -> Result<Vec<u8>, i64> {
    crate::heap::try_vec(n, false)
}

/// 待つことのないもの (ふつうのファイル、/dev/zero など、ディスク): 頼まれた分を最後まで運ぶ (Linux と同じ)。
/// パイプや端末やソケットは、1 回分 (届いている分) だけ
fn whole(f: &FileRef) -> bool {
    matches!(f.borrow().kind, Kind::Inode(..) | Kind::Null | Kind::Zero | Kind::Random | Kind::Block(_) | Kind::Fb)
}

/// Linux の rw_verify_area: 位置のあるもの (ファイル、ディレクトリ) の位置 + 長さが i64 をこえるなら EINVAL
/// (ext4 のディレクトリを読み終えると位置は i64::MAX)。読み書きの口のしらべ (EBADF) のあと、中身の前に
fn verify_area(f: &FileRef, len: usize) -> Result<(), i64> {
    let b = f.borrow();
    if matches!(b.kind, Kind::Inode(..)) && (b.offset as i64).checked_add(len.min(i64::MAX as usize) as i64).is_none() {
        return Err(crate::why::inval());
    }
    Ok(())
}

pub fn read(fd: u64, buf: usize, len: usize) -> R {
    read_flags(fd, buf, len, 0, false, len)
}

// preadv2 / pwritev2 の flags (RWF_*)。Linux 7.2 の kiocb_set_rw_flags と同じ決まり (rwf_check)
pub const RWF_HIPRI: u32 = 0x01;
pub const RWF_DSYNC: u32 = 0x02;
pub const RWF_SYNC: u32 = 0x04;
pub const RWF_NOWAIT: u32 = 0x08;
pub const RWF_APPEND: u32 = 0x10;
pub const RWF_NOAPPEND: u32 = 0x20;
pub const RWF_ATOMIC: u32 = 0x40;
pub const RWF_DONTCACHE: u32 = 0x80;
pub const RWF_NOSIGNAL: u32 = 0x100;
const RWF_SUPPORTED: u32 = 0x1ff;

/// flags のしらべ (Linux の kiocb_set_rw_flags の順。ディレクトリなど read_iter のないものは do_loop_readv_writev)。
/// aios がえらんだちがい (pipe-nowait): パイプの NOWAIT は、splice や tee に使ったあとも効く (Linux は splice に使った
/// パイプの FMODE_NOWAIT を消し、それからは EOPNOTSUPP。aios のパイプは splice でも写しを入れるので、待つことがない)。
/// doc/aios.md の「aios がえらんだちがい」
/// 知らないものは EOPNOTSUPP、APPEND と NOAPPEND はいっしょに使えない (EINVAL)、NOWAIT は待たないでできるもの
/// (Linux の FMODE_NOWAIT: ext4 のファイル、パイプ、/dev/null /dev/zero /dev/random、ブロックデバイス、ソケット。
/// tmpfs や端末はちがう) だけ、ATOMIC は書くだけで、いまはできない、DONTCACHE は ext4 だけ (FOP_DONTCACHE)。
/// ext4 のバッファ書きは NOWAIT ではできない (ext4_buffered_write_iter)
fn rwf_check(f: &FileRef, flags: u32, write: bool) -> Result<(), i64> {
    const EOPNOTSUPP: i64 = 95;
    if flags == 0 {
        return Ok(());
    }
    let b = f.borrow();
    let (reg, dir, cached) = match &b.kind {
        Kind::Inode(i, _) => {
            let m = i.meta().mode & S_IFMT;
            (m == vfs::S_IFREG, m == vfs::S_IFDIR, i.page_cacheable())
        }
        _ => (false, false, false),
    };
    if dir && flags & !RWF_HIPRI != 0 {
        return Err(-EOPNOTSUPP);
    }
    if flags & !RWF_SUPPORTED != 0 {
        return Err(-EOPNOTSUPP);
    }
    if flags & RWF_APPEND != 0 && flags & RWF_NOAPPEND != 0 {
        return Err(crate::why::inval());
    }
    if flags & RWF_NOWAIT != 0 {
        let ok = match &b.kind {
            Kind::Inode(..) => reg && cached,
            Kind::PipeRead(_) | Kind::PipeWrite(_) | Kind::PipeRw(_) | Kind::Unix(_) | Kind::Inet(_) | Kind::Null | Kind::Zero | Kind::Random | Kind::Block(_) => true,
            _ => false,
        };
        if !ok {
            return Err(-EOPNOTSUPP);
        }
    }
    if flags & RWF_ATOMIC != 0 {
        return Err(-EOPNOTSUPP);
    }
    if flags & RWF_DONTCACHE != 0 && !(reg && cached) {
        return Err(-EOPNOTSUPP);
    }
    if write && flags & RWF_NOWAIT != 0 && reg {
        return Err(-EOPNOTSUPP);
    }
    Ok(())
}

/// AF_UNIX のソケットなら、その中身
fn unix_of(f: &FileRef) -> Option<crate::unix::UnixRef> {
    match &f.borrow().kind {
        Kind::Unix(u) => Some(u.clone()),
        _ => None,
    }
}

/// AF_INET の TCP / UDP のソケットなら、その中身
fn inet_of(f: &FileRef) -> Option<crate::inet::InetRef> {
    match &f.borrow().kind {
        Kind::Inet(u) => Some(u.clone()),
        _ => None,
    }
}

/// ソケット (sockfs の inode) の持ち主と許可 (fstat、fchmod、fchown、utimensat)
enum SockFs {
    Unix(crate::unix::UnixRef),
    Inet(crate::inet::InetRef),
}

impl SockFs {
    fn of(f: &FileRef) -> Option<SockFs> {
        match &f.borrow().kind {
            Kind::Unix(u) => Some(SockFs::Unix(u.clone())),
            Kind::Inet(u) => Some(SockFs::Inet(u.clone())),
            _ => None,
        }
    }

    fn owner_mode(&self) -> (u32, u32, u32) {
        match self {
            SockFs::Unix(u) => crate::unix::owner_mode(u),
            SockFs::Inet(u) => crate::inet::owner_mode(u),
        }
    }

    fn set_owner_mode(&self, uid: Option<u32>, gid: Option<u32>, mode: u32) {
        match self {
            SockFs::Unix(u) => crate::unix::set_owner_mode(u, uid, gid, mode),
            SockFs::Inet(u) => crate::inet::set_owner_mode(u, uid, gid, mode),
        }
    }

    fn touch(&self, mtime: Option<u64>) {
        match self {
            SockFs::Unix(u) => crate::unix::touch(u, mtime),
            SockFs::Inet(u) => crate::inet::touch(u, mtime),
        }
    }
}

/// パイプや socketpair にいま中身があるか (ほかの流れ (端末など) は、あるものとして)
fn stream_has_data(f: &FileRef) -> bool {
    match &f.borrow().kind {
        Kind::PipeRead(p) | Kind::PipeRw(p) => p.borrow().len() > 0,
        Kind::Unix(u) => crate::unix::pending(u) > 0,
        Kind::Inet(u) => crate::inet::pending(u) > 0,
        _ => true,
    }
}

/// ユーザーの va から len バイトに書けるか (読んで同じものを書きもどす)
fn probe_writable(va: usize, len: usize) -> bool {
    let pt = proc::current().pt();
    let mut page = [0u8; 256];
    let mut at = va;
    let end = va.saturating_add(len);
    while at < end {
        let n = (end - at).min(page.len()).min(crate::memlayout::PGSIZE - at % crate::memlayout::PGSIZE);
        if pt.copy_in(&mut page[..n], at).is_none() || pt.copy_out(at, &page[..n]).is_none() {
            return false;
        }
        // ページの残りは同じ答え: 次のページへ
        at = (at + n).max((at / crate::memlayout::PGSIZE + 1) * crate::memlayout::PGSIZE).min(end.max(at + n));
    }
    true
}

/// NOWAIT (noio) と DONTCACHE の区間で f を
fn read_wrapped<T>(flags: u32, f: impl FnOnce() -> T) -> T {
    match (flags & RWF_NOWAIT != 0, flags & RWF_DONTCACHE != 0) {
        (true, true) => proc::noio(|| proc::dontcache(f)),
        (true, false) => proc::noio(f),
        (false, true) => proc::dontcache(f),
        (false, false) => f(),
    }
}

/// 書く: DONTCACHE の区間で f を (書いたページをページキャッシュに残さない: vm::file_written。Linux は書き出したあとに捨てる)
fn write_wrapped<T>(flags: u32, f: impl FnOnce() -> T) -> T {
    if flags & RWF_DONTCACHE != 0 { proc::dontcache(f) } else { f() }
}

/// ページキャッシュを通すふつうのファイルなら、その inode
fn cached_inode(f: &FileRef) -> Option<InodeRef> {
    match &f.borrow().kind {
        Kind::Inode(i, _) if i.page_cacheable() && i.meta().mode & S_IFMT == vfs::S_IFREG => Some(i.clone()),
        _ => None,
    }
}

/// read の中身。flags は preadv2 の RWF_* (NOWAIT: ふつうのファイルはキャッシュになければ EAGAIN。そのときそのページを
/// 読んでおく (Linux は先読みを出す) ので、次は読める。DONTCACHE: 新しく読んだものを残さない)。dontwait は readv の
/// 2 つめからの入れ物 (パイプなら、1 つめに入った分で返る: Linux の pipe_read)
/// RWF_NOWAIT の読みがキャッシュになくて EAGAIN になったとき (Linux の先読みのかわり): pos から len バイト (128 KiB まで)
/// のページをみな読んでおく (次の NOWAIT で読める。ディスクでつながっていないページも)
fn nowait_kick(ino: &InodeRef, pos: usize, len: usize) {
    let mut one = [0u8; 1];
    let end = pos.saturating_add(len.min(128 * 1024));
    let mut p = pos;
    while p < end {
        if !matches!(file::read_sleepable(ino, p, &mut one), Ok(1)) {
            break;
        }
        p = (p / 4096 + 1) * 4096;
    }
}

/// ra: NOWAIT で読めなかったときに読んでおく長さ (readv / preadv2 は入れ物みなの長さ: Linux の先読みは頼まれた全部)
fn read_flags(fd: u64, buf: usize, len: usize, flags: u32, dontwait: bool, ra: usize) -> R {
    const EAGAIN: i64 = 11;
    let f = file_of(fd)?;
    if f.borrow().readable() {
        verify_area(&f, len)?;
    }
    if let Some(u) = unix_of(&f) {
        return crate::socket::unix_readv(&f, &u, &[(buf, len)], flags & RWF_NOWAIT != 0 || dontwait);
    }
    if let Some(u) = inet_of(&f) {
        return crate::socket::inet_readv(&f, &u, &[(buf, len)], flags & RWF_NOWAIT != 0 || dontwait);
    }
    // パイプから 0 バイト: 何もないときも待たずに 0 (Linux の pipe_read)
    if len == 0 && matches!(f.borrow().kind, Kind::PipeRead(_) | Kind::PipeRw(_)) {
        return if f.borrow().readable() { Ok(0) } else { Err(-EBADF) };
    }
    let whole = whole(&f);
    // ふつうのファイルは、読んだ分 (n) をかならず埋める (穴も 0 で)。パイプと socketpair も取りだした分だけ。
    // これらは 0 で埋めておかなくてよい (IPC の小さな read のたびに 64 KiB を埋めると重い)
    let inode = matches!(f.borrow().kind, Kind::Inode(..) | Kind::PipeRead(_) | Kind::PipeRw(_) | Kind::Unix(_));
    let nowait = flags & RWF_NOWAIT != 0 || dontwait;
    let file = if flags & (RWF_NOWAIT | RWF_DONTCACHE) != 0 { cached_inode(&f) } else { None };
    // パイプなどから取り出したものは戻せないので、中身があるなら、入れ物にさわれるかを先に確かめる (さわれなければ
    // EFAULT で、中身はそのまま。空なら read のとおり EAGAIN か待つ)。aios がえらんだちがい (pipe-fault): さわれる分は
    // みな取り出してその数を返す。Linux はパイプの箱ごとに写し、途中でさわれなくなった箱は写した分も数えず残す
    let stream = !matches!(f.borrow().kind, Kind::Inode(..) | Kind::Null | Kind::Zero | Kind::Random | Kind::Block(_) | Kind::Fb);
    let mut done = 0;
    loop {
        let want = (len - done).min(CHUNK);
        let mut tmp = crate::heap::try_vec(want, !inode)?;
        if stream && stream_has_data(&f) && !probe_writable(buf + done, want) {
            if done == 0 {
                return Err(-EFAULT);
            }
            break;
        }
        let r = match &file {
            Some(ino) => {
                let pos = f.borrow().offset;
                let r = read_wrapped(flags, || file::read_opt(&f, &mut tmp, nowait));
                if flags & RWF_NOWAIT != 0 && r == Err(-EAGAIN) {
                    nowait_kick(ino, pos, ra.max(len) - done);
                }
                r
            }
            None => file::read_opt(&f, &mut tmp, nowait),
        };
        let n = match r {
            Ok(n) => n,
            Err(e) if done == 0 => return Err(e),
            Err(_) => break,
        };
        if let Err(e) = out(buf + done, &tmp[..n]) {
            // 入れ物に書けない (EFAULT): ふつうのファイルなら読んだことにしない (Linux はさわれる分だけ進む)
            if matches!(f.borrow().kind, Kind::Inode(..)) {
                f.borrow_mut().offset -= n;
            }
            if done == 0 {
                return Err(e);
            }
            break;
        }
        done += n;
        if !whole || n < tmp.len() || done >= len {
            break;
        }
    }
    Ok(done as i64)
}

pub fn write(fd: u64, buf: usize, len: usize) -> R {
    write_flags(fd, buf, len, 0, None)
}

/// write の中身。append: Some(true) で終わりに (RWF_APPEND)、Some(false) で O_APPEND でも位置に (RWF_NOAPPEND)。
/// RWF_NOSIGNAL: 読み手のいないパイプでも SIGPIPE を送らない。RWF_DSYNC / SYNC: 書けたらディスクに書く。
/// RWF_NOWAIT: パイプなら待たない (ふつうのファイルは rwf_check で EOPNOTSUPP)
fn write_flags(fd: u64, buf: usize, len: usize, flags: u32, append: Option<bool>) -> R {
    let f = file_of(fd)?;
    if f.borrow().writable() {
        verify_area(&f, len)?;
    }
    if let Some(u) = unix_of(&f) {
        return crate::socket::unix_writev(&f, &u, &[(buf, len)], flags & RWF_NOWAIT != 0, flags & RWF_NOSIGNAL != 0);
    }
    if let Some(u) = inet_of(&f) {
        return crate::socket::inet_writev(&f, &u, &[(buf, len)], flags & RWF_NOWAIT != 0, flags & RWF_NOSIGNAL != 0);
    }
    let whole = whole(&f);
    // 読み手のいないパイプは、入れ物を写す前に EPIPE (Linux の pipe_write: さわれない入れ物でも EPIPE が先。0 バイトは 0)
    let dead_pipe = len > 0
        && match &f.borrow().kind {
            Kind::PipeWrite(p) | Kind::PipeRw(p) => p.borrow().readers() == 0,
            _ => false,
        };
    if dead_pipe {
        if flags & RWF_NOSIGNAL == 0 {
            let info = crate::signal::SigInfo::from(crate::signal::SI_KERNEL);
            crate::signal::send_thread(proc::current(), crate::signal::SIGPIPE, info);
        }
        return Err(-file::EPIPE);
    }
    // ふつうのファイルは、入れ物を写す前に setuid / setgid を落とす (Linux の file_modified は generic_perform_write の
    // 前: さわれない入れ物で EFAULT になっても落ちている。pwrite も同じ)
    let ino = match &f.borrow().kind {
        Kind::Inode(i, _) if len > 0 && f.borrow().writable() => Some(i.clone()),
        _ => None,
    };
    if let Some(i) = ino {
        vfs::drop_privs(&i)?;
    }
    let mut done = 0;
    loop {
        let mut tmp = uninit((len - done).min(CHUNK))?;
        proc::current().pt().copy_in(&mut tmp, buf + done).ok_or(-EFAULT)?;
        let r = file::write_opt2(&f, &tmp, flags & RWF_NOWAIT != 0, append);
        // AF_UNIX のソケットは unix.rs で (SOCK_STREAM だけ: Linux の unix_stream_sendmsg)
        if r == Err(-file::EPIPE) && flags & RWF_NOSIGNAL == 0 && !matches!(f.borrow().kind, Kind::Unix(_) | Kind::Inet(_)) {
            // 読み手のいないパイプ: SIGPIPE (既定なら EL0 へ戻るときに終わる)
            let info = crate::signal::SigInfo::from(crate::signal::SI_KERNEL);
            crate::signal::send_thread(proc::current(), crate::signal::SIGPIPE, info);
        }
        let n = match r {
            Ok(n) => n,
            Err(e) if done == 0 => return Err(e),
            Err(_) => break,
        };
        done += n;
        if !whole || n < tmp.len() || done >= len {
            break;
        }
    }
    if done > 0 && flags & (RWF_DSYNC | RWF_SYNC) != 0 {
        vfs::sync_all_wait();
    }
    Ok(done as i64)
}

/// pread64 / pwrite64 (と preadv / pwritev) の確かめ。Linux の ksys_pread64 と vfs_read と同じ順に: 位置が負なら
/// EINVAL、fd がない (O_PATH も) なら EBADF、位置のないもの (パイプ、端末、ソケットなど) は ESPIPE、読めない
/// (書けない) 口なら EBADF
fn positional(fd: u64, off: i64, write: bool) -> Result<FileRef, i64> {
    if off < 0 {
        return Err(crate::why::inval());
    }
    let f = file_of(fd)?;
    {
        let b = f.borrow();
        if b.flags & file::O_PATH != 0 {
            return Err(-EBADF);
        }
        if !matches!(b.kind, Kind::Inode(..) | Kind::Block(_) | Kind::Fb | Kind::Null | Kind::Zero | Kind::Random) {
            return Err(-file::ESPIPE);
        }
        if !(if write { b.writable() } else { b.readable() }) {
            return Err(-EBADF);
        }
    }
    Ok(f)
}

/// ふつうのファイルなら、その inode と開いたときのパス (inotify)
fn inode_path(f: &FileRef) -> Option<(InodeRef, String)> {
    match &f.borrow().kind {
        Kind::Inode(i, p) => Some((i.clone(), p.clone())),
        _ => None,
    }
}

pub fn pread(fd: u64, buf: usize, len: usize, off: i64) -> R {
    pread_flags(fd, buf, len, off, 0, len)
}

/// pread の中身 (flags は read_flags と同じ)
fn pread_flags(fd: u64, buf: usize, len: usize, off: i64, flags: u32, ra: usize) -> R {
    const EAGAIN: i64 = 11;
    let f = positional(fd, off, false)?;
    let ino = inode_path(&f);
    if ino.as_ref().is_some_and(|(i, _)| i.meta().mode & S_IFMT == vfs::S_IFDIR) {
        return Err(-EISDIR);
    }
    // 頼まれた分を終わりまで、64 KiB ずつ (read と同じ。Linux も、ふつうのファイルでは短く返さない)。
    // ふつうのファイルは、開いたファイルを借りずに読む (ディスクを眠って待つことがある)
    let mut done = 0;
    while done < len {
        let mut tmp = uninit((len - done).min(CHUNK))?;
        let pos = off as usize + done;
        let r = match &ino {
            Some((i, _)) if i.page_cacheable() => {
                let r = read_wrapped(flags, || file::read_sleepable(i, pos, &mut tmp));
                // NOWAIT でキャッシュになかった: 読もうとしたところを読んでおいて (Linux の先読みのかわり)、EAGAIN
                if flags & RWF_NOWAIT != 0 && r == Err(-EAGAIN) {
                    nowait_kick(i, pos, ra.max(len) - done);
                }
                r
            }
            Some((i, _)) => i.read_at(pos, &mut tmp),
            None => file::pread_dev(&f, pos, &mut tmp),
        };
        let n = match r {
            Ok(n) => n,
            Err(e) if done == 0 => return Err(e),
            Err(_) => break,
        };
        if out(buf + done, &tmp[..n]).is_err() {
            if done == 0 {
                return Err(-EFAULT);
            }
            break;
        }
        done += n;
        if n < tmp.len() {
            break;
        }
    }
    if let Some((i, path)) = &ino
        && done > 0
    {
        inotify::file_event(path, i, inotify::IN_ACCESS);
    }
    Ok(done as i64)
}

pub fn pwrite(fd: u64, buf: usize, len: usize, off: i64) -> R {
    pwrite_flags(fd, buf, len, off, 0)
}

/// pwrite の中身 (RWF_DSYNC / SYNC: 書けたらディスクに書く)
fn pwrite_flags(fd: u64, buf: usize, len: usize, off: i64, flags: u32) -> R {
    let f = positional(fd, off, true)?;
    // 開いたときのパス (inotify のため) は写さない: 書けたあとに借りる
    let ino = match &f.borrow().kind {
        Kind::Inode(i, _) => Some(i.clone()),
        _ => None,
    };
    // aios がえらんだちがい (pwrite-append): O_APPEND で開いたものにも off に書く (POSIX)。Linux は O_APPEND なら
    // off を見ずに終わりに書く (pwrite(2) の BUGS が POSIX とちがうと書いている)。doc/aios.md の「aios がえらんだちがい」
    if let Some(i) = &ino
        && len > 0
    {
        vfs::drop_privs(i)?;
    }
    let mut done = 0;
    while done < len {
        let mut tmp = uninit((len - done).min(CHUNK))?;
        proc::current().pt().copy_in(&mut tmp, buf + done).ok_or(-EFAULT)?;
        let r = match &ino {
            Some(i) => {
                let pos = off as usize + done;
                vfs::write_sealed(i, pos, tmp.len()).and_then(|_| i.write_at(pos, &tmp))
            }
            None => file::pwrite_dev(&f, off as usize + done, &tmp),
        };
        let n = match r {
            Ok(n) => n,
            Err(e) if done == 0 => return Err(e),
            Err(_) => break,
        };
        done += n;
        if n < tmp.len() {
            break;
        }
    }
    if let Some(i) = &ino
        && done > 0
        && let Kind::Inode(_, path) = &f.borrow().kind
    {
        inotify::file_event(path, i, inotify::IN_MODIFY);
    }
    if done > 0 && flags & (RWF_DSYNC | RWF_SYNC) != 0 {
        vfs::sync_all_wait();
    }
    Ok(done as i64)
}

/// fsync / fdatasync / syncfs (fs): 書き残しをみな書く (aios はファイルごとに分けない)。Linux と同じく、
/// fd がない (O_PATH も) なら EBADF、fsync のできないもの (パイプ、端末、ソケット、/dev/null など) は EINVAL
/// (syncfs はそのファイルシステムを書くだけなので、よい)
pub fn fsync(fd: u64, fs: bool) -> R {
    let f = file_of(fd)?;
    {
        let b = f.borrow();
        if b.flags & file::O_PATH != 0 {
            return Err(-EBADF);
        }
        if !fs && !matches!(b.kind, Kind::Inode(..) | Kind::Block(_)) {
            return Err(crate::why::inval());
        }
    }
    vfs::sync_all_wait();
    Ok(0)
}

fn iovecs(iov: usize, cnt: usize) -> Result<Vec<(usize, usize)>, i64> {
    if cnt > 1024 {
        return Err(crate::why::inval());
    }
    let mut v = Vec::with_capacity(cnt);
    for i in 0..cnt {
        let mut b = [0u8; 16];
        proc::current().pt().copy_in(&mut b, iov + i * 16).ok_or(-EFAULT)?;
        v.push((
            u64::from_le_bytes(b[..8].try_into().unwrap()) as usize,
            u64::from_le_bytes(b[8..].try_into().unwrap()) as usize,
        ));
    }
    Ok(v)
}

/// iovec の長さの和 (1 つが i64 をこえれば EINVAL: Linux の iovec_from_user)
fn iov_total(v: &[(usize, usize)]) -> Result<usize, i64> {
    let mut t: usize = 0;
    for &(_, l) in v {
        if l > i64::MAX as usize {
            return Err(crate::why::inval());
        }
        t = t.saturating_add(l);
    }
    Ok(t)
}

pub fn writev(fd: u64, iov: usize, cnt: usize) -> R {
    writev2(fd, iov, cnt, 0)
}

pub fn readv(fd: u64, iov: usize, cnt: usize) -> R {
    readv2(fd, iov, cnt, 0)
}

/// readv (preadv2 の off が -1 のときも): Linux の do_readv → vfs_readv の順 (EBADF、iovec の EINVAL / EFAULT、長さ 0 なら
/// flags も見ずに 0、位置のしらべ、flags、読む)。入れ物ごとに読み、2 つめからは待たない (パイプは 1 つめに入った分で)。
/// しくじっても、それまでに読めた分があればそれ (さわれない入れ物の EFAULT など)
fn readv2(fd: u64, iov: usize, cnt: usize, flags: u32) -> R {
    let f = file_of(fd)?;
    if !f.borrow().readable() {
        return Err(-EBADF);
    }
    let v = iovecs(iov, cnt)?;
    let total = iov_total(&v)?;
    if total == 0 {
        return Ok(0);
    }
    verify_area(&f, total)?;
    rwf_check(&f, flags, false)?;
    // AF_UNIX のソケットは、入れ物みなで 1 回受ける (dgram の 1 つのメッセージ)
    if let Some(u) = unix_of(&f) {
        return crate::socket::unix_readv(&f, &u, &v, flags & RWF_NOWAIT != 0);
    }
    if let Some(u) = inet_of(&f) {
        return crate::socket::inet_readv(&f, &u, &v, flags & RWF_NOWAIT != 0);
    }
    let mut done: usize = 0;
    for (base, len) in v {
        if len == 0 {
            continue;
        }
        match read_flags(fd, base, len, flags, done > 0, total - done) {
            Ok(n) => {
                done += n as usize;
                if (n as usize) < len {
                    break;
                }
            }
            Err(e) if done == 0 => return Err(e),
            Err(_) => break,
        }
    }
    Ok(done as i64)
}

/// writev (pwritev2 の off が -1 のときも): RWF_APPEND なら終わりに、RWF_NOAPPEND なら O_APPEND でも位置に。
/// aios がえらんだちがい (pipe-fault): 途中の入れ物にさわれなければ、そこまでの分をみな書いてその数を返す (なにも
/// 書けなければ EFAULT)。Linux のパイプは 4 KiB のページごとに写し、足りないページは捨てる (7 バイトなら EFAULT、
/// 12000 バイトなら 4096)。読むほうは read_flags。doc/aios.md の「aios がえらんだちがい」
fn writev2(fd: u64, iov: usize, cnt: usize, flags: u32) -> R {
    let f = file_of(fd)?;
    if !f.borrow().writable() {
        return Err(-EBADF);
    }
    let v = iovecs(iov, cnt)?;
    let total = iov_total(&v)?;
    if total == 0 {
        return Ok(0);
    }
    verify_area(&f, total)?;
    rwf_check(&f, flags, true)?;
    // AF_UNIX のソケットは、入れ物みなを 1 回で送る (dgram の 1 つのメッセージ)
    if let Some(u) = unix_of(&f) {
        return crate::socket::unix_writev(&f, &u, &v, flags & RWF_NOWAIT != 0, flags & RWF_NOSIGNAL != 0);
    }
    if let Some(u) = inet_of(&f) {
        return crate::socket::inet_writev(&f, &u, &v, flags & RWF_NOWAIT != 0, flags & RWF_NOSIGNAL != 0);
    }
    let append = if flags & RWF_APPEND != 0 {
        Some(true)
    } else if flags & RWF_NOAPPEND != 0 {
        Some(false)
    } else {
        None
    };
    let done = write_wrapped(flags, || {
        let mut done: usize = 0;
        for (base, len) in v {
            if len == 0 {
                continue;
            }
            match write_flags(fd, base, len, flags & !(RWF_DSYNC | RWF_SYNC), append) {
                Ok(n) => {
                    done += n as usize;
                    if (n as usize) < len {
                        break;
                    }
                }
                Err(e) if done == 0 => return Err(e),
                Err(_) => break,
            }
        }
        Ok(done)
    })?;
    if done > 0 && flags & (RWF_DSYNC | RWF_SYNC) != 0 {
        vfs::sync_all_wait();
    }
    Ok(done as i64)
}

/// preadv / preadv2: off から順に、iovec ごとに pread。off が -1 なら、いまの位置で readv (Linux の do_preadv:
/// off < 0 は EINVAL、位置のないもの (パイプ) は ESPIPE、が先)
pub fn preadv2(fd: u64, iov: usize, cnt: usize, off: i64, flags: u32) -> R {
    if off == -1 {
        return readv2(fd, iov, cnt, flags);
    }
    let f = positional(fd, off, false)?;
    let v = iovecs(iov, cnt)?;
    let total = iov_total(&v)?;
    if total == 0 {
        return Ok(0);
    }
    verify_pos(off, total)?;
    rwf_check(&f, flags, false)?;
    let mut done: usize = 0;
    for (base, len) in v {
        if len == 0 {
            continue;
        }
        match pread_flags(fd, base, len, off + done as i64, flags, total - done) {
            Ok(n) => {
                done += n as usize;
                if (n as usize) < len {
                    break;
                }
            }
            Err(e) if done == 0 => return Err(e),
            Err(_) => break,
        }
    }
    Ok(done as i64)
}

/// pwritev / pwritev2。RWF_APPEND なら off は見ずに終わりに (Linux の generic_write_checks)。O_APPEND の fd でも off に
/// 書くのは pwrite と同じ (aios がえらんだちがい pwrite-append。Linux は RWF_NOAPPEND のときだけ)
pub fn pwritev2(fd: u64, iov: usize, cnt: usize, off: i64, flags: u32) -> R {
    if off == -1 {
        return writev2(fd, iov, cnt, flags);
    }
    let f = positional(fd, off, true)?;
    let v = iovecs(iov, cnt)?;
    let total = iov_total(&v)?;
    if total == 0 {
        return Ok(0);
    }
    verify_pos(off, total)?;
    rwf_check(&f, flags, true)?;
    let off = match inode_path(&f) {
        Some((i, _)) if flags & RWF_APPEND != 0 => i.meta().size as i64,
        _ => off,
    };
    let done = write_wrapped(flags, || {
        let mut done: usize = 0;
        for (base, len) in v {
            if len == 0 {
                continue;
            }
            match pwrite_flags(fd, base, len, off + done as i64, flags & !(RWF_DSYNC | RWF_SYNC)) {
                Ok(n) => {
                    done += n as usize;
                    if (n as usize) < len {
                        break;
                    }
                }
                Err(e) if done == 0 => return Err(e),
                Err(_) => break,
            }
        }
        Ok(done)
    })?;
    if done > 0 && flags & (RWF_DSYNC | RWF_SYNC) != 0 {
        vfs::sync_all_wait();
    }
    Ok(done as i64)
}

/// posix_fadvise (fadvise64): DONTNEED だけ本当にする (ページキャッシュと ext4 のブロックのキャッシュから、だれも写して
/// いない・まだ書いていないものを外す: そのあとの RWF_NOWAIT は EAGAIN)。ほかは Linux と同じく受けるだけ。
/// パイプには ESPIPE、知らない advice と負の off / len は EINVAL
pub fn fadvise(fd: u64, off: i64, len: i64, advice: u64) -> R {
    const POSIX_FADV_DONTNEED: u64 = 4;
    let f = file_of(fd)?;
    if matches!(f.borrow().kind, Kind::PipeRead(_) | Kind::PipeWrite(_) | Kind::PipeRw(_)) {
        return Err(-file::ESPIPE);
    }
    if off < 0 || len < 0 || advice > 5 {
        return Err(crate::why::inval());
    }
    if advice == POSIX_FADV_DONTNEED
        && let Some((i, _)) = inode_path(&f)
    {
        crate::vm::uncache(&i, off as usize, len as usize);
        i.uncache(off as u64, len as u64);
    }
    Ok(0)
}

/// openat のフラグ (Linux の build_open_how と build_open_flags): O_PATH なら、ほかは O_DIRECTORY、O_NOFOLLOW、
/// O_CLOEXEC だけ残す (知らないものは見ない: openat2 とちがう)。O_CREAT と O_DIRECTORY はいっしょに使えない
/// (O_TMPFILE も O_DIRECTORY をふくむので O_CREAT とは使えない)。O_TMPFILE は O_DIRECTORY とともに、書く口で。
/// パスも dirfd も見る前に確かめる (EINVAL が先)
fn open_flags(flags: u64) -> Result<u64, i64> {
    let flags = if flags & O_PATH != 0 { flags & (O_PATH | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC) } else { flags };
    if flags & (O_CREAT | O_DIRECTORY) == O_CREAT | O_DIRECTORY {
        return Err(crate::why::inval());
    }
    if flags & O_TMPFILE != 0 && (flags & O_DIRECTORY == 0 || flags as u32 & file::O_ACCMODE == file::O_RDONLY) {
        return Err(crate::why::inval());
    }
    Ok(flags)
}

pub fn openat(dirfd: i64, pathp: usize, flags: u64, mode: u64) -> R {
    let path = user_str(pathp)?;
    let flags = open_flags(flags)?;
    if path.is_empty() {
        return Err(-ENOENT);
    }
    // 開いているディレクトリからはそこから (ルートからたどりなおさない: 上のディレクトリが通れなくなっても、
    // 名前が変わっても。Linux と同じ)
    let start = base_ino(dirfd, &path)?;
    let cwd = proc::current().files().cwd.clone();
    let lookup = |follow: bool| match &start {
        Some((dir, dir_path)) => vfs::lookup_at(dir, dir_path, &path, follow),
        None => vfs::lookup(&cwd, &path, follow),
    };
    // O_CREAT と O_EXCL なら、最後のリンクはたどらない (リンクがあれば EEXIST。Linux と同じ)
    let follow = flags & O_NOFOLLOW == 0 && flags & (O_CREAT | O_EXCL) != O_CREAT | O_EXCL;
    if follow && let Some(f) = fd_link(&path) {
        let fd = proc::current().files().add(f, flags & O_CLOEXEC != 0, 0).ok_or(-EMFILE)?;
        return Ok(fd as i64);
    }
    // O_CREAT: 親までたどってから (そこまでのしくじりが先)、. .. / はもうあるディレクトリ (O_EXCL なら EEXIST、
    // でなければ EISDIR。あとに / があっても)、name/ は作らない (あってもなくても EISDIR)。Linux の
    // open_last_lookups (. と .. を先に見る) と do_open
    // そのあと最後の名前を 1 つずつ (リンクの中身の最後の名前も)。なければ作り、リンクの中身が name/ なら EISDIR
    // (O_CREAT と O_TMPFILE はいっしょに使えない: open_flags)
    if flags & O_CREAT != 0 {
        let p = vfs::parent_last(start, &cwd, &path)?;
        p.alive()?;
        if p.last != vfs::Last::Norm {
            return Err(if flags & O_EXCL != 0 { -EEXIST } else { -EISDIR });
        }
        if p.slash {
            return Err(-EISDIR);
        }
        learn_parent(&p);
        return open_found(create_target(p, follow)?, flags, mode);
    }
    // O_TMPFILE: そのディレクトリに名前のないファイル
    if flags & O_TMPFILE != 0 {
        let (full, dir) = lookup(flags & O_NOFOLLOW == 0)?;
        return open_tmpfile(full, dir, flags, mode);
    }
    // 大きなロックなしの open (file::fast_open) が引けるように、読むだけで開くものの答えを statc に覚えさせる
    // (newfstatat と同じく、開いているディレクトリからのものは覚えない。「ない」も)
    let learn = flags & !SIMPLE_OPEN == 0 && (dirfd == AT_FDCWD || path.starts_with('/'));
    let found = match lookup(follow) {
        Err(e) if e == -ENOENT && learn => {
            crate::statc::learn_missing(&path, true);
            return Err(e);
        }
        r => r?,
    };
    if learn && found.1.fast_stat() {
        crate::statc::learn(&path, true, &found.1, mgtime(&found.1));
    }
    open_found(Ok(found), flags, mode)
}

/// 大きなロックなしの open が開けるフラグ (O_RDONLY と、O_LARGEFILE、O_CLOEXEC、O_NOCTTY): file::fast_open
const SIMPLE_OPEN: u64 = 0o400000 | O_CLOEXEC | 0o400;

/// O_CREAT で開くもの: あるもの (パス, inode) か、作るところ (親, 名前, パス)。Linux の open_last_lookups と同じく、
/// 最後の名前がリンクなら (follow のとき) その中身をリンクのあるディレクトリからたどり、中身の最後の名前で
/// また同じことを (合わせて 40 まで)。中身の終わりが / なら引く前に EISDIR、. や .. ならディレクトリ (EISDIR)。
/// 指す先のないリンクなら、その指す先に作る
type Found = Result<(String, InodeRef), (InodeRef, String, String)>;

fn create_target(mut p: vfs::Parent, follow: bool) -> Result<Found, i64> {
    for _ in 0..40 {
        if p.last != vfs::Last::Norm {
            return Err(-EISDIR);
        }
        // name/ には作れない (あってもなくても EISDIR): たどったリンクの中身の終わりの / も (Linux の
        // open_last_lookups は、引く前に nd->last のあとの / を見る)
        if p.slash {
            return Err(-EISDIR);
        }
        p.alive()?;
        p.name_ok()?;
        let link = match p.dir.lookup(&p.name) {
            Ok(c) if follow && c.meta().mode & S_IFMT == vfs::S_IFLNK => c.readlink()?,
            Ok(c) => return Ok(Ok((p.full, c))),
            Err(e) if e == -ENOENT => return Ok(Err((p.dir, p.name, p.full))),
            Err(e) => return Err(e),
        };
        if link.is_empty() {
            return Err(-ENOENT);
        }
        p = vfs::parent_last(None, &alloc::format!("/{}", dir_of(&p.full)), &link)?;
        if p.slash {
            return Err(-EISDIR);
        }
    }
    Err(-vfs::ELOOP)
}

/// O_TMPFILE で開く (Linux の do_tmpfile と vfs_tmpfile): dir はディレクトリで、書けて通れること。
/// mode は umask をかけて (作ったばかりなので mode に関係なく開ける)。O_EXCL がなければ、あとで linkat
/// (AT_EMPTY_PATH か /proc/self/fd/N) で名前をつけられる
fn open_tmpfile(full: String, dir: InodeRef, flags: u64, mode: u64) -> R {
    if !dir.meta().is_dir() {
        return Err(-ENOTDIR);
    }
    vfs::check_writable(&full)?;
    let c = cred::current();
    parent_writable(&c, &dir)?;
    // 砂場: そのディレクトリの下にふつうのファイルを作り、読み書きする
    {
        use crate::landlock::*;
        let rw = if flags as u32 & file::O_ACCMODE == file::O_WRONLY { WRITE_FILE } else { READ_FILE | WRITE_FILE };
        check_fs(&full, MAKE_REG | rw)?;
    }
    let ino = dir.tmpfile(mode as u32 & 0o7777 & !umask(), flags & O_EXCL == 0)?;
    own_new(&c, &dir, &ino)?;
    let name = alloc::format!("{}/#{} (deleted)", full, ino.meta().ino);
    let f = file::new(Kind::Inode(ino, name), file_flags(flags));
    let fd = proc::current().files().add(f, flags & O_CLOEXEC != 0, 0).ok_or(-EMFILE)?;
    Ok(fd as i64)
}

// openat2 の open_how.flags に使えるもの (arm64 の値: O_ACCMODE と O_CREAT から __O_TMPFILE まで)。知らないものは EINVAL
// (openat は知らないものを見ない)
const VALID_OPEN_FLAGS: u64 = 0o37777703;
const O_PATH: u64 = 0o10000000;
const O_TMPFILE: u64 = 0o20000000;

/// openat2(dirfd, path, how, size): struct open_how { u64 flags; u64 mode; u64 resolve; } (版 0 は 24 バイト。後ろに増えた
/// ところは 0 でなければ E2BIG)。openat とちがい、知らない flags、O_CREAT (か O_TMPFILE) なしの mode、O_PATH と
/// いっしょに使えない flags は EINVAL。resolve (RESOLVE_*) があれば vfs::lookup_resolve でたどる (cap-std (uv など) は
/// RESOLVE_BENEATH で、ディレクトリの外に出ない open に使う)。RESOLVE_CACHED はいつも EAGAIN: 覚えだけで答えるとは
/// 約束しないので、呼んだほうが RESOLVE_CACHED なしでやりなおす (Linux でも覚えになければそう答える)
pub fn openat2(dirfd: i64, pathp: usize, howp: usize, size: usize) -> R {
    const E2BIG: i64 = 7;
    const EAGAIN: i64 = 11;
    const ENAMETOOLONG: i64 = 36;
    const VER0: usize = 24;
    if size < VER0 {
        return Err(crate::why::inval());
    }
    if size > 4096 {
        return Err(-E2BIG);
    }
    let mut b = vec![0u8; size];
    proc::current().pt().copy_in(&mut b, howp).ok_or(-EFAULT)?;
    if b[VER0..].iter().any(|&x| x != 0) {
        return Err(-E2BIG);
    }
    let word = |i: usize| u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
    let (flags, mode, resolve) = (word(0), word(1), word(2));
    if flags & !VALID_OPEN_FLAGS != 0 || mode & !0o7777 != 0 || resolve & !vfs::RESOLVE_ALL != 0 {
        return Err(crate::why::inval());
    }
    if mode != 0 && flags & (O_CREAT | O_TMPFILE) == 0 {
        return Err(crate::why::inval());
    }
    if flags & O_PATH != 0 && flags & !(O_PATH | O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC) != 0 {
        return Err(crate::why::inval());
    }
    open_flags(flags)?;
    if resolve & vfs::RESOLVE_BENEATH != 0 && resolve & vfs::RESOLVE_IN_ROOT != 0 {
        return Err(crate::why::inval());
    }
    if resolve & vfs::RESOLVE_CACHED != 0 {
        return Err(-EAGAIN);
    }
    if resolve == 0 {
        return openat(dirfd, pathp, flags, mode);
    }
    let path = user_str(pathp)?;
    if path.is_empty() {
        return Err(-ENOENT);
    }
    // 絶対パスでも dirfd は使う (IN_ROOT のルート)
    let start = start_dir(dirfd)?;
    // O_CREAT と O_EXCL なら、最後のリンクはたどらない (リンクがあれば EEXIST。Linux と同じ)
    let follow = flags & O_NOFOLLOW == 0 && !(flags & O_CREAT != 0 && flags & O_EXCL != 0);
    // O_CREAT で name/ は、親までたどって (RESOLVE_* もそこまでで)、そこを通れれば、最後の名前を見ずに EISDIR
    // (Linux の open_last_lookups)。. .. / は open_found で (O_EXCL なら EEXIST、でなければ EISDIR)
    if flags & O_CREAT != 0 && path.ends_with('/') {
        let trimmed = path.trim_end_matches('/');
        let (dpart, name) = match trimmed.rsplit_once('/') {
            Some(("", n)) => ("/", n),
            Some((d, n)) => (d, n),
            None => ("", trimmed),
        };
        if !trimmed.is_empty() && name != "." && name != ".." {
            let dir = if dpart.is_empty() {
                start.0.clone()
            } else {
                match vfs::lookup_resolve(start.clone(), dpart, true, resolve, false)? {
                    vfs::Resolved::Found(_, d) => d,
                    vfs::Resolved::Missing(..) => return Err(-ENOENT),
                }
            };
            if !dir.meta().is_dir() {
                return Err(-ENOTDIR);
            }
            cred::current().check(&dir.meta(), cred::X)?;
            return Err(-EISDIR);
        }
    }
    if flags & O_TMPFILE != 0 {
        return match vfs::lookup_resolve(start, &path, follow, resolve, false)? {
            vfs::Resolved::Found(full, dir) => open_tmpfile(full, dir, flags, mode),
            vfs::Resolved::Missing(..) => Err(-ENOENT),
        };
    }
    let found = match vfs::lookup_resolve(start, &path, follow, resolve, flags & O_CREAT != 0)? {
        vfs::Resolved::Found(full, ino) => Ok((full, ino)),
        vfs::Resolved::Missing(parent, dir, name) if flags & O_CREAT != 0 => {
            // name/ は作れない
            if path.ends_with('/') {
                return Err(-EISDIR);
            }
            if name.len() > 255 {
                return Err(-ENAMETOOLONG);
            }
            let full = if dir.is_empty() { name.clone() } else { alloc::format!("{}/{}", dir, name) };
            Err((parent, name, full))
        }
        vfs::Resolved::Missing(..) => return Err(-ENOENT),
    };
    open_found(found, flags, mode)
}

/// 開いたファイルに残すフラグ (F_GETFL で見えるもの): 開くときだけのもの (O_CREAT、O_EXCL、O_NOCTTY、O_TRUNC) と
/// fd のもの (O_CLOEXEC) は残さない (Linux の do_dentry_open)
pub(crate) fn file_flags(flags: u64) -> u32 {
    const O_NOCTTY: u64 = 0o400;
    (flags & !(O_CREAT | O_EXCL | O_NOCTTY | O_TRUNC | O_CLOEXEC)) as u32
}

/// dirfd のディレクトリ (inode, パス)。AT_FDCWD ならいまのディレクトリ
fn start_dir(dirfd: i64) -> Result<(InodeRef, String), i64> {
    if dirfd == AT_FDCWD {
        let cwd = proc::current().files().cwd.clone();
        return Ok((vfs::resolve("", &cwd, true)?, cwd));
    }
    let f = file_of(dirfd as u64)?;
    let f = f.borrow();
    match &f.kind {
        Kind::Inode(i, p) if i.meta().is_dir() => Ok((i.clone(), p.clone())),
        _ => Err(-ENOTDIR),
    }
}

/// openat と openat2 の残り: あったもの (パス, inode) を開くか、なかったもの (親, 名前, パス) を O_CREAT で作って開く
fn open_found(found: Found, flags: u64, mode: u64) -> R {
    let (full, ino) = match found {
        Ok(found) => {
            if flags & O_CREAT != 0 && flags & O_EXCL != 0 {
                return Err(-EEXIST);
            }
            // O_CREAT であるディレクトリを開くことはできない。O_DIRECTORY でディレクトリでないものは ENOTDIR
            // (どちらも許可を見る前: Linux の do_open)
            if flags & O_CREAT != 0 && found.1.meta().is_dir() {
                return Err(-EISDIR);
            }
            if flags & O_DIRECTORY != 0 && !found.1.meta().is_dir() {
                return Err(-ENOTDIR);
            }
            found
        }
        Err((parent, name, full)) => {
            vfs::check_writable(&full)?;
            crate::landlock::check_parent(&full, crate::landlock::MAKE_REG)?;
            if flags as u32 & file::O_ACCMODE != file::O_RDONLY {
                crate::landlock::check_fs(&full, crate::landlock::WRITE_FILE)?;
            }
            let c = cred::current();
            parent_writable(&c, &parent)?;
            let ino = parent.create(&name, mode as u32 & 0o7777 & !umask(), NewNode::File)?;
            own_new(&c, &parent, &ino)?;
            vfs::remember_new(&full, &parent, &name, &ino);
            inotify::dir_event(&parent, &name, inotify::IN_CREATE, 0);
            inotify::file_event(&full, &ino, inotify::IN_OPEN);
            // 作ったばかりのものは、mode に関係なく開ける
            let f = file::new(Kind::Inode(ino, full), file_flags(flags));
            let fd = proc::current().files().add(f, flags & O_CLOEXEC != 0, 0).ok_or(-EMFILE)?;
            return Ok(fd as i64);
        }
    };
    // O_PATH: 場所だけ。開く許可はいらず (たどれればよい)、デバイスや FIFO を開かず、リンクも (O_NOFOLLOW なら)
    // そのまま
    if flags & O_PATH != 0 {
        if flags & O_DIRECTORY != 0 && !ino.meta().is_dir() {
            return Err(-ENOTDIR);
        }
        let f = file::new(Kind::Inode(ino, full), file_flags(flags));
        let fd = proc::current().files().add(f, flags & O_CLOEXEC != 0, 0).ok_or(-EMFILE)?;
        return Ok(fd as i64);
    }
    let accmode = flags as u32 & file::O_ACCMODE;
    // ディレクトリを書く口で (O_TRUNC も書くこと) 開くのは、許可を見る前に EISDIR (Linux の may_open)
    if ino.meta().is_dir() && (accmode != file::O_RDONLY || flags & O_TRUNC != 0) {
        return Err(-EISDIR);
    }
    let want = match accmode {
        file::O_RDONLY => PR,
        file::O_WRONLY => PW,
        _ => PR | PW,
    } | if flags & O_TRUNC != 0 { PW } else { 0 };
    cred::current().check(&ino.meta(), want)?;
    // 砂場: ディレクトリは READ_DIR、ほか (デバイスも) は READ_FILE / WRITE_FILE、O_TRUNC は TRUNCATE
    {
        use crate::landlock::*;
        let a = if ino.meta().is_dir() {
            READ_DIR
        } else {
            (if want & PR != 0 { READ_FILE } else { 0 }) | (if accmode != file::O_RDONLY { WRITE_FILE } else { 0 }) | (if flags & O_TRUNC != 0 { TRUNCATE } else { 0 })
        };
        check_fs(&full, a)?;
    }
    let kind = match ino.meta().mode & S_IFMT {
        // ディレクトリは読むだけ (O_TRUNC も書くことになる: Linux の may_open)
        vfs::S_IFDIR => {
            if accmode != file::O_RDONLY || flags & O_TRUNC != 0 {
                return Err(-EISDIR);
            }
            None
        }
        _ if flags & O_DIRECTORY != 0 => return Err(-ENOTDIR),
        vfs::S_IFCHR => {
            let (ma, mi) = fs::dev_of(&ino).unwrap();
            Some(Kind::of_dev(ma, mi, flags as u32)?)
        }
        vfs::S_IFBLK => {
            let (ma, mi) = fs::dev_of(&ino).unwrap();
            Some(Kind::Block(crate::block::part_of_dev(ma, mi).ok_or(-6)?))
        }
        vfs::S_IFIFO => {
            let p = fifo_pipe(&ino);
            let (r, w) = match accmode {
                file::O_RDONLY => (true, false),
                file::O_WRONLY => (false, true),
                _ => (true, true),
            };
            Some(Pipe::open_fifo(&p, r, w, flags as u32 & O_NONBLOCK != 0)?)
        }
        vfs::S_IFLNK => return Err(-40), // ELOOP (O_NOFOLLOW)
        vfs::S_IFSOCK => return Err(-6),  // ENXIO (unix ソケットは開けない。connect する)
        _ => {
            // ふつうのファイルを書くために開く: 読むだけのマウントの下なら EROFS (デバイスはかまわない)。
            // 動かしているプログラムなら ETXTBSY (Linux の get_write_access)
            if accmode != file::O_RDONLY || flags & O_TRUNC != 0 {
                vfs::check_writable(&full)?;
                if proc::executing(ino.id()) {
                    return Err(-ETXTBSY);
                }
            }
            // O_TRUNC は O_RDONLY でも切りつめる (書く許可は上で確かめた。Linux の do_open と同じ)
            if flags & O_TRUNC != 0 {
                vfs::drop_privs(&ino)?;
                ino.truncate(0)?;
                inotify::file_event(&full, &ino, inotify::IN_MODIFY);
            }
            None
        }
    };
    if kind.is_none() {
        inotify::file_event(&full, &ino, inotify::IN_OPEN);
    }
    // 閉じたら棚にしまってよいもの (file::Shelf): ext4 のふつうのファイルを、読むだけの口でほかの印なしに
    let reuse = kind.is_none() && flags & !SIMPLE_OPEN == 0 && ino.meta().mode & S_IFMT == vfs::S_IFREG && ino.fast_stat();
    let kind = kind.unwrap_or(Kind::Inode(ino, full));
    let f = file::new(kind, file_flags(flags));
    if reuse {
        file::reuse_on(&f);
    }
    let fd = proc::current().files().add(f, flags & O_CLOEXEC != 0, 0).ok_or(-EMFILE)?;
    Ok(fd as i64)
}

pub fn close(fd: u64) -> R {
    let p = proc::current().files();
    if p.get(fd).is_none() {
        return Err(-EBADF);
    }
    // 閉じたもの (最後の参照なら OpenFile も) は、表を変え終わってから落とす
    let old = p.set(fd as usize, None);
    drop(old);
    // 棚のもので、マウントが変わる前のもの (もう開きなおせない) を捨てる
    file::shelf_prune(p);
    Ok(0)
}

pub fn lseek(fd: u64, off: i64, whence: u64) -> R {
    let f = file_of(fd)?;
    if f.borrow().flags & file::O_PATH != 0 {
        return Err(-EBADF);
    }
    // whence は、位置のないもの (ESPIPE) より先に (Linux の ksys_lseek)
    if whence > 4 {
        return Err(crate::why::inval());
    }
    let n = f.borrow_mut().lseek(off, whence as u32)?;
    Ok(n as i64)
}

/// /dev/stdin や /proc/self/fd/N で、開いているものが名前のないもの (パイプ、ソケット、端末): そのもの [quirk tty-root-fdlink]。
/// 開くと stat はそれを分ける (Linux はパイプを開きなおし、端末はログインした人のもの。aios の端末は
/// root のものなので、開きなおすと書けない)。ほかの場所では探さない (ふつうの open を遅くしない)
fn fd_link(path: &str) -> Option<file::FileRef> {
    if !(path.starts_with("/dev/") || path.starts_with("/proc/")) || crate::parts::off(crate::parts::Part::TtyRootFdlink) {
        return None;
    }
    crate::procfs::fd_link_file("", path)
}

pub fn fstat(fd: u64, st: usize) -> R {
    let f = file_of(fd)?;
    let s = f.borrow().stat();
    out(st, &s.to_bytes())?;
    Ok(0)
}

pub fn newfstatat(dirfd: i64, pathp: usize, st: usize, flags: u64) -> R {
    let path = user_str(pathp)?;
    if path.is_empty() && flags & AT_EMPTY_PATH != 0 && dirfd != AT_FDCWD {
        return fstat(dirfd as u64, st);
    }
    if flags & AT_SYMLINK_NOFOLLOW == 0 && let Some(f) = fd_link(&path) {
        out(st, &f.borrow().stat().to_bytes())?;
        return Ok(0);
    }
    // 大きなロックなしの stat の覚えに (statc.rs。開いているディレクトリからのものは覚えない。「ない」も)
    let learn = (dirfd == AT_FDCWD || path.starts_with('/')) && flags & !(AT_SYMLINK_NOFOLLOW | 0x800) == 0;
    let follow = flags & AT_SYMLINK_NOFOLLOW == 0;
    let ino = match at_path(dirfd, &path, flags) {
        Err(e) if e == -ENOENT && learn => {
            crate::statc::learn_missing(&path, follow);
            return Err(e);
        }
        r => r?,
    };
    let s = Stat::of_inode(&ino);
    out(st, &s.to_bytes())?;
    if learn && ino.fast_stat() {
        crate::statc::learn(&path, follow, &ino, mgtime(&ino));
    }
    Ok(0)
}

/// statx(2): newfstatat と同じものを struct statx で
/// Linux の multigrain の時刻のファイルシステム (ext4 と tmpfs)
fn mgtime(ino: &InodeRef) -> bool {
    const EXT4_SUPER_MAGIC: u64 = 0xef53;
    const TMPFS_MAGIC: u64 = 0x0102_1994;
    matches!(u64::from_le_bytes(ino.statfs()[0..8].try_into().unwrap()), EXT4_SUPER_MAGIC | TMPFS_MAGIC)
}

pub fn statx(dirfd: i64, pathp: usize, flags: u64, mask: u32, buf: usize) -> R {
    const AT_NO_AUTOMOUNT: u64 = 0x800;
    const AT_STATX_SYNC_TYPE: u64 = 0x6000;
    const STATX__RESERVED: u32 = 0x8000_0000;
    // Linux の statx の順: mask の予約のビットと、SYNC の 2 つともと、知らないフラグ (EINVAL)。パスのまちがい
    // (空なのに AT_EMPTY_PATH がない ENOENT、読めない EFAULT) は、getname が持ってきてそのあと。それからたどる。
    // AT_EMPTY_PATH なら NULL でもよい
    let path = if pathp == 0 && flags & AT_EMPTY_PATH != 0 { Ok(String::new()) } else { user_str(pathp) };
    if mask & STATX__RESERVED != 0 || flags & AT_STATX_SYNC_TYPE == AT_STATX_SYNC_TYPE {
        return Err(crate::why::inval());
    }
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_NO_AUTOMOUNT | AT_EMPTY_PATH | AT_STATX_SYNC_TYPE) != 0 {
        return Err(crate::why::inval());
    }
    let path = path?;
    if path.is_empty() && flags & AT_EMPTY_PATH == 0 {
        return Err(-ENOENT);
    }
    let of_file = |f: &FileRef| {
        let b = f.borrow();
        let mg = matches!(&b.kind, Kind::Inode(i, _) if mgtime(i));
        (b.stat(), mg)
    };
    let (s, mg) = if path.is_empty() {
        if dirfd == AT_FDCWD {
            let i = vfs::resolve("", &proc::current().files().cwd.clone(), true)?;
            (Stat::of_inode(&i), mgtime(&i))
        } else {
            of_file(&file_of(dirfd as u64)?)
        }
    } else if flags & AT_SYMLINK_NOFOLLOW == 0
        && let Some(f) = fd_link(&path)
    {
        of_file(&f)
    } else {
        // 大きなロックなしの statx の覚えに (statc.rs。newfstatat と同じ)
        let (learn, follow) = (dirfd == AT_FDCWD || path.starts_with('/'), flags & AT_SYMLINK_NOFOLLOW == 0);
        let i = match at_path(dirfd, &path, flags & AT_SYMLINK_NOFOLLOW) {
            Err(e) if e == -ENOENT && learn => {
                crate::statc::learn_missing(&path, follow);
                return Err(e);
            }
            r => r?,
        };
        let (s, mg) = (Stat::of_inode(&i), mgtime(&i));
        out(buf, &s.to_statx(mask, mg))?;
        if learn && i.fast_stat() {
            crate::statc::learn(&path, follow, &i, mg);
        }
        return Ok(0);
    };
    out(buf, &s.to_statx(mask, mg))?;
    Ok(0)
}

/// access(2): 実 uid/gid で確かめる (AT_EACCESS なら実効)
pub fn faccessat(dirfd: i64, pathp: usize, mode: u64, flags: u64) -> R {
    const AT_EACCESS: u64 = 0x200;
    let ino = at(dirfd, pathp, flags & AT_SYMLINK_NOFOLLOW)?;
    let want = (mode & 7) as u32;
    let (c, real) = (cred::current(), flags & AT_EACCESS == 0);
    if want != 0 && !c.may(&ino.meta(), want, real) {
        crate::why::perm(&ino.meta(), want, c, real);
        return Err(-cred::EACCES);
    }
    Ok(0)
}

pub fn readlinkat(dirfd: i64, pathp: usize, buf: usize, len: usize) -> R {
    // 大きさは int (0 以下は、パスを見る前に EINVAL)
    if len == 0 || len > i32::MAX as usize {
        return Err(crate::why::inval());
    }
    let path = user_str(pathp)?;
    // 空のパスは dirfd そのもの (O_PATH|O_NOFOLLOW で開いたリンク)。リンクでなければ ENOENT (Linux の do_readlinkat)
    let ino = if path.is_empty() {
        let i = if dirfd == AT_FDCWD {
            vfs::resolve("", &proc::current().files().cwd.clone(), true)?
        } else {
            match inode_of(dirfd as u64) {
                Err(e) if e == -EINVAL => return Err(-ENOENT),
                r => r?,
            }
        };
        if i.meta().mode & S_IFMT != vfs::S_IFLNK {
            return Err(-ENOENT);
        }
        i
    } else {
        at_path(dirfd, &path, AT_SYMLINK_NOFOLLOW)?
    };
    let t = ino.readlink().map_err(|e| if e == -EINVAL { crate::why::not_link(&path) } else { e })?;
    let n = t.len().min(len);
    out(buf, &t.as_bytes()[..n])?;
    Ok(n as i64)
}

pub fn getdents64(fd: u64, buf: usize, len: usize) -> R {
    let f = file_of(fd)?;
    if f.borrow().flags & file::O_PATH != 0 {
        return Err(-EBADF);
    }
    let mut v = Vec::new();
    f.borrow_mut().getdents(&mut v, len)?;
    out(buf, &v)?;
    Ok(v.len() as i64)
}

pub fn mkdirat(dirfd: i64, pathp: usize, mode: u64) -> R {
    let path = user_str(pathp)?;
    let p = last_at(dirfd, &path)?;
    // . と .. と / はもうある。name/ はよい (ディレクトリを作る)。Linux と同じく、あれば書けなくても (砂場の
    // landlock でも、読むだけのマウントでも) EEXIST (mkdir -p や create_dir_all がそれで「ある」とわかる。前は
    // landlock が先で EACCES: aibox の中の aish の write が、書けるディレクトリに書けなかった)
    if p.last != vfs::Last::Norm {
        return Err(-EEXIST);
    }
    p.name_ok()?;
    if p.dir.lookup(&p.name).is_ok() {
        return Err(-EEXIST);
    }
    let (parent, name, full) = (p.dir, p.name, p.full);
    vfs::check_writable(&full)?;
    crate::landlock::check_parent(&full, crate::landlock::MAKE_DIR)?;
    let c = cred::current();
    parent_writable(&c, &parent)?;
    // setuid と setgid は落とす (Linux の vfs_mkdir は S_IRWXUGO と S_ISVTX だけ。setgid は親が setgid なら own_new が)
    let ino = parent.create(&name, mode as u32 & 0o1777 & !umask(), NewNode::Dir)?;
    own_new(&c, &parent, &ino)?;
    vfs::remember_new(&full, &parent, &name, &ino);
    inotify::dir_event(&parent, &name, inotify::IN_CREATE | inotify::IN_ISDIR, 0);
    Ok(0)
}

/// swapon / swapoff の path: ブロックデバイスのノードの区画か、ふつうのファイル。と、その名前
fn swap_source(pathp: usize) -> Result<(crate::swap::Source, String), i64> {
    use crate::swap::Source;
    if cred::current().euid != 0 {
        return Err(-cred::EPERM);
    }
    let path = user_str(pathp)?;
    let ino = at(AT_FDCWD, pathp, 0)?;
    match ino.meta().mode & S_IFMT {
        vfs::S_IFBLK => {
            let (ma, mi) = fs::dev_of(&ino).ok_or_else(|| crate::why::inval())?;
            let p = crate::block::part_of_dev(ma, mi).ok_or(-6)?;
            Ok((Source::Part(p), crate::block::part_name(&p)))
        }
        vfs::S_IFREG => {
            let name = if path.starts_with('/') { path } else { alloc::format!("/{}/{}", proc::current().files().cwd, path).replace("//", "/") };
            Ok((Source::File(ino), name))
        }
        _ => Err(crate::why::inval()),
    }
}

pub fn swapon(pathp: usize) -> R {
    let (src, name) = swap_source(pathp)?;
    crate::swap::on(src, name).map(|_| 0)
}

pub fn swapoff(pathp: usize) -> R {
    let (src, _) = swap_source(pathp)?;
    crate::swap::off(&src).map(|_| 0)
}

pub fn mknodat(dirfd: i64, pathp: usize, mode: u64, dev: u64) -> R {
    let path = user_str(pathp)?;
    let p = last_at(dirfd, &path)?;
    // あれば landlock より先に EEXIST (Linux と同じ)
    new_name(&p)?;
    let (parent, name, full) = (p.dir, p.name, p.full);
    vfs::check_writable(&full)?;
    let mode = mode as u32;
    crate::landlock::check_parent(&full, crate::landlock::make_right(mode))?;
    let node = match mode & S_IFMT {
        vfs::S_IFIFO => NewNode::Fifo,
        vfs::S_IFCHR => NewNode::Dev((dev >> 8) as u32 & 0xfff, (dev & 0xff) as u32),
        vfs::S_IFBLK => NewNode::Blk((dev >> 8) as u32 & 0xfff, (dev & 0xff) as u32),
        0 | vfs::S_IFREG => NewNode::File,
        _ => return Err(crate::why::inval()),
    };
    let c = cred::current();
    if matches!(node, NewNode::Dev(..) | NewNode::Blk(..)) && c.euid != 0 {
        return Err(-cred::EPERM);
    }
    // Linux と同じく、あれば書けなくても EEXIST (mkdir -p や create_dir_all がそれで「ある」とわかる)
    if parent.lookup(&name).is_ok() {
        return Err(-EEXIST);
    }
    parent_writable(&c, &parent)?;
    let ino = parent.create(&name, mode & 0o7777 & !umask(), node)?;
    own_new(&c, &parent, &ino)?;
    inotify::dir_event(&parent, &name, inotify::IN_CREATE, 0);
    Ok(0)
}

/// unix ソケットの bind (Linux の unix_bind_bsd): mknod と同じく最後の名前を作る (S_IFSOCK。許可はソケットの許可
/// (fchmod で変わる。はじめは 0777) から umask を引いたもの)。あれば EADDRINUSE (Linux と同じく、前のものが残って
/// いれば、使う側が unlink してから bind する)。作った inode (ソケットが持っておく: 名前を消されても、つなぐ相手は
/// この inode で見つける) と、その親と名前 (unbind_sock 用)
pub fn bind_sock(path: &str, mode: u32) -> Result<(InodeRef, InodeRef, String), i64> {
    const EADDRINUSE: i64 = 98;
    let p = last_at(AT_FDCWD, path)?;
    new_name(&p).map_err(|e| if e == -EEXIST { -EADDRINUSE } else { e })?;
    let (parent, name, full) = (p.dir, p.name, p.full);
    vfs::check_writable(&full)?;
    crate::landlock::check_parent(&full, crate::landlock::MAKE_SOCK)?;
    let c = cred::current();
    parent_writable(&c, &parent)?;
    let ino = parent.create(&name, mode & 0o7777 & !umask(), NewNode::Sock)?;
    own_new(&c, &parent, &ino)?;
    inotify::dir_event(&parent, &name, inotify::IN_CREATE, 0);
    Ok((ino, parent, name))
}

/// bind_sock で作った名前を消す (もう名前のあるソケットだった: Linux の unix_bind_bsd の vfs_unlink。許可は見ない)
pub fn unbind_sock(parent: &InodeRef, name: &str) {
    if parent.unlink(name, false).is_ok() {
        inotify::dir_event(parent, name, inotify::IN_DELETE, 0);
    }
}

/// unix ソケットの connect / sendto の相手 (Linux の unix_find_bsd): パスをたどり (リンクも)、書ける (W) ものか。
/// ソケットのファイルでなくても、ここでは inode を返す (ECONNREFUSED は呼ぶほうで)
pub fn find_sock(path: &str) -> Result<InodeRef, i64> {
    let (_, i) = vfs::lookup(&proc::current().files().cwd.clone(), path, true)?;
    proc::current_cred_ref().check(&i.meta(), PW)?;
    Ok(i)
}

pub fn unlinkat(dirfd: i64, pathp: usize, flags: u64) -> R {
    if flags & !AT_REMOVEDIR != 0 {
        return Err(crate::why::inval());
    }
    let path = user_str(pathp)?;
    let p = last_at(dirfd, &path)?;
    if p.last == vfs::Last::Norm && !p.slash {
        learn_parent(&p);
    }
    // 最後が . .. / なら (Linux の do_rmdir と do_unlinkat): rmdir は .. が ENOTEMPTY、. が EINVAL、/ が EBUSY。
    // unlink はどれも EISDIR
    match (p.last, flags & AT_REMOVEDIR != 0) {
        (vfs::Last::Norm, _) => {}
        (vfs::Last::DotDot, true) => return Err(-ENOTEMPTY),
        (vfs::Last::Dot, true) => return Err(crate::why::inval()),
        (vfs::Last::Root, true) => return Err(-EBUSY),
        (_, false) => return Err(-EISDIR),
    }
    vfs::check_writable(&p.full)?;
    p.name_ok()?;
    // unlink の name/: なければ ENOENT、ディレクトリなら EISDIR、ほか (リンクも) は ENOTDIR (Linux の slashes)
    if p.slash && flags & AT_REMOVEDIR == 0 {
        return Err(match p.dir.lookup(&p.name) {
            Err(e) => e,
            Ok(i) if i.meta().is_dir() => -EISDIR,
            Ok(_) => -ENOTDIR,
        });
    }
    let (parent, name, full) = (p.dir, p.name, p.full);
    crate::landlock::check_parent(&full, if flags & AT_REMOVEDIR != 0 { crate::landlock::REMOVE_DIR } else { crate::landlock::REMOVE_FILE })?;
    let c = cred::current();
    parent_writable(&c, &parent)?;
    sticky_ok(&c, &parent, &name)?;
    // 消えたものへの inotify のため。見張っている人がいるときだけ持っておく (持っていると ext4 は、
    // 消したファイルをまだ使われているものとして orphan のリストに入れ、手を放したときに片づけることになる)
    let victim = if inotify::active() { parent.lookup(&name).ok() } else { None };
    let held = vfs::forget_path(&full);
    parent.unlink(&name, flags & AT_REMOVEDIR != 0)?;
    if let Some(id) = held {
        vfs::forget_gone(id);
    }
    let dir = flags & AT_REMOVEDIR != 0;
    // Linux と同じ順: 消えたもの (リンクの数が変わった IN_ATTRIB、なくなれば DELETE_SELF と IGNORED)、それから親に DELETE
    if let Some(v) = victim {
        if !dir {
            inotify::self_event(&v, inotify::IN_ATTRIB);
        }
        if dir || v.meta().nlink == 0 {
            inotify::gone(&v);
        }
    }
    inotify::dir_event(&parent, &name, inotify::IN_DELETE | if dir { inotify::IN_ISDIR } else { 0 }, 0);
    Ok(0)
}

pub fn symlinkat(targetp: usize, dirfd: i64, pathp: usize) -> R {
    let target = user_str(targetp)?;
    // 中身が空のリンクは作れない (Linux の getname)
    if target.is_empty() {
        return Err(-ENOENT);
    }
    let path = user_str(pathp)?;
    let p = last_at(dirfd, &path)?;
    // Linux と同じく、あれば書けなくても (landlock でも) EEXIST
    new_name(&p)?;
    let (parent, name, full) = (p.dir, p.name, p.full);
    vfs::check_writable(&full)?;
    crate::landlock::check_parent(&full, crate::landlock::MAKE_SYM)?;
    let c = cred::current();
    parent_writable(&c, &parent)?;
    let ino = parent.create(&name, 0o777, NewNode::Symlink(target))?;
    own_new(&c, &parent, &ino)?;
    inotify::dir_event(&parent, &name, inotify::IN_CREATE, 0);
    Ok(0)
}

pub fn linkat(olddir: i64, oldp: usize, newdir: i64, newp: usize, flags: u64) -> R {
    if flags & !(AT_SYMLINK_FOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(crate::why::inval());
    }
    let follow = if flags & AT_SYMLINK_FOLLOW != 0 { 0 } else { AT_SYMLINK_NOFOLLOW };
    // AT_EMPTY_PATH で fd が inode でないもの (パイプなど) は、Linux では別のマウント (pipefs) のもの: None
    let other = flags & AT_EMPTY_PATH != 0 && olddir != AT_FDCWD && user_str(oldp)?.is_empty() && !matches!(file_of(olddir as u64)?.borrow().kind, Kind::Inode(..));
    let ino = if other { None } else { Some(at(olddir, oldp, follow | (flags & AT_EMPTY_PATH))?) };
    // Linux の順: 元をたどり、新しい名前の親とその名前 (あれば EEXIST)、ファイルシステムがちがえば EXDEV、
    // ディレクトリは最後に EPERM (vfs_link)
    let path = user_str(newp)?;
    let p = last_at(newdir, &path)?;
    new_name(&p)?;
    let Some(ino) = ino else { return Err(-vfs::EXDEV) };
    if p.dir.id().0 != ino.id().0 {
        return Err(-vfs::EXDEV);
    }
    if ino.meta().is_dir() {
        return Err(-vfs::EPERM);
    }
    let (parent, name, full) = (p.dir, p.name, p.full);
    vfs::check_writable(&full)?;
    // 砂場: 作るところの MAKE_*。ほかのディレクトリのものをつなぐなら、両方に REFER も
    {
        use crate::landlock::*;
        check_parent(&full, make_right(ino.meta().mode))?;
        let src = old_path(olddir, oldp, follow | (flags & AT_EMPTY_PATH))?;
        if dir_of(&src) != dir_of(&full) {
            check_parent(&src, REFER)?;
            check_parent(&full, REFER)?;
        }
    }
    parent_writable(&cred::current(), &parent)?;
    parent.link(&name, &ino)?;
    inotify::dir_event(&parent, &name, inotify::IN_CREATE, 0);
    inotify::self_event(&ino, inotify::IN_ATTRIB);
    Ok(0)
}

pub fn renameat(olddir: i64, oldp: usize, newdir: i64, newp: usize, flags: u64) -> R {
    const RENAME_NOREPLACE: u64 = 1;
    const RENAME_EXCHANGE: u64 = 2;
    if flags & !(RENAME_NOREPLACE | RENAME_EXCHANGE) != 0 || flags & (RENAME_NOREPLACE | RENAME_EXCHANGE) == RENAME_NOREPLACE | RENAME_EXCHANGE {
        return Err(crate::why::inval());
    }
    // Linux の do_renameat2 と同じ順に: 両方の親 (消したディレクトリでもまだよい)、ファイルシステムがちがえば EXDEV、
    // 最後が . .. / なら EBUSY (新しいほうは NOREPLACE なら EEXIST)、読むだけのマウントなら EROFS、それから名前を
    // 引く (消したディレクトリの中は ENOENT、元がなければ ENOENT)
    let (opath, npath) = (user_str(oldp)?, user_str(newp)?);
    let o = last_at_dead(olddir, &opath)?;
    let n = last_at_dead(newdir, &npath)?;
    if o.dir.id().0 != n.dir.id().0 {
        return Err(-vfs::EXDEV);
    }
    if o.last != vfs::Last::Norm {
        return Err(-EBUSY);
    }
    if n.last != vfs::Last::Norm {
        return Err(if flags & RENAME_NOREPLACE != 0 { -EEXIST } else { -EBUSY });
    }
    vfs::check_writable(&o.full)?;
    vfs::check_writable(&n.full)?;
    o.alive()?;
    o.name_ok()?;
    let moving = o.dir.lookup(&o.name)?;
    n.alive()?;
    n.name_ok()?;
    let target = n.dir.lookup(&n.name).ok();
    if flags & RENAME_NOREPLACE != 0 && target.is_some() {
        return Err(-EEXIST);
    }
    if flags & RENAME_EXCHANGE != 0 && target.is_none() {
        return Err(-ENOENT);
    }
    // 自分の下へは動かせない (EINVAL)。動かすものの上にあるものへは、上書きできない (ENOTEMPTY。Linux の lock_rename の
    // trap)。Linux 7 からは、名前を引いたすぐあと (__start_renaming) で、下の name/ のしらべより先
    let (odir, ndir) = (String::from(dir_of(&o.full)), String::from(dir_of(&n.full)));
    let under = |a: &str, b: &str| b == a || b.starts_with(&alloc::format!("{}/", a));
    if odir != ndir {
        if moving.meta().is_dir() && under(&o.full, &ndir) {
            return Err(crate::why::inval());
        }
        if target.is_some() && under(&n.full, &odir) {
            return Err(if flags & RENAME_EXCHANGE != 0 { -EINVAL } else { -ENOTEMPTY });
        }
    }
    // name/ はディレクトリのもの (EXCHANGE なら、それぞれがそれぞれの名前の)
    if flags & RENAME_EXCHANGE != 0 && n.slash && !target.as_ref().is_some_and(|t| t.meta().is_dir()) {
        return Err(-ENOTDIR);
    }
    if !moving.meta().is_dir() && (o.slash || (flags & RENAME_EXCHANGE == 0 && n.slash)) {
        return Err(-ENOTDIR);
    }
    // 同じもの (同じ inode を指す 2 つの名前) なら何もしない (Linux と同じ)
    if target.as_ref().is_some_and(|t| t.id() == moving.id()) {
        return Ok(0);
    }
    let (op, oname, ofull, np, nname, nfull) = (o.dir, o.name, o.full, n.dir, n.name, n.full);
    // 砂場: 元を消して、先に作る (先にあるものは消す)。ディレクトリをまたぐなら両方に REFER も
    {
        use crate::landlock::*;
        let kind = |m: u32| if m & S_IFMT == vfs::S_IFDIR { REMOVE_DIR } else { REMOVE_FILE };
        if let Ok(m) = op.lookup(&oname) {
            let mode = m.meta().mode;
            check_parent(&ofull, kind(mode))?;
            check_parent(&nfull, make_right(mode))?;
        }
        if let Ok(r) = np.lookup(&nname) {
            check_parent(&nfull, kind(r.meta().mode))?;
            // 入れかえなら、それも元のところへ作る
            if flags & RENAME_EXCHANGE != 0 {
                check_parent(&ofull, make_right(r.meta().mode))?;
            }
        }
        if dir_of(&ofull) != dir_of(&nfull) {
            check_parent(&ofull, REFER)?;
            check_parent(&nfull, REFER)?;
        }
    }
    if flags & RENAME_NOREPLACE != 0 && np.lookup(&nname).is_ok() {
        return Err(-EEXIST);
    }
    // Linux の vfs_rename と同じ順: 元を消せるか (親に書けて sticky も)、先に作れるか (あれば消せるか、種類が合うか)
    let c = cred::current();
    parent_writable(&c, &op)?;
    sticky_ok(&c, &op, &oname)?;
    parent_writable(&c, &np)?;
    if let Some(t) = &target {
        sticky_ok(&c, &np, &nname)?;
        if flags & RENAME_EXCHANGE == 0 {
            match (moving.meta().is_dir(), t.meta().is_dir()) {
                (true, false) => return Err(-ENOTDIR),
                (false, true) => return Err(-EISDIR),
                _ => {}
            }
        }
    }
    // ほかの親へ移るディレクトリは、その .. を書きかえるので、それ自身にも書けること (Linux の vfs_rename)
    if odir != ndir {
        if moving.meta().is_dir() {
            c.check(&moving.meta(), PW)?;
        }
        if flags & RENAME_EXCHANGE != 0
            && let Some(t) = &target
            && t.meta().is_dir()
        {
            c.check(&t.meta(), PW)?;
        }
    }
    if flags & RENAME_EXCHANGE != 0 {
        op.exchange(&oname, &np, &nname)?;
        let ck = inotify::cookie();
        for (from_dir, from, to_dir, to, ino) in [(&op, &oname, &np, &nname, &moving), (&np, &nname, &op, &oname, target.as_ref().unwrap())] {
            let isdir = if ino.meta().is_dir() { inotify::IN_ISDIR } else { 0 };
            inotify::dir_event(from_dir, from, inotify::IN_MOVED_FROM | isdir, ck);
            inotify::dir_event(to_dir, to, inotify::IN_MOVED_TO | isdir, ck);
            inotify::self_event(ino, inotify::IN_MOVE_SELF | isdir);
        }
        return Ok(0);
    }
    let moved = op.lookup(&oname).ok();
    let replaced = np.lookup(&nname).ok().filter(|r| moved.as_ref().is_none_or(|m| m.id() != r.id()));
    op.rename(&oname, &np, &nname)?;
    vfs::forget(&ofull);
    vfs::forget(&nfull);
    if let Some(m) = moved {
        let isdir = if m.meta().is_dir() { inotify::IN_ISDIR } else { 0 };
        let ck = inotify::cookie();
        inotify::dir_event(&op, &oname, inotify::IN_MOVED_FROM | isdir, ck);
        inotify::dir_event(&np, &nname, inotify::IN_MOVED_TO | isdir, ck);
        inotify::self_event(&m, inotify::IN_MOVE_SELF | isdir);
    }
    // 上書きされたもの
    if let Some(r) = replaced
        && (r.meta().is_dir() || r.meta().nlink == 0)
    {
        inotify::gone(&r);
    }
    Ok(0)
}

/// memfd_create: どこにも名前のない tmpfs のファイル (Wayland の共有メモリなど)
pub fn memfd_create(name: usize, flags: u64) -> R {
    use crate::tmpfs::{F_SEAL_EXEC, F_SEAL_SEAL};
    const MFD_CLOEXEC: u64 = 1;
    const MFD_ALLOW_SEALING: u64 = 2;
    const MFD_EXEC: u64 = 0x10;
    const MFD_NOEXEC_SEAL: u64 = 8;
    if flags & !(MFD_CLOEXEC | MFD_ALLOW_SEALING | MFD_EXEC | MFD_NOEXEC_SEAL) != 0 || flags & (MFD_EXEC | MFD_NOEXEC_SEAL) == MFD_EXEC | MFD_NOEXEC_SEAL {
        return Err(crate::why::inval());
    }
    let name = user_str(name)?;
    // 封: MFD_ALLOW_SEALING がなければ「もう封はつけられない」から。MFD_NOEXEC_SEAL は封ができて、実行できない
    let seals = if flags & MFD_NOEXEC_SEAL != 0 {
        F_SEAL_EXEC
    } else if flags & MFD_ALLOW_SEALING != 0 {
        0
    } else {
        F_SEAL_SEAL
    };
    let c = crate::cred::current();
    let ino = crate::tmpfs::anon_file(c.euid, c.egid, seals);
    let f = file::new(Kind::Inode(ino, alloc::format!("/memfd:{} (deleted)", name)), file::O_RDWR);
    let fd = proc::current().files().add(f, flags & MFD_CLOEXEC != 0, 0).ok_or(-EMFILE)?;
    Ok(fd as i64)
}

pub fn ftruncate(fd: u64, len: i64) -> R {
    // Linux の do_sys_ftruncate と同じ順: 長さが負なら EINVAL、fd がない (O_PATH も) なら EBADF、ふつうのファイルを
    // 書く口で開いたものでなければ EINVAL
    if len < 0 {
        return Err(crate::why::inval());
    }
    let f = file_of(fd)?;
    if f.borrow().flags & file::O_PATH != 0 {
        return Err(-EBADF);
    }
    let ino = match &f.borrow().kind {
        Kind::Inode(i, _) if i.meta().mode & S_IFMT == vfs::S_IFREG => i.clone(),
        _ => return Err(crate::why::inval()),
    };
    if !f.borrow().writable() {
        return Err(crate::why::inval());
    }
    writable_fd(fd)?;
    vfs::drop_privs(&ino)?;
    // ftruncate(2) は大きさが変わらなくても時刻を変える (Linux の do_truncate が ATTR_MTIME | ATTR_CTIME を渡す。
    // truncate(2) は渡さないので、tmpfs は大きさが変わらなければ変えない: tmpfs::truncate)
    let before = ino.meta().mtime;
    ino.truncate(len as usize)?;
    if ino.meta().mtime == before {
        ino.set_mtime(crate::timer::stamp_ns())?;
    }
    if let Kind::Inode(_, p) = &f.borrow().kind {
        inotify::file_event(p, &ino, inotify::IN_MODIFY);
    }
    Ok(0)
}

/// fallocate: mode 0 (場所を確保する) だけ。足りなければファイルを伸ばす (中身は 0、ページは使うときに)。
/// KEEP_SIZE (1) は何もしない。穴をあけるもの (PUNCH_HOLE など) はまだできない
pub fn fallocate(fd: u64, mode: u64, off: i64, len: i64) -> R {
    const EOPNOTSUPP: i64 = 95;
    const ENODEV: i64 = 19;
    const EFBIG: i64 = 27;
    const KEEP_SIZE: u32 = 0x01;
    const PUNCH_HOLE: u32 = 0x02;
    const COLLAPSE_RANGE: u32 = 0x08;
    const ZERO_RANGE: u32 = 0x10;
    const INSERT_RANGE: u32 = 0x20;
    const UNSHARE_RANGE: u32 = 0x40;
    const WRITE_ZEROES: u32 = 0x80;
    const MODE_MASK: u32 = PUNCH_HOLE | COLLAPSE_RANGE | ZERO_RANGE | INSERT_RANGE | UNSHARE_RANGE | WRITE_ZEROES;
    // Linux の順 (ksys_fallocate と vfs_fallocate): fd (O_PATH も EBADF)、範囲 (EINVAL)、mode の組 (EOPNOTSUPP。
    // PUNCH_HOLE は KEEP_SIZE といっしょだけ、COLLAPSE / INSERT / WRITE_ZEROES は KEEP_SIZE なしだけ)、書ける口か
    // (EBADF)、種類 (パイプは ESPIPE、ディレクトリは EISDIR、ほかのふつうのファイルでないものは ENODEV)、
    // 大きすぎ (EFBIG)、それからファイルシステム
    let f = file_of(fd)?;
    if f.borrow().flags & file::O_PATH != 0 {
        return Err(-EBADF);
    }
    if off < 0 || len <= 0 {
        return Err(crate::why::inval());
    }
    let mode = mode as u32;
    if mode & !(MODE_MASK | KEEP_SIZE) != 0 {
        return Err(-EOPNOTSUPP);
    }
    let ok = match mode & MODE_MASK {
        0 | UNSHARE_RANGE | ZERO_RANGE => true,
        PUNCH_HOLE => mode & KEEP_SIZE != 0,
        COLLAPSE_RANGE | INSERT_RANGE | WRITE_ZEROES => mode & KEEP_SIZE == 0,
        _ => false,
    };
    if !ok {
        return Err(-EOPNOTSUPP);
    }
    if !f.borrow().writable() {
        return Err(-EBADF);
    }
    match f.borrow().stat().mode & S_IFMT {
        vfs::S_IFIFO => return Err(-file::ESPIPE),
        vfs::S_IFDIR => return Err(-EISDIR),
        vfs::S_IFREG => {}
        _ => return Err(-ENODEV),
    }
    let end = off.checked_add(len).ok_or(-EFBIG)?;
    let ino = inode_of(fd)?;
    if end as u64 > ino.max_bytes() {
        return Err(-EFBIG);
    }
    writable_fd(fd)?;
    ino.fallocate(mode, off as u64, len as u64, &|| vfs::drop_privs(&ino))?;
    Ok(0)
}

pub fn truncate(pathp: usize, len: i64) -> R {
    if len < 0 {
        return Err(crate::why::inval());
    }
    let path = user_str(pathp)?;
    let (full, ino) = vfs::lookup(&proc::current().files().cwd.clone(), &path, true)?;
    // Linux の vfs_truncate の順: ディレクトリは EISDIR、ほかのふつうのファイルでないものは EINVAL、読むだけの
    // マウントなら EROFS、書けなければ EACCES、それから砂場
    match ino.meta().mode & S_IFMT {
        vfs::S_IFDIR => return Err(-EISDIR),
        vfs::S_IFREG => {}
        _ => return Err(crate::why::inval()),
    }
    vfs::check_writable(&full)?;
    cred::current().check(&ino.meta(), PW)?;
    // 動かしているプログラムは切りつめられない (Linux の vfs_truncate の get_write_access)
    if proc::executing(ino.id()) {
        return Err(-ETXTBSY);
    }
    crate::landlock::check_fs(&full, crate::landlock::TRUNCATE)?;
    vfs::drop_privs(&ino)?;
    ino.truncate(len as usize)?;
    inotify::self_event(&ino, inotify::IN_MODIFY);
    Ok(0)
}

/// 持ち主か root だけ。グループに入っていなければ setgid は落とす
fn chmod(ino: &InodeRef, mode: u64) -> R {
    let c = cred::current();
    let m = ino.meta();
    if !c.owns(&m) {
        return Err(-cred::EPERM);
    }
    let mut mode = mode as u32;
    if c.euid != 0 && !c.in_group(m.gid) {
        mode &= !cred::S_ISGID;
    }
    ino.set_mode(mode)?;
    Ok(0)
}

pub fn fchmod(fd: u64, mode: u64) -> R {
    let f = file_of(fd)?;
    if f.borrow().flags & file::O_PATH != 0 {
        return Err(-EBADF);
    }
    // ソケット: 持ち主か root (Linux の sockfs の inode。bind で作るファイルの許可にもなる)
    if let Kind::Unix(u) = &f.borrow().kind {
        return crate::unix::chmod(u, mode as u32);
    }
    if let Kind::Inet(u) = &f.borrow().kind {
        return crate::inet::chmod(u, mode as u32);
    }
    // パイプ: 持ち主か root が、fstat に見える許可を変えられる (Linux の pipefs の inode)
    if let Kind::PipeRead(p) | Kind::PipeWrite(p) | Kind::PipeRw(p) = &f.borrow().kind {
        use core::sync::atomic::Ordering::Relaxed;
        let c = cred::current();
        if c.euid != 0 && c.euid != p.uid.load(Relaxed) {
            return Err(-cred::EPERM);
        }
        p.perm.store(mode as u32 & 0o7777, Relaxed);
        return Ok(0);
    }
    writable_fd(fd)?;
    chmod(&inode_of(fd)?, mode)?;
    attrib_fd(fd);
    Ok(0)
}

pub fn fchmodat(dirfd: i64, pathp: usize, mode: u64) -> R {
    writable_at(dirfd, pathp, 0)?;
    chmod(&at(dirfd, pathp, 0)?, mode)?;
    attrib_at(dirfd, pathp);
    Ok(0)
}

/// fchmodat2 (Linux 6.6): fchmodat にフラグ。AT_SYMLINK_NOFOLLOW でリンクそのものは変えられない (EOPNOTSUPP。
/// Linux もリンクの許可は持たない)、AT_EMPTY_PATH なら dirfd そのもの
pub fn fchmodat2(dirfd: i64, pathp: usize, mode: u64, flags: u64) -> R {
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(crate::why::inval());
    }
    let path = user_str(pathp)?;
    if path.is_empty() && flags & AT_EMPTY_PATH != 0 {
        return fchmod(dirfd as u64, mode);
    }
    let ino = at_path(dirfd, &path, flags & AT_SYMLINK_NOFOLLOW)?;
    if ino.meta().mode & S_IFMT == vfs::S_IFLNK {
        return Err(-95); // EOPNOTSUPP
    }
    writable_at(dirfd, pathp, flags & AT_SYMLINK_NOFOLLOW)?;
    chmod(&ino, mode)?;
    attrib_at(dirfd, pathp);
    Ok(0)
}

/// 属性が変わった (inotify の IN_ATTRIB): Linux と同じく、入っているディレクトリ (名前つき)、それからそのもの
fn attrib_at(dirfd: i64, pathp: usize) {
    if inotify::active()
        && let Ok((parent, name)) = parent_at(dirfd, pathp)
        && let Ok(ino) = parent.lookup(&name)
    {
        let isdir = if ino.meta().is_dir() { inotify::IN_ISDIR } else { 0 };
        inotify::dir_event(&parent, &name, inotify::IN_ATTRIB | isdir, 0);
        inotify::self_event(&ino, inotify::IN_ATTRIB | isdir);
    }
}

fn attrib_fd(fd: u64) {
    if inotify::active()
        && let Ok(f) = file_of(fd)
        && let Kind::Inode(ino, p) = &f.borrow().kind
    {
        let isdir = if ino.meta().is_dir() { inotify::IN_ISDIR } else { 0 };
        if let Ok((parent, name)) = vfs::parent_of("", &alloc::format!("/{}", p)) {
            inotify::dir_event(&parent, &name, inotify::IN_ATTRIB | isdir, 0);
        }
        inotify::self_event(ino, inotify::IN_ATTRIB | isdir);
    }
}

fn id_arg(v: u64) -> Option<u32> {
    (v as u32 != u32::MAX).then_some(v as u32)
}

/// chown で変えたあとの mode (Linux の chown_common と setattr_prepare): root でなければ、uid は持ち主が自分のままに
/// するだけ、gid は持ち主が自分の入っているグループへだけ (EPERM)。ディレクトリでなければ、だれが変えても (変えなくても)
/// setuid を落とし、setgid はグループが実行できるか、変える人がそのグループでも root でもなければ落とす。落とすのも
/// mode を変えることなので、持ち主か root だけ (EPERM)
fn chown_mode(c: &Cred, (cur_uid, cur_gid, mode): (u32, u32, u32), uid: Option<u32>, gid: Option<u32>) -> Result<u32, i64> {
    if c.euid != 0 {
        if uid.is_some_and(|u| c.euid != cur_uid || u != cur_uid) {
            return Err(-cred::EPERM);
        }
        if gid.is_some_and(|g| c.euid != cur_uid || (g != cur_gid && !c.in_group(g))) {
            return Err(-cred::EPERM);
        }
    }
    if mode & S_IFMT == vfs::S_IFDIR {
        return Ok(mode);
    }
    let mut kill = cred::S_ISUID;
    if mode & cred::S_ISGID != 0 && (mode & 0o010 != 0 || !(c.euid == 0 || c.in_group(cur_gid))) {
        kill |= cred::S_ISGID;
    }
    if mode & kill != 0 && c.euid != 0 && c.euid != cur_uid {
        return Err(-cred::EPERM);
    }
    Ok(mode & !kill)
}

/// ユーザーの namespace の中なら、中の番号を外の番号に (地図になければ EINVAL)。-1 は None
fn chown_ids(uid: u64, gid: u64) -> Result<(Option<u32>, Option<u32>), i64> {
    let uid = id_arg(uid).map(crate::ns::take_uid).transpose()?;
    let gid = id_arg(gid).map(crate::ns::take_gid).transpose()?;
    Ok((uid, gid))
}

fn chown(ino: &InodeRef, uid: u64, gid: u64) -> R {
    let c = cred::current();
    let m = ino.meta();
    let (uid, gid) = chown_ids(uid, gid)?;
    let mode = chown_mode(&c, (m.uid, m.gid, m.mode), uid, gid)?;
    if uid.is_some() || gid.is_some() {
        ino.set_owner(uid, gid)?;
    }
    if mode != m.mode {
        ino.set_mode(mode & 0o7777)?;
    }
    Ok(0)
}

/// fd のもの (ファイルとパイプ。パイプは pipefs の inode の持ち主を変える)。O_PATH の fd は、fchown では EBADF、
/// fchownat の AT_EMPTY_PATH (パスでたどる) ではよい
fn chown_fd(fd: u64, uid: u64, gid: u64, opath_ok: bool) -> R {
    use core::sync::atomic::Ordering::Relaxed;
    let f = file_of(fd)?;
    if f.borrow().flags & file::O_PATH != 0 && !opath_ok {
        return Err(-EBADF);
    }
    if let Some(u) = SockFs::of(&f) {
        let (uid, gid) = chown_ids(uid, gid)?;
        let (cu, cg, cm) = u.owner_mode();
        let mode = chown_mode(&cred::current(), (cu, cg, vfs::S_IFSOCK | cm), uid, gid)?;
        u.set_owner_mode(uid, gid, mode);
        return Ok(0);
    }
    if let Some(p) = pipe_of(fd) {
        let (uid, gid) = chown_ids(uid, gid)?;
        let cur = (p.uid.load(Relaxed), p.gid.load(Relaxed), vfs::S_IFIFO | p.perm.load(Relaxed));
        let mode = chown_mode(&cred::current(), cur, uid, gid)?;
        if let Some(u) = uid {
            p.uid.store(u, Relaxed);
        }
        if let Some(g) = gid {
            p.gid.store(g, Relaxed);
        }
        p.perm.store(mode & 0o7777, Relaxed);
        p.ctime.store(crate::timer::stamp_ns(), Relaxed);
        return Ok(0);
    }
    writable_fd(fd)?;
    let ino = inode_of(fd)?;
    chown(&ino, uid, gid)?;
    attrib_fd(fd);
    Ok(0)
}

pub fn fchown(fd: u64, uid: u64, gid: u64) -> R {
    chown_fd(fd, uid, gid, false)
}

pub fn fchownat(dirfd: i64, pathp: usize, uid: u64, gid: u64, flags: u64) -> R {
    if flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(crate::why::inval());
    }
    let path = user_str(pathp)?;
    if path.is_empty() && flags & AT_EMPTY_PATH != 0 {
        if dirfd != AT_FDCWD {
            return chown_fd(dirfd as u64, uid, gid, true);
        }
        let cwd = proc::current().files().cwd.clone();
        vfs::check_writable(cwd.trim_start_matches('/'))?;
        return chown(&vfs::resolve("", &cwd, true)?, uid, gid);
    }
    writable_at(dirfd, pathp, flags)?;
    chown(&at(dirfd, pathp, flags)?, uid, gid)?;
    attrib_at(dirfd, pathp);
    Ok(0)
}

pub fn utimensat(dirfd: i64, pathp: usize, times: usize, flags: u64) -> R {
    const UTIME_NOW: i64 = (1 << 30) - 1;
    const UTIME_OMIT: i64 = (1 << 30) - 2;
    // Linux の utimensat の順: 時刻を写し、fd (パスが NULL) ならフラグは 0 だけ、パスなら AT_SYMLINK_NOFOLLOW と
    // AT_EMPTY_PATH だけ (EINVAL)。たどってから ns のまちがい (EINVAL。vfs_utimes)、読むだけのマウント (EROFS)、許可
    let t = if times == 0 {
        None
    } else {
        let mut b = [0u8; 32];
        proc::current().pt().copy_in(&mut b, times).ok_or(-EFAULT)?;
        let w = |i: usize| i64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
        Some([(w(0), w(1)), (w(2), w(3))])
    };
    // aios がえらんだちがい (utimensat-omit): どちらも OMIT でも、フラグと fd とパスはほかのときと同じにしらべる
    // (ないファイルは ENOENT、閉じた fd は EBADF。POSIX)。変えるものはないので、たどれたら許可や読むだけのマウントは
    // 見ずに 0。Linux はパスも見ずに 0 を返す (ないファイルでも)。doc/aios.md の「aios がえらんだちがい」
    let omit = t.is_some_and(|t| t[0].1 == UTIME_OMIT && t[1].1 == UTIME_OMIT);
    let null_fd = pathp == 0 && dirfd != AT_FDCWD;
    if null_fd && flags != 0 {
        return Err(crate::why::inval());
    }
    if !null_fd && flags & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(crate::why::inval());
    }
    let path = if null_fd { String::new() } else { user_str(pathp)? };
    // 空のパスで AT_EMPTY_PATH なら、パスでたどる fd (O_PATH でもよい)
    let by_fd = null_fd || (path.is_empty() && flags & AT_EMPTY_PATH != 0 && dirfd != AT_FDCWD);
    let (ino, full) = if by_fd {
        let f = file_of(dirfd as u64)?;
        if null_fd && f.borrow().flags & file::O_PATH != 0 {
            return Err(-EBADF);
        }
        if omit {
            return Ok(0);
        }
        // パイプとソケット (pipefs と sockfs の inode): 持ち主と許可は fstat に見せるもの
        let pipe = pipe_of(dirfd as u64);
        let sock = SockFs::of(&f);
        let owner = if let Some(p) = &pipe {
            use core::sync::atomic::Ordering::Relaxed;
            Some((vfs::S_IFIFO | p.perm.load(Relaxed), p.uid.load(Relaxed), p.gid.load(Relaxed)))
        } else {
            sock.as_ref().map(|u| {
                let (uid, gid, mode) = u.owner_mode();
                (vfs::S_IFSOCK | mode, uid, gid)
            })
        };
        if let Some((mode, uid, gid)) = owner {
            use core::sync::atomic::Ordering::Relaxed;
            let ns_ok = |ns: i64| ns == UTIME_NOW || ns == UTIME_OMIT || (0..1_000_000_000).contains(&ns);
            if t.is_some_and(|t| !ns_ok(t[0].1) || !ns_ok(t[1].1)) {
                return Err(crate::why::inval());
            }
            let c = cred::current();
            let m = vfs::Meta { ino: 0, mode, nlink: 1, uid, gid, size: 0, rdev: 0, blocks: 0, mtime: 0, ctime: 0 };
            let touch = t.is_none_or(|t| t[0].1 == UTIME_NOW && t[1].1 == UTIME_NOW);
            if !c.owns(&m) {
                if !touch {
                    return Err(-cred::EPERM);
                }
                if !c.may(&m, PW, false) {
                    return Err(-cred::EACCES);
                }
            }
            let now = crate::timer::stamp_ns();
            let mtime = match t.map(|t| t[1]) {
                Some((_, UTIME_OMIT)) => None,
                None | Some((_, UTIME_NOW)) => Some(now),
                Some((sec, ns)) => Some(sec.max(0) as u64 * 1_000_000_000 + ns as u64),
            };
            if let Some(p) = &pipe {
                if let Some(m) = mtime {
                    p.mtime.store(m, Relaxed);
                }
                p.ctime.store(now, Relaxed);
            } else if let Some(u) = &sock {
                u.touch(mtime);
            }
            return Ok(0);
        }
        let b = f.borrow();
        match &b.kind {
            Kind::Inode(i, p) => (i.clone(), String::from(p.trim_start_matches('/'))),
            _ => return Ok(0),
        }
    } else {
        let full = old_path(dirfd, pathp, flags)?;
        let ino = at_path(dirfd, &path, flags)?;
        if omit {
            return Ok(0);
        }
        (ino, full)
    };
    let ns_ok = |ns: i64| ns == UTIME_NOW || ns == UTIME_OMIT || (0..1_000_000_000).contains(&ns);
    if t.is_some_and(|t| !ns_ok(t[0].1) || !ns_ok(t[1].1)) {
        return Err(crate::why::inval());
    }
    vfs::check_writable(&full)?;
    // 許可: いまにするだけ (NULL か {NOW, NOW}) なら持ち主か書ける人 (EACCES)、ほかは持ち主か root (EPERM)
    let c = cred::current();
    let m = ino.meta();
    let touch = t.is_none_or(|t| t[0].1 == UTIME_NOW && t[1].1 == UTIME_NOW);
    if !c.owns(&m) {
        if !touch {
            return Err(-cred::EPERM);
        }
        if !c.may(&m, PW, false) {
            return Err(-cred::EACCES);
        }
    }
    let now = crate::timer::stamp_ns();
    let mtime = match t {
        None => Some(now),
        Some(t) => match t[1] {
            (_, UTIME_OMIT) => None,
            (_, UTIME_NOW) => Some(now),
            (sec, ns) => Some(sec.max(0) as u64 * 1_000_000_000 + ns as u64),
        },
    };
    if let Some(x) = mtime {
        ino.set_mtime(x)?;
    }
    if by_fd {
        attrib_fd(dirfd as u64);
    } else {
        attrib_at(dirfd, pathp);
    }
    Ok(0)
}

pub fn statfs(pathp: usize, buf: usize) -> R {
    let ino = at(AT_FDCWD, pathp, 0)?;
    out(buf, &ino.statfs())?;
    Ok(0)
}

pub fn fstatfs(fd: u64, buf: usize) -> R {
    let f = file_of(fd)?;
    let b = match &f.borrow().kind {
        Kind::Inode(i, _) => i.statfs(),
        _ => vfs::root().statfs(),
    };
    out(buf, &b)?;
    Ok(0)
}

pub fn dup(fd: u64) -> R {
    let f = file_of(fd)?;
    let n = proc::current().files().add(f, false, 0).ok_or(-EMFILE)?;
    Ok(n as i64)
}

pub fn dup3(old: u64, new: u64, flags: u64) -> R {
    // Linux と同じ順: フラグ (O_CLOEXEC だけ)、同じ fd、新しい fd の大きさ、それから古い fd
    if flags & !O_CLOEXEC != 0 || old == new {
        return Err(crate::why::inval());
    }
    let new = new as usize;
    if new >= proc::current().files().nofile {
        return Err(-EBADF);
    }
    let f = file_of(old)?;
    proc::current().files().set(new, Some(Fd { file: f, cloexec: flags & O_CLOEXEC != 0 }));
    Ok(new as i64)
}

pub fn fcntl(fd: u64, cmd: u64, arg: u64) -> R {
    let p = proc::current().files();
    let entry = p.fds.get_mut(fd as usize).and_then(|f| f.as_mut()).ok_or(-EBADF)?;
    // O_PATH の fd にできるのは、写す、FD_CLOEXEC、F_GETFL だけ (Linux の check_fcntl_cmd)
    if entry.file.borrow().flags & file::O_PATH != 0 && !matches!(cmd, F_DUPFD | F_DUPFD_CLOEXEC | F_GETFD | F_SETFD | F_GETFL) {
        return Err(-EBADF);
    }
    match cmd {
        F_DUPFD | F_DUPFD_CLOEXEC => {
            let f = entry.file.clone();
            let n = p.add(f, cmd == F_DUPFD_CLOEXEC, arg as usize).ok_or(-EMFILE)?;
            Ok(n as i64)
        }
        F_GETFD => Ok(if entry.cloexec { FD_CLOEXEC as i64 } else { 0 }),
        F_SETFD => {
            entry.cloexec = arg & FD_CLOEXEC != 0;
            Ok(0)
        }
        F_GETFL => Ok(entry.file.borrow().flags as i64),
        F_SETFL => {
            // 変えられるのは O_APPEND、O_NONBLOCK、O_DIRECT、O_NOATIME (Linux の SETFL_MASK)。O_NOATIME をつけるのは
            // 持ち主か root だけ (EPERM)、O_DIRECT はふつうのファイルとパイプだけ (EINVAL。setfl)
            const O_DIRECT: u32 = 0o200000;
            const O_NOATIME: u32 = 0o1000000;
            const MASK: u32 = file::O_APPEND | O_NONBLOCK | O_DIRECT | O_NOATIME;
            let arg = arg as u32;
            let mut f = entry.file.borrow_mut();
            let (owner, odirect) = match &f.kind {
                Kind::Inode(i, _) => (Some(i.meta().uid), i.meta().mode & S_IFMT == vfs::S_IFREG),
                Kind::PipeRead(p) | Kind::PipeWrite(p) | Kind::PipeRw(p) => (Some(p.uid.load(core::sync::atomic::Ordering::Relaxed)), true),
                Kind::Socket(_) => (Some(cred::current().euid), false),
                Kind::Unix(u) => (Some(crate::unix::owner(u)), false),
                Kind::Inet(u) => (Some(crate::inet::owner(u)), false),
                // eventfd などの名前のない inode は root のもの
                _ => (None, false),
            };
            let euid = cred::current().euid;
            if arg & O_NOATIME != 0 && f.flags & O_NOATIME == 0 && euid != 0 && owner != Some(euid) {
                return Err(-cred::EPERM);
            }
            if arg & O_DIRECT != 0 && !odirect {
                return Err(crate::why::inval());
            }
            f.flags = (f.flags & !MASK) | (arg & MASK);
            if let Kind::Socket(s) = &f.kind {
                s.borrow_mut().nonblock = arg as u32 & O_NONBLOCK != 0;
            }
            Ok(0)
        }
        // memfd の封 (seal)。memfd でなければ EINVAL
        F_GET_SEALS | F_ADD_SEALS => {
            let f = entry.file.borrow();
            let Kind::Inode(ino, _) = &f.kind else { return Err(crate::why::inval()) };
            if cmd == F_GET_SEALS {
                return ino.seals().map(|s| s as i64).ok_or_else(|| crate::why::inval());
            }
            if f.flags & file::O_ACCMODE == file::O_RDONLY {
                return Err(-cred::EPERM);
            }
            ino.add_seals(arg as u32)?;
            Ok(0)
        }
        // POSIX のレコードロック (F_GETLK / F_SETLK / F_SETLKW) と OFD ロック (36〜38): locks.rs
        5..=7 | 36..=38 => {
            let f = entry.file.clone();
            let files = p as *const proc::Files as usize;
            crate::locks::fcntl(&f, files, cmd, arg as usize)
        }
        F_GETPIPE_SZ | F_SETPIPE_SZ => match &entry.file.borrow().kind {
            Kind::PipeRead(p) | Kind::PipeWrite(p) | Kind::PipeRw(p) => {
                let mut p = p.borrow_mut();
                if cmd == F_SETPIPE_SZ {
                    let want = (arg as usize).clamp(4096, usize::MAX).next_power_of_two();
                    if want > file::PIPE_MAX {
                        return Err(-1); // EPERM
                    }
                    // 使っている箱が入らなければ EBUSY (Linux の pipe_resize_ring)
                    if want / 4096 < p.used_slots() {
                        return Err(-16); // EBUSY
                    }
                    p.cap = want;
                }
                Ok(p.cap as i64)
            }
            _ => Err(-EBADF),
        },
        _ => Err(crate::why::inval()),
    }
}

pub fn pipe2(fds: usize, flags: u64) -> R {
    let (r, w) = Pipe::new();
    let cloexec = flags & O_CLOEXEC != 0;
    let nb = flags as u32 & O_NONBLOCK;
    let p = proc::current().files();
    let rfd = p.add(file::new(r, file::O_RDONLY | nb), cloexec, 0).ok_or(-EMFILE)?;
    let Some(wfd) = p.add(file::new(w, file::O_WRONLY | nb), cloexec, 0) else {
        p.set(rfd, None);
        return Err(-EMFILE);
    };
    let mut b = [0u8; 8];
    b[..4].copy_from_slice(&(rfd as i32).to_le_bytes());
    b[4..].copy_from_slice(&(wfd as i32).to_le_bytes());
    if let Err(e) = out(fds, &b) {
        p.set(rfd, None);
        p.set(wfd, None);
        return Err(e);
    }
    Ok(0)
}

pub fn ioctl(fd: u64, req: u64, arg: usize) -> R {
    let f = file_of(fd)?;
    // O_PATH の fd は ioctl に使えない (Linux の fdget)
    if f.borrow().flags & file::O_PATH != 0 {
        return Err(-EBADF);
    }
    if req == FIONBIO {
        let mut v = [0u8; 4];
        proc::current().pt().copy_in(&mut v, arg).ok_or(-EFAULT)?;
        let on = u32::from_le_bytes(v) != 0;
        let mut f = f.borrow_mut();
        f.flags = if on { f.flags | O_NONBLOCK } else { f.flags & !O_NONBLOCK };
        if let Kind::Socket(s) = &f.kind {
            s.borrow_mut().nonblock = on;
        }
        return Ok(0);
    }
    // ネットワークのインターフェース (ip、networkd): ソケットの fd に
    if matches!(f.borrow().kind, Kind::Socket(_) | Kind::Inet(_) | Kind::Unix(_)) && crate::netif::handles(req) {
        return crate::netif::ioctl(req, arg);
    }
    if let Kind::Block(p) = &f.borrow().kind {
        // BLKGETSIZE64 (バイト数)、BLKGETSIZE (セクタ数)、BLKSSZGET / BLKBSZGET (セクタの大きさ)
        const BLKGETSIZE: u64 = 0x1260;
        const BLKSSZGET: u64 = 0x1268;
        const BLKBSZGET: u64 = 0x80081270;
        const BLKGETSIZE64: u64 = 0x80081272;
        return match req {
            BLKGETSIZE64 => out(arg, &(file::blk_size(p) as u64).to_le_bytes()).map(|_| 0),
            BLKGETSIZE => out(arg, &p.len.to_le_bytes()).map(|_| 0),
            BLKSSZGET | BLKBSZGET => out(arg, &(crate::block::SECTOR as u32).to_le_bytes()).map(|_| 0),
            _ => Err(-ENOTTY),
        };
    }
    if matches!(f.borrow().kind, Kind::Fb) {
        return crate::gpu::ioctl(req, arg);
    }
    if let Kind::Input(n) = f.borrow().kind {
        return crate::input::ioctl(n, req, arg);
    }
    // 音 (ALSA の形)。DRAIN と WRITEI は待つことがあるので、OpenFile を借りたままにしない
    let snd = match f.borrow().kind {
        Kind::SndCtl => Some(false),
        Kind::SndPcm => Some(true),
        _ => None,
    };
    if let Some(pcm) = snd {
        let nonblock = f.borrow().flags & crate::file::O_NONBLOCK != 0;
        return if pcm { crate::sound::pcm_ioctl(req, arg, nonblock) } else { crate::sound::ctl_ioctl(req, arg) };
    }
    // AF_UNIX のソケット (Linux の unix_ioctl): SIOCOUTQ は送って読まれていない大きさ、SIOCATMARK は MSG_OOB の印か
    const SIOCOUTQ: u64 = 0x5411;
    const SIOCATMARK: u64 = 0x8905;
    let sock = match &f.borrow().kind {
        Kind::Unix(u) => Some(u.clone()),
        _ => None,
    };
    if let Some(u) = sock.filter(|_| req == SIOCOUTQ || req == SIOCATMARK) {
        let v = if req == SIOCOUTQ { crate::unix::outq(&u) as i32 } else { crate::unix::atmark(&u)? as i32 };
        return out(arg, &v.to_le_bytes()).map(|_| 0);
    }
    // TCP / UDP (Linux の tcp_ioctl と udp_ioctl)
    if let Some(u) = inet_of(&f).filter(|_| req == SIOCOUTQ || req == SIOCATMARK || req == 0x541b) {
        let v = match req {
            SIOCOUTQ => crate::inet::outq(&u)?,
            SIOCATMARK => crate::inet::atmark(&u)?,
            _ => crate::inet::inq(&u)?,
        };
        return out(arg, &v.to_le_bytes()).map(|_| 0);
    }
    // FIONREAD: いま読めるバイト数 (パイプ、AF_UNIX のソケット (listen しているものは EINVAL)、TCP / UDP のソケット、
    // ふつうのファイル)。端末のものは tty の ioctl で
    const FIONREAD: u64 = 0x541b;
    if req == FIONREAD {
        let f = f.borrow();
        let n = match &f.kind {
            Kind::PipeRead(p) | Kind::PipeWrite(p) | Kind::PipeRw(p) => Some(p.borrow().len()),
            Kind::Unix(u) => Some(crate::unix::inq(u)?),
            Kind::Socket(s) => Some(s.borrow().available()),
            Kind::Inotify(n) => Some(inotify::pending(n)),
            // ふつうのファイルは大きさ - 位置 (終わりより先なら負: Linux の do_vfs_ioctl)
            Kind::Inode(ino, _) if ino.meta().mode & S_IFMT == vfs::S_IFREG => {
                let n = ino.meta().size as i64 - f.offset as i64;
                return out(arg, &(n as i32).to_le_bytes()).map(|_| 0);
            }
            _ => None,
        };
        if let Some(n) = n {
            return out(arg, &(n.min(i32::MAX as usize) as i32).to_le_bytes()).map(|_| 0);
        }
    }
    let (tty, master) = match &f.borrow().kind {
        Kind::Tty(t) => (t.clone(), false),
        Kind::PtyMaster(t) => (t.clone(), true),
        _ => return Err(-ENOTTY),
    };
    crate::tty::ioctl(&tty, master, req, arg)
}

pub fn getcwd(buf: usize, len: usize) -> R {
    let files = proc::current().files();
    // chroot の中ではルートからのパス
    let cwd = match files.cwd.strip_prefix(files.root.as_str()) {
        Some(c) if !files.root.is_empty() && (c.is_empty() || c.starts_with('/')) => String::from(c.trim_start_matches('/')),
        _ => files.cwd.clone(),
    };
    let mut s = Vec::with_capacity(cwd.len() + 2);
    s.push(b'/');
    s.extend_from_slice(cwd.as_bytes());
    s.push(0);
    if s.len() > len {
        return Err(-ERANGE);
    }
    out(buf, &s)?;
    Ok(s.len() as i64)
}

pub fn chdir(pathp: usize) -> R {
    let path = user_str(pathp)?;
    let files = proc::current().files();
    let (full, ino) = vfs::lookup(&files.cwd, &path, true)?;
    if !ino.meta().is_dir() {
        return Err(-ENOTDIR);
    }
    cred::current().check(&ino.meta(), PX)?;
    files.cwd = full;
    Ok(0)
}

/// chroot(path): このプロセス (と子) の / を path にする。root だけ
pub fn chroot(pathp: usize) -> R {
    let path = user_str(pathp)?;
    // 砂場の中からは、見える根を変えられない (Linux の landlock と同じ)
    if cred::current().euid != 0 || cred::current().landlock.is_some() {
        return Err(-cred::EPERM);
    }
    let files = proc::current().files();
    let (full, ino) = vfs::lookup(&files.cwd, &path, true)?;
    if !ino.meta().is_dir() {
        return Err(-ENOTDIR);
    }
    files.root = full;
    Ok(0)
}

/// マウントしてよいか: (外の) root か、自分のマウントの namespace の中で、no_new_privs か、そのマウントの
/// namespace を作ったユーザーの namespace の中 (外には見えないし、setuid のプログラムをだませない)。
/// 砂場 (landlock) の中からはだめ (Linux と同じ)
fn may_mount() -> Result<(), i64> {
    let c = cred::current();
    let own = c.ns.mnt.as_ref().is_some_and(|m| c.no_new_privs || (c.ns.user.is_some() && m.owner == c.ns.user_id()));
    if c.landlock.is_some() || !((c.euid == 0 && c.ns.user.is_none()) || own) {
        return Err(-cred::EPERM);
    }
    Ok(())
}

/// mount(source, target, fstype, flags, data): tmpfs、proc、sysfs、bind (MS_BIND、ディレクトリ)。
/// 伝わり方 (MS_PRIVATE など) は、aios ではいつも private なので何もしない。MS_RDONLY で読むだけ
/// (MS_REMOUNT で付けなおせる)。移す (MS_MOVE) はない
pub fn mount(srcp: usize, targetp: usize, typep: usize, flags: u64) -> R {
    const MS_RDONLY: u64 = 1;
    const MS_REMOUNT: u64 = 32;
    const MS_BIND: u64 = 4096;
    const MS_MOVE: u64 = 8192;
    const MS_PROPAGATION: u64 = (1 << 17) | (1 << 18) | (1 << 19) | (1 << 20);
    const ENODEV: i64 = 19;
    may_mount()?;
    let cwd = proc::current().files().cwd.clone();
    let target = user_str(targetp)?;
    if flags & MS_PROPAGATION != 0 {
        vfs::resolve(&cwd, &target, true)?;
        return Ok(0);
    }
    if flags & MS_MOVE != 0 {
        return Err(crate::why::inval());
    }
    // 付けなおし (mount -o remount,ro): いちばん上のマウントを読むだけに / 書けるように
    if flags & MS_REMOUNT != 0 {
        let (full, _) = vfs::lookup(&cwd, &target, true)?;
        vfs::remount(&full, flags & MS_RDONLY != 0)?;
        return Ok(0);
    }
    let (full, dir) = vfs::lookup(&cwd, &target, true)?;
    if !dir.meta().is_dir() {
        return Err(-ENOTDIR);
    }
    let (root, source, fstype) = if flags & MS_BIND != 0 {
        let src = user_str(srcp)?;
        let (sfull, s) = vfs::lookup(&cwd, &src, true)?;
        if !s.meta().is_dir() {
            return Err(crate::why::inval());
        }
        (s, alloc::format!("/{}", sfull), String::from("none"))
    } else {
        let fstype = user_str(typep)?;
        let root = match fstype.as_str() {
            // /tmp と同じく、だれでも作れて他人のものは消せない (Linux の tmpfs の既定)
            "tmpfs" => {
                let r: InodeRef = crate::tmpfs::new_root();
                r.set_mode(0o1777)?;
                r
            }
            "proc" => crate::procfs::new_root(),
            "sysfs" => crate::sysfs::new_root(),
            _ => return Err(-ENODEV),
        };
        let source = if srcp != 0 { user_str(srcp)? } else { fstype.clone() };
        (root, source, fstype)
    };
    vfs::mount_ro(&full, root, &source, &fstype, flags & MS_RDONLY != 0)?;
    Ok(0)
}

/// umount2(target, flags): いちばん上にかぶせたものを外す
pub fn umount2(targetp: usize, _flags: u64) -> R {
    may_mount()?;
    let cwd = proc::current().files().cwd.clone();
    let (full, _) = vfs::lookup(&cwd, &user_str(targetp)?, true)?;
    vfs::umount(&full)?;
    Ok(0)
}

pub fn fchdir(fd: u64) -> R {
    let f = file_of(fd)?;
    let path = match &f.borrow().kind {
        Kind::Inode(i, p) if i.meta().is_dir() => p.clone(),
        _ => return Err(-ENOTDIR),
    };
    proc::current().files().cwd = path;
    Ok(0)
}


const POLLIN: i16 = 0x1;
const POLLOUT: i16 = 0x4;
const POLLERR: i16 = 0x8;
const POLLHUP: i16 = 0x10;
const POLLNVAL: i16 = 0x20;

/// ppoll(fds, nfds, timeout) (シグナルマスクは syscall.rs で signal::wait_mask)
pub fn ppoll(fds: usize, nfds: usize, tmo: usize) -> R {
    if nfds > proc::NOFILE {
        return Err(crate::why::inval());
    }
    let mut raw = vec![0u8; nfds * 8];
    proc::current().pt().copy_in(&mut raw, fds).ok_or(-EFAULT)?;
    let deadline = if tmo == 0 {
        None
    } else {
        let mut ts = [0u8; 16];
        proc::current().pt().copy_in(&mut ts, tmo).ok_or(-EFAULT)?;
        let ns = u64::from_le_bytes(ts[..8].try_into().unwrap()) * 1_000_000_000 + u64::from_le_bytes(ts[8..].try_into().unwrap());
        Some(crate::timer::ticks_after(ns))
    };
    // 見張るものの印 (どれかわからないものがあれば、何でも起こしてもらう)
    let mut keys = Some(alloc::vec::Vec::new());
    for i in 0..nfds {
        let fd = i32::from_le_bytes(raw[i * 8..i * 8 + 4].try_into().unwrap());
        if fd >= 0 {
            match proc::current().files().get(fd as u64).map(|f| f.borrow().poll_keys()) {
                Some(Some(k)) => keys.as_mut().map_or((), |v: &mut alloc::vec::Vec<usize>| v.extend(k)),
                Some(None) => keys = None,
                None => {}
            }
        }
    }
    // 待つかもしれないなら、見ているパイプに印 (タイムアウト 0 のぞき見はつけない)
    let _watch = (!deadline.is_some_and(|d| d <= crate::timer::ticks())).then(|| {
        let files = proc::current().files();
        let fs: alloc::vec::Vec<FileRef> = (0..nfds)
            .filter_map(|i| {
                let fd = i32::from_le_bytes(raw[i * 8..i * 8 + 4].try_into().unwrap());
                if fd >= 0 { files.get(fd as u64).cloned() } else { None }
            })
            .collect();
        file::PollWatch::new(fs.iter())
    });
    loop {
        let mut count = 0;
        for i in 0..nfds {
            let e = &mut raw[i * 8..i * 8 + 8];
            let fd = i32::from_le_bytes(e[0..4].try_into().unwrap());
            let events = i16::from_le_bytes(e[4..6].try_into().unwrap());
            let mut rev = 0i16;
            if fd >= 0 {
                match proc::current().files().get(fd as u64) {
                    None => rev = POLLNVAL,
                    // O_PATH の fd は poll に使えない (Linux の fdget)
                    Some(f) if f.borrow().flags & file::O_PATH != 0 => rev = POLLNVAL,
                    Some(f) => {
                        // Linux と同じビットがわかるものは、そのうち頼まれたもの (と、いつも POLLERR と POLLHUP)
                        if let Some(m) = f.borrow().poll_mask() {
                            rev = (m & (events as u16 as u32 | 0x18)) as u16 as i16;
                            e[6..8].copy_from_slice(&rev.to_le_bytes());
                            if rev != 0 {
                                count += 1;
                            }
                            continue;
                        }
                        let (r, w, hup) = f.borrow().readiness();
                        if r {
                            rev |= events & POLLIN;
                        }
                        if w {
                            rev |= events & POLLOUT;
                        }
                        if hup {
                            rev |= POLLHUP | (events & POLLOUT != 0).then_some(POLLERR).unwrap_or(0);
                        }
                    }
                }
            }
            e[6..8].copy_from_slice(&rev.to_le_bytes());
            if rev != 0 {
                count += 1;
            }
        }
        let expired = deadline.is_some_and(|d| crate::timer::ticks() >= d);
        if count > 0 || expired {
            out(fds, &raw)?;
            return Ok(count);
        }
        proc::poll_sleep(keys.clone(), deadline.unwrap_or(0))?;
    }
}

/// pselect6(nfds, readfds, writefds, exceptfds, timeout, sigmask): ppoll と同じ待ち方を fd_set (ビットの並び) で。
/// 読める = データがあるか相手が閉じた、書ける = 書けるか相手が閉じた。例外 (帯域外データ) はないので空にする。
/// sigmask は ppoll と同じく signal::wait_mask で
pub fn pselect6(nfds: usize, rd: usize, wr: usize, ex: usize, tmo: usize) -> R {
    if nfds > proc::NOFILE {
        return Err(crate::why::inval());
    }
    let bytes = nfds.div_ceil(64) * 8;
    let load = |va: usize| -> Result<alloc::vec::Vec<u8>, i64> {
        let mut b = vec![0u8; bytes];
        if va != 0 {
            proc::current().pt().copy_in(&mut b, va).ok_or(-EFAULT)?;
        }
        Ok(b)
    };
    let (want_r, want_w, want_e) = (load(rd)?, load(wr)?, load(ex)?);
    let bit = |b: &[u8], i: usize| b[i / 8] & (1 << (i % 8)) != 0;
    let deadline = if tmo == 0 {
        None
    } else {
        let mut ts = [0u8; 16];
        proc::current().pt().copy_in(&mut ts, tmo).ok_or(-EFAULT)?;
        let ns = u64::from_le_bytes(ts[..8].try_into().unwrap()) * 1_000_000_000 + u64::from_le_bytes(ts[8..].try_into().unwrap());
        Some(crate::timer::ticks_after(ns))
    };
    let mut keys = Some(alloc::vec::Vec::new());
    for i in 0..nfds {
        if bit(&want_r, i) || bit(&want_w, i) || bit(&want_e, i) {
            match proc::current().files().get(i as u64).map(|f| f.borrow().poll_keys()) {
                Some(Some(k)) => keys.as_mut().map_or((), |v: &mut alloc::vec::Vec<usize>| v.extend(k)),
                Some(None) => keys = None,
                None => {}
            }
        }
    }
    let _watch = (!deadline.is_some_and(|d| d <= crate::timer::ticks())).then(|| {
        let files = proc::current().files();
        let fs: alloc::vec::Vec<FileRef> = (0..nfds).filter(|&i| bit(&want_r, i) || bit(&want_w, i) || bit(&want_e, i)).filter_map(|i| files.get(i as u64).cloned()).collect();
        file::PollWatch::new(fs.iter())
    });
    loop {
        let (mut got_r, mut got_w, mut got_e) = (vec![0u8; bytes], vec![0u8; bytes], vec![0u8; bytes]);
        let mut count = 0;
        for i in 0..nfds {
            let (r_, w_, e_) = (bit(&want_r, i), bit(&want_w, i), bit(&want_e, i));
            if !r_ && !w_ && !e_ {
                continue;
            }
            let f = proc::current().files().get(i as u64).cloned().ok_or(-EBADF)?;
            if f.borrow().flags & file::O_PATH != 0 {
                return Err(-EBADF);
            }
            // Linux と同じビットがわかるもの: 読める = POLLIN_SET (IN、RDNORM、RDBAND、HUP、ERR)、書ける = POLLOUT_SET
            // (OUT、WRNORM、WRBAND、ERR)、例外 = POLLPRI
            let mask = f.borrow().poll_mask();
            if let Some(m) = mask {
                for (want, set, got) in [(r_, 0x1 | 0x40 | 0x80 | 0x10 | 0x8, &mut got_r), (w_, 0x4 | 0x100 | 0x200 | 0x8, &mut got_w), (e_, 0x2, &mut got_e)] {
                    if want && m & set != 0 {
                        got[i / 8] |= 1 << (i % 8);
                        count += 1;
                    }
                }
                continue;
            }
            if !r_ && !w_ {
                continue;
            }
            let (r, w, hup) = f.borrow().readiness();
            if r_ && (r || hup) {
                got_r[i / 8] |= 1 << (i % 8);
                count += 1;
            }
            if w_ && (w || hup) {
                got_w[i / 8] |= 1 << (i % 8);
                count += 1;
            }
        }
        let expired = deadline.is_some_and(|d| crate::timer::ticks() >= d);
        if count > 0 || expired {
            for (va, b) in [(rd, &got_r), (wr, &got_w), (ex, &got_e)] {
                if va != 0 {
                    out(va, b)?;
                }
            }
            return Ok(count);
        }
        proc::poll_sleep(keys.clone(), deadline.unwrap_or(0))?;
    }
}

/// FIFO の inode ごとのパイプ (誰かが開いている間だけ生きている)
static FIFOS: crate::cell::Bkl<alloc::vec::Vec<((usize, u64), alloc::rc::Weak<crate::file::PipeCell>)>> = crate::cell::Bkl::new(alloc::vec::Vec::new());

fn fifo_pipe(ino: &InodeRef) -> alloc::rc::Rc<crate::file::PipeCell> {
    let fifos = FIFOS.get();
    fifos.retain(|(_, w)| w.strong_count() > 0);
    if let Some(p) = fifos.iter().find(|(id, _)| *id == ino.id()).and_then(|(_, w)| w.upgrade()) {
        return p;
    }
    let p = Pipe::empty();
    p.fifo.store(true, core::sync::atomic::Ordering::Relaxed);
    fifos.push((ino.id(), alloc::rc::Rc::downgrade(&p)));
    p
}

fn pipe_of(fd: u64) -> Option<alloc::rc::Rc<crate::file::PipeCell>> {
    let f = file_of(fd).ok()?;
    let f = f.borrow();
    match &f.kind {
        Kind::PipeRead(p) | Kind::PipeWrite(p) | Kind::PipeRw(p) => Some(p.clone()),
        _ => None,
    }
}

const SPLICE_F_NONBLOCK: u64 = 2;
/// SPLICE_F_MOVE | SPLICE_F_NONBLOCK | SPLICE_F_MORE | SPLICE_F_GIFT
const SPLICE_F_ALL: u64 = 0x0f;

/// splice(2) (Linux の splice、__do_splice、do_splice の順): 長さ 0 なら 0、flags (SPLICE_F_ALL のほかは EINVAL)、
/// 両方の fd (O_PATH も EBADF)、パイプの側にわたされた位置 (ESPIPE)、位置を写し (EFAULT)、読める口と書ける口か
/// (EBADF)。それから:
///   パイプ → パイプ: 同じパイプは EINVAL。箱ごとに動かす (Pipe::splice_pipe)
///   パイプ → ほか: わたされた位置 (位置のないものは EINVAL) かファイルの位置、位置のしらべ (EINVAL)、中身を待って
///     (書き手がいなければ 0、nonblock は EAGAIN) そこに書き、書けた分を読み減らす (splice_pipe_to)
///   ほか → パイプ: わたされた位置かファイルの位置、位置のしらべ、splice_file_to_pipe (sendfile と同じ)
///   どちらもパイプでなければ EINVAL
/// nonblock は SPLICE_F_NONBLOCK か、パイプの側の O_NONBLOCK。O_APPEND のファイルへは、Linux は EINVAL だが、aios は
/// 書く (aios がえらんだちがい copy-append)
pub fn splice(fd_in: u64, off_in: usize, fd_out: u64, off_out: usize, len: usize, flags: u64) -> R {
    if len == 0 {
        return Ok(0);
    }
    if flags & !SPLICE_F_ALL != 0 {
        return Err(crate::why::inval());
    }
    let fi = file_of(fd_in)?;
    let fo = file_of(fd_out)?;
    if fi.borrow().flags & file::O_PATH != 0 || fo.borrow().flags & file::O_PATH != 0 {
        return Err(-EBADF);
    }
    let (pin, pout) = (pipe_of(fd_in), pipe_of(fd_out));
    if (pin.is_some() && off_in != 0) || (pout.is_some() && off_out != 0) {
        return Err(-file::ESPIPE);
    }
    let at_out = if off_out != 0 { Some(read_off(off_out)?) } else { None };
    let at_in = if off_in != 0 { Some(read_off(off_in)?) } else { None };
    if !fi.borrow().readable() || !fo.borrow().writable() {
        return Err(-EBADF);
    }
    let nb = |f: &FileRef| flags & SPLICE_F_NONBLOCK != 0 || f.borrow().flags & file::O_NONBLOCK != 0;
    // 位置をわたせるのは、位置のあるものだけ (Linux の FMODE_PREAD / FMODE_PWRITE)
    let positional = |f: &FileRef| matches!(f.borrow().kind, Kind::Inode(..) | Kind::Block(_));
    match (pin, pout) {
        (Some(a), Some(b)) => {
            if alloc::rc::Rc::ptr_eq(&a, &b) {
                return Err(crate::why::inval());
            }
            let r = Pipe::splice_pipe(&a, &b, len, nb(&fi) || nb(&fo));
            if r == Err(-file::EPIPE) {
                sigpipe();
            }
            Ok(r? as i64)
        }
        (Some(a), None) => {
            if at_out.is_some() && !positional(&fo) {
                return Err(crate::why::inval());
            }
            let mut pos = at_out.unwrap_or(fo.borrow().offset as i64);
            // aios がえらんだちがい (copy-append): O_APPEND で開いたファイルへも書く。位置がなければ write と同じく
            // 終わりに足す
            if at_out.is_none()
                && fo.borrow().flags & file::O_APPEND != 0
                && let Some((io, _)) = inode_path(&fo)
            {
                pos = io.meta().size as i64;
            }
            verify_pos(pos, len)?;
            let n = splice_pipe_to(&a, &fo, pos, len, nb(&fi))?;
            if n > 0 {
                pos += n as i64;
                match at_out {
                    Some(_) => out(off_out, &pos.to_le_bytes())?,
                    None => fo.borrow_mut().offset = pos as usize,
                }
            }
            Ok(n as i64)
        }
        (None, Some(b)) => {
            if at_in.is_some() && !positional(&fi) {
                return Err(crate::why::inval());
            }
            let mut pos = at_in.unwrap_or(fi.borrow().offset as i64);
            verify_pos(pos, len)?;
            let n = if matches!(fi.borrow().kind, Kind::Inode(..)) {
                splice_file_to_pipe(&fi, &mut pos, &b, len, nb(&fo))?
            } else {
                splice_stream_to_pipe(&fi, &b, len, nb(&fo))?
            };
            match at_in {
                Some(_) => out(off_in, &pos.to_le_bytes())?,
                None => fi.borrow_mut().offset = pos as usize,
            }
            Ok(n as i64)
        }
        (None, None) => Err(crate::why::inval()),
    }
}

/// パイプ a の中身を fo の pos へ len バイトまで書く (Linux の iter_file_splice_write): 中身が来るまで待ち (書き手が
/// いなければ 0、nonblock は EAGAIN)、パイプにあるだけ (len まで) 書いて、書けた分を読み減らす。ふつうのファイルは
/// 大きさの上限 (EFBIG) を見てから setuid / setgid を落として書く。/dev/null は捨てるだけ、位置のないもの (端末、
/// ソケット) はふつうに書く
fn splice_pipe_to(a: &alloc::rc::Rc<file::PipeCell>, fo: &FileRef, pos: i64, len: usize, nonblock: bool) -> Result<usize, i64> {
    if matches!(fo.borrow().kind, Kind::Null) {
        return Pipe::discard(a, len, nonblock);
    }
    let data = Pipe::peek_wait(a, len, nonblock)?;
    if data.is_empty() {
        return Ok(0);
    }
    let w = match inode_path(fo) {
        Some((io, path)) => {
            const EFBIG: i64 = 27;
            let max = io.max_bytes() as i64;
            if pos >= max {
                return Err(-EFBIG);
            }
            let n = data.len().min((max - pos) as usize);
            vfs::write_sealed(&io, pos as usize, n)?;
            vfs::drop_privs(&io)?;
            let w = io.write_at(pos as usize, &data[..n])?;
            if w > 0 {
                inotify::file_event(&path, &io, inotify::IN_MODIFY);
            }
            w
        }
        None => {
            let mut done = 0;
            while done < data.len() {
                match file::write(fo, &data[done..]) {
                    Ok(0) => break,
                    Ok(w) => done += w,
                    Err(e) if done == 0 => {
                        if e == -file::EPIPE {
                            sigpipe();
                        }
                        return Err(e);
                    }
                    Err(_) => break,
                }
            }
            done
        }
    };
    let _ = Pipe::discard(a, w, true);
    Ok(w)
}

/// 位置のないもの (端末、ソケットなど) からパイプへ (Linux の copy_splice_read): 箱があくのを待ち (読み手が
/// いなければ SIGPIPE と EPIPE、nonblock は EAGAIN)、あいた箱 × 4KiB まで読んで、4KiB ずつの足さない箱に入れる
fn splice_stream_to_pipe(f: &FileRef, p: &alloc::rc::Rc<file::PipeCell>, len: usize, nonblock: bool) -> Result<usize, i64> {
    if let Err(e) = Pipe::wait_space(p, nonblock) {
        if e == -file::EPIPE {
            sigpipe();
        }
        return Err(e);
    }
    if let Some(u) = unix_of(f) {
        return socket_to_pipe(f, &u, 0, p, len, nonblock);
    }
    let free = {
        let pp = p.borrow();
        pp.slots().saturating_sub(pp.used_slots())
    };
    let mut tmp = uninit(len.min(free * crate::memlayout::PGSIZE))?;
    let n = file::read(f, &mut tmp)?;
    Ok(Pipe::splice_in(p, 0, &tmp[..n]))
}

/// AF_UNIX のソケットからパイプへ (Linux の sock_splice_read)。位置は 0 でなければ ESPIPE。stream は
/// unix_stream_splice_read: ソケットか splice の NONBLOCK なら待たず、メッセージごとにパイプの箱を分けて、あいた箱に
/// 入る分だけ (先に MSG_PEEK で見て数える)。dgram と seqpacket は copy_splice_read (sock_read_iter): ソケットの
/// O_NONBLOCK だけを見て、メッセージ 1 つを、あいた箱 × 4 KiB に入る分だけ (のこりは捨てる)。届いた fd は閉じる
fn socket_to_pipe(f: &FileRef, u: &crate::unix::UnixRef, pos: i64, p: &alloc::rc::Rc<file::PipeCell>, len: usize, nonblock: bool) -> Result<usize, i64> {
    const PG: usize = crate::memlayout::PGSIZE;
    const MSG_PEEK: u64 = 0x2;
    if pos != 0 {
        return Err(-file::ESPIPE);
    }
    if len == 0 {
        return Ok(0);
    }
    let free = {
        let pp = p.borrow();
        pp.slots().saturating_sub(pp.used_slots())
    };
    let room = len.min(free * PG).min(MAX_RW_COUNT);
    let sock_nb = f.borrow().flags & O_NONBLOCK != 0;
    let r = if u.borrow().typ == crate::unix::SOCK_STREAM {
        let nb = sock_nb || nonblock;
        let peek = crate::unix::recv(u, room, MSG_PEEK | crate::unix::MSG_PARTS, nb)?;
        // 箱に入る分: メッセージごとに、その長さの 4 KiB の箱
        let (mut fit, mut left) = (0, free);
        for &n in &peek.parts {
            let need = n.div_ceil(PG);
            if need > left {
                fit += left * PG;
                break;
            }
            fit += n;
            left -= need;
        }
        if fit == 0 {
            return Ok(0);
        }
        crate::unix::recv(u, fit, crate::unix::MSG_PARTS, nb)?
    } else {
        crate::unix::recv(u, room, crate::unix::MSG_PARTS, sock_nb)?
    };
    let mut done = 0;
    let mut at = 0;
    for &n in &r.parts {
        done += Pipe::splice_in(p, 0, &r.data[at..at + n]);
        at += n;
    }
    Ok(done)
}

fn read_off(va: usize) -> Result<i64, i64> {
    let mut b = [0u8; 8];
    proc::current().pt().copy_in(&mut b, va).ok_or(-EFAULT)?;
    Ok(i64::from_le_bytes(b))
}

/// Linux の MAX_RW_COUNT: 1 回の読み書きの上限 (INT_MAX をページでそろえたもの)
const MAX_RW_COUNT: usize = 0x7fff_f000;

/// Linux の rw_verify_area の位置のしらべ: 長さが負 (ssize_t として) か、位置が負か、位置 + 長さがあふれたら EINVAL
fn verify_pos(pos: i64, len: usize) -> Result<(), i64> {
    if (len as i64) < 0 || pos < 0 || pos.checked_add(len as i64).is_none() {
        return Err(crate::why::inval());
    }
    Ok(())
}

/// 読み手のいないパイプに書こうとした: SIGPIPE (sysfile::write と同じ)
fn sigpipe() {
    let info = crate::signal::SigInfo::from(crate::signal::SI_KERNEL);
    crate::signal::send_thread(proc::current(), crate::signal::SIGPIPE, info);
}

/// ふつうのファイルを、ページキャッシュ (ext4) か inode からそのまま読む (pread と同じ)
fn read_inode_at(ino: &InodeRef, pos: usize, buf: &mut [u8]) -> Result<usize, i64> {
    if ino.page_cacheable() { file::read_sleepable(ino, pos, buf) } else { ino.read_at(pos, buf) }
}

/// ふつうのファイル f の *pos から、パイプ p へ len バイトまで (Linux の splice_file_to_pipe と
/// filemap_splice_read / shmem_file_splice_read): 箱があくのを待ち (読み手がいなければ SIGPIPE と EPIPE、
/// nonblock なら EAGAIN)、あいた箱 × 4KiB まで読んで、ファイルの 1 ページの中のかけらごとの箱に入れる。
/// ファイルの終わりより先は 0。*pos を入れた分すすめる。
/// aios がえらんだちがい (splice-snapshot): 中身はそのときのものを箱に写す。あとでファイルを書いたり切りつめたり
/// しても、パイプの中身は変わらない。Linux はページキャッシュのページそのものを箱に入れるので、読む前にファイルが
/// 変わればパイプからもそれが読め、切りつめると端のページの残りは 0 になる。doc/aios.md の「aios がえらんだちがい」
fn splice_file_to_pipe(f: &FileRef, pos: &mut i64, p: &alloc::rc::Rc<file::PipeCell>, len: usize, nonblock: bool) -> Result<usize, i64> {
    if let Err(e) = Pipe::wait_space(p, nonblock) {
        if e == -file::EPIPE {
            sigpipe();
        }
        return Err(e);
    }
    if len == 0 {
        return Ok(0);
    }
    if let Some(u) = unix_of(f) {
        return socket_to_pipe(f, &u, *pos, p, len, nonblock);
    }
    // AF_INET のソケットは位置のないものとして (Linux の tcp_splice_read と copy_splice_read: ソケットの O_NONBLOCK で
    // 待つか決め、TCP は来ているだけ、UDP はメッセージ 1 つ)
    if inet_of(f).is_some() {
        return splice_stream_to_pipe(f, p, len, nonblock);
    }
    // ファイルのほかは splice で読めない (ディレクトリ、パイプ: Linux も EINVAL)
    let Some((ino, path)) = inode_path(f).filter(|(i, _)| i.meta().mode & S_IFMT == vfs::S_IFREG) else { return Err(crate::why::inval()) };
    let room = p.borrow().splice_room(*pos as u64, len.min(MAX_RW_COUNT));
    verify_pos(*pos, room)?;
    let mut tmp = uninit(room)?;
    let n = read_inode_at(&ino, *pos as usize, &mut tmp)?;
    let done = Pipe::splice_in(p, *pos as u64, &tmp[..n]);
    *pos += done as i64;
    if done > 0 {
        inotify::file_event(&path, &ino, inotify::IN_ACCESS);
    }
    Ok(done)
}

/// ファイル in の pos から len バイトを、ファイル out の opos へ写す (Linux の do_splice_direct で、
/// copy_file_range と sendfile が使う)。splice_direct_to_actor と同じく、16 ページの内側のパイプに入る分
/// (位置がページの途中なら、そのぶん少ない) ずつ、読めなくなる (ファイルの終わり) か、書くのが短くなるまで。
/// 同じファイルのときは、呼ぶほうが重ならない範囲にしている (copy_file_range と sendfile-self)。写せたバイト数
fn copy_inode_range(src: &InodeRef, pos: i64, dst: &InodeRef, opos: i64, len: usize) -> Result<usize, i64> {
    const PG: usize = crate::memlayout::PGSIZE;
    let mut done = 0;
    while done < len {
        let at_in = pos as usize + done;
        let mut tmp = uninit((len - done).min(16 * PG - at_in % PG))?;
        let r = read_inode_at(src, at_in, &mut tmp);
        let n = match r {
            Ok(n) => n,
            Err(e) if done == 0 => return Err(e),
            Err(_) => break,
        };
        if n == 0 {
            break;
        }
        let at = opos as usize + done;
        // 書くときに setuid / setgid を落とす (Linux の file_remove_privs)
        if done == 0 {
            vfs::drop_privs(dst)?;
        }
        let w = match vfs::write_sealed(dst, at, n).and_then(|_| dst.write_at(at, &tmp[..n])) {
            Ok(w) => w,
            Err(e) if done == 0 => return Err(e),
            Err(_) => break,
        };
        done += w;
        if w < n {
            break;
        }
    }
    Ok(done)
}

/// copy_file_range(2) (Linux の vfs_copy_file_range): ext4 も tmpfs も自分の copy_file_range を持たないので、
/// 同じファイルシステムの中は splice で写し、ちがうファイルシステムは EXDEV
pub fn copy_file_range(fd_in: u64, off_in: usize, fd_out: u64, off_out: usize, len: usize, flags: u64) -> R {
    const EOVERFLOW: i64 = 75;
    // Linux の順: 両方の fd (O_PATH も EBADF)、位置を写し (EFAULT)、flags (EINVAL)、generic_file_rw_checks
    // (ディレクトリは EISDIR、ふつうのファイルでなければ EINVAL、読めない・書けないなら EBADF)、
    // ファイルシステムがちがえば EXDEV、位置 + 長さがあふれたら EOVERFLOW、長さを読むほうの終わりまでに縮め、
    // 同じファイルで重なれば EINVAL、位置のしらべ (EINVAL)、長さ 0 なら 0
    let fi = file_of(fd_in)?;
    let fo = file_of(fd_out)?;
    if fi.borrow().flags & file::O_PATH != 0 || fo.borrow().flags & file::O_PATH != 0 {
        return Err(-EBADF);
    }
    let mut pos_in = if off_in != 0 { read_off(off_in)? } else { fi.borrow().offset as i64 };
    let mut pos_out = if off_out != 0 { read_off(off_out)? } else { fo.borrow().offset as i64 };
    if flags as u32 != 0 {
        return Err(crate::why::inval());
    }
    let (mi, mo) = (fi.borrow().stat().mode & S_IFMT, fo.borrow().stat().mode & S_IFMT);
    if mi == vfs::S_IFDIR || mo == vfs::S_IFDIR {
        return Err(-EISDIR);
    }
    if mi != vfs::S_IFREG || mo != vfs::S_IFREG {
        return Err(crate::why::inval());
    }
    if !fi.borrow().readable() || !fo.borrow().writable() {
        return Err(-EBADF);
    }
    let (Some((ii, pin)), Some((io, pout))) = (inode_path(&fi), inode_path(&fo)) else { return Err(crate::why::inval()) };
    if ii.id().0 != io.id().0 {
        return Err(-vfs::EXDEV);
    }
    // aios がえらんだちがい (copy-append): O_APPEND で開いたものへも写せる。位置をわたされたらそこへ (pwrite-append と
    // 同じ)、わたされなければ write と同じく終わりへ足す。Linux は EBADF (sendfile は EINVAL) で、写す人は read と
    // write でやりなおす。doc/aios.md の「aios がえらんだちがい」
    if fo.borrow().flags & file::O_APPEND != 0 && off_out == 0 {
        pos_out = io.meta().size as i64;
    }
    let count = len as u64;
    if (pos_in as u64).checked_add(count).is_none() || (pos_out as u64).checked_add(count).is_none() {
        return Err(-EOVERFLOW);
    }
    let size = ii.meta().size;
    let count = if pos_in >= size as i64 { 0 } else { count.min(size.wrapping_sub(pos_in as u64)) };
    // 書くほうの大きさの上限 (generic_write_check_limits)
    const EFBIG: i64 = 27;
    let max = io.max_bytes() as i64;
    if pos_out >= max {
        return Err(-EFBIG);
    }
    let count = count.min(max.wrapping_sub(pos_out) as u64) as usize;
    if ii.id() == io.id() && pos_out as u64 + count as u64 > pos_in as u64 && (pos_out as u64) < pos_in as u64 + count as u64 {
        return Err(crate::why::inval());
    }
    verify_pos(pos_in, count)?;
    verify_pos(pos_out, count)?;
    if count == 0 {
        return Ok(0);
    }
    let n = copy_inode_range(&ii, pos_in, &io, pos_out, count.min(MAX_RW_COUNT))?;
    if n > 0 {
        pos_in += n as i64;
        pos_out += n as i64;
        if off_in != 0 {
            out(off_in, &pos_in.to_le_bytes())?;
        } else {
            fi.borrow_mut().offset = pos_in as usize;
        }
        if off_out != 0 {
            out(off_out, &pos_out.to_le_bytes())?;
        } else {
            fo.borrow_mut().offset = pos_out as usize;
        }
        inotify::file_event(&pin, &ii, inotify::IN_ACCESS);
        inotify::file_event(&pout, &io, inotify::IN_MODIFY);
    }
    Ok(n as i64)
}

/// sendfile(2) (Linux の sendfile64): 位置 (offset) は先に写し、終わったら (しくじっても) 書きもどす
pub fn sendfile(out_fd: u64, in_fd: u64, offp: usize, count: usize) -> R {
    let mut pos = if offp != 0 { Some(read_off(offp)?) } else { None };
    let r = do_sendfile(out_fd, in_fd, pos.as_mut(), count);
    if let Some(p) = pos {
        out(offp, &p.to_le_bytes())?;
    }
    r
}

/// Linux の do_sendfile の順: 読むほうの fd (読めなければ EBADF)、位置 (わたされたのに位置のないものなら ESPIPE)、
/// 位置のしらべ (EINVAL)、書くほうの fd (書けなければ EBADF)。書くほうがパイプならそこへ splice
/// (splice_file_to_pipe)、ほかは位置のしらべ、読むほうに位置がなければ EINVAL (do_splice_direct)、それから写す。
/// O_APPEND なら終わりに足す (aios がえらんだちがい copy-append。Linux は EINVAL)
fn do_sendfile(out_fd: u64, in_fd: u64, ppos: Option<&mut i64>, count: usize) -> R {
    let fi = file_of(in_fd)?;
    if !fi.borrow().readable() {
        return Err(-EBADF);
    }
    let seekable = matches!(fi.borrow().kind, Kind::Inode(..) | Kind::Block(_) | Kind::Fb | Kind::Null | Kind::Zero | Kind::Random);
    let pos = match ppos.as_deref() {
        Some(_) if !seekable => return Err(-file::ESPIPE),
        Some(&p) => p,
        None => fi.borrow().offset as i64,
    };
    verify_pos(pos, count)?;
    let count = count.min(MAX_RW_COUNT);
    let fo = file_of(out_fd)?;
    if !fo.borrow().writable() {
        return Err(-EBADF);
    }
    // aios がえらんだちがい (copy-append): O_APPEND で開いたファイルへは、write と同じく終わりに足す
    let out_pos = match &fo.borrow().kind {
        Kind::Inode(io, _) if fo.borrow().flags & file::O_APPEND != 0 => io.meta().size as i64,
        _ => fo.borrow().offset as i64,
    };
    // 両方のファイルの大きさの上限 (s_maxbytes) をこえるなら EOVERFLOW (読みはじめがもうこえているとき)、
    // こえる分は短く
    const EOVERFLOW: i64 = 75;
    let maxb = |f: &FileRef| inode_path(f).map_or(i64::MAX, |(i, _)| i.max_bytes() as i64);
    let max = maxb(&fi).min(maxb(&fo));
    let mut count = count;
    if pos.saturating_add(count as i64) > max {
        if pos >= max {
            return Err(-EOVERFLOW);
        }
        count = (max - pos) as usize;
    }
    let n = if let Some(p) = pipe_of(out_fd) {
        let nonblock = fo.borrow().flags & file::O_NONBLOCK != 0;
        let mut at = pos;
        splice_file_to_pipe(&fi, &mut at, &p, count, nonblock)?
    } else {
        verify_pos(out_pos, count)?;
        if !seekable {
            return Err(crate::why::inval());
        }
        // aios がえらんだちがい (sendfile-self): 同じファイルの中なら copy_file_range と同じく、読みはじめたときの終わり
        // までを写し、読むところと書くところが重なれば EINVAL。Linux は重なっても写し、読むのがページキャッシュのページ
        // そのもの (写しをとらない) なので、同じ回に書いたものが読みにまざり、答えが内側のくぎり (16 ページ) やページの
        // まとまり (folio) の大きさで変わる。doc/aios.md の「aios がえらんだちがい」
        let mut count = count;
        if let (Some((ii, _)), Some((io, _))) = (inode_path(&fi), inode_path(&fo))
            && ii.id() == io.id()
            && ii.meta().mode & S_IFMT == vfs::S_IFREG
        {
            let size = ii.meta().size as i64;
            count = if pos >= size { 0 } else { count.min((size - pos) as usize) };
            if count > 0 && out_pos < pos + count as i64 && pos < out_pos + count as i64 {
                return Err(crate::why::inval());
            }
        }
        if count == 0 {
            0
        } else {
            // 読めるのはふつうのファイルだけ (ディレクトリは Linux でも splice_read がなく EINVAL)
            let Some((ii, ipath)) = inode_path(&fi).filter(|(i, _)| i.meta().mode & S_IFMT == vfs::S_IFREG) else { return Err(crate::why::inval()) };
            let n = match inode_path(&fo) {
                Some((io, opath)) => {
                    let n = copy_inode_range(&ii, pos, &io, out_pos, count)?;
                    if n > 0 {
                        inotify::file_event(&opath, &io, inotify::IN_MODIFY);
                    }
                    n
                }
                // 端末、ソケット、/dev/null など: ふつうに書く
                None => {
                    let mut done = 0;
                    while done < count {
                        let mut tmp = uninit((count - done).min(CHUNK))?;
                        let r = read_inode_at(&ii, pos as usize + done, &mut tmp);
                        let k = match r {
                            Ok(0) => break,
                            Ok(k) => k,
                            Err(e) if done == 0 => return Err(e),
                            Err(_) => break,
                        };
                        let w = match file::write(&fo, &tmp[..k]) {
                            Ok(w) => w,
                            Err(e) if done == 0 => {
                                if e == -file::EPIPE {
                                    sigpipe();
                                }
                                return Err(e);
                            }
                            Err(_) => break,
                        };
                        done += w;
                        if w < k {
                            break;
                        }
                    }
                    done
                }
            };
            if n > 0 {
                inotify::file_event(&ipath, &ii, inotify::IN_ACCESS);
                fo.borrow_mut().offset = (out_pos + n as i64) as usize;
            }
            n
        }
    };
    if n > 0 {
        let end = pos + n as i64;
        match ppos {
            Some(p) => *p = end,
            None => fi.borrow_mut().offset = end as usize,
        }
    }
    Ok(n as i64)
}

/// tee(2) (Linux の順): flags (SPLICE_F_ALL のほかは EINVAL)、長さ 0 なら 0、両方の fd (O_PATH も EBADF)、読める口と
/// 書ける口か (EBADF)、両方がパイプで同じでなければ EINVAL。それから Pipe::tee_pipe (読み減らさずに箱を写す)。
/// nonblock は SPLICE_F_NONBLOCK かどちらかの O_NONBLOCK
pub fn tee(fd_in: u64, fd_out: u64, len: usize, flags: u64) -> R {
    if flags & !SPLICE_F_ALL != 0 {
        return Err(crate::why::inval());
    }
    if len == 0 {
        return Ok(0);
    }
    let fi = file_of(fd_in)?;
    let fo = file_of(fd_out)?;
    if fi.borrow().flags & file::O_PATH != 0 || fo.borrow().flags & file::O_PATH != 0 {
        return Err(-EBADF);
    }
    if !fi.borrow().readable() || !fo.borrow().writable() {
        return Err(-EBADF);
    }
    let (Some(a), Some(b)) = (pipe_of(fd_in), pipe_of(fd_out)) else { return Err(crate::why::inval()) };
    if alloc::rc::Rc::ptr_eq(&a, &b) {
        return Err(crate::why::inval());
    }
    let nonblock = flags & SPLICE_F_NONBLOCK != 0 || (fi.borrow().flags | fo.borrow().flags) & file::O_NONBLOCK != 0;
    let r = Pipe::tee_pipe(&a, &b, len, nonblock);
    if r == Err(-file::EPIPE) {
        sigpipe();
    }
    Ok(r? as i64)
}

/// flock(fd, op): locks.rs
pub fn flock(fd: u64, op: u64) -> R {
    crate::locks::flock(fd, op)
}

/// close_range(first, last, flags): まとめて閉じる (CLOSE_RANGE_CLOEXEC なら印をつけるだけ)
pub fn close_range(first: u64, last: u64, flags: u64) -> R {
    const CLOSE_RANGE_CLOEXEC: u64 = 4;
    if first > last {
        return Err(crate::why::inval());
    }
    let files = proc::current().files();
    let end = (last as usize).min(files.fds.len().saturating_sub(1));
    for i in first as usize..=end {
        if flags & CLOSE_RANGE_CLOEXEC != 0 {
            if let Some(f) = files.fds[i].as_mut() {
                f.cloexec = true;
            }
        } else {
            files.set(i, None);
        }
    }
    Ok(0)
}

/// inotify_add_watch(fd, path, mask) (inotify.rs)
pub fn inotify_add_watch(fd: u64, pathp: usize, mask: u32) -> R {
    let path = user_str(pathp)?;
    inotify::add_watch(fd, &path, mask)
}

// 新しい mount の API (Linux 5.2: fs/fsopen.c と fs/namespace.c): fsopen、fsconfig、fsmount、fspick、open_tree、
// move_mount、mount_setattr。open_tree_attr (Linux 6.15: open_tree と、写したマウントへの mount_setattr を 1 回で)
//
//   fsopen("tmpfs") でファイルシステムをつくる口 (fs のコンテキスト: Kind::Ops の FsCtx) を開き、fsconfig で値を入れ
//   (知らないものは EINVAL で、口を read すると "e tmpfs: Unknown parameter 'x'" の理由)、FSCONFIG_CMD_CREATE で作る。
//   fsmount でそれを、はなれたマウント (vfs::detached_*: まだどこにも置いていない) の O_PATH の口にし、move_mount で置く。
//   open_tree は O_PATH の口 (OPEN_TREE_CLONE ならそこを写した、はなれたマウント: bind)、move_mount はマウントを移す
//   (はなれたものなら置く)。fspick はマウントの根のファイルシステムの口 (FSCONFIG_CMD_RECONFIGURE で ro / rw)、
//   mount_setattr はマウントの印 (読むだけ、nosuid、nodev、noexec、atime、nosymfollow) を変える
//
// たしかめの順と答えは Linux 7.4 と同じに (test/newmount.c)。aios のマウントはいつも private なので、伝わり方
// (propagation、MOVE_MOUNT_SET_GROUP) は受けるだけ。idmap のマウント、MOVE_MOUNT_BENEATH、FSMOUNT_NAMESPACE はない (EINVAL)
use crate::file::{FileOps, Kind};
use crate::sysfile::{at_full, file_of, may_mount, path_fd, user_str};
use crate::vfs::{self, InodeRef};
use alloc::collections::VecDeque;
use alloc::rc::Rc;
use alloc::string::String;
use core::cell::{Cell, RefCell};

const EPERM: i64 = 1;
const ENOENT: i64 = 2;
const EBADF: i64 = 9;
const EFAULT: i64 = 14;
const EBUSY: i64 = 16;
const ENODEV: i64 = 19;
const E2BIG: i64 = 7;
const ENODATA: i64 = 61;
const EMSGSIZE: i64 = 90;
const EOPNOTSUPP: i64 = 95;
const AT_FDCWD: i64 = -100;
const AT_SYMLINK_NOFOLLOW: u64 = 0x100;
const AT_NO_AUTOMOUNT: u64 = 0x800;
const AT_EMPTY_PATH: u64 = 0x1000;
const AT_RECURSIVE: u64 = 0x8000;
const O_CLOEXEC: u64 = 0o2000000;

/// fsmount と mount_setattr で使える印 (Linux の FSMOUNT_VALID_FLAGS)
const ATTR_VALID: u32 = vfs::ATTR_RDONLY | vfs::ATTR_NOSUID | vfs::ATTR_NODEV | vfs::ATTR_NOEXEC | vfs::ATTR_ATIME | vfs::ATTR_NODIRATIME | vfs::ATTR_NOSYMFOLLOW;
const ATTR_IDMAP: u32 = 0x10_0000;

/// fs のコンテキストのいまのところ (Linux の fs_context_phase)
#[derive(Clone, Copy, PartialEq)]
enum Phase {
    /// 値を入れている (fsopen のあと)
    Create,
    /// 作った: fsmount を待つ
    Await,
    /// 付けなおしの値を入れている (fspick と fsmount のあと)
    Reconf,
    /// 作るのにしくじった
    Failed,
}

/// fsconfig で入れた値
#[derive(Default)]
struct Params {
    source: Option<String>,
    /// ro / rw (Linux の sb_flags の SB_RDONLY)
    ro: Option<bool>,
    mode: Option<u32>,
    uid: Option<u32>,
    gid: Option<u32>,
}

/// fs のコンテキスト (fsopen と fspick の口: Linux の struct fs_context)
pub struct FsCtx {
    fstype: String,
    phase: Cell<Phase>,
    p: RefCell<Params>,
    /// 作ったもの (fspick なら、そのマウントの根)
    root: RefCell<Option<InodeRef>>,
    /// 理由の書き置き (read で 1 つずつ。Linux と同じく 8 つまで)
    log: RefCell<VecDeque<String>>,
}

impl FsCtx {
    fn new(fstype: String, phase: Phase, root: Option<InodeRef>) -> Rc<FsCtx> {
        Rc::new(FsCtx { fstype, phase: Cell::new(phase), p: RefCell::new(Params::default()), root: RefCell::new(root), log: RefCell::new(VecDeque::new()) })
    }

    /// 理由を書き置いて EINVAL (Linux の invalf: "e " から始まり改行で終わる)
    fn invalf(&self, msg: String) -> i64 {
        let mut l = self.log.borrow_mut();
        if l.len() == 8 {
            l.pop_front();
        }
        l.push_back(alloc::format!("e {}\n", msg));
        crate::why::inval()
    }

    /// 1 つの値 (Linux の vfs_parse_fs_param: 共通の印、ファイルシステムのもの、source の順。知らなければ
    /// "<種類>: Unknown parameter '<名前>'")
    fn param(&self, key: &str, val: Option<&str>) -> Result<(), i64> {
        let mut p = self.p.borrow_mut();
        match key {
            "ro" => p.ro = Some(true),
            "rw" => p.ro = Some(false),
            "sync" | "async" | "dirsync" | "lazytime" | "nolazytime" | "mand" | "nomand" => {}
            _ => {
                let t = self.fstype.as_str();
                let bad = |me: &FsCtx| me.invalf(alloc::format!("{}: Bad value for '{}'", t, key));
                match (t, key) {
                    ("tmpfs", "size" | "nr_blocks" | "nr_inodes") => {
                        let v = val.ok_or_else(|| bad(self))?;
                        let n = v.trim_end_matches(['k', 'K', 'm', 'M', 'g', 'G', '%']);
                        n.parse::<u64>().map_err(|_| bad(self))?;
                    }
                    ("tmpfs", "mode") => p.mode = Some(val.and_then(|v| u32::from_str_radix(v, 8).ok()).ok_or_else(|| bad(self))? & 0o7777),
                    ("tmpfs", "uid") => p.uid = Some(val.and_then(|v| v.parse().ok()).ok_or_else(|| bad(self))?),
                    ("tmpfs", "gid") => p.gid = Some(val.and_then(|v| v.parse().ok()).ok_or_else(|| bad(self))?),
                    ("tmpfs", "huge" | "mpol" | "inode32" | "inode64" | "noswap" | "quota" | "usrquota" | "grpquota") => {}
                    ("proc", "hidepid" | "gid" | "subset" | "pidns") => {}
                    (_, "source") => {
                        let v = val.ok_or_else(|| self.invalf(String::from("Non-string source")))?;
                        if p.source.is_some() {
                            return Err(self.invalf(String::from("Multiple sources")));
                        }
                        p.source = Some(v.into());
                    }
                    _ => return Err(self.invalf(alloc::format!("{}: Unknown parameter '{}'", t, key))),
                }
            }
        }
        Ok(())
    }

    /// FSCONFIG_CMD_CREATE (Linux の vfs_cmd_create): 値を入れているところでなければ EBUSY、マウントしてよくなければ EPERM。
    /// tmpfs は mode (既定 1777) と持ち主 (既定は呼んだ人) で、ほかは装置のないもの (proc、sysfs ...)
    fn create(&self) -> Result<i64, i64> {
        if self.phase.get() != Phase::Create {
            return Err(-EBUSY);
        }
        may_mount()?;
        let p = self.p.borrow();
        let made = (|| -> Result<InodeRef, i64> {
            if self.fstype == "tmpfs" {
                let r: InodeRef = crate::tmpfs::new_root();
                r.set_mode(p.mode.unwrap_or(0o1777))?;
                let c = crate::cred::current();
                r.set_owner(Some(p.uid.unwrap_or(c.fsuid)), Some(p.gid.unwrap_or(c.fsgid)))?;
                Ok(r)
            } else {
                vfs::mount_nodev(&self.fstype)
            }
        })();
        match made {
            Ok(r) => {
                if p.ro == Some(true) {
                    vfs::set_sb_ro(r.id().0, true);
                }
                *self.root.borrow_mut() = Some(r);
                self.phase.set(Phase::Await);
                Ok(0)
            }
            Err(e) => {
                self.phase.set(Phase::Failed);
                Err(e)
            }
        }
    }

    /// FSCONFIG_CMD_RECONFIGURE (Linux の vfs_cmd_reconfigure): 付けなおしの値を入れているところでなければ EBUSY。ro / rw を
    /// ファイルシステムに。そのあとはまた値を入れられる
    fn reconfigure(&self) -> Result<i64, i64> {
        if self.phase.get() != Phase::Reconf {
            return Err(-EBUSY);
        }
        if !crate::cred::current().capable(crate::cred::CAP_SYS_ADMIN) {
            self.phase.set(Phase::Failed);
            return Err(-EPERM);
        }
        let ro = core::mem::take(&mut *self.p.borrow_mut()).ro;
        if let (Some(ro), Some(r)) = (ro, self.root.borrow().as_ref()) {
            vfs::set_sb_ro(r.id().0, ro);
        }
        Ok(0)
    }
}

impl FileOps for FsCtx {
    fn describe(&self) -> String {
        String::from("anon_inode:[fscontext]")
    }

    /// 理由を 1 つ (なければ ENODATA、入らなければ EMSGSIZE: Linux の fscontext_read)
    fn read(&self, dst: &mut [u8], _nonblock: bool) -> Result<usize, i64> {
        let m = self.log.borrow_mut().pop_front().ok_or(-ENODATA)?;
        if m.len() > dst.len() {
            return Err(-EMSGSIZE);
        }
        dst[..m.len()].copy_from_slice(m.as_bytes());
        Ok(m.len())
    }
}

/// fs のコンテキストの口を置く
fn ctx_fd(c: Rc<FsCtx>, cloexec: bool) -> Result<i64, i64> {
    let f = crate::file::new(Kind::Ops(c), crate::file::O_RDWR);
    crate::proc::current().files().add(f, cloexec, 0).map(|fd| fd as i64).ok_or(-24)
}

/// fsopen(fs_name, flags) (Linux の順: マウントしてよいか EPERM、flags は FSOPEN_CLOEXEC だけ EINVAL、名前 EFAULT、
/// 知らない種類 ENODEV)
fn fsopen(namep: usize, flags: u64) -> Result<i64, i64> {
    may_mount()?;
    if flags & !1 != 0 {
        return Err(crate::why::inval());
    }
    let name = user_str(namep)?;
    if !vfs::has_filesystem(&name) {
        return Err(-ENODEV);
    }
    ctx_fd(FsCtx::new(name, Phase::Create, None), flags & 1 != 0)
}

/// fsconfig(fd, cmd, key, value, aux) (Linux の順: cmd ごとの key・value・aux のそろい EINVAL (知らない cmd は
/// EOPNOTSUPP)、fd (EBADF、fs のコンテキストでなければ EINVAL)、key と value を写す、それから cmd)
fn fsconfig(fd: i64, cmd: u64, keyp: usize, valp: usize, aux: i64) -> Result<i64, i64> {
    const SET_FLAG: u64 = 0;
    const SET_STRING: u64 = 1;
    const SET_BINARY: u64 = 2;
    const SET_PATH: u64 = 3;
    const SET_PATH_EMPTY: u64 = 4;
    const SET_FD: u64 = 5;
    const CMD_CREATE: u64 = 6;
    const CMD_RECONFIGURE: u64 = 7;
    const CMD_CREATE_EXCL: u64 = 8;
    if fd < 0 {
        return Err(crate::why::inval());
    }
    let (k, v) = (keyp != 0, valp != 0);
    let ok = match cmd {
        SET_FLAG => k && !v && aux == 0,
        SET_STRING => k && v && aux == 0,
        SET_BINARY => k && v && aux > 0 && aux <= 1 << 20,
        SET_PATH | SET_PATH_EMPTY => k && v && (aux == AT_FDCWD || aux >= 0),
        SET_FD => k && !v && aux >= 0,
        CMD_CREATE | CMD_RECONFIGURE | CMD_CREATE_EXCL => !k && !v && aux == 0,
        _ => return Err(-EOPNOTSUPP),
    };
    if !ok {
        return Err(crate::why::inval());
    }
    let f = file_of(fd as u64).map_err(|_| -EBADF)?;
    let c = f.borrow().kind.ops::<FsCtx>().ok_or_else(crate::why::inval)?;
    let key = if k { Some(user_str(keyp)?) } else { None };
    let val = match cmd {
        SET_STRING | SET_PATH | SET_PATH_EMPTY => Some(user_str(valp)?),
        SET_FD => {
            file_of(aux as u64).map_err(|_| -EBADF)?;
            None
        }
        _ => None,
    };
    match cmd {
        CMD_CREATE | CMD_CREATE_EXCL => c.create(),
        CMD_RECONFIGURE => c.reconfigure(),
        _ => {
            if !matches!(c.phase.get(), Phase::Create | Phase::Reconf) {
                return Err(-EBUSY);
            }
            c.param(&key.unwrap_or_default(), val.as_deref()).map(|_| 0)
        }
    }
}

/// fsmount(fs_fd, flags, attr_flags) (Linux の順: EPERM、flags は FSMOUNT_CLOEXEC だけ、印 (atime は 1 つ) EINVAL、fd
/// (EBADF、fs のコンテキストでなければ EINVAL)、作っていなければ EINVAL、もうマウントにしたものは EBUSY)。答えは
/// はなれたマウントの根の O_PATH の口
fn fsmount(fd: i64, flags: u64, attrs: u64) -> Result<i64, i64> {
    may_mount()?;
    let attrs = attrs as u32;
    if flags & !1 != 0 || attrs & !ATTR_VALID != 0 || !matches!(attrs & vfs::ATTR_ATIME, 0 | vfs::ATTR_NOATIME | vfs::ATTR_STRICTATIME) {
        return Err(crate::why::inval());
    }
    let f = file_of(fd as u64).map_err(|_| -EBADF)?;
    let c = f.borrow().kind.ops::<FsCtx>().ok_or_else(crate::why::inval)?;
    let root = c.root.borrow().clone().ok_or_else(crate::why::inval)?;
    if c.phase.get() != Phase::Await {
        return Err(-EBUSY);
    }
    let source = core::mem::take(&mut *c.p.borrow_mut()).source.unwrap_or_else(|| String::from("none"));
    let id = vfs::detached_new(root.clone(), &source, &c.fstype, attrs & vfs::ATTR_RDONLY != 0, attrs & !vfs::ATTR_RDONLY);
    c.phase.set(Phase::Reconf);
    let (fd, file) = path_fd(root, vfs::detached_path(id), flags & 1 != 0)?;
    vfs::detached_file(id, &file);
    Ok(fd)
}

/// dfd と path から (たどったパス, inode, 空のパスなら dfd の口)。empty (AT_EMPTY_PATH) でなければ空は ENOENT
fn path_of(dfd: i64, path: &str, follow: bool, empty: bool) -> Result<(String, InodeRef, Option<crate::file::FileRef>), i64> {
    if path.is_empty() {
        if !empty {
            return Err(-ENOENT);
        }
        if dfd == AT_FDCWD {
            let (p, i) = vfs::lookup("", &crate::proc::current().files().cwd.clone(), true)?;
            return Ok((p, i, None));
        }
        let f = file_of(dfd as u64)?;
        let (p, i) = match &f.borrow().kind {
            Kind::Inode(i, p) => (String::from(p.trim_start_matches('/')), i.clone()),
            _ => return Err(crate::why::inval()),
        };
        return Ok((p, i, Some(f)));
    }
    let (p, i) = at_full(dfd, path, if follow { 0 } else { AT_SYMLINK_NOFOLLOW })?;
    Ok((p, i, None))
}

/// fspick(dfd, path, flags) (Linux の順: EPERM、flags EINVAL、たどる、マウントの根でなければ EINVAL)。答えは付けなおしの
/// fs のコンテキスト
fn fspick(dfd: i64, pathp: usize, flags: u64) -> Result<i64, i64> {
    const CLOEXEC: u64 = 1;
    const NOFOLLOW: u64 = 2;
    const NO_AUTOMOUNT: u64 = 4;
    const EMPTY: u64 = 8;
    may_mount()?;
    if flags & !(CLOEXEC | NOFOLLOW | NO_AUTOMOUNT | EMPTY) != 0 {
        return Err(crate::why::inval());
    }
    let (full, ino, _) = path_of(dfd, &user_str(pathp)?, flags & NOFOLLOW == 0, flags & EMPTY != 0)?;
    let (root, fstype) = vfs::mount_root(&full).filter(|(r, _)| r.id() == ino.id()).ok_or_else(crate::why::inval)?;
    ctx_fd(FsCtx::new(fstype, Phase::Reconf, Some(root)), flags & CLOEXEC != 0)
}

/// open_tree(dfd, path, flags): attr のない open_tree_attr
fn open_tree(dfd: i64, pathp: usize, flags: u64) -> Result<i64, i64> {
    open_tree_attr(dfd, pathp, flags, 0, 0)
}

/// open_tree_attr(dfd, path, flags, attr, size) (Linux 6.15。Linux の順: attr がないのに size があれば EINVAL、
/// vfs_open_tree (flags、AT_RECURSIVE は OPEN_TREE_CLONE とだけ EINVAL、CLONE はマウントしてよいか EPERM、たどる)、
/// それから attr (read_attr)。CLONE ならそこを写したはなれたマウントに印をつけ (AT_RECURSIVE なら下のものにも)、その口。
/// でなければ、印があればそこのマウントそのものにつけ (マウントの根でなければ EINVAL)、そこの O_PATH の口
fn open_tree_attr(dfd: i64, pathp: usize, flags: u64, attrp: usize, size: usize) -> Result<i64, i64> {
    const CLONE: u64 = 1;
    if attrp == 0 && size != 0 {
        return Err(crate::why::inval());
    }
    if flags & !(AT_EMPTY_PATH | AT_NO_AUTOMOUNT | AT_RECURSIVE | AT_SYMLINK_NOFOLLOW | CLONE | O_CLOEXEC) != 0 || flags & (AT_RECURSIVE | CLONE) == AT_RECURSIVE {
        return Err(crate::why::inval());
    }
    let clone = flags & CLONE != 0;
    if clone {
        may_mount()?;
    }
    let (full, ino, _) = path_of(dfd, &user_str(pathp)?, flags & AT_SYMLINK_NOFOLLOW == 0, flags & AT_EMPTY_PATH != 0)?;
    let attr = if attrp != 0 { read_attr(attrp, size)? } else { None };
    let cloexec = flags & O_CLOEXEC != 0;
    if !clone {
        if let Some((set, clr)) = attr {
            if !vfs::mount_root(&full).is_some_and(|(r, _)| r.id() == ino.id()) {
                return Err(crate::why::inval());
            }
            vfs::set_mount_attrs(&full, set, clr, false)?;
        }
        return path_fd(ino, full, cloexec).map(|(fd, _)| fd);
    }
    let recursive = flags & AT_RECURSIVE != 0;
    let id = vfs::detached_clone(&full, ino.clone(), recursive)?;
    if let Some((set, clr)) = attr {
        vfs::set_mount_attrs(&vfs::detached_path(id), set, clr, recursive)?;
    }
    let (fd, file) = path_fd(ino, vfs::detached_path(id), cloexec)?;
    vfs::detached_file(id, &file);
    Ok(fd)
}

/// move_mount(from_dfd, from_path, to_dfd, to_path, flags) (Linux の順: EPERM、flags EINVAL、from と to をたどる
/// (F_ / T_SYMLINKS でたどり、F_ / T_EMPTY_PATH で空)、それから do_move_mount: はなれたマウントなら置き、ほかはマウントの
/// 根を移す。置いたら、from の口のパスも置いたところに
fn move_mount(fdfd: i64, fpathp: usize, tdfd: i64, tpathp: usize, flags: u64) -> Result<i64, i64> {
    const F_SYMLINKS: u64 = 1;
    const F_EMPTY_PATH: u64 = 4;
    const T_SYMLINKS: u64 = 0x10;
    const T_EMPTY_PATH: u64 = 0x40;
    const SET_GROUP: u64 = 0x100;
    const BENEATH: u64 = 0x200;
    may_mount()?;
    if flags & !0x377 != 0 || flags & (SET_GROUP | BENEATH) == SET_GROUP | BENEATH {
        return Err(crate::why::inval());
    }
    let (from, _, ffile) = path_of(fdfd, &user_str(fpathp)?, flags & F_SYMLINKS != 0, flags & F_EMPTY_PATH != 0)?;
    let (to, _, _) = path_of(tdfd, &user_str(tpathp)?, flags & T_SYMLINKS != 0, flags & T_EMPTY_PATH != 0)?;
    if flags & SET_GROUP != 0 {
        // 伝わり方のなかま: aios のマウントはいつも private なので、どちらもマウントの根であればよい
        return if vfs::mount_root(&from).is_some() && vfs::mount_root(&to).is_some() { Ok(0) } else { Err(crate::why::inval()) };
    }
    if flags & BENEATH != 0 {
        return Err(crate::why::inval());
    }
    match vfs::detached_id(&from) {
        Some(id) => {
            if from != vfs::detached_path(id) {
                return Err(crate::why::inval());
            }
            vfs::detached_attach(id, &to)?;
            if let Some(f) = ffile
                && let Kind::Inode(_, p) = &mut f.borrow_mut().kind
            {
                *p = to;
            }
        }
        None => vfs::move_mount(&from, &to)?,
    }
    Ok(0)
}

/// mount_attr を読んでたしかめる (Linux の wants_mount_setattr と build_mount_kattr の順): 大きすぎる E2BIG、小さい
/// EINVAL、マウントしてよいか EPERM、写す EFAULT (知らないところが 0 でなければ E2BIG)、なにもしなければ None、印の
/// たしかめ (伝わり方は 1 つ、atime は ATTR_ATIME を clr にして set に 1 つ) EINVAL、idmap のマウントはない (EINVAL)。
/// (つける印, 外す印)
fn read_attr(attrp: usize, size: usize) -> Result<Option<(u32, u32)>, i64> {
    const PROPAGATION: u64 = (1 << 17) | (1 << 18) | (1 << 19) | (1 << 20);
    if size > 4096 {
        return Err(-E2BIG);
    }
    if size < 32 {
        return Err(crate::why::inval());
    }
    may_mount()?;
    let mut b = alloc::vec![0u8; size];
    crate::proc::current().pt().copy_in(&mut b, attrp).ok_or(-EFAULT)?;
    if b[32..].iter().any(|&x| x != 0) {
        return Err(-E2BIG);
    }
    let w = |i: usize| u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
    let (set, clr, prop) = (w(0), w(1), w(2));
    if set == 0 && clr == 0 && prop == 0 {
        return Ok(None);
    }
    if prop & !PROPAGATION != 0 || prop.count_ones() > 1 || (set | clr) & !(ATTR_VALID | ATTR_IDMAP) as u64 != 0 {
        return Err(crate::why::inval());
    }
    let (set, clr) = (set as u32, clr as u32);
    if clr & vfs::ATTR_ATIME != 0 {
        if clr & vfs::ATTR_ATIME != vfs::ATTR_ATIME || !matches!(set & vfs::ATTR_ATIME, 0 | vfs::ATTR_NOATIME | vfs::ATTR_STRICTATIME) {
            return Err(crate::why::inval());
        }
    } else if set & vfs::ATTR_ATIME != 0 {
        return Err(crate::why::inval());
    }
    if set & ATTR_IDMAP != 0 {
        return Err(crate::why::inval());
    }
    Ok(Some((set & !ATTR_IDMAP, clr & !ATTR_IDMAP)))
}

/// mount_setattr(dfd, path, flags, attr, size) (Linux の順: flags EINVAL、attr (read_attr。なにもしなければ 0)、たどる、
/// マウントの根でなければ EINVAL)
fn mount_setattr(dfd: i64, pathp: usize, flags: u64, attrp: usize, size: usize) -> Result<i64, i64> {
    if flags & !(AT_EMPTY_PATH | AT_RECURSIVE | AT_SYMLINK_NOFOLLOW | AT_NO_AUTOMOUNT) != 0 {
        return Err(crate::why::inval());
    }
    let Some((set, clr)) = read_attr(attrp, size)? else { return Ok(0) };
    let (full, ino, _) = path_of(dfd, &user_str(pathp)?, flags & AT_SYMLINK_NOFOLLOW == 0, flags & AT_EMPTY_PATH != 0)?;
    if !vfs::mount_root(&full).is_some_and(|(r, _)| r.id() == ino.id()) {
        return Err(crate::why::inval());
    }
    vfs::set_mount_attrs(&full, set, clr, flags & AT_RECURSIVE != 0)?;
    Ok(0)
}

/// システムコール (main.rs が syscall::register で置く)
pub const SYSCALLS: &[u64] = {
    use crate::syscall::nr::*;
    &[OPEN_TREE, MOVE_MOUNT, FSOPEN, FSCONFIG, FSMOUNT, FSPICK, MOUNT_SETATTR, OPEN_TREE_ATTR]
};

pub fn sys(n: u64, a: &[u64; 6]) -> Result<i64, i64> {
    use crate::syscall::nr::*;
    let int = |x: u64| x as i32 as i64;
    match n {
        FSOPEN => fsopen(a[0] as usize, a[1] as u32 as u64),
        FSCONFIG => fsconfig(int(a[0]), a[1] as u32 as u64, a[2] as usize, a[3] as usize, int(a[4])),
        FSMOUNT => fsmount(int(a[0]), a[1] as u32 as u64, a[2] as u32 as u64),
        FSPICK => fspick(int(a[0]), a[1] as usize, a[2] as u32 as u64),
        OPEN_TREE => open_tree(int(a[0]), a[1] as usize, a[2] as u32 as u64),
        MOVE_MOUNT => move_mount(int(a[0]), a[1] as usize, int(a[2]), a[3] as usize, a[4] as u32 as u64),
        MOUNT_SETATTR => mount_setattr(int(a[0]), a[1] as usize, a[2] as u32 as u64, a[3] as usize, a[4] as usize),
        OPEN_TREE_ATTR => open_tree_attr(int(a[0]), a[1] as usize, a[2] as u32 as u64, a[3] as usize, a[4] as usize),
        _ => Err(-38),
    }
}

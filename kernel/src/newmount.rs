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
    /// tmpfs のオプション (mode、uid、gid、size、nr_inodes ...)
    tmpfs: crate::tmpfs::Options,
    /// ほかの種類のオプション ("," でつないだもの: cgroup2 の nsdelegate など)
    data: String,
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
                if t == "tmpfs" && key != "source" {
                    match p.tmpfs.param(key, val) {
                        Ok(true) => return Ok(()),
                        Err(()) => return Err(self.invalf(alloc::format!("{}: Bad value for '{}'", t, key))),
                        Ok(false) => {}
                    }
                }
                match (t, key) {
                    ("proc", "hidepid" | "gid" | "subset" | "pidns") => {}
                    // cgroup2 のオプション (FSCONFIG_CREATE で cgroup.rs に渡す)
                    ("cgroup2", "nsdelegate" | "favordynmods" | "memory_localevents" | "memory_recursiveprot" | "memory_hugetlb_accounting" | "pids_localevents") => {
                        if !p.data.is_empty() {
                            p.data.push(',');
                        }
                        p.data.push_str(key);
                    }
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
                p.tmpfs.new_root()
            } else {
                vfs::mount_nodev_data(&self.fstype, &p.data)
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

// ---- statmount と listmount (Linux 6.8。7.4 の形: fs/namespace.c) ----

/// 64 ビットのマウントの番号はこれより大きい (Linux の MNT_UNIQUE_ID_OFFSET)
const MNT_UNIQUE_ID_OFFSET: u64 = 1 << 31;
/// listmount の「根のマウント」(Linux の LSMT_ROOT)
const LSMT_ROOT: u64 = u64::MAX;
/// Linux の arm64 の TASK_SIZE_MAX (access_ok)
const TASK_SIZE_MAX: usize = 1 << 48;
const EOVERFLOW: i64 = 75;
/// struct statmount の決まったところの大きさ (7.4)
const SM_SIZE: usize = 512;
const SM_SB_BASIC: u64 = 0x1;
const SM_MNT_BASIC: u64 = 0x2;
const SM_PROPAGATE_FROM: u64 = 0x4;
const SM_MNT_ROOT: u64 = 0x8;
const SM_MNT_POINT: u64 = 0x10;
const SM_FS_TYPE: u64 = 0x20;
const SM_MNT_NS_ID: u64 = 0x40;
const SM_MNT_OPTS: u64 = 0x80;
const SM_FS_SUBTYPE: u64 = 0x100;
const SM_SB_SOURCE: u64 = 0x200;
const SM_OPT_ARRAY: u64 = 0x400;
const SM_OPT_SEC_ARRAY: u64 = 0x800;
const SM_SUPPORTED_MASK: u64 = 0x1000;
const SM_MNT_UIDMAP: u64 = 0x2000;
const SM_MNT_GIDMAP: u64 = 0x4000;
/// Linux の STATMOUNT_SUPPORTED
const SM_SUPPORTED: u64 = 0x7fff;
/// 文字を返すもの (Linux の STATMOUNT_STRING_REQ)
const SM_STRING_REQ: u64 = SM_MNT_ROOT | SM_MNT_POINT | SM_FS_TYPE | SM_MNT_OPTS | SM_FS_SUBTYPE | SM_SB_SOURCE | SM_OPT_ARRAY | SM_OPT_SEC_ARRAY | SM_MNT_UIDMAP | SM_MNT_GIDMAP;

/// struct mnt_id_req (7.4: 32 バイト。mnt_ns_fd / mnt_fd は 4 バイトめから)
struct IdReq {
    fd: u32,
    mnt_id: u64,
    param: u64,
    ns_id: u64,
}

/// mnt_id_req を読む (Linux の copy_mnt_id_req: 大きさ EFAULT / E2BIG / EINVAL、知らないところが 0 でなければ E2BIG、
/// BY_FD なら番号と ns の番号は 0 (EINVAL)、でなければ ns の口と番号の両方は EINVAL、番号が小さすぎれば EINVAL)
fn read_req(p: usize, by_fd: bool) -> Result<IdReq, i64> {
    let pt = crate::proc::current().pt();
    let mut sz = [0u8; 4];
    pt.copy_in(&mut sz, p).ok_or(-EFAULT)?;
    let size = u32::from_le_bytes(sz) as usize;
    if size > 4096 {
        return Err(-E2BIG);
    }
    if size < 24 {
        return Err(crate::why::inval());
    }
    let mut b = alloc::vec![0u8; size];
    crate::proc::current().pt().copy_in(&mut b, p).ok_or(-EFAULT)?;
    if b.len() > 32 && b[32..].iter().any(|&x| x != 0) {
        return Err(-E2BIG);
    }
    b.resize(32, 0);
    let w = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
    let r = IdReq { fd: u32::from_le_bytes(b[4..8].try_into().unwrap()), mnt_id: w(8), param: w(16), ns_id: w(24) };
    if by_fd {
        if r.mnt_id != 0 || r.ns_id != 0 {
            return Err(crate::why::inval());
        }
    } else if (r.fd != 0 && r.ns_id != 0) || r.mnt_id <= MNT_UNIQUE_ID_OFFSET {
        return Err(crate::why::inval());
    }
    Ok(r)
}

/// 頼まれた namespace (Linux の grab_requested_mnt_ns: 番号 (なければ ENOENT)、nsfs の口 (EBADF、マウントのものでなければ
/// EINVAL)、どちらもなければいまのもの)
fn req_ns(r: &IdReq) -> Result<Rc<vfs::MountNs>, i64> {
    const ENOENT: i64 = 2;
    if r.ns_id != 0 {
        return vfs::mnt_ns_by_id(r.ns_id).ok_or(-ENOENT);
    }
    if r.fd != 0 {
        let f = file_of(r.fd as u64)?;
        let b = f.borrow();
        return match &b.kind {
            Kind::Inode(ino, _) => crate::ns::mnt_ns_of(ino).ok_or_else(crate::why::inval),
            _ => Err(crate::why::inval()),
        };
    }
    Ok(vfs::this_mnt_ns())
}

/// ns の持ち主のユーザーの namespace で CAP_SYS_ADMIN (Linux の ns_capable_noaudit(ns->user_ns, ...))
fn ns_admin(ns: &vfs::MountNs) -> bool {
    crate::ns::ns_capable(&crate::proc::current().cred, ns.owner, crate::cred::CAP_SYS_ADMIN)
}

/// path が t の下か (先頭 / なし。t が "" ならいつも)
fn under(path: &str, t: &str) -> bool {
    t.is_empty() || path == t || (path.len() > t.len() && path.starts_with(t) && path.as_bytes()[t.len()] == b'/')
}

/// 見るときの根 (Linux の grab_requested_root): いまの namespace ならプロセスの root (chroot の根)、ほかなら その根
fn view_root(ns: &Rc<vfs::MountNs>) -> String {
    if Rc::ptr_eq(ns, &vfs::this_mnt_ns()) { crate::proc::current().files().root.clone() } else { String::new() }
}

/// i が o か、o の下 (親をたどって o に着く) のマウントか
fn descends(v: &[vfs::MountView], mut i: usize, o: usize) -> bool {
    for _ in 0..=v.len() {
        if i == o {
            return true;
        }
        let p = v[i].parent;
        if p == v[i].id {
            return false;
        }
        match v.iter().position(|m| m.id == p) {
            Some(j) => i = j,
            None => return false,
        }
    }
    false
}

/// 文字を 1 つ足す (Linux の statmount_string): はじめに空の文字を 1 つ置き、空ならなにもしない (印もつけない)。入れ物に
/// 入らなければ EOVERFLOW。足したところ
fn sm_string(strs: &mut alloc::vec::Vec<u8>, s: &[u8], bufsize: usize) -> Result<Option<u32>, i64> {
    if strs.is_empty() {
        strs.push(0);
    }
    if s.is_empty() {
        return Ok(None);
    }
    let start = strs.len();
    strs.extend_from_slice(s);
    if SM_SIZE + strs.len() >= bufsize {
        return Err(-EOVERFLOW);
    }
    strs.push(0);
    Ok(Some(start as u32))
}

/// statmount(req, buf, bufsize, flags) (Linux の順: flags EINVAL、mnt_id_req (read_req)、BY_FD なら口 (EBADF)、でなければ
/// namespace (req_ns。ほかの namespace の番号は CAP_SYS_ADMIN でなければ EPERM)、入れ物 (EFAULT。文字を頼むのに決まった
/// ところしか入らなければ EOVERFLOW)、マウントをさがす (ない・はなれたマウントは ENOENT、根から見えないものは
/// CAP_SYS_ADMIN でなければ EPERM)。それから Linux の do_statmount の順に入れる。マウントの伝わり方はいつも MS_PRIVATE、
/// セキュリティのオプション、subtype、idmap はない (印をつけない)
fn statmount(reqp: usize, bufp: usize, bufsize: usize, flags: u64) -> Result<i64, i64> {
    const BY_FD: u64 = 1;
    const ENOENT: i64 = 2;
    const SB_RDONLY: u32 = 1;
    const MS_PRIVATE: u64 = 1 << 18;
    if flags & !BY_FD != 0 {
        return Err(crate::why::inval());
    }
    let by_fd = flags & BY_FD != 0;
    let r = read_req(reqp, by_fd)?;
    let file_path = if by_fd {
        let f = file_of(r.fd as u64)?;
        let b = f.borrow();
        match &b.kind {
            Kind::Inode(_, p) => Some(String::from(p.trim_start_matches('/'))),
            _ => return Err(crate::why::inval()),
        }
    } else {
        None
    };
    let ns = if by_fd { vfs::this_mnt_ns() } else { req_ns(&r)? };
    let same = Rc::ptr_eq(&ns, &vfs::this_mnt_ns());
    if !by_fd && r.ns_id != 0 && !same && !ns_admin(&ns) {
        return Err(-EPERM);
    }
    let mask = r.param;
    if bufp.checked_add(bufsize).is_none_or(|e| e > TASK_SIZE_MAX) {
        return Err(-EFAULT);
    }
    if mask & SM_STRING_REQ != 0 && bufsize <= SM_SIZE {
        return Err(-EOVERFLOW);
    }
    let views = vfs::mount_views(&ns);
    let root = view_root(&ns);
    let i = match &file_path {
        Some(p) => {
            if vfs::detached_id(p).is_some() {
                return Err(-ENOENT);
            }
            let (id, _) = vfs::mount_of(p);
            views.iter().position(|m| m.id as u64 == id).ok_or(-ENOENT)?
        }
        None => {
            let i = views.iter().position(|m| vfs::unique_mnt_id(m.id as u64) == r.mnt_id).ok_or(-ENOENT)?;
            if !under(&views[i].target, &root) && !ns_admin(&ns) {
                return Err(-EPERM);
            }
            i
        }
    };
    let m = &views[i];
    let fs = m.root.id().0;
    let mut sm = [0u8; SM_SIZE];
    let mut got: u64 = 0;
    let put32 = |sm: &mut [u8; SM_SIZE], at: usize, v: u32| sm[at..at + 4].copy_from_slice(&v.to_le_bytes());
    let put64 = |sm: &mut [u8; SM_SIZE], at: usize, v: u64| sm[at..at + 8].copy_from_slice(&v.to_le_bytes());
    if mask & SM_MNT_BASIC != 0 {
        got |= SM_MNT_BASIC;
        put64(&mut sm, 40, vfs::unique_mnt_id(m.id as u64));
        put64(&mut sm, 48, vfs::unique_mnt_id(m.parent as u64));
        put32(&mut sm, 56, m.id);
        put32(&mut sm, 60, m.parent);
        put64(&mut sm, 64, (m.attrs | if m.ro { vfs::ATTR_RDONLY } else { 0 }) as u64);
        put64(&mut sm, 72, MS_PRIVATE);
    }
    if mask & SM_SB_BASIC != 0 {
        got |= SM_SB_BASIC;
        let (major, minor) = vfs::fs_dev(fs);
        put32(&mut sm, 16, major);
        put32(&mut sm, 20, minor);
        put64(&mut sm, 24, u64::from_le_bytes(m.root.statfs()[0..8].try_into().unwrap()));
        put32(&mut sm, 32, if vfs::sb_readonly(fs) { SB_RDONLY } else { 0 });
    }
    if mask & SM_PROPAGATE_FROM != 0 {
        got |= SM_PROPAGATE_FROM;
    }
    // 文字 (Linux と同じ順)
    let mut strs = alloc::vec::Vec::new();
    let opts = vfs::fs_opts(fs);
    let point = if under(&m.target, &root) {
        let rel = if root.is_empty() { m.target.as_str() } else { m.target[root.len()..].trim_start_matches('/') };
        alloc::format!("/{}", rel)
    } else {
        String::new()
    };
    let opt_list: alloc::vec::Vec<&str> = opts.split(',').filter(|s| !s.is_empty()).collect();
    let strings: [(u64, usize, alloc::vec::Vec<u8>); 7] = [
        (SM_FS_TYPE, 36, m.fstype.clone().into_bytes()),
        (SM_MNT_ROOT, 104, m.root_path.clone().into_bytes()),
        (SM_MNT_POINT, 108, point.into_bytes()),
        (SM_MNT_OPTS, 4, String::from(opts.trim_start_matches(',')).into_bytes()),
        (SM_OPT_ARRAY, 132, opt_list.join("\0").into_bytes()),
        (SM_FS_SUBTYPE, 120, alloc::vec::Vec::new()),
        (SM_SB_SOURCE, 124, m.source.clone().into_bytes()),
    ];
    for (bit, at, s) in strings.iter() {
        if mask & bit != 0 {
            if let Some(off) = sm_string(&mut strs, s, bufsize)? {
                got |= bit;
                put32(&mut sm, *at, off);
                if *bit == SM_OPT_ARRAY {
                    put32(&mut sm, 128, opt_list.len() as u32);
                }
            }
        }
    }
    // セキュリティのオプションと idmap はない: 文字を置くだけ (空の文字)
    for bit in [SM_OPT_SEC_ARRAY, SM_MNT_UIDMAP, SM_MNT_GIDMAP] {
        if mask & bit != 0 {
            sm_string(&mut strs, b"", bufsize)?;
        }
    }
    if mask & SM_MNT_NS_ID != 0 {
        got |= SM_MNT_NS_ID;
        put64(&mut sm, 112, crate::ns::ns_id(ns.id));
    }
    if mask & SM_SUPPORTED_MASK != 0 {
        got |= SM_SUPPORTED_MASK;
        put64(&mut sm, 144, SM_SUPPORTED);
    }
    put64(&mut sm, 8, got);
    let pt = crate::proc::current().pt();
    if !strs.is_empty() {
        pt.copy_out(bufp + SM_SIZE, &strs).ok_or(-EFAULT)?;
    }
    let copy = bufsize.min(SM_SIZE);
    put32(&mut sm, 0, (copy + strs.len()) as u32);
    crate::proc::current().pt().copy_out(bufp, &sm[..copy]).ok_or(-EFAULT)?;
    Ok(0)
}

/// listmount(req, mnt_ids, nr_mnt_ids, flags) (Linux の順: flags EINVAL、多すぎる EOVERFLOW、入れ物 EFAULT、mnt_id_req、
/// 続きの番号が小さすぎれば EINVAL、namespace (ほかの namespace の番号は CAP_SYS_ADMIN でなければ ENOENT)、もとの
/// マウント (LSMT_ROOT は根。なければ ENOENT、根から見えなければ CAP_SYS_ADMIN でなければ EPERM))。もとの下にある
/// マウント (LSMT_ROOT なら根も) を番号の順に (LISTMOUNT_REVERSE は逆)、param の番号の次から、nr_mnt_ids こまで
fn listmount(reqp: usize, idsp: usize, nr: usize, flags: u64) -> Result<i64, i64> {
    const REVERSE: u64 = 1;
    const ENOENT: i64 = 2;
    if flags & !REVERSE != 0 {
        return Err(crate::why::inval());
    }
    if nr > 1_000_000 {
        return Err(-EOVERFLOW);
    }
    if idsp.checked_add(nr * 8).is_none_or(|e| e > TASK_SIZE_MAX) {
        return Err(-EFAULT);
    }
    let r = read_req(reqp, false)?;
    let last = r.param;
    if last != 0 && last <= MNT_UNIQUE_ID_OFFSET {
        return Err(crate::why::inval());
    }
    let ns = req_ns(&r)?;
    if r.ns_id != 0 && !Rc::ptr_eq(&ns, &vfs::this_mnt_ns()) && !ns_admin(&ns) {
        return Err(-ENOENT);
    }
    let views = vfs::mount_views(&ns);
    let root = view_root(&ns);
    let (orig, orig_path) = if r.mnt_id == LSMT_ROOT {
        let i = (0..views.len()).filter(|&i| under(&root, &views[i].target)).max_by_key(|&i| (views[i].target.len(), i)).unwrap_or(0);
        (i, root.clone())
    } else {
        let i = views.iter().position(|m| vfs::unique_mnt_id(m.id as u64) == r.mnt_id).ok_or(-ENOENT)?;
        if !under(&views[i].target, &root) && !ns_admin(&ns) {
            return Err(-EPERM);
        }
        (i, views[i].target.clone())
    };
    let mut order: alloc::vec::Vec<usize> = (0..views.len()).collect();
    order.sort_by_key(|&i| views[i].id);
    if flags & REVERSE != 0 {
        order.reverse();
    }
    let mut out = alloc::vec::Vec::new();
    for i in order {
        if out.len() / 8 == nr {
            break;
        }
        let uid = vfs::unique_mnt_id(views[i].id as u64);
        let after = match (last, flags & REVERSE != 0) {
            (0, _) => true,
            (_, false) => last != u64::MAX && uid > last,
            (_, true) => uid < last,
        };
        if !after || uid == r.mnt_id || !descends(&views, i, orig) || !under(&views[i].target, &orig_path) {
            continue;
        }
        out.extend_from_slice(&uid.to_le_bytes());
    }
    if !out.is_empty() {
        crate::proc::current().pt().copy_out(idsp, &out).ok_or(-EFAULT)?;
    }
    Ok((out.len() / 8) as i64)
}

/// システムコール (main.rs が syscall::register で置く)
pub const SYSCALLS: &[u64] = {
    use crate::syscall::nr::*;
    &[OPEN_TREE, MOVE_MOUNT, FSOPEN, FSCONFIG, FSMOUNT, FSPICK, MOUNT_SETATTR, OPEN_TREE_ATTR, STATMOUNT, LISTMOUNT]
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
        STATMOUNT => statmount(a[0] as usize, a[1] as usize, a[2] as usize, a[3] as u32 as u64),
        LISTMOUNT => listmount(a[0] as usize, a[1] as usize, a[2] as usize, a[3] as u32 as u64),
        _ => Err(-38),
    }
}

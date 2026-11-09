// LSM のフック (Linux の security/security.c と lsm_syscalls.c)。core は AppArmor や landlock を名前で呼ばず、ここを呼ぶ。
// LSM は Lsm を実装した static (Linux の DEFINE_LSM) を持ち、main.rs が起動のときに register でつなぐ (security_init)。
// LSM が書かないフックは既定のまま: その LSM を入れずに作った Linux と同じ (許す)。
// 呼ぶ順は Linux の CONFIG_LSM="landlock,apparmor" と同じ: 重ねる LSM (landlock) が先、ラベルを持つ LSM (AppArmor。Linux の
// LSM_FLAG_EXCLUSIVE) があと。だめと言った LSM で止め、あとの LSM は呼ばない (call_int_hook)。いまの aios の順がこれと
// ちがうところは、そのフックに「aios の順」と書く (Linux に合わせるのは別のコミットで)。
// aios.off= で切った LSM もつないだまま: 切ったかは LSM がフックの中で見る (動かしたまま入れなおせるように)
use crate::cell::Early;
use crate::cred::Cred;
use crate::file::FileRef;
use crate::proc::Proc;
use crate::vfs::InodeRef;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::sync::Arc;
use core::any::Any;

const E2BIG: i64 = 7;
const EFAULT: i64 = 14;
const EINVAL: i64 = 22;
const ENOSYS: i64 = 38;
const EOPNOTSUPP: i64 = 95;

/// LSM がプロセスに持たせるもの (Linux の cred->security のその LSM の場所)。中はその LSM だけが知る。core は写す、
/// None にする、is_some を見るだけ。fork した子と分けあうので、変えるときは中を変えずに新しい Rc に入れかえる
pub type Blob = Rc<dyn Any>;

/// LSM のラベル (Linux の secctx。AppArmor のプロファイルの名前)。名前でくらべる。中を読むのはラベルを持つ LSM だけ。
/// Arc: 大きなロックのない file::new でも写す
#[derive(Clone, PartialEq)]
pub struct Label(pub Arc<str>);

/// exec で LSM が決めたこと (Linux の linux_binprm の cred のラベルと secureexec)。fatal: 新しいラベルでプログラムを
/// 写せない (戻れないところの先で SIGSEGV)
#[derive(Clone, Default)]
pub struct Bprm {
    pub label: Option<Label>,
    pub secure: bool,
    pub fatal: bool,
}

/// security_file_open に見せるもの (aios は開く前に見る)。dev: デバイス (文字か塊) を開く
pub struct FileOpen<'a> {
    pub path: &'a str,
    pub flags: u32,
    pub dir: bool,
    pub ouid: u32,
    pub how: How,
    pub dev: bool,
}

/// どこから開くか (Linux の security_file_open はみな同じ口: aios はそれぞれの道の場所で呼ぶ)
#[derive(Clone, Copy, PartialEq)]
pub enum How {
    /// すでにあるファイルを開く (path は開くもの)
    Open,
    /// O_CREAT で作ったばかりのものを開く (作る前のたしかめは may_o_create)
    Created,
    /// O_TMPFILE (path はディレクトリ)
    Tmpfile,
    /// exec するプログラム (#! のインタプリタと ld.so も)
    Exec,
}

/// security_path_rename に見せるもの
pub struct Rename<'a> {
    pub old: &'a str,
    pub new: &'a str,
    pub moving: &'a InodeRef,
    pub target: Option<&'a InodeRef>,
    pub exchange: bool,
}

/// パスの名前で決める LSM (AppArmor) だけが見るもの (Linux の security_inode_getattr、path_chmod、path_chown、
/// file_truncate、file_lock)。core は path_mediated のときだけ名前を引いて path_perm に渡す
#[derive(Clone, Copy)]
pub enum PathOp {
    Getattr,
    Chmod,
    Chown,
    Truncate,
    Lock { write: bool },
}

// uapi/linux/lsm.h と ptrace.h の番号
pub const LSM_ID_CAPABILITY: u64 = 100;
pub const LSM_ID_APPARMOR: u64 = 104;
pub const LSM_ID_LANDLOCK: u64 = 110;
pub const LSM_ATTR_CURRENT: u32 = 100;
pub const LSM_ATTR_EXEC: u32 = 101;
pub const LSM_ATTR_PREV: u32 = 104;
const LSM_FLAG_SINGLE: u32 = 1;
/// struct lsm_ctx の頭 (id、flags、len、ctx_len。どれも u64)
const LSM_CTX: usize = 32;
pub const PTRACE_MODE_READ: u32 = 0x01;
pub const PTRACE_MODE_ATTACH: u32 = 0x02;

/// LSM ひとつ (Linux の DEFINE_LSM と、つなぐフック)。既定は、その LSM を入れずに作った Linux と同じ (許す)。
/// フックは大きなロックの中で呼ぶ。capable だけは大きなロックのない道 (statc の Cred::may) からも来る (ラベルの
/// あるものはそこへ来ない: syscall::fast)
pub trait Lsm {
    /// Linux の struct lsm_id (名前と番号)
    fn id(&self) -> (&'static str, u64);
    /// 動いているか (lsm_* のシステムコールだけが見る。フックは切ってあっても呼ぶ)
    fn enabled(&self) -> bool;
    /// ラベルを持つ LSM か (Linux の LSM_FLAG_EXCLUSIVE)
    fn exclusive(&self) -> bool {
        false
    }
    fn capable(&self, _c: &Cred, _cap: u32, _audit: bool) -> bool {
        true
    }
    /// root に見せる capability をせばめる (capget)
    fn capget(&self, _c: &Cred) -> u64 {
        u64::MAX
    }
    fn task_kill(&self, _target: &Proc, _sig: i32) -> Result<(), i64> {
        Ok(())
    }
    fn ptrace_access_check(&self, _child: &Cred, _mode: u32) -> Result<(), i64> {
        Ok(())
    }
    fn bprm_creds_for_exec(&self, _b: &mut Bprm, _exe: &str, _ouid: u32) -> Result<(), i64> {
        Ok(())
    }
    /// aios: exec で写すプログラムを、新しいラベルで写せるか (Linux は elf_map の security_mmap_file)
    fn bprm_mmap_file(&self, _b: &Bprm, _full: &str, _ouid: u32) -> bool {
        true
    }
    fn bprm_committed_creds(&self, _c: &mut Cred) {}
    fn mmap_file(&self, _full: &str, _prot: u64, _private: bool, _ouid: u32) -> Result<(), i64> {
        Ok(())
    }
    fn file_permission(&self, _f: &FileRef, _write: bool) -> Result<(), i64> {
        Ok(())
    }
    /// 返すのは開いたファイルに覚えるビット
    fn file_open(&self, _o: &FileOpen) -> Result<u32, i64> {
        Ok(0)
    }
    /// aios: O_CREAT で作る前 (Linux の may_o_create の security_path_mknod と、作ったあとの security_file_open)
    fn may_o_create(&self, _full: &str, _flags: u32) -> Result<(), i64> {
        Ok(())
    }
    /// aios: パスの名前で決める決まりが、いまのプロセスにあるか (そのときだけ core が名前を引きなおす)
    fn path_mediated(&self) -> bool {
        false
    }
    fn path_perm(&self, _op: PathOp, _full: &str, _dir: bool, _ouid: u32) -> Result<(), i64> {
        Ok(())
    }
    fn path_mkdir(&self, _full: &str) -> Result<(), i64> {
        Ok(())
    }
    fn path_mknod(&self, _full: &str, _mode: u32, _dev: u64) -> Result<(), i64> {
        Ok(())
    }
    fn path_symlink(&self, _full: &str) -> Result<(), i64> {
        Ok(())
    }
    /// Linux の path_unlink と path_rmdir。消すもの (dir の name) は、要る LSM だけが引く
    fn path_unlink(&self, _full: &str, _dir: &InodeRef, _name: &str, _rmdir: bool) -> Result<(), i64> {
        Ok(())
    }
    /// aios: link で作る名前 (つなぐ元を引く前: Linux は security_path_link のなかで)
    fn path_link_make(&self, _full: &str, _mode: u32) -> Result<(), i64> {
        Ok(())
    }
    fn path_link(&self, _src: &str, _full: &str, _ino: &InodeRef) -> Result<(), i64> {
        Ok(())
    }
    fn path_rename(&self, _r: &Rename) -> Result<(), i64> {
        Ok(())
    }
    fn path_truncate(&self, _full: &str, _ouid: u32) -> Result<(), i64> {
        Ok(())
    }
    /// 開いたファイル (OpenFile の lsm_deny が 0 でないもの) を ftruncate する
    fn file_truncate(&self, _f: &FileRef) -> Result<(), i64> {
        Ok(())
    }
    /// 開いたファイル (OpenFile の lsm_deny が 0 でないもの) に ioctl する
    fn file_ioctl(&self, _f: &FileRef, _req: u64) -> Result<(), i64> {
        Ok(())
    }
    fn sb_mount(&self, _name: &str, _dev: Option<&str>, _fstype: Option<&str>, _flags: u64, _data: Option<&str>) -> Result<(), i64> {
        Ok(())
    }
    /// aios: bind の元が引けなかった (記録して、そのエラーを返す)
    fn sb_mount_error(&self, _flags: u64, e: i64) -> i64 {
        e
    }
    fn sb_umount(&self, _name: &str) -> Result<(), i64> {
        Ok(())
    }
    /// lsm_get_self_attr の中身 (改行なし)。None は答えない
    fn getselfattr(&self, _attr: u32) -> Result<Option<String>, i64> {
        Ok(None)
    }
    fn setselfattr(&self, _attr: u32, _value: &[u8]) -> Result<(), i64> {
        Err(-EOPNOTSUPP)
    }
    /// LSM のシステムコール (landlock_*: Linux の security/landlock/syscalls.c)。その LSM のものでなければ None
    fn syscall(&self, _nr: u64, _a: &[u64; 6]) -> Option<Result<i64, i64>> {
        None
    }
}

/// まだつないでいないところ
struct NoLsm;

impl Lsm for NoLsm {
    fn id(&self) -> (&'static str, u64) {
        ("", 0)
    }

    fn enabled(&self) -> bool {
        false
    }
}

/// 重ねる LSM (landlock) と、ラベルを持つ LSM (AppArmor)。起動のときに register が入れ、あとは読むだけ (Linux の
/// __ro_after_init。ロックなしで読める)
static STACKED: Early<&'static dyn Lsm> = Early::new(&NoLsm);
static EXCLUSIVE: Early<&'static dyn Lsm> = Early::new(&NoLsm);

/// 起動のとき (main.rs、ほかの CPU を起こす前) に LSM をつなぐ。aios.off= で切ったものもつなぐ
pub fn register(l: &'static dyn Lsm) {
    let slot = if l.exclusive() { &EXCLUSIVE } else { &STACKED };
    assert!(slot.val().id().1 == 0, "security: two LSMs in one slot");
    slot.set(l);
}

/// CONFIG_LSM の順 (capability は cred.rs の has_cap でいつも先)
#[inline]
fn lsms() -> [&'static dyn Lsm; 2] {
    [STACKED.val(), EXCLUSIVE.val()]
}

/// security_capable: commoncap (Cred::has_cap) が許したあと。だめと言った LSM で止まる (あとの LSM は記録もしない)。
/// ラベルも砂場もない cred は聞かない (どの LSM も許す)
#[inline]
pub fn capable(c: &Cred, cap: u32, audit: bool) -> bool {
    (c.label.is_none() && c.sandbox.is_none()) || lsms().iter().all(|l| l.capable(c, cap, audit))
}

/// security_capget: root に見せる capability を、それぞれの LSM がせばめる
pub fn capget(c: &Cred) -> u64 {
    lsms().iter().fold(u64::MAX, |m, l| m & l.capget(c))
}

/// security_task_kill
pub fn task_kill(target: &Proc, sig: i32) -> Result<(), i64> {
    for l in lsms() {
        l.task_kill(target, sig)?;
    }
    Ok(())
}

/// security_ptrace_access_check
pub fn ptrace_access_check(child: &Cred, mode: u32) -> Result<(), i64> {
    for l in lsms() {
        l.ptrace_access_check(child, mode)?;
    }
    Ok(())
}

/// aios の順: ラベルを持つ LSM だけ (get_robust_list。Linux の ptrace_may_access は landlock も見る)
pub fn ptrace_access_check_exclusive(child: &Cred, mode: u32) -> Result<(), i64> {
    EXCLUSIVE.val().ptrace_access_check(child, mode)
}

/// security_bprm_creds_for_exec: はじめはいまのラベル (prepare_exec_creds)。LSM が変える
pub fn bprm_creds_for_exec(exe: &str, ouid: u32) -> Result<Bprm, i64> {
    let mut b = Bprm { label: crate::cred::current().label.clone(), ..Default::default() };
    for l in lsms() {
        l.bprm_creds_for_exec(&mut b, exe, ouid)?;
    }
    Ok(b)
}

/// exec で写すプログラムを、新しいラベルで写せるか
pub fn bprm_mmap_file(b: &Bprm, full: &str, ouid: u32) -> bool {
    lsms().iter().all(|l| l.bprm_mmap_file(b, full, ouid))
}

/// security_bprm_committed_creds: exec で新しいラベルになったあと
pub fn bprm_committed_creds(c: &mut Cred) {
    for l in lsms() {
        l.bprm_committed_creds(c);
    }
}

/// security_mmap_file
pub fn mmap_file(full: &str, prot: u64, private: bool, ouid: u32) -> Result<(), i64> {
    for l in lsms() {
        l.mmap_file(full, prot, private, ouid)?;
    }
    Ok(())
}

/// security_file_permission (rw_verify_area)。ラベルのないプロセスが開いたファイルは見ない (AppArmor の __file_is_delegated)
#[inline]
pub fn file_permission(f: &FileRef, write: bool) -> Result<(), i64> {
    if f.borrow().label.is_none() {
        return Ok(());
    }
    for l in lsms() {
        l.file_permission(f, write)?;
    }
    Ok(())
}

/// security_file_open。返すのは開いたファイルに覚えるビット (OpenFile の lsm_deny)。aios の順: ラベルを持つ LSM が先
/// (Linux の CONFIG_LSM では landlock が先)
pub fn file_open(o: &FileOpen) -> Result<u32, i64> {
    let x = EXCLUSIVE.val().file_open(o)?;
    Ok(x | STACKED.val().file_open(o)?)
}

/// O_CREAT で作る前
pub fn may_o_create(full: &str, flags: u32) -> Result<(), i64> {
    for l in lsms() {
        l.may_o_create(full, flags)?;
    }
    Ok(())
}

/// パスの名前で決める決まりが、いまのプロセスにあるか
#[inline]
pub fn path_mediated() -> bool {
    crate::cred::current().label.is_some() && lsms().iter().any(|l| l.path_mediated())
}

/// security_inode_getattr、path_chmod、path_chown、file_truncate、file_lock (path_mediated のときだけ)
pub fn path_perm(op: PathOp, full: &str, dir: bool, ouid: u32) -> Result<(), i64> {
    for l in lsms() {
        l.path_perm(op, full, dir, ouid)?;
    }
    Ok(())
}

/// security_path_mkdir
pub fn path_mkdir(full: &str) -> Result<(), i64> {
    for l in lsms() {
        l.path_mkdir(full)?;
    }
    Ok(())
}

/// security_path_mknod (AF_UNIX の bind も)
pub fn path_mknod(full: &str, mode: u32, dev: u64) -> Result<(), i64> {
    for l in lsms() {
        l.path_mknod(full, mode, dev)?;
    }
    Ok(())
}

/// security_path_symlink
pub fn path_symlink(full: &str) -> Result<(), i64> {
    for l in lsms() {
        l.path_symlink(full)?;
    }
    Ok(())
}

/// security_path_unlink と path_rmdir
pub fn path_unlink(full: &str, dir: &InodeRef, name: &str, rmdir: bool) -> Result<(), i64> {
    for l in lsms() {
        l.path_unlink(full, dir, name, rmdir)?;
    }
    Ok(())
}

/// link で作る名前 (aios の順: つなぐ元を引く前。mode はつなぐもの)
pub fn path_link_make(full: &str, mode: u32) -> Result<(), i64> {
    for l in lsms() {
        l.path_link_make(full, mode)?;
    }
    Ok(())
}

/// security_path_link
pub fn path_link(src: &str, full: &str, ino: &InodeRef) -> Result<(), i64> {
    for l in lsms() {
        l.path_link(src, full, ino)?;
    }
    Ok(())
}

/// security_path_rename
pub fn path_rename(r: &Rename) -> Result<(), i64> {
    for l in lsms() {
        l.path_rename(r)?;
    }
    Ok(())
}

/// security_path_truncate
pub fn path_truncate(full: &str, ouid: u32) -> Result<(), i64> {
    for l in lsms() {
        l.path_truncate(full, ouid)?;
    }
    Ok(())
}

/// security_file_truncate。開いたときに覚えたものがなければ見ない
#[inline]
pub fn file_truncate(f: &FileRef) -> Result<(), i64> {
    if f.borrow().lsm_deny == 0 {
        return Ok(());
    }
    for l in lsms() {
        l.file_truncate(f)?;
    }
    Ok(())
}

/// security_file_ioctl。開いたときに覚えたものがなければ見ない
#[inline]
pub fn file_ioctl(f: &FileRef, req: u64) -> Result<(), i64> {
    if f.borrow().lsm_deny == 0 {
        return Ok(());
    }
    for l in lsms() {
        l.file_ioctl(f, req)?;
    }
    Ok(())
}

/// LSM のシステムコール (landlock_*)。答える LSM がなければ ENOSYS (Linux の COND_SYSCALL)
pub fn lsm_syscall(nr: u64, a: &[u64; 6]) -> Result<i64, i64> {
    for l in lsms() {
        if let Some(r) = l.syscall(nr, a) {
            return r;
        }
    }
    Err(-ENOSYS)
}

/// security_sb_mount
pub fn sb_mount(name: &str, dev: Option<&str>, fstype: Option<&str>, flags: u64, data: Option<&str>) -> Result<(), i64> {
    for l in lsms() {
        l.sb_mount(name, dev, fstype, flags, data)?;
    }
    Ok(())
}

/// bind の元が引けなかったとき (LSM が記録して、返すエラーを決める)
pub fn sb_mount_error(flags: u64, e: i64) -> i64 {
    lsms().iter().fold(e, |e, l| l.sb_mount_error(flags, e))
}

/// security_sb_umount
pub fn sb_umount(name: &str) -> Result<(), i64> {
    for l in lsms() {
        l.sb_umount(name)?;
    }
    Ok(())
}

/// パスの名前: 頭に /、ディレクトリなら終わりにも / (aa_path_name の PATH_IS_DIR)
pub fn path_name(full: &str, dir: bool) -> String {
    let full = full.trim_matches('/');
    if full.is_empty() {
        "/".into()
    } else if dir {
        format!("/{}/", full)
    } else {
        format!("/{}", full)
    }
}

// ---- LSM のシステムコール (Linux 6.8 から: security/lsm_syscalls.c と security.c。libapparmor は /proc/self/attr より先に使う) ----

fn get_user(va: usize, b: &mut [u8]) -> Result<(), i64> {
    crate::proc::current().pt().copy_in(b, va).ok_or(-EFAULT)
}

fn put_user(va: usize, b: &[u8]) -> Result<(), i64> {
    crate::proc::current().pt().copy_out(va, b).ok_or(-EFAULT)
}

fn get_u32(va: usize) -> Result<u32, i64> {
    let mut b = [0u8; 4];
    get_user(va, &mut b)?;
    Ok(u32::from_le_bytes(b))
}

/// lsm_list_modules(ids, size, flags): 動いている LSM の番号 (/sys/kernel/security/lsm と同じ順)
pub fn lsm_list_modules(ids: usize, size: usize, flags: u32) -> Result<i64, i64> {
    if flags != 0 {
        return Err(-EINVAL);
    }
    let have = get_u32(size)?;
    let mut v = alloc::vec![LSM_ID_CAPABILITY];
    for l in lsms() {
        if l.enabled() {
            v.push(l.id().1);
        }
    }
    let total = (v.len() * 8) as u32;
    put_user(size, &total.to_le_bytes())?;
    if have < total {
        return Err(-E2BIG);
    }
    for (i, id) in v.iter().enumerate() {
        put_user(ids + i * 8, &id.to_le_bytes())?;
    }
    Ok(v.len() as i64)
}

/// lsm_get_self_attr(attr, ctx, size, flags): いまのプロセスの current / exec / prev を struct lsm_ctx で (答えるのは
/// ラベルを持つ LSM: 中身は /proc/self/attr と同じ「名前 (モード)」の改行なし)。ctx が 0 か size が足りなければ、要る
/// 大きさを size に書く (足りなければ E2BIG)。flags の LSM_FLAG_SINGLE なら ctx の id の LSM だけ。答えた LSM の数を返す
pub fn lsm_get_self_attr(attr: u32, ctx: usize, size: usize, flags: u32) -> Result<i64, i64> {
    if attr == 0 || size == 0 {
        return Err(-EINVAL);
    }
    let left = get_u32(size)?;
    let mut single = None;
    if flags != 0 {
        if flags != LSM_FLAG_SINGLE || ctx == 0 {
            return Err(-EINVAL);
        }
        let mut b = [0u8; 8];
        get_user(ctx, &mut b)?;
        let id = u64::from_le_bytes(b);
        if id == 0 {
            return Err(-EINVAL);
        }
        single = Some(id);
    }
    let (mut total, mut count, mut toobig) = (0u32, 0i64, false);
    for l in lsms() {
        let id = l.id().1;
        if !l.enabled() || single.is_some_and(|s| s != id) {
            continue;
        }
        let Some(text) = l.getselfattr(attr)? else { continue };
        let v = text.as_bytes();
        // lsm_fill_user_ctx: 頭と中身を 8 バイトにそろえて
        let len = (LSM_CTX + v.len()).div_ceil(8) * 8;
        if len as u32 > left {
            toobig = true;
        } else {
            if ctx != 0 {
                let mut b = alloc::vec![0u8; len];
                for (i, x) in [id, 0, len as u64, v.len() as u64].iter().enumerate() {
                    b[i * 8..i * 8 + 8].copy_from_slice(&x.to_le_bytes());
                }
                b[LSM_CTX..LSM_CTX + v.len()].copy_from_slice(v);
                put_user(ctx, &b)?;
            }
            count += 1;
        }
        total += len as u32;
    }
    put_user(size, &total.to_le_bytes())?;
    if toobig {
        return Err(-E2BIG);
    }
    if count == 0 {
        return Err(-EOPNOTSUPP);
    }
    Ok(count)
}

/// lsm_set_self_attr(attr, ctx, size, flags): ctx の id の LSM に current か exec を書く (AppArmor なら
/// /proc/self/attr に書くのと同じ "changeprofile NAME" など)
pub fn lsm_set_self_attr(attr: u32, ctx: usize, size: u32, flags: u32) -> Result<i64, i64> {
    if flags != 0 || (size as usize) < LSM_CTX {
        return Err(-EINVAL);
    }
    if size > 4096 {
        return Err(-E2BIG);
    }
    let mut b = alloc::vec![0u8; size as usize];
    get_user(ctx, &mut b)?;
    let field = |i: usize| u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
    let (id, len, ctx_len) = (field(0), field(2), field(3));
    let need = (LSM_CTX as u64).checked_add(ctx_len);
    if (size as u64) < len || need.is_none_or(|n| len < n) {
        return Err(-EINVAL);
    }
    let Some(l) = lsms().into_iter().find(|l| l.enabled() && l.id().1 == id) else { return Err(-EOPNOTSUPP) };
    l.setselfattr(attr, &b[LSM_CTX..LSM_CTX + ctx_len as usize])?;
    Ok(0)
}

// ---- seccomp (Linux の kernel/seccomp.c と include/linux/seccomp.h)。LSM ではない (Linux も): 入り口だけをここに
// 置き、seccomp.rs が register_seccomp でつなぐ。入っていなければ Linux の !CONFIG_SECCOMP と同じ ----

/// seccomp のフィルタが決めたこと (Linux の __secure_computing の答え。user notification の listener の答えも)
pub enum Permit {
    /// システムコールをする
    Allow,
    /// この errno で返す (SECCOMP_RET_ERRNO)
    Errno(i64),
    /// SIGSYS を送る (SECCOMP_RET_TRAP。値は si_errno)
    Trap(u16),
    /// プロセスを止める (SECCOMP_RET_KILL_*)
    Kill,
    /// listener の答えの値で返す (システムコールはしない)
    Return(i64),
    /// listener を待つあいだにシグナルで割りこまれた (always ならハンドラのあとでもいつもやりなおす)
    Interrupted { always: bool },
}

/// seccomp (Linux の CONFIG_SECCOMP)。既定は入れずに作った Linux と同じ
pub trait Seccomp {
    /// s (Cred の seccomp) のフィルタで nr を見る (pc はシステムコールの場所)。user notification なら答えを待つ
    /// (眠る: s は呼ぶほうが写して持つ。ほかのスレッドの TSYNC で Cred の seccomp が入れかわっても消えないように)
    fn permit(&self, _s: &Blob, _nr: u64, _a: &[u64; 6], _pc: u64) -> Permit {
        Permit::Allow
    }
    /// スレッドの Proc を片づける (listener を起こす: 使う人がいなくなれば POLLHUP)
    fn release(&self, _c: &Cred) {}
    /// prctl(PR_GET_SECCOMP)
    fn mode(&self, _c: &Cred) -> i64 {
        0
    }
    /// prctl(PR_SET_SECCOMP)
    fn prctl_set(&self, _mode: u64, _prog: usize) -> Result<i64, i64> {
        Err(-EINVAL)
    }
    /// seccomp(2)
    fn syscall(&self, _op: u64, _flags: u64, _args: usize) -> Result<i64, i64> {
        Err(-ENOSYS)
    }
}

/// listener の fd (anon_inode:seccomp notify。file::Kind::SeccompNotify) の中身。さいごの参照を閉じると、中が待っている
/// ものを起こす (Drop)
pub trait Listener {
    /// poll: (読める, 書ける, POLLHUP)
    fn readiness(&self) -> (bool, bool, bool);
    /// poll と NOTIF_RECV が眠る chan
    fn chan(&self) -> usize;
    /// ioctl (NOTIF_RECV は眠る: 呼ぶほうは OpenFile を借りずに Rc を写して持つ)
    fn ioctl(&self, req: u64, arg: usize) -> Result<i64, i64>;
}

struct NoSeccomp;

impl Seccomp for NoSeccomp {}

static SECCOMP: Early<&'static dyn Seccomp> = Early::new(&NoSeccomp);

/// 起動のとき (main.rs) に seccomp をつなぐ。aios.off=seccomp でもつなぐ (切るのはシステムコールだけ: core の part_of)
pub fn register_seccomp(s: &'static dyn Seccomp) {
    SECCOMP.set(s);
}

/// seccomp_permit_syscall (Linux 7.4 の secure_computing): Cred の seccomp があるときだけ呼ぶ
#[inline]
pub fn seccomp_permit_syscall(s: &Blob, nr: u64, a: &[u64; 6], pc: u64) -> Permit {
    SECCOMP.val().permit(s, nr, a, pc)
}

/// seccomp_filter_release: スレッドの Proc を片づける
#[inline]
pub fn seccomp_filter_release(c: &Cred) {
    if c.seccomp.is_some() {
        SECCOMP.val().release(c);
    }
}

/// prctl_get_seccomp
pub fn prctl_get_seccomp(c: &Cred) -> i64 {
    SECCOMP.val().mode(c)
}

/// prctl_set_seccomp
pub fn prctl_set_seccomp(mode: u64, prog: usize) -> Result<i64, i64> {
    SECCOMP.val().prctl_set(mode, prog)
}

/// seccomp(2)
pub fn sys_seccomp(op: u64, flags: u64, args: usize) -> Result<i64, i64> {
    SECCOMP.val().syscall(op, flags, args)
}

// VFS: ファイルシステムの共通の形と、パスの探索、マウント
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::any::Any;

pub const EPERM: i64 = 1;
pub const ENOENT: i64 = 2;
pub const EEXIST: i64 = 17;
pub const EXDEV: i64 = 18;
pub const ENOTDIR: i64 = 20;
pub const EISDIR: i64 = 21;
pub const EINVAL: i64 = 22;
pub const ENOSPC: i64 = 28;
pub const ENODEV: i64 = 19;
pub const ENAMETOOLONG: i64 = 36;
pub const ENOTEMPTY: i64 = 39;
pub const ELOOP: i64 = 40;
pub const EOPNOTSUPP: i64 = 95;
pub const EIO: i64 = 5;

pub const S_IFMT: u32 = 0o170000;
pub const S_IFIFO: u32 = 0o010000;
pub const S_IFSOCK: u32 = 0o140000;
pub const S_IFCHR: u32 = 0o020000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFBLK: u32 = 0o060000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFLNK: u32 = 0o120000;

pub type InodeRef = Rc<dyn Inode>;

/// stat に必要なもの
#[derive(Clone, Copy, Default)]
pub struct Meta {
    pub ino: u64,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub rdev: u64,
    pub blocks: u64,
    pub mtime: u64,
    pub ctime: u64,
}

impl Meta {
    pub fn is_dir(&self) -> bool {
        self.mode & S_IFMT == S_IFDIR
    }
}

/// create で作るもの
pub enum NewNode {
    File,
    Dir,
    Symlink(String),
    Dev(u32, u32),
    /// ブロックデバイス (major, minor)
    Blk(u32, u32),
    Fifo,
    /// unix ソケットの名前 (bind が作る。開けない)
    Sock,
}

impl NewNode {
    pub fn type_bits(&self) -> u32 {
        match self {
            NewNode::File => S_IFREG,
            NewNode::Dir => S_IFDIR,
            NewNode::Symlink(_) => S_IFLNK,
            NewNode::Dev(..) => S_IFCHR,
            NewNode::Blk(..) => S_IFBLK,
            NewNode::Fifo => S_IFIFO,
            NewNode::Sock => S_IFSOCK,
        }
    }
}

pub struct DirEntry {
    pub name: String,
    pub ino: u64,
    pub mode: u32,
}

/// 1 つのファイルシステムの中のもの (ファイル、ディレクトリ、リンク、デバイス)
pub trait Inode {
    /// (ファイルシステムの番号, inode 番号)
    fn id(&self) -> (usize, u64);
    fn meta(&self) -> Meta;
    fn as_any(&self) -> &dyn Any;

    fn read_at(&self, off: usize, buf: &mut [u8]) -> Result<usize, i64>;
    fn write_at(&self, off: usize, buf: &[u8]) -> Result<usize, i64>;
    /// read をページキャッシュ (vm.rs) を通してよいか: 中身が変わるときにかならず vm::file_changed を呼ぶ
    /// ファイルシステム (ext4) のもの。procfs のように読むたびに作るものは false
    /// posix_fadvise の DONTNEED: off から len バイト (0 なら終わりまで) の、まだ書いていないキャッシュを外す
    /// (ファイルシステムの側: ページキャッシュは vm::uncache)
    fn uncache(&self, _off: u64, _len: u64) {}
    /// ふつうのファイルの foff (ページの境目) のページそのもの (ファイルシステムが持っている 1 枚: ext4 ならブロックの
    /// キャッシュのページ、tmpfs ならファイルのページ。参照をひとつ足して返す。使い終えたら kalloc::put)。
    /// read / write / 写し (mmap) がみな同じ 1 枚を見るので、どれで書いてもほかにすぐ映る (vm.rs の「ファイルのページは 1 枚」)。
    /// Ok(None) は穴 (ブロックがない: vm.rs が 0 のページを持ち、ブロックをとるとき adopt_hole でそのページを使ってもらう)。
    /// alloc なら、ブロックをとって (穴の 0 のページを引きとって) 書けるようにする (写しに書くとき: Linux の page_mkwrite)。
    /// ファイルの終わりの先 (同じページの中) は 0 にしてある。ページで持たないもの (procfs、ブロックがページでないもの) は ENODEV
    fn page_frame(&self, _foff: usize, _alloc: bool) -> Result<Option<*mut u8>, i64> {
        Err(-ENODEV)
    }
    /// 写し (MAP_SHARED) で foff のページに書いた (書いていた): ディスクに書き出す印をつけ、touch なら更新時刻をいまに
    /// (はじめて書いたとき。Linux の page_mkwrite の file_update_time)
    fn page_written(&self, _foff: usize, _touch: bool) -> Result<(), i64> {
        Ok(())
    }
    fn page_cacheable(&self) -> bool {
        false
    }
    /// パスの答えを覚えてよいか (名前が勝手に変わらないもの: ext4、tmpfs)。名前や属性を変えるときは dir_changed を呼ぶこと
    fn path_cacheable(&self) -> bool {
        false
    }
    fn truncate(&self, len: usize) -> Result<(), i64>;
    fn readlink(&self) -> Result<String, i64>;

    fn lookup(&self, name: &str) -> Result<InodeRef, i64>;
    /// . と .. は含めない
    fn readdir(&self) -> Result<Vec<DirEntry>, i64>;
    /// Linux と同じ位置で読めるディレクトリ (ext4 の dir_index): (読む位置, 項目)。. と .. もふくめ、位置の順。
    /// 終わりの位置は i64::MAX。None ならこのディレクトリは readdir で (位置は何番めか)
    fn readdir_cookies(&self) -> Option<Result<Vec<(u64, DirEntry)>, i64>> {
        None
    }
    /// lseek の SEEK_DATA (hole が false) と SEEK_HOLE: off (ファイルの中) から先の、データか穴の始まり。
    /// None なら穴を知らないもの (全部がデータで、終わりにだけ穴)
    fn seek_data(&self, _off: u64, _hole: bool) -> Option<Result<u64, i64>> {
        None
    }
    /// Linux 6.18 の tmpfs と同じ位置で読めるもの (tmpfs) だけ: 位置 pos (. と .. のあと、2 から) から読む項目を、
    /// 読む順に f へ (その位置 (offset), 名前, ino, mode)。f が false を返したら止める。ぜんぶをコピーしないので、
    /// 大きなディレクトリでもヒープを使わない。読みながら消したり足したりしても、名前の位置は変わらない。
    /// 終わりの位置は tmpfs::DIR_OFFSET_EOD。None なら使えない (readdir を使う)
    fn readdir_offsets(&self, _pos: u64, _f: &mut dyn FnMut(u64, &str, u64, u32) -> bool) -> Option<Result<(), i64>> {
        None
    }
    fn create(&self, name: &str, mode: u32, node: NewNode) -> Result<InodeRef, i64>;
    fn link(&self, name: &str, target: &InodeRef) -> Result<(), i64>;
    fn unlink(&self, name: &str, rmdir: bool) -> Result<(), i64>;
    /// newdir は同じファイルシステムのディレクトリ
    fn rename(&self, old: &str, newdir: &InodeRef, new: &str) -> Result<(), i64>;
    /// RENAME_EXCHANGE: old (このディレクトリの名前) と new (newdir の名前) が指すものを入れかえる (どちらもあること)。
    /// できないファイルシステムは EINVAL (Linux と同じ)
    fn exchange(&self, _old: &str, _newdir: &InodeRef, _new: &str) -> Result<(), i64> {
        Err(-EINVAL)
    }
    /// O_TMPFILE: このディレクトリのファイルシステムに、名前のない (リンクが 0 の) ふつうのファイルを作る。
    /// 閉じたら消える。linkable なら一度だけ link で名前をつけられる (Linux の I_LINKABLE。ほかのリンクが 0 の
    /// ファイルは link できない: ENOENT)。できないファイルシステムは EOPNOTSUPP
    fn tmpfile(&self, _mode: u32, _linkable: bool) -> Result<InodeRef, i64> {
        Err(-EOPNOTSUPP)
    }

    /// fallocate (Linux の f_op->fallocate)。mode は FALLOC_FL_*。fd、範囲、mode の組、書ける口か、種類 (ふつうの
    /// ファイル) は sysfile が見てある。modified は file_modified の前半 (setuid と setgid を落とす): Linux のその
    /// ファイルシステムと同じところで呼び、時刻はファイルシステムが変える。できないものは EOPNOTSUPP
    fn fallocate(&self, _mode: u32, _off: u64, _len: u64, _modified: &dyn Fn() -> Result<(), i64>) -> Result<(), i64> {
        Err(-EOPNOTSUPP)
    }

    fn set_mode(&self, mode: u32) -> Result<(), i64>;
    fn set_owner(&self, uid: Option<u32>, gid: Option<u32>) -> Result<(), i64>;
    fn set_mtime(&self, ns: u64) -> Result<(), i64>;
    /// struct statfs
    fn statfs(&self) -> [u8; 120];
    /// /proc/ai/fs に出す、このファイルシステムの中の様子 (JSON のオブジェクト 1 つ)。ないものは None
    fn fs_stats(&self) -> Option<String> {
        None
    }
    /// ファイルの大きさの上限 (Linux の s_maxbytes。fallocate の EFBIG、sendfile の EOVERFLOW)。既定は
    /// MAX_LFS_FILESIZE (tmpfs、パイプなど)
    fn max_bytes(&self) -> u64 {
        i64::MAX as u64
    }
    /// まだディスクに書いていないものを書く (sync、fsync)
    fn sync(&self) -> Result<(), i64> {
        Ok(())
    }
    /// /proc/PID/fd/N のような「魔法のリンク」: たどると、名前ではなくその fd のファイルそのもの
    /// (消えた memfd も開ける)。(パス, inode)
    fn magic_link(&self) -> Option<(String, InodeRef)> {
        None
    }
    /// memfd の封 (F_GET_SEALS の F_SEAL_*)。封のないファイルは None
    fn seals(&self) -> Option<u32> {
        None
    }
    /// 封を足す (F_ADD_SEALS)
    fn add_seals(&self, _seals: u32) -> Result<(), i64> {
        Err(-EINVAL)
    }
}

/// カーネルの中だけの戻り値: ディスクを眠って待つあいだにファイルシステムが書きかえられたので、読みを
/// 頭からやりなおす (extfs の with_block → file::read_sleepable がやりなおす。ユーザーには返さない)
pub const ERETRY: i64 = 512;

/// write / pwrite / sendfile で off から len 書いてよいか (memfd の封)。
/// 共有の写像の書き戻しはここを通らない (Linux でも F_SEAL_FUTURE_WRITE の前の写像は書ける)
pub fn write_sealed(ino: &InodeRef, off: usize, len: usize) -> Result<(), i64> {
    const F_SEAL_GROW: u32 = 4;
    const F_SEAL_WRITE: u32 = 8;
    const F_SEAL_FUTURE_WRITE: u32 = 0x10;
    let Some(s) = ino.seals() else { return Ok(()) };
    if len > 0 && s & (F_SEAL_WRITE | F_SEAL_FUTURE_WRITE) != 0 {
        return Err(-EPERM);
    }
    if s & F_SEAL_GROW != 0 && off + len > ino.meta().size as usize {
        return Err(-EPERM);
    }
    Ok(())
}

/// ふつうのファイルを書く・切りつめるときに setuid と setgid を落とす (Linux の file_remove_privs と do_truncate)。
/// root (CAP_FSETID) はそのまま。setuid はいつも、setgid はグループが実行できるか、書く人がそのグループでないとき
/// (グループの x のない setgid は、昔の強制ロックのしるし)
pub fn drop_privs(ino: &InodeRef) -> Result<(), i64> {
    use crate::cred::{S_ISGID, S_ISUID};
    let m = ino.meta();
    if m.mode & S_IFMT != S_IFREG || m.mode & (S_ISUID | S_ISGID) == 0 {
        return Ok(());
    }
    let c = crate::cred::current();
    if c.euid == 0 {
        return Ok(());
    }
    let mut kill = m.mode & S_ISUID;
    if m.mode & S_ISGID != 0 && (m.mode & 0o010 != 0 || !c.in_group(m.gid)) {
        kill |= S_ISGID;
    }
    if kill != 0 {
        ino.set_mode(m.mode & 0o7777 & !kill)?;
    }
    Ok(())
}

static mut ROOT: Option<InodeRef> = None;
/// ルートの (もと, 種類) (/proc/mounts の 1 行目)
static mut ROOT_INFO: (String, String) = (String::new(), String::new());
static mut NEXT_FS: usize = 0;

pub fn new_fs_id() -> usize {
    unsafe {
        NEXT_FS += 1;
        NEXT_FS
    }
}

pub fn set_root(r: InodeRef, source: &str, fstype: &str) {
    names_changed();
    unsafe {
        *(&raw mut ROOT) = Some(r);
        *(&raw mut ROOT_INFO) = (source.into(), fstype.into());
    }
}

pub fn root() -> InodeRef {
    unsafe { (*(&raw const ROOT)).clone().expect("no root filesystem") }
}

// ---- マウント表と、マウントの namespace ----
// マウントは「ディレクトリ (id) の上に、ほかの根をかぶせる」こと。表は namespace ごと (CLONE_NEWNS で写す)。
// パスをたどるとき (cross)、いまのプロセスの表を見る。プロセスがいない (起動中) ならはじめの表

#[derive(Clone)]
pub struct Mount {
    /// マウント先のディレクトリの id
    at: (usize, u64),
    root: InodeRef,
    source: String,
    /// マウント先のパス (先頭 / なし。/proc/mounts 用)
    target: String,
    fstype: String,
    /// 読むだけ (MS_RDONLY): この下では作る・消す・書くは EROFS
    ro: bool,
}

pub struct MountNs {
    pub id: u64,
    /// 作ったときのユーザーの namespace (その中の root がマウントしてよい)
    pub owner: u64,
    list: core::cell::RefCell<Vec<Mount>>,
}

static mut INIT_MNT: Option<Rc<MountNs>> = None;
/// 生きている表 (sync で全部を書き出すため)
static mut ALL_MNT: Vec<alloc::rc::Weak<MountNs>> = Vec::new();

fn init_mnt() -> Rc<MountNs> {
    unsafe {
        (*(&raw mut INIT_MNT))
            .get_or_insert_with(|| Rc::new(MountNs { id: crate::ns::INIT_MNT, owner: crate::ns::INIT_USER, list: core::cell::RefCell::new(Vec::new()) }))
            .clone()
    }
}

/// いまのプロセスのマウント表
fn current_mnt() -> Rc<MountNs> {
    crate::proc::current_mnt().unwrap_or_else(init_mnt)
}

/// いまの表を写した新しい namespace (unshare / clone の CLONE_NEWNS)
pub fn new_mnt_ns(id: u64, owner: u64) -> Rc<MountNs> {
    let n = Rc::new(MountNs { id, owner, list: core::cell::RefCell::new(current_mnt().list.borrow().clone()) });
    unsafe {
        let all = &mut *(&raw mut ALL_MNT);
        all.retain(|w| w.strong_count() > 0);
        all.push(Rc::downgrade(&n));
    }
    n
}

/// dir (すでにあるディレクトリ) の上に fs の根をかぶせる (いまの表に)
pub fn mount(path: &str, fsroot: InodeRef, source: &str, fstype: &str) -> Result<(), i64> {
    mount_ro(path, fsroot, source, fstype, false)
}

pub fn mount_ro(path: &str, fsroot: InodeRef, source: &str, fstype: &str, ro: bool) -> Result<(), i64> {
    let (target, dir) = lookup("", path, true)?;
    if !dir.meta().is_dir() {
        return Err(-ENOTDIR);
    }
    current_mnt().list.borrow_mut().push(Mount { at: dir.id(), root: fsroot, source: source.into(), target, fstype: fstype.into(), ro });
    names_changed();
    Ok(())
}

/// path にかぶせてあるもの (いちばん上) を、読むだけに / 書けるように (mount -o remount,ro)
pub fn remount(path: &str, ro: bool) -> Result<(), i64> {
    let top = resolve("", path, true)?;
    let ns = current_mnt();
    let mut l = ns.list.borrow_mut();
    let m = l.iter_mut().rev().find(|m| m.root.id() == top.id()).ok_or(-EINVAL)?;
    m.ro = ro;
    Ok(())
}

/// path (先頭 / なし、たどったあとのもの) に書いてよいか: いちばん深いマウント (path がその下にあるもの) が
/// 読むだけなら EROFS
pub fn check_writable(path: &str) -> Result<(), i64> {
    const EROFS: i64 = 30;
    let ns = current_mnt();
    let l = ns.list.borrow();
    let under = |t: &str| path == t || (path.len() > t.len() && path.starts_with(t) && path.as_bytes()[t.len()] == b'/');
    let deepest = l.iter().filter(|m| under(&m.target)).max_by_key(|m| m.target.len());
    if deepest.is_some_and(|m| m.ro) { Err(-EROFS) } else { Ok(()) }
}

/// path にかぶせてあるもの (いちばん上) を外す
pub fn umount(path: &str) -> Result<(), i64> {
    let top = resolve("", path, true)?;
    let ns = current_mnt();
    let mut l = ns.list.borrow_mut();
    let i = l.iter().rposition(|m| m.root.id() == top.id()).ok_or(-EINVAL)?;
    // その上にまだかぶせてあるものがあれば外せない
    if l[i + 1..].iter().any(|m| m.at == top.id()) {
        return Err(-16); // EBUSY
    }
    l.remove(i);
    names_changed();
    Ok(())
}

/// /proc/mounts (いまの表)
pub fn mounts_text() -> String {
    mounts_text_of(crate::proc::current_mnt())
}

/// /proc/PID/mounts (ns の表。None ははじめのもの)
pub fn mounts_text_of(ns: Option<Rc<MountNs>>) -> String {
    let (src, ty) = unsafe { (*(&raw const ROOT_INFO)).clone() };
    let mut s = alloc::format!("{} / {} rw 0 0\n", src, ty);
    for m in ns.unwrap_or_else(init_mnt).list.borrow().iter() {
        s.push_str(&alloc::format!("{} /{} {} {} 0 0\n", m.source, m.target, m.fstype, if m.ro { "ro" } else { "rw" }));
    }
    s
}

/// マウントしているすべてのファイルシステムを書き出す (sync)
pub fn sync_all() {
    // MAP_SHARED で書いたページを先にファイルへ
    crate::vm::sync_shared();
    let mut roots: Vec<InodeRef> = unsafe { (*(&raw const ROOT)).iter().cloned().collect() };
    let mut tables = alloc::vec![init_mnt()];
    tables.extend(unsafe { (*(&raw const ALL_MNT)).iter().filter_map(|w| w.upgrade()) });
    for t in tables {
        roots.extend(t.list.borrow().iter().map(|m| m.root.clone()));
    }
    for r in roots {
        if let Err(e) = r.sync() {
            println!("sync: error {}", e);
        }
    }
}

/// 次に暇なときに書き出す時刻 (ticks)
static mut NEXT_IDLE_SYNC: u64 = 0;

/// することがないとき (スケジューラから): 1 秒ごとに書き出す
pub fn idle_sync() {
    let now = crate::timer::ticks();
    unsafe {
        if now >= NEXT_IDLE_SYNC {
            NEXT_IDLE_SYNC = now + crate::timer::HZ;
            sync_all();
        }
    }
}

/// マウント先ならかぶせたものの根に置きかえる
fn cross(mut i: InodeRef) -> InodeRef {
    let ns = current_mnt();
    let l = ns.list.borrow();
    if l.is_empty() {
        return i;
    }
    // かぶせたものの根がまたマウント先なら、その上へ (重ねたマウント)。同じディレクトリに bind したときなどに
    // 回りつづけないよう、1 つのマウントは 1 回だけ
    // (表は小さいので、使った印は 64 個までビットで。それより多ければ、新しい 64 個だけを見る)
    let base = l.len().saturating_sub(64);
    let mut used = 0u64;
    while let Some(k) = (base..l.len()).rev().find(|&k| used & (1 << (k - base)) == 0 && l[k].at == i.id()) {
        used |= 1 << (k - base);
        i = l[k].root.clone();
    }
    i
}

/// cwd (先頭 / なし) を基準に path を絶対化し、. と .. を畳む
pub fn normalize(cwd: &str, path: &str) -> String {
    // chroot: / はプロセスのルートから始まり、.. はルートより上に出ない
    let root = crate::proc::current_root();
    let mut parts: Vec<&str> = Vec::new();
    let base = if path.starts_with('/') { root.as_str() } else { cwd };
    let inside = !root.is_empty() && (base == root || base.starts_with(&(root.clone() + "/")));
    let floor = if inside { root.split('/').filter(|c| !c.is_empty()).count() } else { 0 };
    for c in base.split('/').chain(path.split('/')) {
        match c {
            "" | "." => {}
            ".." => {
                if parts.len() > floor {
                    parts.pop();
                }
            }
            c => parts.push(c),
        }
    }
    parts.join("/")
}

/// path をたどって (正規化済みパス, inode) を返す。
/// 途中のリンクは常に、最後のリンクは follow のときだけたどる
pub fn lookup(cwd: &str, path: &str, follow: bool) -> Result<(String, InodeRef), i64> {
    let _p = crate::prof::span("vfs:lookup");
    if path.is_empty() {
        return Err(-ENOENT);
    }
    // 名前のあとの .. や、最後の / と /. のあるパスは、文字の上で畳むと Linux とちがう (c/../d は c がなくても
    // d になり、a/ は a がファイルでも通る) ので、1 つずつたどる (覚えない)
    if !simple(path) {
        let start = (resolve("", cwd, true)?, cwd.to_string());
        return found(lookup_resolve(start, path, follow, 0, false)?);
    }
    let key = (current_mnt().id, normalize(cwd, path), follow);
    let cache = unsafe { &mut *(&raw mut PATHS) };
    // 変わったものを捨てる (ここで: inode を捨てると ext4 の後片づけが動くので、ファイルシステムの中ではなく)
    let pending = unsafe { &mut *(&raw mut PATHS_PENDING) };
    if PATHS_ALL.swap(false, core::sync::atomic::Ordering::Relaxed) {
        pending.clear();
        let old = core::mem::take(cache);
        drop(old);
    } else if !pending.is_empty() {
        let dirs = core::mem::take(pending);
        cache.retain(|_, e| !e.dirs.iter().any(|d| dirs.contains(d)));
    }
    if let Some(e) = cache.get_mut(&key) {
        PATHS_HIT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        e.used = true;
        // 覚えたのはほかの人かもしれない: だれでもは通れないディレクトリを、引く人が通れるか (たどるときと同じ順に)
        if !e.gates.is_empty() {
            let cred = crate::proc::current_cred_ref();
            for g in &e.gates {
                cred.check(&Meta { mode: g.mode, uid: g.uid, gid: g.gid, ..Meta::default() }, crate::cred::X)?;
            }
        }
        return match &e.found {
            Some((walked, ino)) => Ok((walked.clone(), ino.clone())),
            None => Err(-ENOENT),
        };
    }
    PATHS_MISS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let mut t = Trace { ok: true, dirs: Vec::new(), gates: Vec::new(), prefixes: Vec::new() };
    // 親のディレクトリを覚えていれば、そこから最後の名前だけを引く (Linux の dcache も名前ごと: 作ったばかりの
    // ファイルの stat で、ディレクトリまでを引きなおさない)。通ったディレクトリと通れるかは親の覚えから
    let mut start = None;
    let mut rest = key.1.clone();
    if let Some(cut) = key.1.rfind('/') {
        if let Some(e) = cache.get_mut(&(key.0, key.1[..cut].to_string(), true)) {
            let cred = crate::proc::current_cred_ref();
            if let Some((walked, ino)) = &e.found {
                if e.gates.iter().all(|g| cred.check(&Meta { mode: g.mode, uid: g.uid, gid: g.gid, ..Meta::default() }, crate::cred::X).is_ok()) {
                    e.used = true;
                    start = Some((ino.clone(), walked.clone()));
                    rest = key.1[cut + 1..].to_string();
                    t.dirs = e.dirs.clone();
                    t.gates = e.gates.clone();
                }
            }
        }
    }
    let r = walk(start, rest, follow, &mut t);
    // あったものと、ない (ENOENT) ものを覚える。たどっているあいだに変わったものがあれば覚えない (念のため)
    let keep = match &r {
        Ok(_) => true,
        Err(e) => *e == -ENOENT,
    };
    if keep && t.ok && pending.is_empty() && !PATHS_ALL.load(core::sync::atomic::Ordering::Relaxed) && !heap_crowded() {
        let max = paths_max();
        if cache.len() >= max {
            // 前にいっぱいになったときから引かれなかったものを捨てる (引かれたものは印を消して残す)。
            // あまり減らなければ全部
            PATHS_SWEEPS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            cache.retain(|_, e| core::mem::take(&mut e.used));
            if cache.len() >= max / 4 * 3 {
                cache.clear();
            }
        }
        // 途中のディレクトリも (次にその中の名前を引くときは、そこから)
        for (walked, ino, d, g) in core::mem::take(&mut t.prefixes) {
            let (dirs, gates) = (t.dirs[..d].to_vec(), t.gates[..g].to_vec());
            cache.entry((key.0, walked.clone(), true)).or_insert_with(|| PathEntry { found: Some((walked, ino)), used: false, dirs, gates });
        }
        cache.insert(key, PathEntry { found: r.as_ref().ok().cloned(), used: false, dirs: t.dirs, gates: t.gates });
    }
    r
}

/// パスの答えの覚え (dentry のキャッシュ): (マウントの namespace, 正規化したパス, 最後のリンクをたどるか) →
/// (たどったパスと inode か、ない (ENOENT)、通ったディレクトリ)。stat や open のたびに 1 つずつ名前を引いて
/// inode を作るのは高い (QEMU の TCG で、要素ごとに 20 us)。ないものも覚える (PATH やライブラリ、#include を
/// 探すときは、同じないものを何度も引く。ext4 は、ないものを引くたびにディレクトリのブロックをみな読む)。
/// 覚えるのは、たどったものがみな path_cacheable で、魔法のリンクがないときだけ。
/// 覚えはだれが引いても同じものを返すので、通ったディレクトリのうち u・g・o のどれかに x がないもの (/home/ai の
/// 0700 など) は属性も覚えておき、覚えから答えるたびに、引く人がそこを通れるかを確かめなおす (通れなければ、
/// たどったときと同じ EACCES)。ディレクトリの中の名前や、ディレクトリの属性 (chmod、chown) が変わったら
/// dir_changed で、そこを通ったものだけを捨てる (ないものも、その名前を探したディレクトリを通ったものに
/// 入れてあるので、作れば捨てられる。/run の rename が 5 秒ごとにあっても、ほかのものは残る)。
/// マウントが変わったら names_changed で全部。いっぱいになったら (paths_max)、前にいっぱいになったときから
/// 引かれなかったものを捨てる。たどった途中のディレクトリも (ルートからリンクをたどらずに来たもの) 覚え、覚えに
/// ないパスは、親のディレクトリを覚えていればそこから最後の名前だけを引く (Linux の dcache が名前ごとに覚えるのと
/// 同じ: 作ったばかりのファイルの stat も、ディレクトリまでは引きなおさない)
struct PathEntry {
    /// たどったパスと inode。None はない (ENOENT)
    found: Option<(String, InodeRef)>,
    /// 前にいっぱいになったときから引かれたか
    used: bool,
    dirs: Vec<(usize, u64)>,
    /// 通ったディレクトリのうち、だれでもは通れないもの (通った順)
    gates: Vec<Gate>,
}

/// だれでもは通れないディレクトリの、通れるかを決める属性 (Meta まるごとでは大きいので)
#[derive(Clone)]
struct Gate {
    mode: u32,
    uid: u32,
    gid: u32,
}

/// walk が集めるもの: 覚えてよいか、通ったディレクトリ、そのうちだれでもは通れないもの、それと途中のディレクトリ
/// (ルートからリンクをたどらずに来たもの: たどったパス、inode、そこまでの dirs と gates の数。lookup がそれも覚える)
struct Trace {
    ok: bool,
    dirs: Vec<(usize, u64)>,
    gates: Vec<Gate>,
    prefixes: Vec<(String, InodeRef, usize, usize)>,
}

static mut PATHS: alloc::collections::BTreeMap<(u64, String, bool), PathEntry> = alloc::collections::BTreeMap::new();
/// 変わったディレクトリ (次の lookup で、そこを通ったものを捨てる)
static mut PATHS_PENDING: Vec<(usize, u64)> = Vec::new();
/// 全部捨てる
static PATHS_ALL: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// 覚えから答えた数 (/proc/bkl)
pub static PATHS_HIT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// 覚えになくてたどった数、いっぱいになって捨てた回数、変わったディレクトリを知らせた数 (/proc/ai/fs)
pub static PATHS_MISS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static PATHS_SWEEPS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static PATHS_INVAL: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// 覚えるのは (ヒープの大きさ / 16 KiB) こまで: 1 つ 700 バイトほどなので、ヒープの 4% ほど
/// (32 MiB で 2048、64 MiB で 4096、16384 まで)
fn paths_max() -> usize {
    (crate::heap::size() / 16384).clamp(2048, 16384)
}

/// ヒープがこれより混んでいたら覚えない (vm.rs のページの表と同じ)
fn heap_crowded() -> bool {
    crate::heap::used() > crate::heap::size() / 4 * 3
}

/// マウントやルートが変わった: パスの覚えを全部捨てる (次に引くときに)
pub fn names_changed() {
    PATHS_ALL.store(true, core::sync::atomic::Ordering::Relaxed);
}

/// ディレクトリ dir の中の名前 (作る・消す・名前を変える) か、dir の属性 (chmod、chown) が変わった:
/// dir を通ったパスの覚えを捨てる (次に引くときに)。ふつうのファイルの属性が変わったときに呼んでもよい
pub fn dir_changed(dir: (usize, u64)) {
    PATHS_INVAL.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    unsafe { DIR_GENS[gen_slot(dir)] += 1 };
    let p = unsafe { &mut *(&raw mut PATHS_PENDING) };
    if p.len() >= 64 {
        names_changed();
    } else if !p.contains(&dir) {
        p.push(dir);
    }
}

/// ディレクトリの中が変わった数。id で 256 の箱に分けて数える (同じ箱のほかのディレクトリで増えることもある)。
/// 開いたディレクトリの一覧を読みなおすかどうか (file.rs の getdents)
static mut DIR_GENS: [u64; 256] = [0; 256];

fn gen_slot(dir: (usize, u64)) -> usize {
    ((dir.1 ^ (dir.0 as u64).rotate_left(32)).wrapping_mul(0x9e37_79b9_7f4a_7c15) >> 56) as usize
}

pub fn dir_gen(dir: (usize, u64)) -> u64 {
    unsafe { (*(&raw const DIR_GENS))[gen_slot(dir)] }
}

/// いま覚えている数と、そのうちないもの (/proc/bkl)
pub fn paths_len() -> (usize, usize) {
    let cache = unsafe { &*(&raw const PATHS) };
    (cache.len(), cache.values().filter(|e| e.found.is_none()).count())
}

/// /proc/ai/fs の paths: 数、ないもの、上限、当たり、外れ、捨てた回数、変わったディレクトリの数
pub fn paths_json() -> String {
    use core::sync::atomic::Ordering::Relaxed;
    let (n, neg) = paths_len();
    alloc::format!(
        "{{\"entries\":{},\"negative\":{},\"max\":{},\"hits\":{},\"misses\":{},\"sweeps\":{},\"invalidations\":{}}}",
        n,
        neg,
        paths_max(),
        PATHS_HIT.load(Relaxed),
        PATHS_MISS.load(Relaxed),
        PATHS_SWEEPS.load(Relaxed),
        PATHS_INVAL.load(Relaxed)
    )
}

/// id の inode の名前 (パスの覚えにあれば。/proc/ai/mem のページキャッシュのファイルの名前)
pub fn path_of_id(id: (usize, u64)) -> Option<String> {
    let cache = unsafe { &*(&raw const PATHS) };
    cache.values().filter_map(|e| e.found.as_ref()).find(|(_, i)| i.id() == id).map(|(w, _)| alloc::format!("/{}", w))
}

/// /proc/ai/fs の mounts: はじめの namespace のマウント (ルートも) と、それぞれのファイルシステムの中の様子 (fs_stats)
pub fn mounts_json() -> String {
    let esc = crate::aiproc::esc;
    let stats = |i: Option<&InodeRef>| i.and_then(|i| i.fs_stats()).unwrap_or_else(|| String::from("null"));
    let (src, ty) = unsafe { (*(&raw const ROOT_INFO)).clone() };
    let root = unsafe { (*(&raw const ROOT)).clone() };
    let mut v = alloc::vec![alloc::format!("{{\"target\":\"/\",\"source\":\"{}\",\"type\":\"{}\",\"ro\":false,\"stats\":{}}}", esc(&src), esc(&ty), stats(root.as_ref()))];
    for m in init_mnt().list.borrow().iter() {
        v.push(alloc::format!(
            "{{\"target\":\"/{}\",\"source\":\"{}\",\"type\":\"{}\",\"ro\":{},\"stats\":{}}}",
            esc(&m.target),
            esc(&m.source),
            esc(&m.fstype),
            m.ro,
            stats(Some(&m.root))
        ));
    }
    alloc::format!("[{}]", v.join(","))
}

/// 開いているディレクトリ dir (パスは dir_path) から、相対の path を (openat などの dirfd)。
/// .. があるときは、パスの文字の上で畳むので、ふつうの lookup で
pub fn lookup_at(dir: &InodeRef, dir_path: &str, path: &str, follow: bool) -> Result<(String, InodeRef), i64> {
    if !path.is_empty() && !simple(path) {
        return found(lookup_resolve((dir.clone(), dir_path.trim_matches('/').to_string()), path, follow, 0, false)?);
    }
    if path.is_empty() || path.starts_with('/') || path.split('/').any(|c| c == "..") {
        return lookup(dir_path, path, follow);
    }
    let rel = path.split('/').filter(|c| !c.is_empty() && *c != ".").collect::<Vec<_>>().join("/");
    walk(Some((dir.clone(), dir_path.trim_matches('/').to_string())), rel, follow, &mut Trace { ok: false, dirs: Vec::new(), gates: Vec::new(), prefixes: Vec::new() })
}

/// 文字の上で畳んでも Linux と同じ答えになるパス: 名前のあとの .. がなく (先頭の .. は基準のディレクトリを戻る
/// だけ: 基準はたどったあとのパスなので、文字の上で戻ってよい)、最後が / や /. でない (name/ と name/. は、
/// name がディレクトリでなければならず、リンクならたどる)。ほかは lookup_resolve で 1 つずつ
fn simple(path: &str) -> bool {
    let mut named = false;
    for c in path.split('/') {
        match c {
            "" | "." => {}
            ".." if named => return false,
            ".." => {}
            _ => named = true,
        }
    }
    // "." だけも: そのディレクトリを通れること (Linux はどの名前の前にも may_lookup をする。. でも)
    path != "." && !(path.len() > 1 && (path.ends_with('/') || path.ends_with("/.")))
}

/// lookup_resolve の答えを lookup の形に (最後の名前がなければ ENOENT)
fn found(r: Resolved) -> Result<(String, InodeRef), i64> {
    match r {
        Resolved::Found(p, i) => Ok((p, i)),
        Resolved::Missing(..) => Err(-ENOENT),
    }
}

/// path (start があればそこからの相対、なければルートから) をたどる
/// t: 答えを覚えてよいか (だめなら ok を false に) と、通ったディレクトリ (lookup の PATHS)、そのうちだれでもは通れないもの
fn walk(mut start: Option<(InodeRef, String)>, mut path: String, follow: bool, t: &mut Trace) -> Result<(String, InodeRef), i64> {
    // 覚え (PATHS) になかったもの。vfs:lookup との回数の差が、覚えから答えた数
    let _p = crate::prof::span("vfs:walk");
    let cred = crate::cred::current();
    // ルートからリンクをたどらずに来たあいだだけ、途中のディレクトリを t.prefixes に (覚えてよいもの)
    let mut direct = start.is_none();
    'restart: for _ in 0..16 {
        let comps: Vec<String> = path.split('/').filter(|c| !c.is_empty()).map(String::from).collect();
        let (mut cur, mut walked) = start.take().unwrap_or_else(|| (cross(root()), String::new()));
        // いまのディレクトリの属性 (次の要素の分は、たどったときに読んだものを使う)
        let mut m = cur.meta();
        t.ok &= cur.path_cacheable();
        for (i, c) in comps.iter().enumerate() {
            if !m.is_dir() {
                return Err(-ENOTDIR);
            }
            // ディレクトリを通るには x が要る
            cred.check(&m, crate::cred::X)?;
            if m.mode & 0o111 != 0o111 {
                t.gates.push(Gate { mode: m.mode, uid: m.uid, gid: m.gid });
            }
            t.dirs.push(cur.id());
            // 長すぎる名前は、引くところで (Linux のファイルシステムの lookup と同じ)
            if c.len() > 255 {
                return Err(-ENAMETOOLONG);
            }
            let next = cross(cur.lookup(c)?);
            t.ok &= next.path_cacheable();
            let last = i + 1 == comps.len();
            let nm = next.meta();
            if nm.mode & S_IFMT == S_IFLNK && (!last || follow) {
                // リンクの中身が変わるのは消して作りなおすとき (names_changed) なので、たどった答えも覚えてよい。
                // 魔法のリンク (/proc/self/fd/N) は覚えない
                if let Some((p, ino)) = next.magic_link() {
                    t.ok = false;
                    if last {
                        return Ok((p.trim_start_matches('/').to_string(), ino));
                    }
                    // /proc/self/fd/N/... : 開いているディレクトリから先をたどる
                    if !ino.meta().is_dir() {
                        return Err(-ENOTDIR);
                    }
                    walked = p.trim_start_matches('/').to_string();
                    m = ino.meta();
                    cur = ino;
                    continue;
                }
                let target = next.readlink()?;
                if target.is_empty() {
                    return Err(-ENOENT);
                }
                // .. のあるリンクや、/ で終わるリンク: 文字の上で畳まず、いまのディレクトリから 1 つずつ (覚えない)
                let mut rest = target.clone();
                for c in &comps[i + 1..] {
                    rest.push('/');
                    rest.push_str(c);
                }
                if !simple(&rest) {
                    t.ok = false;
                    return found(lookup_resolve((cur.clone(), walked.clone()), &rest, follow, 0, false)?);
                }
                // 相対で .. のないリンク (bin/true -> coreutils など) は、いまのディレクトリから続ける
                // (ルートからたどりなおさない)。絶対のリンクはルートから
                let mut np = if !target.starts_with('/') {
                    start = Some((cur.clone(), walked.clone()));
                    target.split('/').filter(|c| !c.is_empty() && *c != ".").collect::<Vec<_>>().join("/")
                } else {
                    normalize(&walked, &target)
                };
                for rest in &comps[i + 1..] {
                    np.push('/');
                    np.push_str(rest);
                }
                path = np;
                direct = false;
                continue 'restart;
            }
            if !walked.is_empty() {
                walked.push('/');
            }
            walked.push_str(c);
            if direct && t.ok && !last && nm.is_dir() {
                t.prefixes.push((walked.clone(), next.clone(), t.dirs.len(), t.gates.len()));
            }
            cur = next;
            m = nm;
        }
        return Ok((walked, cur));
    }
    Err(-ELOOP)
}

pub fn resolve(cwd: &str, path: &str, follow: bool) -> Result<InodeRef, i64> {
    if path.is_empty() {
        return Ok(cross(root()));
    }
    lookup(cwd, path, follow).map(|(_, i)| i)
}

/// 最後の要素の親ディレクトリと名前
pub fn parent_of(cwd: &str, path: &str) -> Result<(InodeRef, String), i64> {
    parent_path(cwd, path).map(|(parent, name, _)| (parent, name))
}

/// 最後の要素の親ディレクトリと名前、それに作るもののパス (リンクをたどったあとの、先頭 / なし)。
/// 最後が . や .. や / なら EEXIST (作るものの答え。消す・名前を変えるものは parent_last でそれぞれに)
pub fn parent_path(cwd: &str, path: &str) -> Result<(InodeRef, String, String), i64> {
    let p = parent_last(None, cwd, path)?;
    if p.last != Last::Norm {
        return Err(-EEXIST);
    }
    p.alive()?;
    p.name_ok()?;
    Ok((p.dir, p.name, p.full))
}

/// 最後の名前の種類 (Linux の LAST_NORM、LAST_DOT、LAST_DOTDOT、LAST_ROOT)
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Last {
    Norm,
    Dot,
    DotDot,
    Root,
}

/// 作る・消す・名前を変えるところ (Linux の filename_parentat): 最後の名前の親と、その名前と種類、名前のあとに /
/// があったか、作るもののパス (先頭 / なし)。最後の名前を引くのも、. と .. と / をどうするかもシステムコールごとに
pub struct Parent {
    pub dir: InodeRef,
    pub name: String,
    pub last: Last,
    pub slash: bool,
    pub full: String,
    /// 親が消したディレクトリ (開いたまま rmdir されたもの) で、名前を引けば ENOENT (Linux の IS_DEADDIR)。
    /// 名前を引くところで alive を見る (rename は . .. / の EBUSY と EXDEV をそれより先に見る)
    pub dead: bool,
}

impl Parent {
    /// 親が消したディレクトリなら ENOENT (Linux の __lookup_slow: ファイルシステムの lookup より先)
    pub fn alive(&self) -> Result<(), i64> {
        if self.dead { Err(-ENOENT) } else { Ok(()) }
    }

    /// 最後の名前を引けるか (Linux のファイルシステムの lookup と同じく、長すぎれば引くところで ENAMETOOLONG)
    pub fn name_ok(&self) -> Result<(), i64> {
        if self.name.len() > 255 { Err(-ENAMETOOLONG) } else { Ok(()) }
    }
}

/// path の Parent。start (dirfd のディレクトリ (inode, パス)) か、なければ cwd から。親までは lookup と同じく
/// (.. は 1 つずつ: その前の名前もあってディレクトリでなければならない)、親は通れる (x) こと (Linux の
/// link_path_walk は、最後の名前の前にも may_lookup をする)
pub fn parent_last(start: Option<(InodeRef, String)>, cwd: &str, path: &str) -> Result<Parent, i64> {
    if path.is_empty() {
        return Err(-ENOENT);
    }
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        // / そのもの
        let (w, r, _) = proc_root()?;
        return Ok(Parent { dir: r, name: String::new(), last: Last::Root, slash: false, full: w, dead: false });
    }
    let slash = trimmed.len() < path.len();
    let (dpart, name) = match trimmed.rsplit_once('/') {
        Some(("", n)) => ("/", n),
        Some((d, n)) => (d, n),
        None => ("", trimmed),
    };
    let last = match name {
        "." => Last::Dot,
        ".." => Last::DotDot,
        _ => Last::Norm,
    };
    let (dw, dir) = match (dpart.is_empty(), start) {
        (true, Some((i, p))) => (p.trim_matches('/').to_string(), i),
        (true, None) => (cwd.trim_matches('/').to_string(), resolve("", cwd, true)?),
        (false, Some((i, p))) => lookup_at(&i, &p, dpart, true)?,
        (false, None) => lookup(cwd, dpart, true)?,
    };
    let m = dir.meta();
    if !m.is_dir() {
        return Err(-ENOTDIR);
    }
    crate::cred::current().check(&m, crate::cred::X)?;
    // 消したディレクトリ (開いたまま rmdir されたもの) の中の名前は、引くところで ENOENT (Linux の IS_DEADDIR。alive)
    let dead = last == Last::Norm && m.nlink == 0;
    let full = if dw.is_empty() { name.to_string() } else { alloc::format!("{}/{}", dw, name) };
    Ok(Parent { dir, name: name.to_string(), last, slash, full, dead })
}

// ---- openat2 の resolve (RESOLVE_*) ----

pub const RESOLVE_NO_XDEV: u64 = 0x01;
pub const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
pub const RESOLVE_NO_SYMLINKS: u64 = 0x04;
pub const RESOLVE_BENEATH: u64 = 0x08;
pub const RESOLVE_IN_ROOT: u64 = 0x10;
pub const RESOLVE_CACHED: u64 = 0x20;
pub const RESOLVE_ALL: u64 = 0x3f;

/// lookup_resolve の答え: あったもの (パス, inode) か、最後の名前だけがないもの (作るところ: 親, そのパス, 名前)
pub enum Resolved {
    Found(String, InodeRef),
    Missing(InodeRef, String, String),
}

/// たどっているところ: (パス (先頭 / なし), inode, マウントをまたいで入ったか (マウントの根))
type Step = (String, InodeRef, bool);

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_string() } else { alloc::format!("{}/{}", dir, name) }
}

/// プロセスのルート (chroot。なければ名前空間のルート)
fn proc_root() -> Result<Step, i64> {
    let r = crate::proc::current_root();
    if r.is_empty() {
        return Ok((String::new(), cross(root()), false));
    }
    let (p, i) = lookup("", &r, true)?;
    Ok((p, i, false))
}

/// ディレクトリのパス (たどったあとのもの) と、その上のディレクトリをみな。いちばん上はプロセスのルート (パスが
/// ルートの外なら (chroot の前に開いたもの)、名前空間のルート。normalize と同じく、そこからは .. で上に出る)
fn chain(path: &str) -> Result<Vec<Step>, i64> {
    let r = proc_root()?;
    let inside = r.0.is_empty() || path == r.0 || path.starts_with(&alloc::format!("{}/", r.0));
    let (rest, mut out) = if inside { (&path[r.0.len()..], alloc::vec![r]) } else { (path, alloc::vec![(String::new(), cross(root()), false)]) };
    for c in rest.split('/').filter(|c| !c.is_empty()) {
        let (w, cur) = out.last().map(|s| (s.0.clone(), s.1.clone())).unwrap();
        let raw = cur.lookup(c)?;
        let next = cross(raw.clone());
        let crossed = next.id() != raw.id();
        out.push((join(&w, c), next, crossed));
    }
    Ok(out)
}

/// openat2 の resolve のきまりで、start (dirfd のディレクトリ (inode, パス)) から path をたどる。lookup とちがい、.. と
/// リンクは 1 つずつ (lookup のようにパスの文字の上で先に畳まない) なので、どこから外に出ようとしたかがわかる:
///   BENEATH: start の外に出るもの (start の .. 、絶対パス、絶対のリンク、魔法のリンク) は EXDEV
///   IN_ROOT: start をルートに (絶対パスと絶対のリンクは start から、start の .. は start)。魔法のリンクは EXDEV
///   NO_SYMLINKS: リンクは ELOOP (魔法のリンクも)。NO_MAGICLINKS: 魔法のリンク (/proc/self/fd/N) は ELOOP
///   NO_XDEV: マウントをまたぐもの (入るのも、.. で出るのも、ほかのマウントへのリンクも) は EXDEV
/// 最後の名前だけがなければ Missing (O_CREAT が作るところ)。答えは覚えない (lookup の PATHS は使わない)
pub fn lookup_resolve(start: (InodeRef, String), path: &str, follow: bool, resolve: u64, creat: bool) -> Result<Resolved, i64> {
    let _p = crate::prof::span("vfs:resolve");
    let beneath = resolve & RESOLVE_BENEATH != 0;
    let in_root = resolve & RESOLVE_IN_ROOT != 0;
    let no_xdev = resolve & RESOLVE_NO_XDEV != 0;
    let no_symlinks = resolve & RESOLVE_NO_SYMLINKS != 0;
    let no_magic = no_symlinks || resolve & RESOLVE_NO_MAGICLINKS != 0;
    let cred = crate::cred::current();
    let start: Step = (start.1.trim_matches('/').to_string(), start.0, false);
    // name/ はディレクトリでなければならない (リンクならたどる)。最後のリンクの中身が / で終わるときも
    let mut must_dir = path.ends_with('/');
    let mut follow = follow || must_dir;
    // stack[0] がいちばん下: BENEATH と IN_ROOT なら start (その下には出ない)。ほかは、はじめは start で、.. で上に
    // 行くときにルートからの並び (chain) にかえる (at_root: いちばん下がもうルート)
    let mut at_root = false;
    let mut stack: Vec<Step> = if path.starts_with('/') {
        if beneath {
            return Err(-EXDEV);
        }
        if in_root {
            alloc::vec![start]
        } else {
            at_root = true;
            alloc::vec![proc_root()?]
        }
    } else {
        alloc::vec![start]
    };
    let mut todo: alloc::collections::VecDeque<String> = path.split('/').filter(|c| !c.is_empty()).map(String::from).collect();
    let mut links = 0;
    while let Some(c) = todo.pop_front() {
        let (w, cur) = stack.last().map(|s| (s.0.clone(), s.1.clone())).unwrap();
        let m = cur.meta();
        if !m.is_dir() {
            return Err(-ENOTDIR);
        }
        // ディレクトリを通るには x が要る (. と .. も。Linux と同じ)
        cred.check(&m, crate::cred::X)?;
        if c == "." {
            continue;
        }
        if c == ".." {
            if stack.len() == 1 {
                if beneath {
                    return Err(-EXDEV);
                }
                if in_root || at_root {
                    continue;
                }
                stack = chain(&stack[0].0)?;
                at_root = true;
                if stack.len() == 1 {
                    continue;
                }
            }
            let (_, _, crossed) = stack.pop().unwrap();
            if crossed && no_xdev {
                return Err(-EXDEV);
            }
            continue;
        }
        let last = todo.is_empty();
        // O_CREAT で、たどったリンクの中身の終わりが / なら、最後の名前を引く前に EISDIR (あってもなくても。Linux の
        // open_last_lookups)
        if last && must_dir && creat {
            return Err(-EISDIR);
        }
        if c.len() > 255 {
            return Err(-ENAMETOOLONG);
        }
        let raw = match cur.lookup(&c) {
            Ok(r) => r,
            Err(e) if e == -ENOENT && last => return Ok(Resolved::Missing(cur, w, c)),
            Err(e) => return Err(e),
        };
        let next = cross(raw.clone());
        let crossed = next.id() != raw.id();
        if crossed && no_xdev {
            return Err(-EXDEV);
        }
        if next.meta().mode & S_IFMT == S_IFLNK && (!last || follow) {
            if no_symlinks {
                return Err(-ELOOP);
            }
            links += 1;
            if links > 40 {
                return Err(-ELOOP);
            }
            if let Some((p, ino)) = next.magic_link() {
                if no_magic {
                    return Err(-ELOOP);
                }
                if beneath || in_root || (no_xdev && ino.id().0 != cur.id().0) {
                    return Err(-EXDEV);
                }
                let p = p.trim_start_matches('/').to_string();
                if todo.is_empty() {
                    if must_dir && !ino.meta().is_dir() {
                        return Err(-ENOTDIR);
                    }
                    return Ok(Resolved::Found(p, ino));
                }
                // /proc/self/fd/N/... : 開いているディレクトリから先を (その .. は、そのディレクトリの上)
                stack = alloc::vec![(p, ino, false)];
                at_root = false;
                continue;
            }
            let t = next.readlink()?;
            if t.is_empty() {
                return Err(-ENOENT);
            }
            if t.starts_with('/') {
                if beneath {
                    return Err(-EXDEV);
                }
                let r = if in_root { stack[0].clone() } else { proc_root()? };
                if no_xdev && r.1.id().0 != cur.id().0 {
                    return Err(-EXDEV);
                }
                if in_root {
                    stack.truncate(1);
                } else {
                    stack = alloc::vec![r];
                    at_root = true;
                }
            }
            // 最後の名前のリンクの中身が / で終わるなら、たどった先はディレクトリでなければならない (Linux の
            // lookup_last の LOOKUP_DIRECTORY。その中の最後のリンクもたどる)
            if todo.is_empty() && t.ends_with('/') {
                must_dir = true;
                follow = true;
            }
            // リンクの中身を、残りの前に
            for x in t.split('/').filter(|x| !x.is_empty()).rev() {
                todo.push_front(String::from(x));
            }
            continue;
        }
        stack.push((join(&w, &c), next, crossed));
    }
    let (w, i, _) = stack.pop().unwrap();
    if must_dir && !i.meta().is_dir() {
        return Err(-ENOTDIR);
    }
    Ok(Resolved::Found(w, i))
}

/// なければディレクトリを作りながら path をたどる (起動時用)
pub fn mkdir_p(path: &str, mode: u32) -> Result<InodeRef, i64> {
    let mut cur = cross(root());
    for c in path.split('/').filter(|c| !c.is_empty()) {
        cur = match cur.lookup(c) {
            Ok(n) => cross(n),
            Err(_) => cur.create(c, S_IFDIR | mode, NewNode::Dir)?,
        };
    }
    Ok(cur)
}

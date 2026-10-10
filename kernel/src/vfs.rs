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
pub const ENAMETOOLONG: i64 = 36;
pub const ENOTEMPTY: i64 = 39;
pub const ELOOP: i64 = 40;
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
    fn create(&self, name: &str, mode: u32, node: NewNode) -> Result<InodeRef, i64>;
    fn link(&self, name: &str, target: &InodeRef) -> Result<(), i64>;
    fn unlink(&self, name: &str, rmdir: bool) -> Result<(), i64>;
    /// newdir は同じファイルシステムのディレクトリ
    fn rename(&self, old: &str, newdir: &InodeRef, new: &str) -> Result<(), i64>;

    fn set_mode(&self, mode: u32) -> Result<(), i64>;
    fn set_owner(&self, uid: Option<u32>, gid: Option<u32>) -> Result<(), i64>;
    fn set_mtime(&self, ns: u64) -> Result<(), i64>;
    /// struct statfs
    fn statfs(&self) -> [u8; 120];
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
    if path.is_empty() {
        return Err(-ENOENT);
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
    if let Some(e) = cache.get(&key) {
        PATHS_HIT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        return Ok((e.walked.clone(), e.ino.clone()));
    }
    let mut t = Trace { ok: true, dirs: Vec::new() };
    let r = walk(None, key.1.clone(), follow, &mut t)?;
    // たどっているあいだに変わったものがあれば覚えない (念のため)
    if t.ok && pending.is_empty() && !PATHS_ALL.load(core::sync::atomic::Ordering::Relaxed) {
        if cache.len() >= PATHS_MAX {
            cache.clear();
        }
        cache.insert(key, PathEntry { walked: r.0.clone(), ino: r.1.clone(), dirs: t.dirs });
    }
    Ok(r)
}

/// パスの答えの覚え (dentry のキャッシュ): (マウントの namespace, 正規化したパス, 最後のリンクをたどるか) →
/// (たどったパス, inode, 通ったディレクトリ)。stat や open のたびに 1 つずつ名前を引いて inode を作るのは高い
/// (QEMU の TCG で、要素ごとに 20 us)。覚えるのは、たどったものがみな path_cacheable で、
/// 魔法のリンクがなく、通ったディレクトリがみな u・g・o とも x のとき (だれが引いても、権限の確かめが同じに通る) だけ。
/// ディレクトリの中の名前や、ディレクトリの属性が変わったら dir_changed で、そこを通ったものだけを捨てる
/// (/run の rename が 5 秒ごとにあっても、ほかのものは残る)。マウントが変わったら names_changed で全部
struct PathEntry {
    walked: String,
    ino: InodeRef,
    dirs: Vec<(usize, u64)>,
}

/// walk が集めるもの: 覚えてよいか、通ったディレクトリ
struct Trace {
    ok: bool,
    dirs: Vec<(usize, u64)>,
}

static mut PATHS: alloc::collections::BTreeMap<(u64, String, bool), PathEntry> = alloc::collections::BTreeMap::new();
/// 変わったディレクトリ (次の lookup で、そこを通ったものを捨てる)
static mut PATHS_PENDING: Vec<(usize, u64)> = Vec::new();
/// 全部捨てる
static PATHS_ALL: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// 覚えから答えた数 (/proc/bkl)
pub static PATHS_HIT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
const PATHS_MAX: usize = 2048;

/// マウントやルートが変わった: パスの覚えを全部捨てる (次に引くときに)
pub fn names_changed() {
    PATHS_ALL.store(true, core::sync::atomic::Ordering::Relaxed);
}

/// ディレクトリ dir の中の名前 (作る・消す・名前を変える) か、dir の属性 (chmod、chown) が変わった:
/// dir を通ったパスの覚えを捨てる (次に引くときに)。ふつうのファイルの属性が変わったときに呼んでもよい
pub fn dir_changed(dir: (usize, u64)) {
    let p = unsafe { &mut *(&raw mut PATHS_PENDING) };
    if p.len() >= 64 {
        names_changed();
    } else if !p.contains(&dir) {
        p.push(dir);
    }
}

/// いま覚えている数 (/proc/bkl)
pub fn paths_len() -> usize {
    unsafe { (*(&raw const PATHS)).len() }
}

/// 開いているディレクトリ dir (パスは dir_path) から、相対の path を (openat などの dirfd)。
/// .. があるときは、パスの文字の上で畳むので、ふつうの lookup で
pub fn lookup_at(dir: &InodeRef, dir_path: &str, path: &str, follow: bool) -> Result<(String, InodeRef), i64> {
    if path.is_empty() || path.starts_with('/') || path.split('/').any(|c| c == "..") {
        return lookup(dir_path, path, follow);
    }
    let rel = path.split('/').filter(|c| !c.is_empty() && *c != ".").collect::<Vec<_>>().join("/");
    walk(Some((dir.clone(), dir_path.trim_matches('/').to_string())), rel, follow, &mut Trace { ok: false, dirs: Vec::new() })
}

/// path (start があればそこからの相対、なければルートから) をたどる
/// t: 答えを覚えてよいか (だめなら ok を false に) と、通ったディレクトリ (lookup の PATHS)
fn walk(mut start: Option<(InodeRef, String)>, mut path: String, follow: bool, t: &mut Trace) -> Result<(String, InodeRef), i64> {
    let cred = crate::cred::current();
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
            t.ok &= m.mode & 0o111 == 0o111;
            t.dirs.push(cur.id());
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
                let t = next.readlink()?;
                // 相対で .. のないリンク (bin/true -> coreutils など) は、いまのディレクトリから続ける
                // (ルートからたどりなおさない)。ほかはパスの文字の上で畳んでルートから
                let mut np = if !t.starts_with('/') && !t.split('/').any(|c| c == "..") {
                    start = Some((cur.clone(), walked.clone()));
                    t.split('/').filter(|c| !c.is_empty() && *c != ".").collect::<Vec<_>>().join("/")
                } else {
                    normalize(&walked, &t)
                };
                for rest in &comps[i + 1..] {
                    np.push('/');
                    np.push_str(rest);
                }
                path = np;
                continue 'restart;
            }
            if !walked.is_empty() {
                walked.push('/');
            }
            walked.push_str(c);
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

/// 最後の要素の親ディレクトリと名前、それに作るもののパス (リンクをたどったあとの、先頭 / なし)
pub fn parent_path(cwd: &str, path: &str) -> Result<(InodeRef, String, String), i64> {
    let full = normalize(cwd, path);
    let (dir, name) = match full.rsplit_once('/') {
        Some((d, n)) => (d.to_string(), n.to_string()),
        None => (String::new(), full.clone()),
    };
    if name.is_empty() {
        return Err(-EEXIST); // ルートそのもの
    }
    if name.len() > 255 {
        return Err(-ENAMETOOLONG);
    }
    let (dir, parent) = if dir.is_empty() { (String::new(), cross(root())) } else { lookup("", &dir, true)? };
    if !parent.meta().is_dir() {
        return Err(-ENOTDIR);
    }
    let full = if dir.is_empty() { name.clone() } else { alloc::format!("{}/{}", dir, name) };
    Ok((parent, name, full))
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

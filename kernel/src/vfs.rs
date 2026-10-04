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
/// (マウント先の id, マウントしたものの根)
static mut MOUNTS: Vec<((usize, u64), InodeRef)> = Vec::new();
static mut NEXT_FS: usize = 0;

pub fn new_fs_id() -> usize {
    unsafe {
        NEXT_FS += 1;
        NEXT_FS
    }
}

pub fn set_root(r: InodeRef) {
    unsafe { *(&raw mut ROOT) = Some(r) };
}

pub fn root() -> InodeRef {
    unsafe { (*(&raw const ROOT)).clone().expect("no root filesystem") }
}

/// dir (すでにあるディレクトリ) の上に fs の根をかぶせる
pub fn mount(path: &str, fsroot: InodeRef) -> Result<(), i64> {
    let dir = resolve("", path, true)?;
    if !dir.meta().is_dir() {
        return Err(-ENOTDIR);
    }
    unsafe { (*(&raw mut MOUNTS)).push((dir.id(), fsroot)) };
    Ok(())
}

/// マウントしているすべてのファイルシステムを書き出す (sync)
pub fn sync_all() {
    // MAP_SHARED で書いたページを先にファイルへ
    crate::vm::sync_shared();
    let roots: Vec<InodeRef> = unsafe {
        let mut v: Vec<InodeRef> = (*(&raw const ROOT)).iter().cloned().collect();
        v.extend((*(&raw const MOUNTS)).iter().map(|(_, r)| r.clone()));
        v
    };
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
fn cross(i: InodeRef) -> InodeRef {
    let mounts = unsafe { &*(&raw const MOUNTS) };
    match mounts.iter().rev().find(|(id, _)| *id == i.id()) {
        Some((_, r)) => cross(r.clone()),
        None => i,
    }
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
    let mut path = normalize(cwd, path);
    let cred = crate::cred::current();
    'restart: for _ in 0..16 {
        let comps: Vec<String> = path.split('/').filter(|c| !c.is_empty()).map(String::from).collect();
        let mut cur = cross(root());
        let mut walked = String::new();
        for (i, c) in comps.iter().enumerate() {
            let m = cur.meta();
            if !m.is_dir() {
                return Err(-ENOTDIR);
            }
            // ディレクトリを通るには x が要る
            cred.check(&m, crate::cred::X)?;
            let next = cross(cur.lookup(c)?);
            let last = i + 1 == comps.len();
            if next.meta().mode & S_IFMT == S_IFLNK && (!last || follow) {
                if let Some((p, ino)) = next.magic_link() {
                    if last {
                        return Ok((p.trim_start_matches('/').to_string(), ino));
                    }
                    // /proc/self/fd/N/... : 開いているディレクトリから先をたどる
                    if !ino.meta().is_dir() {
                        return Err(-ENOTDIR);
                    }
                    walked = p.trim_start_matches('/').to_string();
                    cur = ino;
                    continue;
                }
                let t = next.readlink()?;
                let mut np = normalize(&walked, &t);
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

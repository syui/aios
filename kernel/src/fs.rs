// tmpfs: メモリ上のファイルシステム。起動時に initramfs を展開してルートにする
use crate::initrd;
use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::RefCell;

pub const ENOENT: i64 = 2;
pub const EEXIST: i64 = 17;
pub const ENOTDIR: i64 = 20;
pub const EISDIR: i64 = 21;
pub const EINVAL: i64 = 22;
pub const ENOTEMPTY: i64 = 39;
pub const ELOOP: i64 = 40;

pub const S_IFMT: u32 = 0o170000;
pub const S_IFIFO: u32 = 0o010000;
pub const S_IFCHR: u32 = 0o020000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFREG: u32 = 0o100000;
pub const S_IFLNK: u32 = 0o120000;

/// ファイルの中身。initramfs から来たものは書くまでそのまま参照する
pub enum Data {
    Static(&'static [u8]),
    Owned(Vec<u8>),
}

impl Data {
    pub fn bytes(&self) -> &[u8] {
        match self {
            Data::Static(b) => b,
            Data::Owned(v) => v,
        }
    }

    pub fn owned(&mut self) -> &mut Vec<u8> {
        if let Data::Static(b) = self {
            *self = Data::Owned(b.to_vec());
        }
        match self {
            Data::Owned(v) => v,
            Data::Static(_) => unreachable!(),
        }
    }
}

pub enum Node {
    File(Data),
    Dir(BTreeMap<String, InodeRef>),
    Symlink(String),
    /// キャラクタデバイス (major, minor)
    Dev(u32, u32),
    Fifo,
}

pub struct Inode {
    pub ino: u64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u32,
    pub mtime: u64,
    pub ctime: u64,
    pub node: Node,
}

pub type InodeRef = Rc<RefCell<Inode>>;

impl Inode {
    pub fn is_dir(&self) -> bool {
        matches!(self.node, Node::Dir(_))
    }

    pub fn size(&self) -> u64 {
        match &self.node {
            Node::File(d) => d.bytes().len() as u64,
            Node::Symlink(t) => t.len() as u64,
            Node::Dir(m) => m.len() as u64,
            _ => 0,
        }
    }

    pub fn rdev(&self) -> u64 {
        match self.node {
            Node::Dev(ma, mi) => ((ma as u64) << 8) | mi as u64,
            _ => 0,
        }
    }

    pub fn dir(&mut self) -> Result<&mut BTreeMap<String, InodeRef>, i64> {
        match &mut self.node {
            Node::Dir(m) => Ok(m),
            _ => Err(-ENOTDIR),
        }
    }

    pub fn touch(&mut self) {
        self.mtime = crate::timer::epoch_ns();
        self.ctime = self.mtime;
    }
}

static mut ROOT: Option<InodeRef> = None;
static mut NEXT_INO: u64 = 1;

pub fn root() -> InodeRef {
    unsafe { (*(&raw const ROOT)).clone().expect("fs not initialized") }
}

pub fn new_inode(mode: u32, node: Node) -> InodeRef {
    let ino = unsafe {
        NEXT_INO += 1;
        NEXT_INO
    };
    let now = crate::timer::epoch_ns();
    let nlink = if matches!(node, Node::Dir(_)) { 2 } else { 1 };
    Rc::new(RefCell::new(Inode { ino, mode, uid: 0, gid: 0, nlink, mtime: now, ctime: now, node }))
}

/// initramfs を展開し、/dev と /tmp を用意する
pub fn init() {
    let root = new_inode(S_IFDIR | 0o755, Node::Dir(BTreeMap::new()));
    unsafe { *(&raw mut ROOT) = Some(root.clone()) };
    for e in initrd::entries() {
        let node = match e.mode & S_IFMT {
            S_IFDIR => Node::Dir(BTreeMap::new()),
            S_IFLNK => Node::Symlink(String::from_utf8_lossy(e.data).to_string()),
            _ => Node::File(Data::Static(e.data)),
        };
        let (dir, name) = match e.name.rsplit_once('/') {
            Some((d, n)) => (d, n),
            None => ("", e.name),
        };
        let parent = match resolve("", dir, true) {
            Ok(p) => p,
            Err(_) => continue,
        };
        let ino = new_inode(e.mode, node);
        ino.borrow_mut().mtime = e.mtime as u64 * 1_000_000_000;
        let _ = link_into(&parent, name, ino);
    }
    let dev = mkdir_p("dev");
    for (name, mode, ma, mi) in [
        ("null", 0o666, 1, 3),
        ("zero", 0o666, 1, 5),
        ("random", 0o666, 1, 8),
        ("urandom", 0o666, 1, 9),
        ("tty", 0o666, 5, 0),
        ("console", 0o620, 5, 1),
    ] {
        let _ = link_into(&dev, name, new_inode(S_IFCHR | mode, Node::Dev(ma, mi)));
    }
    let tmp = mkdir_p("tmp");
    tmp.borrow_mut().mode = S_IFDIR | 0o1777;
    for d in ["etc", "home", "root", "var", "var/tmp"] {
        mkdir_p(d);
    }
    // df などが読むマウント表
    let etc = mkdir_p("etc");
    let mtab = b"tmpfs / tmpfs rw 0 0\n";
    let _ = link_into(&etc, "mtab", new_inode(S_IFREG | 0o644, Node::File(Data::Static(mtab))));
}

fn mkdir_p(path: &str) -> InodeRef {
    let mut cur = root();
    for c in path.split('/') {
        let next = cur.borrow_mut().dir().unwrap().get(c).cloned();
        cur = match next {
            Some(n) => n,
            None => {
                let d = new_inode(S_IFDIR | 0o755, Node::Dir(BTreeMap::new()));
                link_into(&cur, c, d.clone()).unwrap();
                d
            }
        };
    }
    cur
}

/// dir に name として ino をつなぐ
pub fn link_into(dir: &InodeRef, name: &str, ino: InodeRef) -> Result<(), i64> {
    let mut d = dir.borrow_mut();
    let m = d.dir()?;
    if m.contains_key(name) {
        return Err(-EEXIST);
    }
    if ino.borrow().is_dir() {
        d.nlink += 1;
    }
    let m = d.dir()?;
    m.insert(name.to_string(), ino);
    d.touch();
    Ok(())
}

/// cwd (先頭 / なし) を基準に path を絶対化し、. と .. を畳む
pub fn normalize(cwd: &str, path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let base = if path.starts_with('/') { "" } else { cwd };
    for c in base.split('/').chain(path.split('/')) {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
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
    'restart: for _ in 0..16 {
        let comps: Vec<String> = path.split('/').filter(|c| !c.is_empty()).map(String::from).collect();
        let mut cur = root();
        let mut walked = String::new();
        for (i, c) in comps.iter().enumerate() {
            let next = cur.borrow_mut().dir()?.get(c.as_str()).cloned().ok_or(-ENOENT)?;
            let last = i + 1 == comps.len();
            let target = match &next.borrow().node {
                Node::Symlink(t) if !last || follow => Some(t.clone()),
                _ => None,
            };
            if let Some(t) = target {
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
        return Ok(root());
    }
    lookup(cwd, path, follow).map(|(_, i)| i)
}

/// 最後の要素の親ディレクトリと名前。親はたどり切ったもの
pub fn parent_of(cwd: &str, path: &str) -> Result<(InodeRef, String), i64> {
    let full = normalize(cwd, path);
    let (dir, name) = match full.rsplit_once('/') {
        Some((d, n)) => (d.to_string(), n.to_string()),
        None => (String::new(), full.clone()),
    };
    if name.is_empty() {
        return Err(-EEXIST); // ルートそのもの
    }
    let parent = if dir.is_empty() { root() } else { resolve("", &dir, true)? };
    if !parent.borrow().is_dir() {
        return Err(-ENOTDIR);
    }
    Ok((parent, name))
}

pub fn unlink(parent: &InodeRef, name: &str, rmdir: bool) -> Result<(), i64> {
    let mut p = parent.borrow_mut();
    let ino = p.dir()?.get(name).cloned().ok_or(-ENOENT)?;
    {
        let mut i = ino.borrow_mut();
        match (&i.node, rmdir) {
            (Node::Dir(m), true) if !m.is_empty() => return Err(-ENOTEMPTY),
            (Node::Dir(_), true) => {}
            (Node::Dir(_), false) => return Err(-EISDIR),
            (_, true) => return Err(-ENOTDIR),
            _ => {}
        }
        i.nlink = i.nlink.saturating_sub(1);
        i.ctime = crate::timer::epoch_ns();
    }
    if rmdir {
        p.nlink -= 1;
    }
    p.dir()?.remove(name);
    p.touch();
    Ok(())
}

pub fn rename(op: &InodeRef, oname: &str, np: &InodeRef, nname: &str) -> Result<(), i64> {
    let ino = op.borrow_mut().dir()?.get(oname).cloned().ok_or(-ENOENT)?;
    let is_dir = ino.borrow().is_dir();
    if let Some(existing) = np.borrow_mut().dir()?.get(nname).cloned() {
        if Rc::ptr_eq(&existing, &ino) {
            return Ok(());
        }
        let e = existing.borrow();
        match (&e.node, is_dir) {
            (Node::Dir(m), true) if !m.is_empty() => return Err(-ENOTEMPTY),
            (Node::Dir(_), false) => return Err(-EISDIR),
            (_, true) if !e.is_dir() => return Err(-ENOTDIR),
            _ => {}
        }
    }
    if is_dir && is_ancestor(&ino, np) {
        return Err(-EINVAL);
    }
    op.borrow_mut().dir()?.remove(oname);
    if let Some(old) = np.borrow_mut().dir()?.insert(nname.to_string(), ino) {
        let mut o = old.borrow_mut();
        o.nlink = o.nlink.saturating_sub(1);
    }
    if is_dir && !Rc::ptr_eq(op, np) {
        op.borrow_mut().nlink -= 1;
        np.borrow_mut().nlink += 1;
    }
    op.borrow_mut().touch();
    np.borrow_mut().touch();
    Ok(())
}

/// a が b 自身か b の祖先か (ディレクトリを自分の中へ動かさないため)
fn is_ancestor(a: &InodeRef, b: &InodeRef) -> bool {
    fn walk(cur: &InodeRef, target: &InodeRef, inside: bool) -> bool {
        if inside && Rc::ptr_eq(cur, target) {
            return true;
        }
        let c = cur.borrow();
        let Node::Dir(m) = &c.node else { return false };
        m.values().any(|child| child.borrow().is_dir() && walk(child, target, inside))
    }
    Rc::ptr_eq(a, b) || walk(a, b, true)
}

pub fn statfs_bytes() -> [u8; 120] {
    use crate::memlayout::{PGSIZE, PHYSBASE, PHYSTOP};
    let mut b = [0u8; 120];
    let total = ((PHYSTOP - PHYSBASE) / PGSIZE) as u64;
    let free = crate::kalloc::nfree() as u64;
    b[0..8].copy_from_slice(&0x0102_1994u64.to_le_bytes()); // TMPFS_MAGIC
    b[8..16].copy_from_slice(&(PGSIZE as u64).to_le_bytes()); // f_bsize
    b[16..24].copy_from_slice(&total.to_le_bytes()); // f_blocks
    b[24..32].copy_from_slice(&free.to_le_bytes()); // f_bfree
    b[32..40].copy_from_slice(&free.to_le_bytes()); // f_bavail
    b[40..48].copy_from_slice(&65536u64.to_le_bytes()); // f_files
    b[48..56].copy_from_slice(&65536u64.to_le_bytes()); // f_ffree
    b[64..72].copy_from_slice(&255u64.to_le_bytes()); // f_namelen
    b[72..80].copy_from_slice(&(PGSIZE as u64).to_le_bytes()); // f_frsize
    b
}

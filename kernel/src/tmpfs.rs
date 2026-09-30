// tmpfs: メモリ上のファイルシステム
use crate::vfs::*;
use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::any::Any;
use core::cell::RefCell;

/// ファイルの中身。initramfs から来たものは書くまでそのまま参照する
pub enum Data {
    Static(&'static [u8]),
    Owned(Vec<u8>),
}

impl Data {
    fn bytes(&self) -> &[u8] {
        match self {
            Data::Static(b) => b,
            Data::Owned(v) => v,
        }
    }

    fn owned(&mut self) -> &mut Vec<u8> {
        if let Data::Static(b) = self {
            *self = Data::Owned(b.to_vec());
        }
        match self {
            Data::Owned(v) => v,
            Data::Static(_) => unreachable!(),
        }
    }
}

enum Node {
    File(Data),
    Dir(BTreeMap<String, Rc<TmpInode>>),
    Symlink(String),
    Dev(u32, u32),
    Fifo,
}

struct Attr {
    mode: u32,
    uid: u32,
    gid: u32,
    nlink: u32,
    mtime: u64,
    ctime: u64,
}

pub struct TmpInode {
    fs: usize,
    ino: u64,
    attr: RefCell<Attr>,
    node: RefCell<Node>,
}

static mut NEXT_INO: u64 = 1;

fn now() -> u64 {
    crate::timer::epoch_ns()
}

impl TmpInode {
    fn new(fs: usize, mode: u32, node: Node) -> Rc<TmpInode> {
        let ino = unsafe {
            NEXT_INO += 1;
            NEXT_INO
        };
        let nlink = if matches!(node, Node::Dir(_)) { 2 } else { 1 };
        let t = now();
        Rc::new(TmpInode { fs, ino, attr: RefCell::new(Attr { mode, uid: 0, gid: 0, nlink, mtime: t, ctime: t }), node: RefCell::new(node) })
    }

    fn touch(&self) {
        let mut a = self.attr.borrow_mut();
        a.mtime = now();
        a.ctime = a.mtime;
    }

    /// 同じ tmpfs の inode か
    fn downcast(i: &InodeRef, fs: usize) -> Result<Rc<TmpInode>, i64> {
        let t = i.as_any().downcast_ref::<TmpInode>().ok_or(-EXDEV)?;
        if t.fs != fs {
            return Err(-EXDEV);
        }
        // 中身が TmpInode だと確かめたので、同じ Rc を具体的な型で取り出す
        let raw = Rc::into_raw(i.clone()) as *const TmpInode;
        Ok(unsafe { Rc::from_raw(raw) })
    }

    fn with_dir<T>(&self, f: impl FnOnce(&mut BTreeMap<String, Rc<TmpInode>>) -> Result<T, i64>) -> Result<T, i64> {
        match &mut *self.node.borrow_mut() {
            Node::Dir(m) => f(m),
            _ => Err(-ENOTDIR),
        }
    }

    /// 起動時に initramfs の中身を置く
    pub fn add_static(&self, name: &str, mode: u32, data: &'static [u8], mtime: u64) -> Result<(), i64> {
        let node = match mode & S_IFMT {
            S_IFDIR => Node::Dir(BTreeMap::new()),
            S_IFLNK => Node::Symlink(String::from_utf8_lossy(data).to_string()),
            _ => Node::File(Data::Static(data)),
        };
        let child = TmpInode::new(self.fs, mode, node);
        child.attr.borrow_mut().mtime = mtime;
        self.insert(name, child)
    }

    fn insert(&self, name: &str, child: Rc<TmpInode>) -> Result<(), i64> {
        let is_dir = matches!(*child.node.borrow(), Node::Dir(_));
        self.with_dir(|m| {
            if m.contains_key(name) {
                return Err(-EEXIST);
            }
            m.insert(name.to_string(), child);
            Ok(())
        })?;
        if is_dir {
            self.attr.borrow_mut().nlink += 1;
        }
        self.touch();
        Ok(())
    }
}

pub fn new_root() -> Rc<TmpInode> {
    TmpInode::new(new_fs_id(), S_IFDIR | 0o755, Node::Dir(BTreeMap::new()))
}

impl Inode for TmpInode {
    fn id(&self) -> (usize, u64) {
        (self.fs, self.ino)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn meta(&self) -> Meta {
        let a = self.attr.borrow();
        let (size, rdev) = match &*self.node.borrow() {
            Node::File(d) => (d.bytes().len() as u64, 0),
            Node::Symlink(t) => (t.len() as u64, 0),
            Node::Dir(m) => (m.len() as u64, 0),
            Node::Dev(ma, mi) => (0, ((*ma as u64) << 8) | *mi as u64),
            Node::Fifo => (0, 0),
        };
        Meta { ino: self.ino, mode: a.mode, nlink: a.nlink, uid: a.uid, gid: a.gid, size, rdev, blocks: size.div_ceil(512), mtime: a.mtime, ctime: a.ctime }
    }

    fn read_at(&self, off: usize, buf: &mut [u8]) -> Result<usize, i64> {
        match &*self.node.borrow() {
            Node::File(d) => {
                let data = d.bytes();
                let n = buf.len().min(data.len().saturating_sub(off));
                buf[..n].copy_from_slice(&data[off..off + n]);
                Ok(n)
            }
            Node::Dir(_) => Err(-EISDIR),
            _ => Err(-EINVAL),
        }
    }

    fn write_at(&self, off: usize, buf: &[u8]) -> Result<usize, i64> {
        match &mut *self.node.borrow_mut() {
            Node::File(d) => {
                let v = d.owned();
                if v.len() < off + buf.len() {
                    v.resize(off + buf.len(), 0);
                }
                v[off..off + buf.len()].copy_from_slice(buf);
            }
            Node::Dir(_) => return Err(-EISDIR),
            _ => return Err(-EINVAL),
        }
        self.touch();
        Ok(buf.len())
    }

    fn truncate(&self, len: usize) -> Result<(), i64> {
        match &mut *self.node.borrow_mut() {
            Node::File(d) => d.owned().resize(len, 0),
            Node::Dir(_) => return Err(-EISDIR),
            _ => return Err(-EINVAL),
        }
        self.touch();
        Ok(())
    }

    fn readlink(&self) -> Result<String, i64> {
        match &*self.node.borrow() {
            Node::Symlink(t) => Ok(t.clone()),
            _ => Err(-EINVAL),
        }
    }

    fn lookup(&self, name: &str) -> Result<InodeRef, i64> {
        self.with_dir(|m| m.get(name).map(|c| c.clone() as InodeRef).ok_or(-ENOENT))
    }

    fn readdir(&self) -> Result<Vec<DirEntry>, i64> {
        self.with_dir(|m| {
            Ok(m.iter()
                .map(|(name, c)| DirEntry { name: name.clone(), ino: c.ino, mode: c.attr.borrow().mode })
                .collect())
        })
    }

    fn create(&self, name: &str, mode: u32, node: NewNode) -> Result<InodeRef, i64> {
        let mode = node.type_bits() | (mode & 0o7777);
        let node = match node {
            NewNode::File => Node::File(Data::Owned(Vec::new())),
            NewNode::Dir => Node::Dir(BTreeMap::new()),
            NewNode::Symlink(t) => Node::Symlink(t),
            NewNode::Dev(ma, mi) => Node::Dev(ma, mi),
            NewNode::Fifo => Node::Fifo,
        };
        let child = TmpInode::new(self.fs, mode, node);
        self.insert(name, child.clone())?;
        Ok(child)
    }

    fn link(&self, name: &str, target: &InodeRef) -> Result<(), i64> {
        let t = TmpInode::downcast(target, self.fs)?;
        if matches!(*t.node.borrow(), Node::Dir(_)) {
            return Err(-EPERM);
        }
        self.insert(name, t.clone())?;
        t.attr.borrow_mut().nlink += 1;
        Ok(())
    }

    fn unlink(&self, name: &str, rmdir: bool) -> Result<(), i64> {
        let child = self.with_dir(|m| m.get(name).cloned().ok_or(-ENOENT))?;
        let is_dir = match &*child.node.borrow() {
            Node::Dir(m) if rmdir && !m.is_empty() => return Err(-ENOTEMPTY),
            Node::Dir(_) if !rmdir => return Err(-EISDIR),
            Node::Dir(_) => true,
            _ if rmdir => return Err(-ENOTDIR),
            _ => false,
        };
        self.with_dir(|m| Ok(m.remove(name)))?;
        {
            let mut a = child.attr.borrow_mut();
            a.nlink = a.nlink.saturating_sub(if is_dir { 2 } else { 1 });
            a.ctime = now();
        }
        if is_dir {
            self.attr.borrow_mut().nlink -= 1;
        }
        self.touch();
        Ok(())
    }

    fn rename(&self, old: &str, newdir: &InodeRef, new: &str) -> Result<(), i64> {
        let nd = TmpInode::downcast(newdir, self.fs)?;
        let child = self.with_dir(|m| m.get(old).cloned().ok_or(-ENOENT))?;
        let is_dir = matches!(*child.node.borrow(), Node::Dir(_));
        if let Ok(existing) = nd.with_dir(|m| m.get(new).cloned().ok_or(-ENOENT)) {
            if existing.ino == child.ino {
                return Ok(());
            }
            nd.unlink(new, is_dir)?;
        }
        if is_dir && contains(&child, nd.ino) {
            return Err(-EINVAL);
        }
        self.with_dir(|m| Ok(m.remove(old)))?;
        if is_dir {
            self.attr.borrow_mut().nlink -= 1;
        }
        nd.insert(new, child)?;
        self.touch();
        Ok(())
    }

    fn set_mode(&self, mode: u32) -> Result<(), i64> {
        let mut a = self.attr.borrow_mut();
        a.mode = (a.mode & S_IFMT) | (mode & 0o7777);
        a.ctime = now();
        Ok(())
    }

    fn set_owner(&self, uid: Option<u32>, gid: Option<u32>) -> Result<(), i64> {
        let mut a = self.attr.borrow_mut();
        if let Some(u) = uid {
            a.uid = u;
        }
        if let Some(g) = gid {
            a.gid = g;
        }
        a.ctime = now();
        Ok(())
    }

    fn set_mtime(&self, ns: u64) -> Result<(), i64> {
        self.attr.borrow_mut().mtime = ns;
        Ok(())
    }

    fn statfs(&self) -> [u8; 120] {
        use crate::memlayout::{PGSIZE, PHYSBASE, PHYSTOP};
        let total = ((PHYSTOP - PHYSBASE) / PGSIZE) as u64;
        let free = crate::kalloc::nfree() as u64;
        statfs_bytes(0x0102_1994, PGSIZE as u64, total, free, 65536, 65536)
    }
}

/// dir の中 (自分も含む) に ino のディレクトリがあるか
fn contains(dir: &Rc<TmpInode>, ino: u64) -> bool {
    if dir.ino == ino {
        return true;
    }
    match &*dir.node.borrow() {
        Node::Dir(m) => m.values().any(|c| contains(c, ino)),
        _ => false,
    }
}

pub fn statfs_bytes(magic: u64, bsize: u64, blocks: u64, bfree: u64, files: u64, ffree: u64) -> [u8; 120] {
    let mut b = [0u8; 120];
    let fields = [(0, magic), (8, bsize), (16, blocks), (24, bfree), (32, bfree), (40, files), (48, ffree), (64, 255), (72, bsize)];
    for (off, v) in fields {
        b[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }
    b
}

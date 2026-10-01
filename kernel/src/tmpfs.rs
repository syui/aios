// tmpfs: メモリ上のファイルシステム
use crate::vfs::*;
use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::any::Any;
use core::cell::RefCell;
use crate::memlayout::PGSIZE;

/// ファイルの中身をページ (4KiB) の並びで持つ。書いていないところ (None) は 0
pub struct Pages {
    pages: Vec<Option<*mut u8>>,
    len: usize,
}

impl Pages {
    const fn new() -> Self {
        Pages { pages: Vec::new(), len: 0 }
    }

    fn read(&self, off: usize, buf: &mut [u8]) -> usize {
        let n = buf.len().min(self.len.saturating_sub(off));
        let mut done = 0;
        while done < n {
            let pos = off + done;
            let (pi, po) = (pos / PGSIZE, pos % PGSIZE);
            let k = (PGSIZE - po).min(n - done);
            match self.pages.get(pi).copied().flatten() {
                Some(p) => unsafe { core::ptr::copy_nonoverlapping(p.add(po), buf[done..].as_mut_ptr(), k) },
                None => buf[done..done + k].fill(0),
            }
            done += k;
        }
        n
    }

    fn write(&mut self, off: usize, buf: &[u8]) -> Result<(), i64> {
        let mut done = 0;
        while done < buf.len() {
            let pos = off + done;
            let (pi, po) = (pos / PGSIZE, pos % PGSIZE);
            let k = (PGSIZE - po).min(buf.len() - done);
            if self.pages.len() <= pi {
                self.pages.resize(pi + 1, None);
            }
            let p = match self.pages[pi] {
                Some(p) => p,
                None => {
                    let p = crate::kalloc::alloc().ok_or(-ENOSPC)?;
                    self.pages[pi] = Some(p);
                    p
                }
            };
            unsafe { core::ptr::copy_nonoverlapping(buf[done..].as_ptr(), p.add(po), k) };
            done += k;
        }
        self.len = self.len.max(off + buf.len());
        Ok(())
    }

    fn truncate(&mut self, len: usize) {
        if len < self.len {
            let keep = len.div_ceil(PGSIZE);
            for p in self.pages.drain(keep.min(self.pages.len())..).flatten() {
                crate::kalloc::free(p);
            }
            // 残ったページの後ろを 0 に
            if len % PGSIZE != 0 {
                if let Some(Some(p)) = self.pages.get(len / PGSIZE) {
                    unsafe { core::ptr::write_bytes(p.add(len % PGSIZE), 0, PGSIZE - len % PGSIZE) };
                }
            }
        }
        self.len = len;
    }
}

impl Drop for Pages {
    fn drop(&mut self) {
        for p in self.pages.drain(..).flatten() {
            crate::kalloc::free(p);
        }
    }
}

/// ファイルの中身。initramfs から来たものは書くまでそのまま参照する
pub enum Data {
    Static(&'static [u8]),
    Owned(Pages),
}

impl Data {
    fn len(&self) -> usize {
        match self {
            Data::Static(b) => b.len(),
            Data::Owned(p) => p.len,
        }
    }

    fn read(&self, off: usize, buf: &mut [u8]) -> usize {
        match self {
            Data::Static(b) => {
                let n = buf.len().min(b.len().saturating_sub(off));
                buf[..n].copy_from_slice(&b[off..off + n]);
                n
            }
            Data::Owned(p) => p.read(off, buf),
        }
    }

    fn owned(&mut self) -> Result<&mut Pages, i64> {
        if let Data::Static(b) = self {
            let mut p = Pages::new();
            p.write(0, b)?;
            *self = Data::Owned(p);
        }
        match self {
            Data::Owned(p) => Ok(p),
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
            Node::File(d) => (d.len() as u64, 0),
            Node::Symlink(t) => (t.len() as u64, 0),
            Node::Dir(m) => (m.len() as u64, 0),
            Node::Dev(ma, mi) => (0, ((*ma as u64) << 8) | *mi as u64),
            Node::Fifo => (0, 0),
        };
        Meta { ino: self.ino, mode: a.mode, nlink: a.nlink, uid: a.uid, gid: a.gid, size, rdev, blocks: size.div_ceil(512), mtime: a.mtime, ctime: a.ctime }
    }

    fn read_at(&self, off: usize, buf: &mut [u8]) -> Result<usize, i64> {
        match &*self.node.borrow() {
            Node::File(d) => Ok(d.read(off, buf)),
            Node::Dir(_) => Err(-EISDIR),
            _ => Err(-EINVAL),
        }
    }

    fn write_at(&self, off: usize, buf: &[u8]) -> Result<usize, i64> {
        match &mut *self.node.borrow_mut() {
            Node::File(d) => d.owned()?.write(off, buf)?,
            Node::Dir(_) => return Err(-EISDIR),
            _ => return Err(-EINVAL),
        }
        self.touch();
        Ok(buf.len())
    }

    fn truncate(&self, len: usize) -> Result<(), i64> {
        match &mut *self.node.borrow_mut() {
            Node::File(d) => d.owned()?.truncate(len),
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
            NewNode::File => Node::File(Data::Owned(Pages::new())),
            NewNode::Dir => Node::Dir(BTreeMap::new()),
            NewNode::Symlink(t) => Node::Symlink(t),
            NewNode::Dev(ma, mi) | NewNode::Blk(ma, mi) => Node::Dev(ma, mi),
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
        use crate::memlayout::{ram_size, PGSIZE};
        let total = (ram_size() / PGSIZE) as u64;
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

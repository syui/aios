// tmpfs: メモリ上のファイルシステム
use crate::cell::Bkl;
use crate::vfs::*;
use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::any::Any;
use core::cell::RefCell;
use crate::memlayout::PGSIZE;

/// tmpfs のファイルが持っているページの数 (ぜんぶの tmpfs で)
static mut USED: usize = 0;

/// tmpfs に使えるのは RAM の半分まで (Linux の tmpfs の既定と同じ)。
/// それより書くと ENOSPC (メモリを使いきって止まらないように。tmpfs のページはスワップへ出せない)
fn limit() -> usize {
    crate::memlayout::ram_size() / PGSIZE / 2
}

fn take_page() -> Result<*mut u8, i64> {
    if unsafe { USED } >= limit() {
        return Err(-ENOSPC);
    }
    let p = crate::kalloc::alloc().ok_or(-ENOSPC)?;
    unsafe { USED += 1 };
    Ok(p)
}

fn put_page(p: *mut u8) {
    // ページは参照で持つ (写しているところがまだ見ているかもしれない)
    crate::kalloc::put(p);
    unsafe { USED -= 1 };
}

/// (いま使っているページ, 上限)。ぜんぶの tmpfs と memfd で。/proc/ai/mem と /proc/ai/fs
pub fn stats() -> (usize, usize) {
    (unsafe { USED }, limit())
}

/// ファイルの中身をページ (4KiB) で持つ。書いたページだけを番号で引く表に入れ、ないところ (穴) は 0 として読む
/// (Linux の tmpfs の xarray と同じく、1 TiB 先に 1 バイト書いても表は 1 つ。前は番号の並び (Vec) を
/// そこまで伸ばしたので、ヒープを使いきって止まった)
pub struct Pages {
    pages: BTreeMap<usize, *mut u8>,
    len: usize,
}

impl Pages {
    const fn new() -> Self {
        Pages { pages: BTreeMap::new(), len: 0 }
    }

    fn read(&self, off: usize, buf: &mut [u8]) -> usize {
        let n = buf.len().min(self.len.saturating_sub(off));
        let mut done = 0;
        while done < n {
            let pos = off + done;
            let (pi, po) = (pos / PGSIZE, pos % PGSIZE);
            let k = (PGSIZE - po).min(n - done);
            match self.pages.get(&pi) {
                Some(&p) => unsafe { core::ptr::copy_nonoverlapping(p.add(po), buf[done..].as_mut_ptr(), k) },
                None => buf[done..done + k].fill(0),
            }
            done += k;
        }
        n
    }

    /// [from, upto) のうち、ページ pi の中を 0 に (ページがあれば)
    fn zero_in(&self, pi: usize, from: usize, upto: usize) {
        if let Some(&p) = self.pages.get(&pi) {
            let (a, b) = (from.max(pi * PGSIZE) - pi * PGSIZE, upto.min((pi + 1) * PGSIZE) - pi * PGSIZE);
            if a < b {
                unsafe { core::ptr::write_bytes(p.add(a), 0, b - a) };
            }
        }
    }

    fn write(&mut self, off: usize, buf: &[u8]) -> Result<(), i64> {
        let mut done = 0;
        // 終わりの先から書く: 終わりとのあいだ (終わりのページの中) を 0 に (写しで終わりの先に書いたものは残らない:
        // ext4 と同じ。Linux の tmpfs は残す)
        if off > self.len && !buf.is_empty() {
            self.zero_in(self.len / PGSIZE, self.len, off);
        }
        while done < buf.len() {
            let pos = off + done;
            let (pi, po) = (pos / PGSIZE, pos % PGSIZE);
            let k = (PGSIZE - po).min(buf.len() - done);
            let p = match self.pages.get(&pi) {
                Some(&p) => p,
                None => {
                    let p = take_page()?;
                    self.pages.insert(pi, p);
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
        if len > self.len {
            // 伸ばす: 終わりのページの、終わりの先を 0 に (写しで書いたものがあれば消える)
            self.zero_in(self.len / PGSIZE, self.len, len);
        }
        if len < self.len {
            let keep = len.div_ceil(PGSIZE);
            for (_, p) in self.pages.split_off(&keep) {
                put_page(p);
            }
            // 残ったページの後ろを 0 に
            if len % PGSIZE != 0
                && let Some(&p) = self.pages.get(&(len / PGSIZE))
            {
                unsafe { core::ptr::write_bytes(p.add(len % PGSIZE), 0, PGSIZE - len % PGSIZE) };
            }
        }
        self.len = len;
    }

    /// [from, to) を 0 に (fallocate の PUNCH_HOLE。Linux の shmem_truncate_range): まるごと入るページは捨てて穴に、
    /// はしのページは、あれば中の分を 0 に
    fn punch(&mut self, from: usize, to: usize) {
        if from >= to {
            return;
        }
        let zero = |pages: &BTreeMap<usize, *mut u8>, pi: usize, a: usize, b: usize| {
            if let Some(&p) = pages.get(&pi) {
                unsafe { core::ptr::write_bytes(p.add(a), 0, b - a) };
            }
        };
        if from / PGSIZE == (to - 1) / PGSIZE && (from % PGSIZE != 0 || to % PGSIZE != 0) {
            zero(&self.pages, from / PGSIZE, from % PGSIZE, (to - 1) % PGSIZE + 1);
        } else {
            if from % PGSIZE != 0 {
                zero(&self.pages, from / PGSIZE, from % PGSIZE, PGSIZE);
            }
            if to % PGSIZE != 0 {
                zero(&self.pages, to / PGSIZE, 0, to % PGSIZE);
            }
        }
        let (first, last) = (from.div_ceil(PGSIZE), to / PGSIZE);
        if first < last {
            let mut mid = self.pages.split_off(&first);
            let mut rest = mid.split_off(&last);
            for (_, p) in mid {
                put_page(p);
            }
            self.pages.append(&mut rest);
        }
    }

    /// SEEK_DATA / SEEK_HOLE (Linux の mapping_seek_hole_data: ページがあればデータ)。off はファイルの中
    fn seek(&self, off: usize, hole: bool) -> Result<usize, i64> {
        const ENXIO: i64 = 6;
        let pi = off / PGSIZE;
        if !hole {
            return match self.pages.range(pi..).next() {
                Some((&k, _)) if (k * PGSIZE).max(off) < self.len => Ok((k * PGSIZE).max(off)),
                _ => Err(-ENXIO),
            };
        }
        let mut cur = pi;
        while self.pages.contains_key(&cur) {
            cur += 1;
        }
        Ok((cur * PGSIZE).max(off).min(self.len))
    }
}

impl Drop for Pages {
    fn drop(&mut self) {
        for (_, p) in core::mem::take(&mut self.pages) {
            put_page(p);
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

/// ディレクトリの項目の読む位置と順 (Linux 6.18 の tmpfs: simple_offset の offset と、d_children の並び)。
/// offset は名前を入れたときに 3 から順に決まり、読む順は新しく入ったものから (d_children の頭に足す)。
/// readdir は位置 (lseek) の名前、つまり offset がそれより大きくない最後のものから、並びの順に読む
#[derive(Default)]
struct DirPos {
    /// 名前 → (offset, 並びの番号: 大きいほど新しい)
    names: BTreeMap<String, (u64, u64)>,
    by_off: BTreeMap<u64, String>,
    by_seq: BTreeMap<u64, String>,
    next_off: u64,
    next_seq: u64,
}

/// 名前につける offset の始まり (0 と 1 は . と ..、2 は「最初の名前から」)
const DIR_OFFSET_MIN: u64 = 3;
/// 終わりまで読んだときの位置 (Linux の DIR_OFFSET_EOD)
pub const DIR_OFFSET_EOD: u64 = i32::MAX as u64;

impl DirPos {
    /// name を並びの先頭に入れる。off があればその offset で (上書きする rename は、上書きされた名前の offset を
    /// 引きつぐ: simple_offset_rename)、なければ次の空いた offset (終わりまで来たら 3 から: mtree_alloc_cyclic)
    fn add(&mut self, name: &str, off: Option<u64>) {
        self.remove(name);
        let off = off.unwrap_or_else(|| {
            let wrap = |o: u64| if o + 1 >= DIR_OFFSET_EOD { DIR_OFFSET_MIN } else { o + 1 };
            let mut o = self.next_off.max(DIR_OFFSET_MIN);
            while self.by_off.contains_key(&o) {
                o = wrap(o);
            }
            self.next_off = wrap(o);
            o
        });
        self.next_seq += 1;
        self.names.insert(name.into(), (off, self.next_seq));
        self.by_off.insert(off, name.into());
        self.by_seq.insert(self.next_seq, name.into());
    }

    fn remove(&mut self, name: &str) -> Option<u64> {
        let (off, seq) = self.names.remove(name)?;
        self.by_off.remove(&off);
        self.by_seq.remove(&seq);
        Some(off)
    }

    /// 名前と offset はそのままで、並びの先頭へ (RENAME_EXCHANGE: d_exchange は両方を d_children の頭に)
    fn to_front(&mut self, name: &str) {
        if let Some(&(off, _)) = self.names.get(name) {
            self.add(name, Some(off));
        }
    }
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
    /// memfd の封 (F_SEAL_*)。memfd でなければ None
    seals: core::cell::Cell<Option<u32>>,
    /// O_TMPFILE で O_EXCL なしに作った、まだ名前のないもの (link で一度だけ名前をつけられる)
    linkable: core::cell::Cell<bool>,
    /// ディレクトリ: 項目の読む位置と順
    pos: RefCell<DirPos>,
}

pub const F_SEAL_SEAL: u32 = 1;
pub const F_SEAL_SHRINK: u32 = 2;
pub const F_SEAL_GROW: u32 = 4;
pub const F_SEAL_WRITE: u32 = 8;
pub const F_SEAL_FUTURE_WRITE: u32 = 0x10;
pub const F_SEAL_EXEC: u32 = 0x20;

static NEXT_INO: Bkl<u64> = Bkl::new(1);

/// inode の数と上限 (ヒープを使いきらないように: 1 つ 300 バイトほどで、作りつづけるとヒープが足りなくなる。
/// 上限で ENOSPC。Linux の nr_inodes と同じ考え。ヒープの大きさに合わせて: 64 MiB で 65536)
static INODES: Bkl<usize> = Bkl::new(0);

fn inode_limit() -> usize {
    (crate::heap::size() / 1024).max(65536)
}

/// (いまの inode の数, 上限)。/proc/ai/mem の tmpfs
pub fn inodes() -> (usize, usize) {
    (*INODES.get(), inode_limit())
}

impl Drop for TmpInode {
    fn drop(&mut self) {
        *INODES.get() -= 1;
        // 写したページ (MAP_PRIVATE) がページキャッシュに残っていれば捨てる (番号はまた使わないが、残ったままになる)
        crate::vm::file_changed((self.fs, self.ino));
    }
}

/// tmpfs の時刻は細かい時計で (床に覚える: timer の粗い時計)
fn now() -> u64 {
    crate::timer::stamp_ns()
}

impl TmpInode {
    fn new(fs: usize, mode: u32, node: Node) -> Rc<TmpInode> {
        *INODES.get() += 1;
        let ino = {
            let n = NEXT_INO.get();
            *n += 1;
            *n
        };
        let nlink = if matches!(node, Node::Dir(_)) { 2 } else { 1 };
        let t = now();
        Rc::new(TmpInode { fs, ino, attr: RefCell::new(Attr { mode, uid: 0, gid: 0, nlink, mtime: t, ctime: t }), node: RefCell::new(node), seals: core::cell::Cell::new(None), linkable: core::cell::Cell::new(false), pos: RefCell::new(DirPos::default()) })
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
        // ほかのものと同じく (パスの覚えは、ないという答えも覚えている)
        crate::vfs::name_changed(self.id(), name);
        self.insert(name, child)
    }

    fn insert(&self, name: &str, child: Rc<TmpInode>) -> Result<(), i64> {
        self.insert_at(name, child, None)
    }

    /// off: 読む位置 (なければ新しく。上書きする rename は、上書きされた名前のものを)
    fn insert_at(&self, name: &str, child: Rc<TmpInode>, off: Option<u64>) -> Result<(), i64> {
        // 消したディレクトリ (開いたまま rmdir されたもの) には作れない (Linux の IS_DEADDIR)
        if self.attr.borrow().nlink == 0 {
            return Err(-ENOENT);
        }
        let is_dir = matches!(*child.node.borrow(), Node::Dir(_));
        self.with_dir(|m| {
            if m.contains_key(name) {
                return Err(-EEXIST);
            }
            m.insert(name.to_string(), child);
            Ok(())
        })?;
        self.pos.borrow_mut().add(name, off);
        if is_dir {
            self.attr.borrow_mut().nlink += 1;
        }
        self.touch();
        Ok(())
    }
}

/// どのディレクトリにも入っていないファイル (memfd_create)。みんな同じ見えない tmpfs のもの。
/// 作ったプロセスのもので、封は seals から始まる
pub fn anon_file(uid: u32, gid: u32, seals: u32) -> InodeRef {
    static mut FS: usize = 0;
    let fs = unsafe {
        if FS == 0 {
            FS = new_fs_id();
        }
        FS
    };
    let t = TmpInode::new(fs, S_IFREG | 0o600, Node::File(Data::Owned(Pages::new())));
    {
        let mut a = t.attr.borrow_mut();
        a.uid = uid;
        a.gid = gid;
    }
    t.seals.set(Some(seals));
    t
}

pub fn new_root() -> Rc<TmpInode> {
    TmpInode::new(new_fs_id(), S_IFDIR | 0o755, Node::Dir(BTreeMap::new()))
}

impl Inode for TmpInode {
    fn path_cacheable(&self) -> bool {
        true
    }

    fn id(&self) -> (usize, u64) {
        (self.fs, self.ino)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn meta(&self) -> Meta {
        let a = self.attr.borrow();
        // blocks は持っているページの数 (Linux の tmpfs と同じ。穴は数えない)
        let pages = match &*self.node.borrow() {
            Node::File(Data::Owned(p)) => p.pages.len() as u64,
            Node::File(Data::Static(b)) => b.len().div_ceil(PGSIZE) as u64,
            _ => 0,
        };
        let (size, rdev) = match &*self.node.borrow() {
            Node::File(d) => (d.len() as u64, 0),
            Node::Symlink(t) => (t.len() as u64, 0),
            // ディレクトリの大きさは Linux の tmpfs と同じ: . と .. で 40、名前ごとに 20 (BOGO_DIRENT_SIZE)
            Node::Dir(m) => (40 + 20 * m.len() as u64, 0),
            Node::Dev(ma, mi) => (0, ((*ma as u64) << 8) | *mi as u64),
            Node::Fifo => (0, 0),
        };
        Meta { ino: self.ino, mode: a.mode, nlink: a.nlink, uid: a.uid, gid: a.gid, size, rdev, blocks: pages * 8, mtime: a.mtime, ctime: a.ctime }
    }

    fn read_at(&self, off: usize, buf: &mut [u8]) -> Result<usize, i64> {
        match &*self.node.borrow() {
            Node::File(d) => Ok(d.read(off, buf)),
            Node::Dir(_) => Err(-EISDIR),
            _ => Err(crate::why::inval()),
        }
    }

    /// ファイルの foff のページそのもの (参照をひとつ足して)。Linux の tmpfs (shmem_fault) と同じく、読むだけでも
    /// ページをとる (穴は 0 のページに。とれなければ ENOSPC: SIGBUS)
    fn page_frame(&self, foff: usize, _alloc: bool) -> Result<Option<*mut u8>, i64> {
        if foff % PGSIZE != 0 {
            return Err(crate::why::inval());
        }
        match &mut *self.node.borrow_mut() {
            Node::File(d) => {
                let p = d.owned()?;
                let pi = foff / PGSIZE;
                let frame = match p.pages.get(&pi) {
                    Some(&f) => f,
                    None => {
                        let f = take_page()?;
                        p.pages.insert(pi, f);
                        f
                    }
                };
                crate::kalloc::get(frame);
                Ok(Some(frame))
            }
            _ => Err(-ENODEV),
        }
    }

    /// 写しで書いた: 中身はもうページにある。はじめてなら更新時刻をいまに
    fn page_written(&self, _foff: usize, touch: bool) -> Result<(), i64> {
        if touch {
            self.touch();
        }
        Ok(())
    }

    fn write_at(&self, off: usize, buf: &[u8]) -> Result<usize, i64> {
        // 中身が変わる: ページキャッシュ (vm.rs) にある分を捨てる
        crate::vm::file_changed(self.id());
        match &mut *self.node.borrow_mut() {
            Node::File(d) => d.owned()?.write(off, buf)?,
            Node::Dir(_) => return Err(-EISDIR),
            _ => return Err(crate::why::inval()),
        }
        self.touch();
        Ok(buf.len())
    }

    fn truncate(&self, len: usize) -> Result<(), i64> {
        // 中身が変わる: ページキャッシュ (vm.rs) にある分を捨てる
        crate::vm::file_changed(self.id());
        if let Some(s) = self.seals.get() {
            let size = self.meta().size as usize;
            if (len < size && s & F_SEAL_SHRINK != 0) || (len > size && s & F_SEAL_GROW != 0) {
                return Err(-EPERM);
            }
        }
        let same = match &mut *self.node.borrow_mut() {
            Node::File(d) => {
                let same = d.len() == len;
                if len < d.len() {
                    // 写しているところから、なくなるページを外す (vm.rs)
                    crate::vm::drop_file_pages(self.id(), len.div_ceil(PGSIZE), usize::MAX);
                }
                d.owned()?.truncate(len);
                same
            }
            Node::Dir(_) => return Err(-EISDIR),
            _ => return Err(crate::why::inval()),
        };
        // 大きさが変わらなければ時刻も変えない (Linux の shmem_setattr。truncate(2) はそれだけ。ftruncate(2) は
        // ATTR_MTIME | ATTR_CTIME も渡すので、sysfile::ftruncate が変える)
        if !same {
            self.touch();
        }
        Ok(())
    }

    fn fallocate(&self, mode: u32, off: u64, len: u64, modified: &dyn Fn() -> Result<(), i64>) -> Result<(), i64> {
        const KEEP_SIZE: u32 = 0x01;
        const PUNCH_HOLE: u32 = 0x02;
        const ZERO_RANGE: u32 = 0x10;
        // Linux の shmem_fallocate: できるのは KEEP_SIZE と PUNCH_HOLE だけ。封を見て、うまくいったら file_modified。
        // 場所をとるとき (mode 0) もページは作らない (Linux でも、書くまで穴として読む)。
        // aios がえらんだちがい (zero-range-tmpfs): ZERO_RANGE もできる (ext4 と同じく 0 にして、KEEP_SIZE でなければ
        // 大きくする)。穴をあけてから場所をとるのと同じで、封は先に両方 (書く、大きくする) を見る。Linux の tmpfs は
        // EOPNOTSUPP で、使う人は穴をあけて場所をとりなおす。doc/aios.md の「aios がえらんだちがい」
        if mode & !(KEEP_SIZE | PUNCH_HOLE | ZERO_RANGE) != 0 {
            return Err(-EOPNOTSUPP);
        }
        let seals = self.seals.get().unwrap_or(0);
        let end = off.checked_add(len).ok_or_else(|| crate::why::inval())? as usize;
        crate::vm::file_changed(self.id());
        // 穴をあける (0 にする) ところのまるごとのページは、写しているところから外す (Linux の unmap_mapping_range)
        let drop_pages = |from: usize, to: usize| {
            let (first, last) = (from.div_ceil(PGSIZE), to / PGSIZE);
            if from < to && first < last {
                crate::vm::drop_file_pages(self.id(), first, last - 1);
            }
        };
        match &mut *self.node.borrow_mut() {
            Node::File(d) if mode & PUNCH_HOLE != 0 => {
                if seals & (F_SEAL_WRITE | F_SEAL_FUTURE_WRITE) != 0 {
                    return Err(-EPERM);
                }
                drop_pages(off as usize, end);
                d.owned()?.punch(off as usize, end);
            }
            Node::File(d) => {
                if mode & ZERO_RANGE != 0 && seals & (F_SEAL_WRITE | F_SEAL_FUTURE_WRITE) != 0 {
                    return Err(-EPERM);
                }
                if seals & F_SEAL_GROW != 0 && end > d.len() {
                    return Err(-EPERM);
                }
                if mode & ZERO_RANGE != 0 {
                    drop_pages(off as usize, end);
                    d.owned()?.punch(off as usize, end);
                }
                if mode & KEEP_SIZE == 0 && end > d.len() {
                    d.owned()?.truncate(end);
                }
            }
            _ => return Err(-EOPNOTSUPP),
        }
        crate::vm::file_changed(self.id());
        modified()?;
        self.touch();
        Ok(())
    }

    fn seals(&self) -> Option<u32> {
        self.seals.get()
    }

    fn add_seals(&self, s: u32) -> Result<(), i64> {
        let cur = self.seals.get().ok_or_else(|| crate::why::inval())?;
        if s & !(F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_WRITE | F_SEAL_FUTURE_WRITE | F_SEAL_EXEC) != 0 {
            return Err(crate::why::inval());
        }
        if cur & F_SEAL_SEAL != 0 {
            return Err(-EPERM);
        }
        // F_SEAL_WRITE は、書ける共有の写像が残っていると Linux では EBUSY。まだ写像までは見ない
        self.seals.set(Some(cur | s));
        Ok(())
    }

    fn readlink(&self) -> Result<String, i64> {
        match &*self.node.borrow() {
            Node::Symlink(t) => Ok(t.clone()),
            _ => Err(crate::why::inval()),
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

    fn readdir_offsets(&self, pos: u64, f: &mut dyn FnMut(u64, &str, u64, u32) -> bool) -> Option<Result<(), i64>> {
        let node = self.node.borrow();
        let Node::Dir(m) = &*node else { return Some(Err(-ENOTDIR)) };
        let p = self.pos.borrow();
        // pos の名前 (offset が pos より大きくない最後のもの) から並びの順に。なければ (2 か、どれよりも前) 先頭から
        let start = if pos <= 2 { None } else { p.by_off.range(..=pos).next_back().map(|(_, n)| p.names[n].1) };
        let mut it: alloc::boxed::Box<dyn Iterator<Item = (&u64, &String)>> = match start {
            Some(s) => alloc::boxed::Box::new(p.by_seq.range(..=s).rev()),
            None => alloc::boxed::Box::new(p.by_seq.iter().rev()),
        };
        for (_, name) in &mut it {
            let Some(c) = m.get(name) else { continue };
            if !f(p.names[name].0, name, c.ino, c.attr.borrow().mode) {
                break;
            }
        }
        Some(Ok(()))
    }

    fn create(&self, name: &str, mode: u32, node: NewNode) -> Result<InodeRef, i64> {
        if *INODES.get() >= inode_limit() {
            return Err(-ENOSPC);
        }
        crate::vfs::name_changed(self.id(), name);
        let mode = node.type_bits() | (mode & 0o7777);
        let node = match node {
            NewNode::File => Node::File(Data::Owned(Pages::new())),
            NewNode::Dir => Node::Dir(BTreeMap::new()),
            NewNode::Symlink(t) => Node::Symlink(t),
            NewNode::Dev(ma, mi) | NewNode::Blk(ma, mi) => Node::Dev(ma, mi),
            // 中身のない特別なもの (種類は mode で見分ける)
            NewNode::Fifo | NewNode::Sock => Node::Fifo,
        };
        let child = TmpInode::new(self.fs, mode, node);
        self.insert(name, child.clone())?;
        Ok(child)
    }

    fn seek_data(&self, off: u64, hole: bool) -> Option<Result<u64, i64>> {
        match &*self.node.borrow() {
            Node::File(Data::Owned(p)) => Some(p.seek(off as usize, hole).map(|x| x as u64)),
            _ => None,
        }
    }

    fn tmpfile(&self, mode: u32, linkable: bool) -> Result<InodeRef, i64> {
        if !matches!(*self.node.borrow(), Node::Dir(_)) {
            return Err(-ENOTDIR);
        }
        if *INODES.get() >= inode_limit() {
            return Err(-ENOSPC);
        }
        let t = TmpInode::new(self.fs, S_IFREG | (mode & 0o7777), Node::File(Data::Owned(Pages::new())));
        t.attr.borrow_mut().nlink = 0;
        t.linkable.set(linkable);
        Ok(t)
    }

    fn link(&self, name: &str, target: &InodeRef) -> Result<(), i64> {
        crate::vfs::name_changed(self.id(), name);
        let t = TmpInode::downcast(target, self.fs)?;
        if matches!(*t.node.borrow(), Node::Dir(_)) {
            return Err(-EPERM);
        }
        // リンクが 0 (消したが開いているもの) は、O_TMPFILE の linkable なものだけ (Linux の vfs_link)
        if t.attr.borrow().nlink == 0 && !t.linkable.get() {
            return Err(-ENOENT);
        }
        self.insert(name, t.clone())?;
        t.linkable.set(false);
        t.attr.borrow_mut().nlink += 1;
        Ok(())
    }

    fn unlink(&self, name: &str, rmdir: bool) -> Result<(), i64> {
        crate::vfs::name_changed(self.id(), name);
        let child = self.with_dir(|m| m.get(name).cloned().ok_or(-ENOENT))?;
        let is_dir = match &*child.node.borrow() {
            Node::Dir(m) if rmdir && !m.is_empty() => return Err(-ENOTEMPTY),
            Node::Dir(_) if !rmdir => return Err(-EISDIR),
            Node::Dir(_) => true,
            _ if rmdir => return Err(-ENOTDIR),
            _ => false,
        };
        self.with_dir(|m| Ok(m.remove(name)))?;
        self.pos.borrow_mut().remove(name);
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
        crate::vfs::name_changed(self.id(), old);
        crate::vfs::name_changed(newdir.id(), new);
        let nd = TmpInode::downcast(newdir, self.fs)?;
        let child = self.with_dir(|m| m.get(old).cloned().ok_or(-ENOENT))?;
        let is_dir = matches!(*child.node.borrow(), Node::Dir(_));
        if is_dir && contains(&child, nd.ino) {
            return Err(crate::why::inval());
        }
        // 上書きするなら、その名前の読む位置を引きつぐ (Linux の simple_offset_rename)
        let mut keep = None;
        if let Ok(existing) = nd.with_dir(|m| m.get(new).cloned().ok_or(-ENOENT)) {
            if existing.ino == child.ino {
                return Ok(());
            }
            keep = nd.pos.borrow().names.get(new).map(|e| e.0);
            nd.unlink(new, is_dir)?;
        }
        self.with_dir(|m| Ok(m.remove(old)))?;
        self.pos.borrow_mut().remove(old);
        if is_dir {
            self.attr.borrow_mut().nlink -= 1;
        }
        nd.insert_at(new, child, keep)?;
        self.touch();
        Ok(())
    }

    fn exchange(&self, old: &str, newdir: &InodeRef, new: &str) -> Result<(), i64> {
        crate::vfs::name_changed(self.id(), old);
        crate::vfs::name_changed(newdir.id(), new);
        let nd = TmpInode::downcast(newdir, self.fs)?;
        let a = self.with_dir(|m| m.get(old).cloned().ok_or(-ENOENT))?;
        let b = nd.with_dir(|m| m.get(new).cloned().ok_or(-ENOENT))?;
        if a.ino == b.ino {
            return Ok(());
        }
        let adir = matches!(*a.node.borrow(), Node::Dir(_));
        let bdir = matches!(*b.node.borrow(), Node::Dir(_));
        // 自分の下のものとは入れかえられない
        if (adir && contains(&a, nd.ino)) || (bdir && contains(&b, self.ino)) {
            return Err(crate::why::inval());
        }
        self.with_dir(|m| Ok(m.insert(old.to_string(), b.clone())))?;
        nd.with_dir(|m| Ok(m.insert(new.to_string(), a.clone())))?;
        // 名前の読む位置はそのままで、どちらも並びの先頭へ (new が先: d_exchange)
        self.pos.borrow_mut().to_front(old);
        nd.pos.borrow_mut().to_front(new);
        // ディレクトリがほかの親へ移ったら、親のリンクの数 (.. の分) も
        if self.ino != nd.ino && adir != bdir {
            let (gain, lose) = if adir { (&nd.attr, &self.attr) } else { (&self.attr, &nd.attr) };
            gain.borrow_mut().nlink += 1;
            lose.borrow_mut().nlink -= 1;
        }
        self.touch();
        nd.touch();
        Ok(())
    }

    fn set_mode(&self, mode: u32) -> Result<(), i64> {
        if self.attr.borrow().mode & S_IFMT == S_IFDIR {
            crate::vfs::dir_changed(self.id());
        }
        let mut a = self.attr.borrow_mut();
        a.mode = (a.mode & S_IFMT) | (mode & 0o7777);
        a.ctime = now();
        Ok(())
    }

    fn set_owner(&self, uid: Option<u32>, gid: Option<u32>) -> Result<(), i64> {
        if self.attr.borrow().mode & S_IFMT == S_IFDIR {
            crate::vfs::dir_changed(self.id());
        }
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

    fn fs_stats(&self) -> Option<String> {
        let (used, limit) = stats();
        Some(alloc::format!("{{\"pages\":{},\"limit\":{}}}", used, limit))
    }

    fn statfs(&self) -> [u8; 120] {
        // df に出る大きさは使える上限 (RAM の半分)
        let total = limit() as u64;
        let free = (limit().saturating_sub(unsafe { USED })).min(crate::kalloc::nfree()) as u64;
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

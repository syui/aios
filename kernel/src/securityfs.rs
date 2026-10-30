// /sys/kernel/security (Linux の securityfs): セキュリティの決まりの口。起動のときにかぶせる (fs.rs。mount -t securityfs も)
//
//   lsm                              動いているもの ("capability,landlock,apparmor"。改行なし)
//   apparmor/.load .replace .remove  プロファイルを足す・入れかえる・外す (apparmor_parser が書く。1 回の write でみな)
//   apparmor/.access                 書いた問いの答えを、同じ開いた口から読む (libapparmor の aa_query_label)
//   apparmor/profiles revision       読みこんだプロファイル (「名前 (モード)」) と、変えた回数
//   apparmor/.ns_name .ns_level .stacked .ns_stacked
//   apparmor/features/...            名のること (apparmor.rs の FEATURES。apparmor_parser が読んで形を選ぶ)
// apparmor を切ると (aios.off=apparmor)、apparmor/ はなくなる
use crate::vfs::*;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::any::Any;
use core::cell::RefCell;

const EACCES: i64 = 13;
const ESPIPE: i64 = 29;
const SECURITYFS_MAGIC: u64 = 0x7365_6375;

/// apparmor/ のファイル
const AA_FILES: [&str; 10] = [".access", ".load", ".ns_level", ".ns_name", ".ns_stacked", ".remove", ".replace", ".stacked", "profiles", "revision"];

#[derive(Clone, Copy, PartialEq)]
enum Node {
    Root,
    Lsm,
    Aa,
    AaFile(u8),
    /// features の下 (パス。FEATURES にあればファイル、それで始まるものがあればディレクトリ)
    Feat(&'static str),
}

pub struct SecInode {
    fs: usize,
    node: Node,
    /// .access: 開いた口ごとの答えと、読んだところ
    answer: RefCell<(Vec<u8>, usize)>,
}

pub fn new_root() -> InodeRef {
    Rc::new(SecInode { fs: new_fs_id(), node: Node::Root, answer: RefCell::new((Vec::new(), 0)) })
}

fn aa_on() -> bool {
    !crate::parts::off(crate::parts::Part::Apparmor)
}

/// FEATURES のうち、path そのもの (ファイル) か、その下のもの (ディレクトリ)。どちらでもなければ None
fn feat_kind(path: &'static str) -> Option<bool> {
    let f = crate::apparmor::FEATURES;
    if f.iter().any(|(p, _)| *p == path) {
        return Some(false);
    }
    f.iter().any(|(p, _)| path.is_empty() || p.strip_prefix(path).is_some_and(|r| r.starts_with('/'))).then_some(true)
}

/// path の下のすぐの名前 (順に、ひとつずつ)
fn feat_children(path: &'static str) -> Vec<&'static str> {
    let mut v: Vec<&'static str> = Vec::new();
    for (p, _) in crate::apparmor::FEATURES {
        let rest = if path.is_empty() { Some(*p) } else { p.strip_prefix(path).and_then(|r| r.strip_prefix('/')) };
        if let Some(r) = rest {
            let name = r.split('/').next().unwrap_or(r);
            let full = &p[..p.len() - r.len() + name.len()];
            if !v.contains(&full) {
                v.push(full);
            }
        }
    }
    v
}

impl SecInode {
    fn child(&self, node: Node) -> InodeRef {
        Rc::new(SecInode { fs: self.fs, node, answer: RefCell::new((Vec::new(), 0)) })
    }

    fn ino(&self) -> u64 {
        match self.node {
            Node::Root => 1,
            Node::Lsm => 2,
            Node::Aa => 3,
            Node::AaFile(i) => 0x10 + i as u64,
            // パスの FNV-1a (ほかと重ならないように上のビットを立てる)
            Node::Feat(p) => (1 << 40) | (p.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3)) & 0xff_ffff_ffff),
        }
    }

    fn is_dir(&self) -> bool {
        match self.node {
            Node::Root | Node::Aa => true,
            Node::Feat(p) => feat_kind(p) == Some(true),
            _ => false,
        }
    }

    fn mode(&self) -> u32 {
        match self.node {
            _ if self.is_dir() => S_IFDIR | 0o755,
            Node::AaFile(i) if matches!(AA_FILES[i as usize], ".access" | ".load" | ".replace" | ".remove") => S_IFREG | 0o666,
            _ => S_IFREG | 0o444,
        }
    }

    fn content(&self) -> Result<String, i64> {
        Ok(match self.node {
            Node::Lsm => {
                let mut s = String::from("capability");
                if !crate::parts::off(crate::parts::Part::Landlock) {
                    s.push_str(",landlock");
                }
                if aa_on() {
                    s.push_str(",apparmor");
                }
                s
            }
            Node::AaFile(i) => match AA_FILES[i as usize] {
                ".ns_level" => "0\n".into(),
                ".ns_name" => "root\n".into(),
                ".ns_stacked" | ".stacked" => "no\n".into(),
                "profiles" => crate::apparmor::profiles_text()?,
                "revision" => format!("{}\n", crate::apparmor::revision()),
                // 書くだけの口 (Linux も read がないので EINVAL)
                _ => return Err(crate::why::inval()),
            },
            Node::Feat(p) => match crate::apparmor::FEATURES.iter().find(|(q, _)| *q == p) {
                Some((_, v)) => format!("{}\n", v),
                None => return Err(-EISDIR),
            },
            _ => return Err(-EISDIR),
        })
    }
}

impl Inode for SecInode {
    fn id(&self) -> (usize, u64) {
        (self.fs, self.ino())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn meta(&self) -> Meta {
        let now = crate::timer::epoch_ns();
        Meta { ino: self.ino(), mode: self.mode(), nlink: 1, uid: 0, gid: 0, size: 0, rdev: 0, blocks: 0, mtime: now, ctime: now }
    }

    fn read_at(&self, off: usize, buf: &mut [u8]) -> Result<usize, i64> {
        if let Node::AaFile(i) = self.node
            && AA_FILES[i as usize] == ".access"
        {
            // 書いた問いの答えを頭から (読んだぶんだけ進む。口の位置は使わない: Linux も書いても位置を進めない)
            let mut a = self.answer.borrow_mut();
            let (ans, at) = &mut *a;
            let n = buf.len().min(ans.len() - *at);
            buf[..n].copy_from_slice(&ans[*at..*at + n]);
            *at += n;
            return Ok(n);
        }
        let s = self.content()?;
        let b = s.as_bytes();
        if off >= b.len() {
            return Ok(0);
        }
        let n = buf.len().min(b.len() - off);
        buf[..n].copy_from_slice(&b[off..off + n]);
        Ok(n)
    }

    fn write_at(&self, off: usize, b: &[u8]) -> Result<usize, i64> {
        let Node::AaFile(i) = self.node else { return Err(crate::why::inval()) };
        let name = AA_FILES[i as usize];
        if name == ".access" {
            let ans = crate::apparmor::query(b)?;
            *self.answer.borrow_mut() = (ans.into_bytes(), 0);
            return Ok(b.len());
        }
        if !matches!(name, ".load" | ".replace" | ".remove") {
            return Err(crate::why::inval());
        }
        // 1 回の write でみな (Linux の aa_simple_write_to_buffer: 頭からでなければ ESPIPE)
        if off != 0 {
            return Err(-ESPIPE);
        }
        match name {
            ".remove" => crate::apparmor::remove(b),
            _ => crate::apparmor::load(b, name == ".replace"),
        }
    }

    fn whole_write(&self) -> bool {
        matches!(self.node, Node::AaFile(_))
    }

    fn truncate(&self, _: usize) -> Result<(), i64> {
        // O_TRUNC で開くのは、書く口ならかまわない (echo NAME > .remove)
        if matches!(self.node, Node::AaFile(_)) { Ok(()) } else { Err(-EACCES) }
    }

    fn readlink(&self) -> Result<String, i64> {
        Err(crate::why::inval())
    }

    fn lookup(&self, name: &str) -> Result<InodeRef, i64> {
        let node = match self.node {
            Node::Root if name == "lsm" => Node::Lsm,
            Node::Root if name == "apparmor" && aa_on() => Node::Aa,
            Node::Aa if name == "features" => Node::Feat(""),
            Node::Aa => Node::AaFile(AA_FILES.iter().position(|f| *f == name).ok_or(-ENOENT)? as u8),
            Node::Feat(p) if self.is_dir() => {
                let full = feat_children(p).into_iter().find(|c| c.rsplit('/').next() == Some(name)).ok_or(-ENOENT)?;
                Node::Feat(full)
            }
            _ if !self.is_dir() => return Err(-ENOTDIR),
            _ => return Err(-ENOENT),
        };
        Ok(self.child(node))
    }

    fn readdir(&self) -> Result<Vec<DirEntry>, i64> {
        let mut v = Vec::new();
        let mut add = |name: &str, node: Node| {
            let c = SecInode { fs: self.fs, node, answer: RefCell::new((Vec::new(), 0)) };
            v.push(DirEntry { name: name.into(), ino: c.ino(), mode: c.mode() });
        };
        match self.node {
            Node::Root => {
                add("lsm", Node::Lsm);
                if aa_on() {
                    add("apparmor", Node::Aa);
                }
            }
            Node::Aa => {
                for (i, f) in AA_FILES.iter().enumerate() {
                    add(f, Node::AaFile(i as u8));
                }
                add("features", Node::Feat(""));
            }
            Node::Feat(p) if self.is_dir() => {
                for c in feat_children(p) {
                    add(c.rsplit('/').next().unwrap_or(c), Node::Feat(c));
                }
            }
            _ => return Err(-ENOTDIR),
        }
        Ok(v)
    }

    fn create(&self, _: &str, _: u32, _: NewNode) -> Result<InodeRef, i64> {
        Err(-EACCES)
    }

    fn link(&self, _: &str, _: &InodeRef) -> Result<(), i64> {
        Err(-EACCES)
    }

    fn unlink(&self, _: &str, _: bool) -> Result<(), i64> {
        Err(-EACCES)
    }

    fn rename(&self, _: &str, _: &InodeRef, _: &str) -> Result<(), i64> {
        Err(-EACCES)
    }

    fn set_mode(&self, _: u32) -> Result<(), i64> {
        Err(-EPERM)
    }

    fn set_owner(&self, _: Option<u32>, _: Option<u32>) -> Result<(), i64> {
        Err(-EPERM)
    }

    fn set_mtime(&self, _: u64) -> Result<(), i64> {
        Err(-EPERM)
    }

    fn statfs(&self) -> [u8; 120] {
        crate::tmpfs::statfs_bytes(SECURITYFS_MAGIC, 4096, 0, 0, 0, 0)
    }
}

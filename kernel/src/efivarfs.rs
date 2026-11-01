// /sys/firmware/efi/efivars (Linux の efivarfs): UEFI の変数を「名前-GUID」のファイルにする (fs/efivarfs と同じ)
//
//   読む     属性 (4 バイト、LE) と中身。まだ書いていないもの (作っただけ) は 0 バイト
//   書く     1 回の write が 1 回の SetVariable: 頭の 4 バイトが属性 (EFI_VARIABLE_*。ほかのビットは EINVAL)、のこりが中身。
//            APPEND_WRITE なら足す。位置は見ない。中身が 0 バイトなら消す (ファイルは閉じるまで 0 バイトで残る)
//   作る     open(O_CREAT): 名前は「名前-xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx」。書くまで UEFI にはない。書かずに閉じると消える
//   消す     unlink: SetVariable を 0 バイトで
//   chattr   Linux が知っている変数 (EFI の BootOrder、Boot####、Timeout など。is_removable) のほかは、変えられない印
//            (FS_IMMUTABLE_FL) がついている: 書く・消すには chattr -i (FS_IOC_SETFLAGS) を先に (systemd、efibootmgr もする)
//   statfs   QueryVariableInfo (f_bsize は 1、f_blocks と f_bfree がバイト数)
// 一覧はマウントのときに GetNextVariableName で作る (そのあとファームウェアが足したものは見えない: Linux と同じ)。
// マウントはひとつを共有する (Linux の get_tree_single)。UEFI から起動していなければ ENODEV
use crate::efirt::{self, Guid};
use crate::vfs::*;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::any::Any;
use core::cell::{Cell, RefCell};

const ENOTTY: i64 = 25;
const EFIVARFS_MAGIC: u64 = 0xde5e_81e4;
/// EFI_VARIABLE_MASK (NON_VOLATILE .. APPEND_WRITE)
const ATTR_MASK: u32 = 0x7f;
const ATTR_NV_BS_RT: u32 = 0x7;
pub const FS_IMMUTABLE_FL: u32 = 0x10;
/// 名前のうしろの GUID の長さ
const GUID_LEN: usize = 36;

/// EFI_GLOBAL_VARIABLE (8be4df61-93ca-11d2-aa0d-00e098032b8c)
const EFI_GLOBAL: Guid = Guid([0x61, 0xdf, 0xe4, 0x8b, 0xca, 0x93, 0xd2, 0x11, 0xaa, 0x0d, 0x00, 0xe0, 0x98, 0x03, 0x2b, 0x8c]);
/// LINUX_EFI_CRASH_GUID (cfc8fc79-be2e-4ddc-97f0-9f98bfe298a0)
const LINUX_CRASH: Guid = Guid([0x79, 0xfc, 0xc8, 0xcf, 0x2e, 0xbe, 0xdc, 0x4d, 0x97, 0xf0, 0x9f, 0x98, 0xbf, 0xe2, 0x98, 0xa0]);
/// LINUX_EFI_RANDOM_SEED_TABLE_GUID (1ce1e5bc-7ceb-42f2-81e5-8aadf180f57b): 見せない、作れない
const RANDOM_SEED: Guid = Guid([0xbc, 0xe5, 0xe1, 0x1c, 0xeb, 0x7c, 0xf2, 0x42, 0x81, 0xe5, 0x8a, 0xad, 0xf1, 0x80, 0xf5, 0x7b]);

/// どの形の中身か (Linux の variable_validate の表)
#[derive(Clone, Copy)]
enum Check {
    None,
    Uint16,
    BootOrder,
    LoadOption,
    DevicePath,
    Ascii,
}

/// Linux が知っている変数 (variable_validate): 中身を確かめ、消せる (変えられない印をつけない)
const KNOWN: [(Guid, &str, Check); 16] = [
    (EFI_GLOBAL, "BootNext", Check::Uint16),
    (EFI_GLOBAL, "BootOrder", Check::BootOrder),
    (EFI_GLOBAL, "Boot*", Check::LoadOption),
    (EFI_GLOBAL, "DriverOrder", Check::BootOrder),
    (EFI_GLOBAL, "Driver*", Check::LoadOption),
    (EFI_GLOBAL, "ConIn", Check::DevicePath),
    (EFI_GLOBAL, "ConInDev", Check::DevicePath),
    (EFI_GLOBAL, "ConOut", Check::DevicePath),
    (EFI_GLOBAL, "ConOutDev", Check::DevicePath),
    (EFI_GLOBAL, "ErrOut", Check::DevicePath),
    (EFI_GLOBAL, "ErrOutDev", Check::DevicePath),
    (EFI_GLOBAL, "Lang", Check::Ascii),
    (EFI_GLOBAL, "OsIndications", Check::None),
    (EFI_GLOBAL, "PlatformLang", Check::Ascii),
    (EFI_GLOBAL, "Timeout", Check::Uint16),
    (LINUX_CRASH, "*", Check::None),
];

/// Linux の variable_matches: name (len 文字) が pat (最後の * はのこりのどれでも) に当たるか。
/// match は pat の * でない文字のうち当たった数 (当たらなくても)
fn matches(name: &[u8], len: usize, pat: &str) -> (bool, usize) {
    let p = pat.as_bytes();
    let mut m = 0;
    loop {
        match p.get(m) {
            Some(b'*') => return (true, m),
            None => return (m == len, m),
            Some(&c) => {
                if m < len && name.get(m) == Some(&c) {
                    m += 1;
                    continue;
                }
                return (false, m);
            }
        }
    }
}

/// 消せる (変えられない印をつけない) 変数か (efivar_variable_is_removable。name は名前のところ)
fn removable(guid: &Guid, name: &[u8]) -> bool {
    KNOWN.iter().any(|(g, pat, _)| g == guid && matches(name, name.len(), pat).0)
}

/// 書く中身を確かめる (efivar_validate)。Linux は名前の長さに NUL を 1 つ足してくらべるので、* のない名前
/// (BootNext、Timeout など) には当たらず、Boot* か Driver* (と crash の *) だけが確かめられる。同じにする
fn validate(guid: &Guid, name16: &[u16], data: &[u8]) -> bool {
    let utf8 = utf8_name(name16);
    let mut name = utf8.into_bytes();
    let len = name.len() + 1;
    name.push(0);
    for (g, pat, check) in KNOWN.iter() {
        if g != guid {
            continue;
        }
        let (hit, m) = matches(&name, len, pat);
        if hit {
            return match check {
                Check::None => true,
                Check::Uint16 => data.len() == 2,
                Check::BootOrder => data.len().is_multiple_of(2),
                Check::LoadOption => load_option(name16, m, data),
                Check::DevicePath => device_path(data),
                Check::Ascii => ascii(data),
            };
        }
    }
    true
}

/// validate_device_path: 終わりの印 (型 0x7f か 0xff、下位の型 0xff) まで、長さがはみ出さずにつながっているか
fn device_path(b: &[u8]) -> bool {
    let len = b.len();
    if len < 4 {
        return false;
    }
    let mut off = 0;
    while off <= len - 4 {
        let (ty, sub, nlen) = (b[off], b[off + 1], u16::from_le_bytes([b[off + 2], b[off + 3]]) as usize);
        if nlen < 4 || nlen > len - off {
            break;
        }
        off += nlen;
        if (ty == 0x7f || ty == 0xff) && sub == 0xff {
            return true;
        }
    }
    false
}

/// validate_load_option: Boot#### / Driver#### (#### は 16 進。でなければ確かめない) の EFI_LOAD_OPTION
fn load_option(name16: &[u16], m: usize, b: &[u8]) -> bool {
    let namelen = name16.iter().take(1024).take_while(|&&c| c != 0).count();
    for i in m..m + 4 {
        let c = name16.get(i).copied().unwrap_or(0);
        if c > 127 || !(c as u8).is_ascii_hexdigit() {
            return true;
        }
    }
    if namelen > m + 4 || b.len() < 8 {
        return false;
    }
    let fplen = u16::from_le_bytes([b[4], b[5]]) as usize;
    // 説明 (UTF-16、NUL まで) の大きさ + NUL
    let desc = b[6..].chunks_exact(2).take_while(|c| c != &[0, 0]).count() * 2 + 2;
    if desc + fplen + 6 > b.len() {
        return false;
    }
    device_path(&b[desc + 6..desc + 6 + fplen])
}

/// validate_ascii_string: 127 より大きいものがなく、NUL で終わる
fn ascii(b: &[u8]) -> bool {
    for &c in b {
        if c > 127 {
            return false;
        }
        if c == 0 {
            return true;
        }
    }
    false
}

/// GUID を小文字の文字列に (Linux の %pUl)
fn guid_str(g: &Guid) -> String {
    let b = &g.0;
    format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
        u16::from_le_bytes([b[4], b[5]]),
        u16::from_le_bytes([b[6], b[7]]),
        b[8],
        b[9],
        b[10],
        b[11],
        b[12],
        b[13],
        b[14],
        b[15]
    )
}

/// 「xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx」を GUID に (uuid_is_valid と guid_parse。大文字もよい)
fn parse_guid(s: &str) -> Option<Guid> {
    let b = s.as_bytes();
    if b.len() != GUID_LEN {
        return None;
    }
    for (i, &c) in b.iter().enumerate() {
        let dash = matches!(i, 8 | 13 | 18 | 23);
        if dash != (c == b'-') || (!dash && !c.is_ascii_hexdigit()) {
            return None;
        }
    }
    let hex = |r: core::ops::Range<usize>| u64::from_str_radix(&s[r], 16).ok();
    let mut g = [0u8; 16];
    g[0..4].copy_from_slice(&(hex(0..8)? as u32).to_le_bytes());
    g[4..6].copy_from_slice(&(hex(9..13)? as u16).to_le_bytes());
    g[6..8].copy_from_slice(&(hex(14..18)? as u16).to_le_bytes());
    for i in 0..2 {
        g[8 + i] = hex(19 + i * 2..21 + i * 2)? as u8;
    }
    for i in 0..6 {
        g[10 + i] = hex(24 + i * 2..26 + i * 2)? as u8;
    }
    Some(Guid(g))
}

/// UCS-2 の名前を UTF-8 に (ucs2_as_utf8)。/ は ! に (kobject の名前と同じ)
fn utf8_name(n: &[u16]) -> String {
    n.iter().take_while(|&&c| c != 0).map(|&c| char::from_u32(c as u32).unwrap_or('\u{fffd}')).map(|c| if c == '/' { '!' } else { c }).collect()
}

struct Var {
    /// ファイルの名前 (名前-guid)
    file: String,
    /// UEFI の名前 (UCS-2、NUL つき)
    name16: Vec<u16>,
    guid: Guid,
    ino: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    immutable: bool,
    /// 4 + 中身 (まだ書いていない・消したものは 0)
    size: u64,
    mtime: u64,
    ctime: u64,
    /// 開いている数 (0 になったとき大きさが 0 なら消える: efivarfs_file_release)
    open: u32,
}

struct Fs {
    id: usize,
    vars: RefCell<Vec<Var>>,
    next_ino: Cell<u64>,
    time: u64,
}

impl Fs {
    fn with<R>(&self, ino: u64, f: impl FnOnce(&mut Var) -> R) -> Result<R, i64> {
        let mut v = self.vars.borrow_mut();
        v.iter_mut().find(|x| x.ino == ino).map(f).ok_or(-ENOENT)
    }
}

static FS: crate::cell::Bkl<Option<Rc<Fs>>> = crate::cell::Bkl::new(None);

pub struct EfiNode {
    fs: Rc<Fs>,
    /// 1 はディレクトリ (いちばん上)
    ino: u64,
}

/// mount -t efivarfs (と起動のとき fs.rs が /sys/firmware/efi/efivars に)。はじめてのときに変数を数える
pub fn new_root() -> Result<InodeRef, i64> {
    if !efirt::available() {
        return Err(-ENODEV);
    }
    if FS.get().is_none() {
        let now = crate::timer::epoch_ns();
        let fs = Rc::new(Fs { id: new_fs_id(), vars: RefCell::new(Vec::new()), next_ino: Cell::new(2), time: now });
        for (name16, guid) in efirt::variables().map_err(efirt::errno)? {
            if guid == RANDOM_SEED {
                continue;
            }
            let mut n16 = name16;
            n16.push(0);
            let size = efirt::variable_size(&n16, &guid).unwrap_or(0) as u64;
            let name = utf8_name(&n16);
            let rm = removable(&guid, name.as_bytes());
            fs.add(format!("{}-{}", name, guid_str(&guid)), n16, guid, S_IFREG | 0o644, !rm, size + 4);
        }
        *FS.get() = Some(fs);
    }
    let fs = FS.get().clone().ok_or(-ENODEV)?;
    Ok(Rc::new(EfiNode { fs, ino: 1 }))
}

impl Fs {
    fn add(&self, file: String, name16: Vec<u16>, guid: Guid, mode: u32, immutable: bool, size: u64) -> u64 {
        let ino = self.next_ino.get();
        self.next_ino.set(ino + 1);
        let now = crate::timer::epoch_ns();
        self.vars.borrow_mut().push(Var { file, name16, guid, ino, mode, uid: 0, gid: 0, immutable, size, mtime: now, ctime: now, open: 0 });
        ino
    }

    /// ファイルの名前で (名前は大文字小文字をくべつし、GUID はしない: efivarfs_d_compare)
    fn find(&self, name: &str) -> Option<u64> {
        if name.len() <= GUID_LEN {
            return None;
        }
        let g = name.len() - GUID_LEN;
        self.vars.borrow().iter().find(|v| v.file.len() == name.len() && v.file.as_bytes()[..g] == name.as_bytes()[..g] && v.file[g..].eq_ignore_ascii_case(&name[g..])).map(|v| v.ino)
    }
}

impl EfiNode {
    fn var<R>(&self, f: impl FnOnce(&mut Var) -> R) -> Result<R, i64> {
        self.fs.with(self.ino, f)
    }

    /// 変えられない印があれば EPERM (Linux の IS_IMMUTABLE)
    fn mutable(&self) -> Result<(), i64> {
        if self.var(|v| v.immutable)? { Err(-EPERM) } else { Ok(()) }
    }
}

impl Inode for EfiNode {
    fn id(&self) -> (usize, u64) {
        (self.fs.id, self.ino)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn meta(&self) -> Meta {
        if self.ino == 1 {
            let t = self.fs.time;
            return Meta { ino: 1, mode: S_IFDIR | 0o755, nlink: 2, uid: 0, gid: 0, size: 0, rdev: 0, blocks: 0, mtime: t, ctime: t };
        }
        self.var(|v| Meta { ino: v.ino, mode: v.mode, nlink: 1, uid: v.uid, gid: v.gid, size: v.size, rdev: 0, blocks: 0, mtime: v.mtime, ctime: v.ctime })
            .unwrap_or(Meta { ino: self.ino, mode: S_IFREG | 0o644, ..Meta::default() })
    }

    fn read_at(&self, off: usize, buf: &mut [u8]) -> Result<usize, i64> {
        if self.ino == 1 {
            return Err(-EISDIR);
        }
        let (name16, guid) = self.var(|v| (v.name16.clone(), v.guid))?;
        // まだない (作っただけ) なら 0 バイト
        let (attr, data) = match efirt::read_variable(&name16, &guid) {
            Ok(x) => x,
            Err(efirt::NOT_FOUND) => return Ok(0),
            Err(e) => return Err(efirt::errno(e)),
        };
        let mut all = attr.to_le_bytes().to_vec();
        all.extend_from_slice(&data);
        if off >= all.len() {
            return Ok(0);
        }
        let n = buf.len().min(all.len() - off);
        buf[..n].copy_from_slice(&all[off..off + n]);
        Ok(n)
    }

    fn write_at(&self, _off: usize, b: &[u8]) -> Result<usize, i64> {
        if self.ino == 1 {
            return Err(-EISDIR);
        }
        if b.len() < 4 {
            return Err(crate::why::inval());
        }
        let attr = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
        if attr & !ATTR_MASK != 0 {
            return Err(crate::why::inval());
        }
        let data = &b[4..];
        // 消えたもの (消したあと閉じた) には書けない
        let (name16, guid) = self.var(|v| (v.name16.clone(), v.guid)).map_err(|_| -EIO)?;
        if !validate(&guid, &name16, data) {
            return Err(crate::why::inval());
        }
        let r = efirt::set_variable(&name16, &guid, attr, data);
        if r != efirt::SUCCESS {
            // SetVariable がしくじった: ENOENT は EIO に (efivarfs_file_write)
            let e = efirt::errno(r);
            return Err(if e == -ENOENT { -EIO } else { e });
        }
        // 書いたあとの大きさ (足した、消した): GetVariable で
        let now = crate::timer::epoch_ns();
        match efirt::variable_size(&name16, &guid) {
            Ok(n) => self.var(|v| {
                v.size = n as u64 + 4;
                v.mtime = now;
                v.ctime = now;
            })?,
            Err(efirt::NOT_FOUND) => self.var(|v| v.size = 0)?,
            Err(e) => return Err(efirt::errno(e)),
        }
        Ok(b.len())
    }

    fn whole_write(&self) -> bool {
        true
    }

    fn opened(&self) {
        let _ = self.var(|v| v.open += 1);
    }

    fn released(&self) {
        let gone = self.var(|v| {
            v.open = v.open.saturating_sub(1);
            v.open == 0 && v.size == 0
        });
        if gone == Ok(true) {
            self.fs.vars.borrow_mut().retain(|v| v.ino != self.ino);
        }
    }

    fn immutable(&self) -> bool {
        self.ino != 1 && self.var(|v| v.immutable).unwrap_or(false)
    }

    fn fs_flags(&self) -> Result<u32, i64> {
        if self.ino == 1 {
            // ディレクトリには fileattr がない
            return Err(-ENOTTY);
        }
        Ok(if self.immutable() { FS_IMMUTABLE_FL } else { 0 })
    }

    fn set_fs_flags(&self, flags: u32) -> Result<(), i64> {
        if self.ino == 1 {
            return Err(-ENOTTY);
        }
        if flags & !FS_IMMUTABLE_FL != 0 {
            return Err(-EOPNOTSUPP);
        }
        self.var(|v| v.immutable = flags & FS_IMMUTABLE_FL != 0)
    }

    fn truncate(&self, _: usize) -> Result<(), i64> {
        if self.ino == 1 {
            return Err(-EISDIR);
        }
        // 大きさは変えない (efivarfs_setattr)
        self.mutable()
    }

    fn readlink(&self) -> Result<String, i64> {
        Err(crate::why::inval())
    }

    fn lookup(&self, name: &str) -> Result<InodeRef, i64> {
        if self.ino != 1 {
            return Err(-ENOTDIR);
        }
        let ino = self.fs.find(name).ok_or(-ENOENT)?;
        Ok(Rc::new(EfiNode { fs: self.fs.clone(), ino }))
    }

    fn readdir(&self) -> Result<Vec<DirEntry>, i64> {
        if self.ino != 1 {
            return Err(-ENOTDIR);
        }
        Ok(self.fs.vars.borrow().iter().map(|v| DirEntry { name: v.file.clone(), ino: v.ino, mode: v.mode }).collect())
    }

    fn create(&self, name: &str, mode: u32, node: NewNode) -> Result<InodeRef, i64> {
        if self.ino != 1 {
            return Err(-ENOTDIR);
        }
        // ふつうのファイルのほか (mkdir、symlink、mknod) はない
        if !matches!(node, NewNode::File) {
            return Err(-EPERM);
        }
        if self.fs.find(name).is_some() {
            return Err(-EEXIST);
        }
        // 名前-GUID (名前は 1 文字から)
        if name.len() < GUID_LEN + 2 || name.as_bytes()[name.len() - GUID_LEN - 1] != b'-' {
            return Err(crate::why::inval());
        }
        let n = name.len() - GUID_LEN - 1;
        let guid = parse_guid(&name[n + 1..]).ok_or_else(crate::why::inval)?;
        if guid == RANDOM_SEED {
            return Err(-EPERM);
        }
        // Linux は名前のバイトをそのまま 1 文字ずつ UCS-2 に
        let mut name16: Vec<u16> = name.as_bytes()[..n].iter().map(|&c| c as u16).collect();
        name16.push(0);
        let rm = removable(&guid, &name.as_bytes()[..n]);
        let ino = self.fs.add(name.into(), name16, guid, S_IFREG | (mode & 0o7777), !rm, 0);
        Ok(Rc::new(EfiNode { fs: self.fs.clone(), ino }))
    }

    fn link(&self, _: &str, _: &InodeRef) -> Result<(), i64> {
        Err(-EPERM)
    }

    fn unlink(&self, name: &str, rmdir: bool) -> Result<(), i64> {
        if self.ino != 1 {
            return Err(-ENOTDIR);
        }
        let ino = self.fs.find(name).ok_or(-ENOENT)?;
        if rmdir {
            return Err(-ENOTDIR);
        }
        let child = EfiNode { fs: self.fs.clone(), ino };
        child.mutable()?;
        let (name16, guid) = child.var(|v| (v.name16.clone(), v.guid))?;
        // ないもの (書いていないもの) を消すのはかまわない
        let r = efirt::set_variable(&name16, &guid, 0, &[]);
        if r != efirt::SUCCESS && r != efirt::NOT_FOUND {
            return Err(crate::why::inval());
        }
        self.fs.vars.borrow_mut().retain(|v| v.ino != ino);
        Ok(())
    }

    fn rename(&self, _: &str, _: &InodeRef, _: &str) -> Result<(), i64> {
        Err(-EPERM)
    }

    fn set_mode(&self, mode: u32) -> Result<(), i64> {
        if self.ino == 1 {
            return Err(-EPERM);
        }
        self.mutable()?;
        let now = crate::timer::epoch_ns();
        self.var(|v| {
            v.mode = (v.mode & S_IFMT) | (mode & 0o7777);
            v.ctime = now;
        })
    }

    fn set_owner(&self, uid: Option<u32>, gid: Option<u32>) -> Result<(), i64> {
        if self.ino == 1 {
            return Err(-EPERM);
        }
        self.mutable()?;
        let now = crate::timer::epoch_ns();
        self.var(|v| {
            if let Some(u) = uid {
                v.uid = u;
            }
            if let Some(g) = gid {
                v.gid = g;
            }
            v.ctime = now;
        })
    }

    fn set_mtime(&self, ns: u64) -> Result<(), i64> {
        if self.ino == 1 {
            return Err(-EPERM);
        }
        self.mutable()?;
        self.var(|v| v.mtime = ns)
    }

    fn statfs(&self) -> [u8; 120] {
        let (total, left) = efirt::query_variable_info(ATTR_NV_BS_RT).map(|(t, l, _)| (t, l)).unwrap_or((0, 0));
        crate::tmpfs::statfs_bytes(EFIVARFS_MAGIC, 1, total, left, 0, 0)
    }
}

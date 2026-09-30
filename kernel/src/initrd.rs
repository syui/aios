// ビルド時に埋め込んだ initramfs (cpio newc)。読み取り専用のファイル置き場
static IMAGE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/initrd.cpio"));

pub const S_IFMT: u32 = 0o170000;
pub const S_IFDIR: u32 = 0o040000;
pub const S_IFLNK: u32 = 0o120000;

#[derive(Clone, Copy)]
pub struct Entry {
    /// 先頭の / を除いたパス。ルートは ""
    pub name: &'static str,
    pub ino: u32,
    pub mode: u32,
    pub mtime: u32,
    pub data: &'static [u8],
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.mode & S_IFMT == S_IFDIR
    }
    pub fn is_symlink(&self) -> bool {
        self.mode & S_IFMT == S_IFLNK
    }
}

const ROOT: Entry = Entry { name: "", ino: 1, mode: S_IFDIR | 0o755, mtime: 0, data: &[] };

fn hex(b: &[u8]) -> usize {
    b.iter().fold(0, |n, &c| n * 16 + (c as char).to_digit(16).unwrap_or(0) as usize)
}

fn align4(n: usize) -> usize {
    (n + 3) & !3
}

pub fn entries() -> impl Iterator<Item = Entry> {
    let mut off = 0;
    core::iter::from_fn(move || {
        if off + 110 > IMAGE.len() || &IMAGE[off..off + 6] != b"070701" {
            return None;
        }
        let h = &IMAGE[off..off + 110];
        let field = |i: usize| hex(&h[6 + i * 8..6 + (i + 1) * 8]);
        let (ino, mode, mtime, size, namesize) = (field(0), field(1), field(5), field(6), field(11));
        let name_start = off + 110;
        let name = core::str::from_utf8(&IMAGE[name_start..name_start + namesize - 1]).ok()?;
        if name == "TRAILER!!!" {
            return None;
        }
        let data_start = align4(name_start + namesize);
        off = align4(data_start + size);
        Some(Entry {
            name,
            ino: ino as u32 + 1,
            mode: mode as u32,
            mtime: mtime as u32,
            data: &IMAGE[data_start..data_start + size],
        })
    })
}

/// 正規化済みのパス (先頭 / なし) をそのまま探す。リンクはたどらない
pub fn lookup(path: &str) -> Option<Entry> {
    if path.is_empty() {
        return Some(ROOT);
    }
    entries().find(|e| e.name == path)
}

/// dir の直下にあるもの
pub fn children(dir: &'static str) -> impl Iterator<Item = Entry> {
    entries().filter(move |e| match e.name.rsplit_once('/') {
        Some((parent, _)) => parent == dir,
        None => dir.is_empty(),
    })
}

pub fn count() -> usize {
    entries().count()
}

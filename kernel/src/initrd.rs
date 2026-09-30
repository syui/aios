// ビルド時に埋め込んだ initramfs (cpio newc) を読むだけのファイル置き場
static IMAGE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/initrd.cpio"));

const S_IFMT: u32 = 0o170000;
const S_IFLNK: u32 = 0o120000;

pub struct Entry {
    pub name: &'static str,
    pub mode: u32,
    pub data: &'static [u8],
}

fn hex(b: &[u8]) -> usize {
    b.iter().fold(0, |n, &c| n * 16 + (c as char).to_digit(16).unwrap_or(0) as usize)
}

fn align4(n: usize) -> usize {
    (n + 3) & !3
}

fn entries() -> impl Iterator<Item = Entry> {
    let mut off = 0;
    core::iter::from_fn(move || {
        if off + 110 > IMAGE.len() || &IMAGE[off..off + 6] != b"070701" {
            return None;
        }
        let h = &IMAGE[off..off + 110];
        let field = |i: usize| hex(&h[6 + i * 8..6 + (i + 1) * 8]);
        let mode = field(1) as u32;
        let size = field(6);
        let namesize = field(11);
        let name_start = off + 110;
        let name = core::str::from_utf8(&IMAGE[name_start..name_start + namesize - 1]).ok()?;
        if name == "TRAILER!!!" {
            return None;
        }
        let data_start = align4(name_start + namesize);
        off = align4(data_start + size);
        Some(Entry { name, mode, data: &IMAGE[data_start..data_start + size] })
    })
}

fn lookup(path: &str) -> Option<Entry> {
    let path = path.trim_start_matches('/');
    entries().find(|e| e.name == path)
}

/// path のファイルの中身。シンボリックリンクはたどる
pub fn read(path: &str) -> Option<&'static [u8]> {
    let mut buf = [0u8; 256];
    let mut len = put(&mut buf, 0, path.as_bytes())?;
    for _ in 0..8 {
        let cur = core::str::from_utf8(&buf[..len]).ok()?;
        let e = lookup(cur)?;
        if e.mode & S_IFMT != S_IFLNK {
            return Some(e.data);
        }
        let mut next = [0u8; 256];
        len = if e.data.first() == Some(&b'/') {
            put(&mut next, 0, e.data)?
        } else {
            let dir = cur.rfind('/').map(|i| &cur[..=i]).unwrap_or("");
            let n = put(&mut next, 0, dir.as_bytes())?;
            put(&mut next, n, e.data)?
        };
        buf = next;
    }
    None
}

fn put(dst: &mut [u8; 256], at: usize, src: &[u8]) -> Option<usize> {
    let end = at + src.len();
    dst.get_mut(at..end)?.copy_from_slice(src);
    Some(end)
}

pub fn count() -> usize {
    entries().count()
}

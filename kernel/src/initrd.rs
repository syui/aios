// ビルド時に埋め込んだ initramfs (cpio newc)。起動時に fs が tmpfs へ展開する
static IMAGE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/initrd.cpio"));

pub struct Entry {
    /// 先頭の / を除いたパス
    pub name: &'static str,
    pub mode: u32,
    pub mtime: u32,
    pub data: &'static [u8],
}

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
        let (mode, mtime, size, namesize) = (field(1), field(5), field(6), field(11));
        let name_start = off + 110;
        let name = core::str::from_utf8(&IMAGE[name_start..name_start + namesize - 1]).ok()?;
        if name == "TRAILER!!!" {
            return None;
        }
        let data_start = align4(name_start + namesize);
        off = align4(data_start + size);
        Some(Entry {
            name,
            mode: mode as u32,
            mtime: mtime as u32,
            data: &IMAGE[data_start..data_start + size],
        })
    })
}

pub fn count() -> usize {
    entries().count()
}

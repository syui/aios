// ディスク: virtio-blk (QEMU virt) か SD カード (ラズパイ)。パーティション表を読んで、
//   root: ext の区画 (GPT の Linux root / Linux filesystem、MBR の 0x83)
//   boot: FAT の区画 (GPT の EFI System Partition、MBR の 0x0b / 0x0c / 0x0e / 0xef)
// を決める。root= で root の区画を選べる (systemd-boot などの loader entry と同じ書き方):
//   root=/dev/vda2, root=/dev/mmcblk0p2      番号
//   root=PARTUUID=<GPT の区画の GUID>        MBR なら PARTUUID=<ディスクの署名 8 桁>-<番号 2 桁>
//   root=UUID=<ext の UUID>                  (blkid の UUID)
// 表がなく、ディスク全体が ext なら、それが root。
use crate::{sd, virtio_blk};
use alloc::format;
use alloc::string::String;

pub const SECTOR: usize = 512;

#[derive(Clone, Copy, PartialEq)]
enum Dev {
    Virtio,
    Sd,
}

/// 区画 (始まりのセクタ、セクタ数、番号。番号 0 はディスク全体)
#[derive(Clone, Copy)]
pub struct Part {
    pub start: u64,
    pub len: u64,
    pub num: usize,
}

static mut DEV: Option<Dev> = None;
static mut ROOT: Part = Part { start: 0, len: u64::MAX, num: 0 };
static mut BOOT: Option<Part> = None;

fn dev() -> Result<Dev, i64> {
    unsafe { DEV }.ok_or(-6)
}

fn raw_read(d: Dev, sector: u64, buf: &mut [u8]) -> Result<(), i64> {
    match d {
        Dev::Virtio => virtio_blk::read(sector, buf),
        Dev::Sd => sd::read(sector, buf),
    }
}

fn raw_write(d: Dev, sector: u64, buf: &[u8]) -> Result<(), i64> {
    match d {
        Dev::Virtio => virtio_blk::write(sector, buf),
        Dev::Sd => sd::write(sector, buf),
    }
}

/// 区画 p の中の sector から読む
pub fn read_part(p: &Part, sector: u64, buf: &mut [u8]) -> Result<(), i64> {
    if sector + (buf.len() / SECTOR) as u64 > p.len {
        return Err(-5);
    }
    raw_read(dev()?, p.start + sector, buf)
}

pub fn write_part(p: &Part, sector: u64, buf: &[u8]) -> Result<(), i64> {
    if sector + (buf.len() / SECTOR) as u64 > p.len {
        return Err(-5);
    }
    raw_write(dev()?, p.start + sector, buf)
}

/// root の区画から読む (extfs)
pub fn read(sector: u64, buf: &mut [u8]) -> Result<(), i64> {
    read_part(&root(), sector, buf)
}

pub fn write(sector: u64, buf: &[u8]) -> Result<(), i64> {
    write_part(&root(), sector, buf)
}

pub fn root() -> Part {
    unsafe { ROOT }
}

pub fn boot() -> Option<Part> {
    unsafe { BOOT }
}

/// 区画の名前 (/dev/vda2 など)
pub fn part_name(p: &Part) -> String {
    let d = unsafe { DEV };
    match (d, p.num) {
        (Some(Dev::Virtio), 0) => "/dev/vda".into(),
        (Some(Dev::Virtio), n) => format!("/dev/vda{}", n),
        (Some(Dev::Sd), 0) => "/dev/mmcblk0".into(),
        (Some(Dev::Sd), n) => format!("/dev/mmcblk0p{}", n),
        (None, _) => "none".into(),
    }
}

/// root の区画の名前 (/proc/mounts などに)
pub fn name() -> String {
    part_name(&root())
}

/// 16 進の文字列 (- は飛ばす) を 16 バイトに。足りなければ None
fn hex16(s: &str) -> Option<[u8; 16]> {
    let digits: alloc::vec::Vec<u8> = s.bytes().filter(|&c| c != b'-').collect();
    if digits.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(core::str::from_utf8(&digits[i * 2..i * 2 + 2]).ok()?, 16).ok()?;
    }
    Some(out)
}

/// GUID の並び (始めの 3 つは little endian) と文字列の並びを入れかえる (どちら向きにも使える)
fn swap_guid(r: [u8; 16]) -> [u8; 16] {
    [r[3], r[2], r[1], r[0], r[5], r[4], r[7], r[6], r[8], r[9], r[10], r[11], r[12], r[13], r[14], r[15]]
}

/// root= で選ぶもの
enum Want {
    Num(usize),
    /// GPT の区画の GUID (ディスク上の並び)
    PartGuid([u8; 16]),
    /// MBR: ディスクの署名と番号
    PartMbr(u32, usize),
    /// ext の UUID
    FsUuid([u8; 16]),
}

fn parse_root(r: &str) -> Option<Want> {
    if let Some(u) = r.strip_prefix("PARTUUID=") {
        if let Some((sig, n)) = u.split_once('-').filter(|(a, _)| a.len() == 8 && u.len() == 11) {
            return Some(Want::PartMbr(u32::from_str_radix(sig, 16).ok()?, usize::from_str_radix(n, 16).ok()?));
        }
        return hex16(u).map(|g| Want::PartGuid(swap_guid(g)));
    }
    if let Some(u) = r.strip_prefix("UUID=") {
        return hex16(u).map(Want::FsUuid);
    }
    // 最後の数字 (vda2 の 2、mmcblk0p2 の 2)
    let digits = r.len() - r.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    let n = &r[r.len() - digits..];
    (r.contains('p') || r.starts_with("/dev/vd") || r.starts_with("/dev/sd")).then(|| n.parse::<usize>().ok().map(Want::Num)).flatten()
}

/// 区画の中の ext の UUID
fn fs_uuid(d: Dev, p: &Part) -> Option<[u8; 16]> {
    let mut sb = [0u8; SECTOR * 2];
    raw_read(d, p.start + 2, &mut sb).ok()?;
    (u16::from_le_bytes([sb[56], sb[57]]) == 0xef53).then(|| sb[104..120].try_into().unwrap())
}

/// GUID の文字列 (xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx) をディスク上の並びに
const fn guid(s: &[u8; 36]) -> [u8; 16] {
    const fn hex(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            _ => c - b'A' + 10,
        }
    }
    let mut raw = [0u8; 16];
    let mut i = 0;
    let mut j = 0;
    while i < 36 {
        if s[i] == b'-' {
            i += 1;
            continue;
        }
        raw[j] = hex(s[i]) << 4 | hex(s[i + 1]);
        i += 2;
        j += 1;
    }
    // 始めの 3 つは little endian
    [raw[3], raw[2], raw[1], raw[0], raw[5], raw[4], raw[7], raw[6], raw[8], raw[9], raw[10], raw[11], raw[12], raw[13], raw[14], raw[15]]
}

const GPT_ESP: [u8; 16] = guid(b"C12A7328-F81F-11D2-BA4B-00A0C93EC93B");
const GPT_ROOT_ARM64: [u8; 16] = guid(b"B921B045-1DF0-41C3-AF44-4C6F280D3FAE");
const GPT_LINUX_FS: [u8; 16] = guid(b"0FC63DAF-8483-4772-8E79-3D69D8477DE4");

/// ディスクを見つけ、root と boot の区画を決める
pub fn init() -> bool {
    let d = if virtio_blk::init() {
        Dev::Virtio
    } else if sd::init() {
        Dev::Sd
    } else {
        return false;
    };
    unsafe { DEV = Some(d) };
    let want = crate::dtb::arg("root").and_then(parse_root);
    let mut head = [0u8; SECTOR * 4];
    if raw_read(d, 0, &mut head).is_err() {
        return true;
    }
    // ディスク全体が ext なら区画はない
    if u16::from_le_bytes([head[1024 + 56], head[1024 + 57]]) == 0xef53 && want.is_none() {
        return true;
    }
    if head[510] != 0x55 || head[511] != 0xaa {
        return true;
    }
    let mut root: Option<Part> = None;
    let mut boot: Option<Part> = None;
    // id: GPT なら区画の GUID、MBR なら (署名, 番号) で照らし合わせる
    let sig = u32::from_le_bytes(head[440..444].try_into().unwrap());
    let mut pick = |p: Part, guid: Option<[u8; 16]>, is_root: bool, is_boot: bool| {
        let chosen = match &want {
            Some(Want::Num(n)) => *n == p.num,
            Some(Want::PartGuid(g)) => guid == Some(*g),
            Some(Want::PartMbr(s, n)) => guid.is_none() && *s == sig && *n == p.num,
            Some(Want::FsUuid(u)) => fs_uuid(d, &p) == Some(*u),
            None => is_root,
        };
        if chosen && root.is_none() {
            root = Some(p);
        }
        if is_boot && boot.is_none() {
            boot = Some(p);
        }
    };
    if head[446 + 4] == 0xee {
        // GPT (保護用の MBR の後ろ、LBA 1 にヘッダー)
        let h = &head[SECTOR..SECTOR * 2];
        if &h[0..8] != b"EFI PART" {
            return true;
        }
        let lba = u64::from_le_bytes(h[72..80].try_into().unwrap());
        let count = u32::from_le_bytes(h[80..84].try_into().unwrap()) as usize;
        let esize = u32::from_le_bytes(h[84..88].try_into().unwrap()) as usize;
        if esize < 128 || count > 256 {
            return true;
        }
        let mut table = alloc::vec![0u8; (count * esize).div_ceil(SECTOR) * SECTOR];
        if raw_read(d, lba, &mut table).is_err() {
            return true;
        }
        for i in 0..count {
            let e = &table[i * esize..i * esize + 128];
            let ty: [u8; 16] = e[0..16].try_into().unwrap();
            if ty == [0; 16] {
                continue;
            }
            let first = u64::from_le_bytes(e[32..40].try_into().unwrap());
            let last = u64::from_le_bytes(e[40..48].try_into().unwrap());
            let p = Part { start: first, len: last + 1 - first, num: i + 1 };
            pick(p, Some(e[16..32].try_into().unwrap()), ty == GPT_ROOT_ARM64 || ty == GPT_LINUX_FS, ty == GPT_ESP);
        }
    } else {
        for i in 0..4 {
            let e = &head[446 + i * 16..446 + i * 16 + 16];
            let ty = e[4];
            let start = u32::from_le_bytes(e[8..12].try_into().unwrap()) as u64;
            let len = u32::from_le_bytes(e[12..16].try_into().unwrap()) as u64;
            if ty == 0 || start == 0 {
                continue;
            }
            pick(Part { start, len, num: i + 1 }, None, ty == 0x83, matches!(ty, 0x0b | 0x0c | 0x0e | 0xef));
        }
    }
    if root.is_none() && want.is_some() {
        println!("block: {} not found", crate::dtb::arg("root").unwrap_or(""));
    }
    if let Some(r) = root {
        println!("block: root {} (sector {})", part_name(&r), r.start);
        unsafe { ROOT = r };
    }
    if let Some(b) = boot {
        println!("block: boot {} (sector {})", part_name(&b), b.start);
        unsafe { BOOT = Some(b) };
    }
    true
}

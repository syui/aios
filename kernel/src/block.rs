// ディスク: virtio-blk (QEMU virt) か SD カード (ラズパイ)。パーティション表を読んで、
//   root: ext の区画 (GPT の Linux root / Linux filesystem、MBR の 0x83)
//   boot: FAT の区画 (GPT の EFI System Partition、MBR の 0x0b / 0x0c / 0x0e / 0xef)
// を決める。root=/dev/vda2 や root=/dev/mmcblk0p2 で root の区画の番号を選べる。
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
    // root=... の最後の数字 (vda2 の 2、mmcblk0p2 の 2) が root の区画の番号
    let want = crate::dtb::arg("root").and_then(|r| {
        let digits = r.len() - r.trim_end_matches(|c: char| c.is_ascii_digit()).len();
        let n = &r[r.len() - digits..];
        (r.contains('p') || r.starts_with("/dev/vd") || r.starts_with("/dev/sd")).then(|| n.parse::<usize>().ok()).flatten()
    });
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
    let mut pick = |p: Part, is_root: bool, is_boot: bool| {
        let chosen = match want {
            Some(n) => n == p.num,
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
            pick(p, ty == GPT_ROOT_ARM64 || ty == GPT_LINUX_FS, ty == GPT_ESP);
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
            pick(Part { start, len, num: i + 1 }, ty == 0x83, matches!(ty, 0x0b | 0x0c | 0x0e | 0xef));
        }
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

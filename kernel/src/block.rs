// ルートにするディスク: virtio-blk (QEMU virt) か SD カード (ラズパイ)。
// MBR のパーティション表があれば、その中の Linux (0x83) の区画を使う
// (ラズパイの SD は 1 つ目が FAT の boot、2 つ目が root)。
// root=/dev/mmcblk0p2 や root=/dev/vda1 のように区画の番号を選ぶこともできる。
use crate::{sd, virtio_blk};
use alloc::format;
use alloc::string::String;

pub const SECTOR: usize = 512;

#[derive(Clone, Copy, PartialEq)]
enum Dev {
    Virtio,
    Sd,
}

static mut DEV: Option<Dev> = None;
/// 区画の始まり (セクタ) と番号 (0 なら区画なしでディスク全体)
static mut START: u64 = 0;
static mut PART: usize = 0;

fn dev() -> Result<Dev, i64> {
    unsafe { DEV }.ok_or(-6)
}

fn raw_read(d: Dev, sector: u64, buf: &mut [u8]) -> Result<(), i64> {
    match d {
        Dev::Virtio => virtio_blk::read(sector, buf),
        Dev::Sd => sd::read(sector, buf),
    }
}

pub fn read(sector: u64, buf: &mut [u8]) -> Result<(), i64> {
    raw_read(dev()?, sector + unsafe { START }, buf)
}

pub fn write(sector: u64, buf: &[u8]) -> Result<(), i64> {
    let s = sector + unsafe { START };
    match dev()? {
        Dev::Virtio => virtio_blk::write(s, buf),
        Dev::Sd => sd::write(s, buf),
    }
}

/// /proc/mounts などに出す名前
pub fn name() -> String {
    let (d, p) = unsafe { (DEV, PART) };
    match (d, p) {
        (Some(Dev::Virtio), 0) => "/dev/vda".into(),
        (Some(Dev::Virtio), p) => format!("/dev/vda{}", p),
        (Some(Dev::Sd), 0) => "/dev/mmcblk0".into(),
        (Some(Dev::Sd), p) => format!("/dev/mmcblk0p{}", p),
        (None, _) => "none".into(),
    }
}

/// ディスクを見つけ、ルートにする区画を決める
pub fn init() -> bool {
    let d = if virtio_blk::init() {
        Dev::Virtio
    } else if sd::init() {
        Dev::Sd
    } else {
        return false;
    };
    unsafe { DEV = Some(d) };
    // root=... の最後の数字 (p2 や vda1 の 1) が区画の番号
    let want = crate::dtb::arg("root").and_then(|r| {
        let digits = r.len() - r.trim_end_matches(|c: char| c.is_ascii_digit()).len();
        let n = &r[r.len() - digits..];
        (r.contains('p') || r.starts_with("/dev/vd") || r.starts_with("/dev/sd")).then(|| n.parse::<usize>().ok()).flatten()
    });
    let mut mbr = [0u8; SECTOR * 4];
    if raw_read(d, 0, &mut mbr).is_err() {
        return true;
    }
    // ディスク全体が ext なら区画はない
    if u16::from_le_bytes([mbr[1024 + 56], mbr[1024 + 57]]) == 0xef53 && want.is_none() {
        return true;
    }
    if mbr[510] != 0x55 || mbr[511] != 0xaa {
        return true;
    }
    for i in 0..4 {
        let e = &mbr[446 + i * 16..446 + i * 16 + 16];
        let ty = e[4];
        let start = u32::from_le_bytes(e[8..12].try_into().unwrap()) as u64;
        let pick = match want {
            Some(n) => n == i + 1,
            None => ty == 0x83,
        };
        if pick && ty != 0 && start != 0 {
            unsafe {
                START = start;
                PART = i + 1;
            }
            println!("block: partition {} (type {:#x}) at sector {}", i + 1, ty, start);
            return true;
        }
    }
    true
}

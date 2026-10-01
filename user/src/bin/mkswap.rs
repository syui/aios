// mkswap: ディスクの区画 (かファイル) をスワップの形にする (Linux と同じ形、version 1)
//   mkswap [-L LABEL] DEVICE
// ページ 0 に見出しを書くだけ (残りはそのまま)。使うには swapon DEVICE
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;

const PAGE: usize = 4096;

fn main() {
    let mut label = String::new();
    let mut dev = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-L" => label = args.next().unwrap_or_default(),
            _ if dev.is_none() => dev = Some(a),
            _ => usage(),
        }
    }
    let Some(dev) = dev else { usage() };
    let mut f = OpenOptions::new().read(true).write(true).open(&dev).unwrap_or_else(|e| die(&dev, e));
    // 大きさ: ブロックデバイスは BLKGETSIZE64、ファイルは長さ
    let mut size: u64 = 0;
    const BLKGETSIZE64: u64 = 0x80081272;
    if unsafe { libc::ioctl(f.as_raw_fd(), BLKGETSIZE64 as _, &mut size) } != 0 || size == 0 {
        size = f.seek(SeekFrom::End(0)).unwrap_or(0);
    }
    let pages = size as usize / PAGE;
    if pages < 10 {
        eprintln!("mkswap: {}: too small ({} bytes)", dev, size);
        std::process::exit(1);
    }
    let pages = pages.min(u32::MAX as usize);
    let mut uuid = [0u8; 16];
    if let Ok(mut r) = std::fs::File::open("/dev/urandom") {
        let _ = r.read_exact(&mut uuid);
    }
    // RFC 4122 の version 4
    uuid[6] = uuid[6] & 0x0f | 0x40;
    uuid[8] = uuid[8] & 0x3f | 0x80;
    let mut h = vec![0u8; PAGE];
    h[1024..1028].copy_from_slice(&1u32.to_le_bytes()); // version
    h[1028..1032].copy_from_slice(&((pages - 1) as u32).to_le_bytes()); // last_page
    h[1032..1036].copy_from_slice(&0u32.to_le_bytes()); // nr_badpages
    h[1036..1052].copy_from_slice(&uuid);
    let l = label.as_bytes();
    h[1052..1052 + l.len().min(16)].copy_from_slice(&l[..l.len().min(16)]);
    h[PAGE - 10..].copy_from_slice(b"SWAPSPACE2");
    f.seek(SeekFrom::Start(0)).and_then(|_| f.write_all(&h)).and_then(|_| f.sync_all()).unwrap_or_else(|e| die(&dev, e));
    let kib = (pages - 1) * PAGE / 1024;
    println!("Setting up swapspace version 1, size = {} KiB ({} bytes)", kib, kib * 1024);
    let u: String = uuid.iter().enumerate().map(|(i, b)| format!("{}{:02x}", if [4, 6, 8, 10].contains(&i) { "-" } else { "" }, b)).collect();
    if label.is_empty() {
        println!("no label, UUID={}", u);
    } else {
        println!("LABEL={}, UUID={}", label, u);
    }
}

fn usage() -> ! {
    eprintln!("usage: mkswap [-L label] device");
    std::process::exit(2);
}

fn die(dev: &str, e: std::io::Error) -> ! {
    eprintln!("mkswap: {}: {}", dev, e);
    std::process::exit(1);
}

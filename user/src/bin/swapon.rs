// swapon / swapoff: スワップを使いはじめる / やめる (どちらもこのプログラム。名前で決める)
//   swapon DEVICE...      swapon -a   /etc/fstab の swap の行をぜんぶ (使っているものと、ないものは飛ばす)
//   swapon [-s|--show]    使っているスワップ (/proc/swaps)
//   swapoff DEVICE...     swapoff -a  使っているものをぜんぶ
use std::ffi::CString;

fn main() {
    let argv0 = std::env::args().next().unwrap_or_default();
    let off = argv0.rsplit('/').next() == Some("swapoff");
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut all = false;
    let mut devs = Vec::new();
    for a in &args {
        match a.as_str() {
            "-a" | "--all" => all = true,
            "-s" | "--show" | "--summary" if !off => return show(),
            "-h" | "--help" => usage(off),
            _ if a.starts_with('-') => usage(off),
            _ => devs.push(a.clone()),
        }
    }
    let active = active();
    if all {
        if off {
            devs.extend(active.iter().cloned());
        } else {
            // ないデバイス (ディスクなしで起動したときなど) は飛ばす
            devs.extend(fstab().into_iter().filter(|d| !active.contains(d) && std::path::Path::new(d).exists()));
        }
    } else if devs.is_empty() {
        if off {
            usage(off);
        }
        return show();
    }
    let mut fail = false;
    for d in &devs {
        let c = CString::new(d.as_str()).unwrap();
        let r = unsafe { if off { libc::swapoff(c.as_ptr()) } else { libc::swapon(c.as_ptr(), 0) } };
        if r != 0 {
            eprintln!("{}: {}: {}", if off { "swapoff" } else { "swapon" }, d, std::io::Error::last_os_error());
            fail = true;
        }
    }
    std::process::exit(fail as i32);
}

/// /proc/swaps の名前
fn active() -> Vec<String> {
    let s = std::fs::read_to_string("/proc/swaps").unwrap_or_default();
    s.lines().skip(1).filter_map(|l| l.split_whitespace().next().map(String::from)).collect()
}

/// /etc/fstab の swap の行の device (UUID= などは見ない)
fn fstab() -> Vec<String> {
    let s = std::fs::read_to_string("/etc/fstab").unwrap_or_default();
    s.lines()
        .map(|l| l.split('#').next().unwrap_or(""))
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            (f.len() >= 3 && f[2] == "swap" && !f.get(3).is_some_and(|o| o.split(',').any(|o| o == "noauto"))).then(|| f[0].to_string())
        })
        .collect()
}

fn show() {
    let s = std::fs::read_to_string("/proc/swaps").unwrap_or_default();
    if s.lines().count() > 1 {
        print!("{}", s);
    }
}

fn usage(off: bool) -> ! {
    if off {
        eprintln!("usage: swapoff -a | swapoff device...");
    } else {
        eprintln!("usage: swapon [-a] [-s] [device...]");
    }
    std::process::exit(2);
}

// モジュール (カーネルの中で眠っているドライバ) を起こす: modprobe と init が使う
#![allow(dead_code)]
//   札は /usr/lib/modules/NAME.ko (unix パッケージ)。起動のときに起こすものは /etc/modules-load.d/*.conf
use std::ffi::CString;
use std::fs;
use std::io;

pub const DIR: &str = "/usr/lib/modules";
pub const LOAD_D: &str = "/etc/modules-load.d";

/// NAME を起こす。もう起きていれば Ok
pub fn load(name: &str) -> io::Result<()> {
    let path = format!("{}/{}.ko", DIR, name.replace('-', "_"));
    let f = fs::File::open(&path).map_err(|e| io::Error::new(e.kind(), format!("{}: {}", path, e)))?;
    use std::os::fd::AsRawFd;
    let r = unsafe { libc::syscall(libc::SYS_finit_module, f.as_raw_fd(), c"".as_ptr(), 0) };
    if r == 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::EEXIST) {
        return Ok(());
    }
    if e.raw_os_error() == Some(libc::ENODEV) {
        // QEMU に装置がない (画面とキーボード・マウスは bin/run.sh を AIOS_DISPLAY=1 で起動したときだけ)
        let hint = if name.starts_with("virtio_gpu") || name.starts_with("virtio_input") { " (QEMU: AIOS_DISPLAY=1 bin/run.sh)" } else { "" };
        return Err(io::Error::other(format!("no such device{}", hint)));
    }
    Err(e)
}

pub fn unload(name: &str) -> io::Result<()> {
    let c = CString::new(name.replace('-', "_")).map_err(|_| io::Error::other("bad name"))?;
    if unsafe { libc::syscall(libc::SYS_delete_module, c.as_ptr(), 0) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// /etc/modules-load.d/*.conf に書いてある名前 (# から行末はコメント)
pub fn boot_list() -> Vec<String> {
    let mut files: Vec<_> = fs::read_dir(LOAD_D).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "conf")).collect();
    files.sort();
    let mut out = vec![];
    for f in files {
        for line in fs::read_to_string(&f).unwrap_or_default().lines() {
            let l = line.split('#').next().unwrap_or("").trim();
            if !l.is_empty() && !out.iter().any(|x: &String| x == l) {
                out.push(l.to_string());
            }
        }
    }
    out
}

/// 起きているもの (/proc/modules の名前)
pub fn loaded() -> Vec<String> {
    fs::read_to_string("/proc/modules").unwrap_or_default().lines().filter_map(|l| l.split_whitespace().next().map(String::from)).collect()
}

// kernel.ld を渡し、ワークスペース直下の rootfs/ を initramfs (cpio newc) に固める
//   AIOS_INITRD=none   initramfs を空にする (パッケージのカーネル。ルートはディスク)
//   AIOS_INITRD=DIR    DIR を initramfs にする
//   AIOS_RELEASE=VER   uname -r に出す版 (なければ Cargo の版)
use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

fn main() {
    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    println!("cargo:rustc-link-arg=-T{}", dir.join("kernel.ld").display());
    println!("cargo:rerun-if-changed=kernel.ld");

    println!("cargo:rerun-if-env-changed=AIOS_INITRD");
    println!("cargo:rerun-if-env-changed=AIOS_RELEASE");
    let release = std::env::var("AIOS_RELEASE").unwrap_or_else(|_| std::env::var("CARGO_PKG_VERSION").unwrap());
    println!("cargo:rustc-env=AIOS_RELEASE={}", release);
    // RTC のない機械 (ラズパイ) の時計の始まり
    let epoch = std::env::var("SOURCE_DATE_EPOCH").ok().unwrap_or_else(|| {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()).to_string()
    });
    println!("cargo:rustc-env=AIOS_BUILD_EPOCH={}", epoch);
    println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");

    let initrd = std::env::var("AIOS_INITRD").ok();
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("initrd.cpio");
    let mut cpio = Cpio::default();
    if initrd.as_deref() != Some("none") {
        let rootfs = initrd.map_or_else(|| dir.join("../rootfs"), PathBuf::from);
        println!("cargo:rerun-if-changed={}", rootfs.display());
        if rootfs.is_dir() {
            cpio.add_tree(&rootfs, "");
        }
    }
    fs::write(out, cpio.finish()).unwrap();
}

#[derive(Default)]
struct Cpio {
    buf: Vec<u8>,
    ino: u32,
}

impl Cpio {
    fn add_tree(&mut self, root: &Path, prefix: &str) {
        let mut entries: Vec<_> = fs::read_dir(root).unwrap().map(|e| e.unwrap()).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = format!("{}{}", prefix, e.file_name().to_string_lossy());
            let path = e.path();
            let meta = fs::symlink_metadata(&path).unwrap();
            let perm = meta.permissions().mode() & 0o7777;
            if meta.file_type().is_symlink() {
                let target = fs::read_link(&path).unwrap();
                self.entry(&name, 0o120000 | 0o777, target.to_string_lossy().as_bytes(), meta.mtime());
            } else if meta.is_dir() {
                self.entry(&name, 0o040000 | perm, &[], meta.mtime());
                self.add_tree(&path, &format!("{}/", name));
            } else if meta.is_file() {
                self.entry(&name, 0o100000 | perm, &fs::read(&path).unwrap(), meta.mtime());
            }
        }
    }

    fn entry(&mut self, name: &str, mode: u32, data: &[u8], mtime: i64) {
        self.ino += 1;
        let nlink = if mode & 0o170000 == 0o040000 { 2 } else { 1 };
        let fields = [self.ino, mode, 0, 0, nlink, mtime as u32, data.len() as u32, 0, 0, 0, 0, name.len() as u32 + 1, 0];
        write!(self.buf, "070701").unwrap();
        for f in fields {
            write!(self.buf, "{:08x}", f).unwrap();
        }
        self.buf.extend_from_slice(name.as_bytes());
        self.buf.push(0);
        self.pad();
        self.buf.extend_from_slice(data);
        self.pad();
    }

    fn pad(&mut self) {
        while self.buf.len() % 4 != 0 {
            self.buf.push(0);
        }
    }

    fn finish(mut self) -> Vec<u8> {
        self.ino = 0;
        self.entry("TRAILER!!!", 0, &[], 0);
        self.buf
    }
}

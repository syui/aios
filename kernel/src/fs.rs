// 起動時にルートファイルシステムを組み立てる
use crate::extfs::ExtFs;
use crate::initrd;
use crate::tmpfs::{self, TmpInode};
use crate::vfs::{self, InodeRef, NewNode, S_IFMT};

/// ディスクに ext2 があればそれを、なければ initramfs を展開した tmpfs をルートにする
pub fn init() {
    if crate::block::init() {
        match ExtFs::mount() {
            Ok(fs) => {
                println!("fs: root is {} on {}", fs.kind(), crate::block::name());
                let kind = fs.kind();
                vfs::set_root(fs.root(), &crate::block::name(), kind);
                // /dev と /tmp はメモリ上に
                for d in ["dev", "tmp", "run"] {
                    if vfs::mkdir_p(d, 0o755).is_ok() {
                        let _ = vfs::mount(d, tmpfs::new_root(), "tmpfs", "tmpfs");
                    }
                }
                let mut mtab = alloc::format!("{} / {} rw 0 0\ntmpfs /dev tmpfs rw 0 0\ntmpfs /tmp tmpfs rw 0 0\ntmpfs /run tmpfs rw 0 0\nproc /proc proc rw 0 0\nsysfs /sys sysfs rw 0 0\n", crate::block::name(), kind);
                // FAT の boot の区画 (ESP / ラズパイの boot) を /boot に
                if let Some(p) = crate::block::boot() {
                    match crate::vfat::FatFs::mount(p) {
                        Ok(b) if vfs::mkdir_p("boot", 0o755).is_ok() && vfs::mount("boot", b.root(), &crate::block::part_name(&p), "vfat").is_ok() => {
                            println!("fs: /boot is {} on {}", b.kind(), crate::block::part_name(&p));
                            mtab.push_str(&alloc::format!("{} /boot vfat rw 0 0\n", crate::block::part_name(&p)));
                        }
                        Ok(_) => {}
                        Err(e) => println!("fs: boot partition not usable ({})", e),
                    }
                }
                setup_dirs(&mtab);
                return;
            }
            Err(e) => println!("fs: disk not usable ({}), using initramfs", e),
        }
    }
    println!("fs: root is tmpfs from initramfs");
    let root = tmpfs::new_root();
    vfs::set_root(root, "rootfs", "tmpfs");
    for e in initrd::entries() {
        let (dir, name) = e.name.rsplit_once('/').unwrap_or(("", e.name));
        let Ok(parent) = vfs::resolve("", dir, true) else { continue };
        if let Some(p) = parent.as_any().downcast_ref::<TmpInode>() {
            let _ = p.add_static(name, e.mode, e.data, e.mtime as u64 * 1_000_000_000);
        }
    }
    setup_dirs("tmpfs / tmpfs rw 0 0\nproc /proc proc rw 0 0\n");
}

/// モジュール (module.rs) が見つけた装置の /dev のノード: 画面 (/dev/fb0) とキーボードやマウス、音 (/dev/snd)
pub fn add_device_nodes() {
    let Ok(dev) = vfs::mkdir_p("dev", 0o755) else { return };
    if crate::gpu::get().is_some() {
        let _ = dev.create("fb0", 0o666, NewNode::Dev(29, 0));
    }
    if crate::sound::present() {
        if let Ok(d) = vfs::mkdir_p("dev/snd", 0o755) {
            let _ = d.create("controlC0", 0o666, NewNode::Dev(116, 0));
            let _ = d.create("pcmC0D0p", 0o666, NewNode::Dev(116, 16));
        }
    }
    if crate::input::count() > 0 {
        if let Ok(d) = vfs::mkdir_p("dev/input", 0o755) {
            for n in 0..crate::input::count() {
                let _ = d.create(&alloc::format!("event{}", n), 0o666, NewNode::Dev(13, 64 + n as u32));
            }
        }
    }
}

fn setup_dirs(mtab: &str) {
    let dev = vfs::mkdir_p("dev", 0o755).expect("mkdir /dev");
    for (name, mode, ma, mi) in [
        ("null", 0o666, 1, 3),
        ("zero", 0o666, 1, 5),
        ("random", 0o666, 1, 8),
        ("urandom", 0o666, 1, 9),
        ("tty", 0o666, 5, 0),
        ("console", 0o620, 5, 1),
        ("ptmx", 0o666, 5, 2),
    ] {
        let _ = dev.create(name, mode, NewNode::Dev(ma, mi));
    }

    // /dev/fd と /dev/stdin /dev/stdout /dev/stderr (Linux と同じく /proc/self/fd へのリンク)
    for (name, to) in [("fd", "/proc/self/fd"), ("stdin", "/proc/self/fd/0"), ("stdout", "/proc/self/fd/1"), ("stderr", "/proc/self/fd/2")] {
        if dev.lookup(name).is_err() {
            let _ = dev.create(name, 0o777, NewNode::Symlink(to.into()));
        }
    }

    // ディスクと区画 (/dev/vda, /dev/vda1, ...)
    for p in crate::block::parts() {
        let (ma, mi) = crate::block::dev_of_part(&p);
        let name = crate::block::part_name(&p);
        let name = name.trim_start_matches("/dev/");
        // 前の起動で作ったもの (ディスクのルート) が違う番号なら作りなおす
        if let Ok(old) = dev.lookup(name) {
            if old.meta().rdev == ((ma as u64) << 8 | mi as u64) && old.meta().mode & S_IFMT == vfs::S_IFBLK {
                continue;
            }
            let _ = dev.unlink(name, false);
        }
        let _ = dev.create(name, 0o660, NewNode::Blk(ma, mi));
    }
    // 疑似端末の子の口 (/dev/pts/N) は ptmx を開くたびにここへ作る
    let _ = vfs::mkdir_p("dev/pts", 0o755);
    // POSIX の共有メモリ (shm_open は /dev/shm/NAME を開く)。/tmp と同じく、だれでも作れて他人のは消せない
    if let Ok(shm) = vfs::mkdir_p("dev/shm", 0o1777) {
        let _ = shm.set_mode(0o1777);
    }
    let tmp = vfs::mkdir_p("tmp", 0o1777).expect("mkdir /tmp");
    let _ = tmp.set_mode(0o1777);
    for d in ["etc", "home", "root", "run", "var/tmp", "var/log"] {
        let _ = vfs::mkdir_p(d, 0o755);
    }
    if vfs::mkdir_p("proc", 0o555).is_ok() {
        let _ = vfs::mount("proc", crate::procfs::new_root(), "proc", "proc");
    }
    if vfs::mkdir_p("sys", 0o555).is_ok() {
        let _ = vfs::mount("sys", crate::sysfs::new_root(), "sysfs", "sysfs");
    }
    // df などが読むマウント表 (起動のたびに書きなおす)
    if let Ok(etc) = vfs::resolve("", "etc", true) {
        let f = etc.lookup("mtab").or_else(|_| etc.create("mtab", 0o644, NewNode::File));
        if let Ok(f) = f {
            let _ = f.truncate(0);
            let _ = f.write_at(0, mtab.as_bytes());
        }
    }
}

/// デバイスファイルなら (major, minor)
pub fn dev_of(i: &InodeRef) -> Option<(u32, u32)> {
    let m = i.meta();
    matches!(m.mode & S_IFMT, vfs::S_IFCHR | vfs::S_IFBLK).then_some(((m.rdev >> 8) as u32, (m.rdev & 0xff) as u32))
}

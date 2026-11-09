// 起動時にルートファイルシステムを組み立てる
use crate::initrd;
use crate::tmpfs::{self, TmpInode};
use crate::vfs::{self, InodeRef, NewNode, S_IFMT};
use alloc::string::String;

/// ディスクから見つけたルート (block::mount_root。Linux の init/do_mounts.c の mount_root): ext2 / ext4 の根と、その装置の
/// 名前と種類の名前。boot は FAT の boot の区画 (ESP / ラズパイの boot) をマウントしようとした答え (根、装置の名前、種類)
pub struct DiskRoot {
    pub root: InodeRef,
    pub dev: String,
    pub kind: &'static str,
    pub boot: Option<Result<(InodeRef, String, &'static str), &'static str>>,
}

/// ルートを組み立てる: ディスクのルート (main.rs が block::mount_root で見つけたもの) があればそれを、なければ initramfs を
/// 展開した tmpfs をルートにする
pub fn init(disk: Option<DiskRoot>) {
    if let Some(d) = disk {
        println!("fs: root is {} on {}", d.kind, d.dev);
        vfs::set_root(d.root, &d.dev, d.kind);
        // /dev と /tmp はメモリ上に
        for dir in ["dev", "tmp", "run"] {
            if vfs::mkdir_p(dir, 0o755).is_ok() {
                let _ = vfs::mount(dir, tmpfs::new_root(), "tmpfs", "tmpfs");
            }
        }
        let mut mtab = alloc::format!("{} / {} rw 0 0\ntmpfs /dev tmpfs rw 0 0\ntmpfs /tmp tmpfs rw 0 0\ntmpfs /run tmpfs rw 0 0\nproc /proc proc rw 0 0\nsysfs /sys sysfs rw 0 0\nsecurityfs /sys/kernel/security securityfs rw 0 0\n", d.dev, d.kind);
        // FAT の boot の区画を /boot に
        match d.boot {
            Some(Ok((b, dev, kind))) => {
                if vfs::mkdir_p("boot", 0o755).is_ok() && vfs::mount("boot", b, &dev, "vfat").is_ok() {
                    println!("fs: /boot is {} on {}", kind, dev);
                    mtab.push_str(&alloc::format!("{} /boot vfat rw 0 0\n", dev));
                }
            }
            None => {}
            Some(Err(e)) => println!("fs: boot partition not usable ({})", e),
        }
        setup_dirs(&mtab);
        return;
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

/// 装置のノード (Linux の devtmpfs: 装置を見つけたもの (block、gpu、sound、input) が add_dev で置く)。/dev ができる前
/// (起動のとき) なら覚えておいて setup_dirs で作り、あと (modprobe) ならすぐに作る。(/dev の下のパス, mode, ブロックか, major, minor)
static DEV_NODES: crate::spinlock::SpinLock<Option<alloc::vec::Vec<(String, u32, bool, u32, u32)>>> = crate::spinlock::SpinLock::new(Some(alloc::vec::Vec::new()));

/// /dev/PATH に装置のノードを (blk ならブロックデバイス)。同じ名前で番号か種類がちがうものがあれば作りなおす
pub fn add_dev(path: &str, mode: u32, blk: bool, ma: u32, mi: u32) {
    {
        let mut g = DEV_NODES.lock();
        if let Some(l) = g.as_mut() {
            l.push((String::from(path), mode, blk, ma, mi));
            return;
        }
    }
    make_dev(path, mode, blk, ma, mi);
}

fn make_dev(path: &str, mode: u32, blk: bool, ma: u32, mi: u32) {
    let (dir, name) = path.rsplit_once('/').map_or(("dev".into(), path), |(d, n)| (alloc::format!("dev/{}", d), n));
    let Ok(d) = vfs::mkdir_p(&dir, 0o755) else { return };
    let ty = if blk { vfs::S_IFBLK } else { vfs::S_IFCHR };
    if let Ok(old) = d.lookup(name) {
        if old.meta().rdev == ((ma as u64) << 8 | mi as u64) && old.meta().mode & S_IFMT == ty {
            return;
        }
        let _ = d.unlink(name, false);
    }
    let _ = d.create(name, mode, if blk { NewNode::Blk(ma, mi) } else { NewNode::Dev(ma, mi) });
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

    // 起動のときに見つけた装置 (ディスクと区画 /dev/vda、/dev/vda1 ...: add_dev)。ここからあとはすぐに作る
    let pending = DEV_NODES.lock().take().unwrap_or_default();
    for (path, mode, blk, ma, mi) in pending {
        make_dev(&path, mode, blk, ma, mi);
    }
    // 疑似端末の子の口 (/dev/pts/N) は ptmx を開くたびにここへ作る
    let _ = vfs::mkdir_p("dev/pts", 0o755);
    // POSIX の共有メモリ (shm_open は /dev/shm/NAME を開く)。/tmp と同じく、だれでも作れて他人のは消せない
    if let Ok(shm) = vfs::mkdir_p("dev/shm", 0o1777) {
        let _ = shm.set_mode(0o1777);
    }
    let tmp = vfs::mkdir_p("tmp", 0o1777).expect("mkdir /tmp");
    let _ = tmp.set_mode(0o1777);
    for d in ["etc", "home", "root", "run", "var/log"] {
        let _ = vfs::mkdir_p(d, 0o755);
    }
    // /var/tmp も /tmp と同じく、だれでも作れて他人のは消せない (FHS。起動をまたいで残るほうの一時置き場)。前は 0755 で
    // 作っていて、root のほかは書けなかった (/var は上で 0755 に作ってある。mkdir_p は途中のものにも mode をつける)
    if let Ok(vtmp) = vfs::mkdir_p("var/tmp", 0o1777) {
        let _ = vtmp.set_mode(0o1777);
    }
    // 装置のないファイルシステム (main.rs が vfs::register_filesystem で置いたもの)
    if vfs::mkdir_p("proc", 0o555).is_ok()
        && let Ok(r) = vfs::mount_nodev("proc")
    {
        let _ = vfs::mount("proc", r, "proc", "proc");
    }
    if vfs::mkdir_p("sys", 0o555).is_ok() {
        if let Ok(r) = vfs::mount_nodev("sysfs") {
            let _ = vfs::mount("sys", r, "sysfs", "sysfs");
        }
        // securityfs (Linux では init がかぶせる。libapparmor は /proc/mounts の securityfs から apparmor/ をさがす)
        if let Ok(r) = vfs::mount_nodev("securityfs") {
            let _ = vfs::mount("sys/kernel/security", r, "securityfs", "securityfs");
        }
        // efivarfs (Linux では systemd がかぶせる)。UEFI から起動していなければない
        if let Ok(efi) = vfs::mount_nodev("efivarfs") {
            let _ = vfs::mount("sys/firmware/efi/efivars", efi, "efivarfs", "efivarfs");
        }
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

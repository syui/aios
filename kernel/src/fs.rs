// 起動時にルートファイルシステムを組み立てる
use crate::extfs::ExtFs;
use crate::initrd;
use crate::tmpfs::{self, TmpInode};
use crate::virtio_blk;
use crate::vfs::{self, InodeRef, NewNode, S_IFMT};

/// ディスクに ext2 があればそれを、なければ initramfs を展開した tmpfs をルートにする
pub fn init() {
    if virtio_blk::init() {
        match ExtFs::mount() {
            Ok(fs) => {
                println!("fs: root is {} on virtio-blk", fs.kind());
                let kind = fs.kind();
                vfs::set_root(fs.root());
                // /dev と /tmp はメモリ上に
                for d in ["dev", "tmp", "run"] {
                    if vfs::mkdir_p(d, 0o755).is_ok() {
                        let _ = vfs::mount(d, tmpfs::new_root());
                    }
                }
                let mtab = alloc::format!("/dev/vda / {} rw 0 0\ntmpfs /dev tmpfs rw 0 0\ntmpfs /tmp tmpfs rw 0 0\ntmpfs /run tmpfs rw 0 0\n", kind);
                setup_dirs(&mtab);
                return;
            }
            Err(e) => println!("fs: disk not usable ({}), using initramfs", e),
        }
    }
    println!("fs: root is tmpfs from initramfs");
    let root = tmpfs::new_root();
    vfs::set_root(root);
    for e in initrd::entries() {
        let (dir, name) = e.name.rsplit_once('/').unwrap_or(("", e.name));
        let Ok(parent) = vfs::resolve("", dir, true) else { continue };
        if let Some(p) = parent.as_any().downcast_ref::<TmpInode>() {
            let _ = p.add_static(name, e.mode, e.data, e.mtime as u64 * 1_000_000_000);
        }
    }
    setup_dirs("tmpfs / tmpfs rw 0 0\n");
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
    // 疑似端末の子の口 (/dev/pts/N) は ptmx を開くたびにここへ作る
    let _ = vfs::mkdir_p("dev/pts", 0o755);
    let tmp = vfs::mkdir_p("tmp", 0o1777).expect("mkdir /tmp");
    let _ = tmp.set_mode(0o1777);
    for d in ["etc", "home", "root", "run", "var/tmp", "var/log"] {
        let _ = vfs::mkdir_p(d, 0o755);
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
    (m.mode & S_IFMT == vfs::S_IFCHR).then_some(((m.rdev >> 8) as u32, (m.rdev & 0xff) as u32))
}

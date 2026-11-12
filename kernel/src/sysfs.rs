// /sys: デバイスの様子を見せる小さなファイルシステム (Linux の sysfs の、プログラムがよく読むところだけ)
//
//   /sys/block/vda/{size,stat,dev,ro,removable}      ディスク全体 (psutil はここがあるものを数える)
//   /sys/block/vda/queue/{hw_sector_size,logical_block_size,physical_block_size,rotational}
//   /sys/block/vda/vda1/{size,start,stat,dev,partition,ro}   区画
//   /sys/class/net/{eth0,lo}/{address,mtu,operstate,carrier,ifindex,type,flags}
//   /sys/class/net/IF/statistics/{rx,tx}_{bytes,packets,errors,dropped}
//   /sys/devices/system/cpu/{online,possible,present} と cpuN/online
//   /sys/devices/system/cpu/vulnerabilities/*       Spectre / Meltdown などの様子 (cpuvuln.rs。Linux 7.3 と同じ 19 のファイル)
//   /sys/kernel/security                             securityfs をかぶせるところ (securityfs.rs)
//   /sys/firmware/efi/{fw_platform_size,systab}      UEFI から起動したとき (arm64 の Linux と同じもの。systab は root だけ)
//   /sys/firmware/efi/efivars                        efivarfs をかぶせるところ (efivarfs.rs)
//   /sys/module/apparmor/parameters/enabled          Y (aios.off=apparmor なら N。libapparmor の aa_is_enabled が読む)
//   /sys/dev/char/MA:MI、/sys/class/CLASS/NAME       ドライバが add_chrdev で置いた文字デバイス (DRM の card0 と renderD128:
//                                                    libdrm の drmGetDevices2 が読む)。どちらも次へのリンク:
//   /sys/devices/platform/PARENT/{uevent,subsystem}  それをつなぐ装置 (a003e00.virtio_mmio: DTB の名前と、Linux と同じ uevent)
//   /sys/devices/platform/PARENT/CLASS/NAME/{uevent,dev,device,subsystem}
//   /sys/bus/platform                                subsystem のリンクの先 (中は空)
// 大きさはセクタ (512 バイト)、stat は /proc/diskstats の 4 列目からの 11 列と同じ
use crate::vfs::*;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::any::Any;

const EACCES: i64 = 13;
const SYSFS_MAGIC: u64 = 0x6265_6572;

const DISK_FILES: [&str; 5] = ["size", "stat", "dev", "ro", "removable"];
const QUEUE_FILES: [&str; 4] = ["hw_sector_size", "logical_block_size", "physical_block_size", "rotational"];
const PART_FILES: [&str; 6] = ["size", "start", "stat", "dev", "partition", "ro"];
const NET_FILES: [&str; 7] = ["address", "mtu", "operstate", "carrier", "ifindex", "type", "flags"];
const NET_STATS: [&str; 8] = ["rx_bytes", "tx_bytes", "rx_packets", "tx_packets", "rx_errors", "tx_errors", "rx_dropped", "tx_dropped"];
const CPU_FILES: [&str; 3] = ["online", "possible", "present"];
const EFI_FILES: [&str; 2] = ["fw_platform_size", "systab"];
/// ネットワークのインターフェース: eth0 と lo (番号は netlink と ioctl の SIOCGIFINDEX と同じ: lo 1、eth0 2)
const NETS: [&str; 2] = ["eth0", "lo"];

#[derive(Clone, Copy, PartialEq)]
enum Node {
    Root,
    Block,
    Disk,
    DiskFile(u8),
    Queue,
    QueueFile(u8),
    /// 区画 (番号)
    Part(u16),
    PartFile(u16, u8),
    Class,
    ClassNet,
    Net(u8),
    NetFile(u8, u8),
    NetStats(u8),
    NetStat(u8, u8),
    Devices,
    System,
    Cpu,
    CpuFile(u8),
    CpuN(u16),
    CpuOnline(u16),
    Vulns,
    Vuln(u8),
    Kernel,
    Security,
    Module,
    ModAa,
    ModAaParams,
    ModAaEnabled,
    Firmware,
    Efi,
    EfiFile(u8),
    EfiVars,
    /// /sys/dev、/sys/dev/char と、その中のリンク (CHRDEVS の番号)
    Dev,
    DevChar,
    DevCharLink(u8),
    /// /sys/bus と /sys/bus/platform
    Bus,
    BusPlatform,
    /// /sys/devices/platform と、その中の装置 (その装置をつなぐ CHRDEVS のはじめの番号)
    Platform,
    PlatDev(u8),
    PlatUevent(u8),
    PlatSubsys(u8),
    /// 装置の下の CLASS のディレクトリ (その CLASS のはじめの番号)
    PlatClass(u8),
    /// 文字デバイスのディレクトリと、その中のもの
    Chr(u8),
    ChrUevent(u8),
    ChrDev(u8),
    ChrDevice(u8),
    ChrSubsys(u8),
    /// /sys/class/CLASS (その CLASS のはじめの番号) と、その中のリンク
    ClassOf(u8),
    ClassLink(u8),
}

/// ドライバが置く文字デバイス (Linux の device_create のうち、プログラムが読むところ)
#[derive(Clone)]
pub struct ChrDev {
    pub major: u32,
    pub minor: u32,
    /// card0
    pub name: String,
    /// /dev の下の名前 (dri/card0)
    pub devname: String,
    /// uevent の DEVTYPE (drm_minor)
    pub devtype: &'static str,
    /// /sys/class の下の名前 (drm)
    pub class: &'static str,
    /// つなぐ装置の名前 (a003e00.virtio_mmio) と、その uevent
    pub parent: String,
    pub parent_uevent: String,
}

static CHRDEVS: crate::spinlock::SpinLock<Vec<ChrDev>> = crate::spinlock::SpinLock::new(Vec::new());

/// 文字デバイスを /sys に (同じ major:minor があれば置きかえる)
pub fn add_chrdev(d: ChrDev) {
    let mut l = CHRDEVS.lock();
    if let Some(o) = l.iter_mut().find(|o| o.major == d.major && o.minor == d.minor) {
        *o = d;
    } else if l.len() < 32 {
        l.push(d);
    }
}

fn chrdevs() -> Vec<ChrDev> {
    CHRDEVS.lock().clone()
}

fn chrdev(i: u8) -> Result<ChrDev, i64> {
    CHRDEVS.lock().get(i as usize).cloned().ok_or(-ENOENT)
}

/// i の装置 (または CLASS) を持つはじめの番号か
fn first_parent(l: &[ChrDev], i: usize) -> bool {
    !l[..i].iter().any(|o| o.parent == l[i].parent)
}

fn first_class(l: &[ChrDev], i: usize) -> bool {
    !l[..i].iter().any(|o| o.class == l[i].class)
}

fn first_class_in(l: &[ChrDev], i: usize) -> bool {
    !l[..i].iter().any(|o| o.parent == l[i].parent && o.class == l[i].class)
}

pub struct SysInode {
    fs: usize,
    node: Node,
}

pub fn new_root() -> InodeRef {
    Rc::new(SysInode { fs: new_fs_id(), node: Node::Root })
}

/// ディスク全体の名前 (vda か mmcblk0)
fn disk_name() -> String {
    crate::block::part_name(&crate::block::Part { start: 0, len: 0, num: 0 }).trim_start_matches("/dev/").to_string()
}

fn part(num: u16) -> Option<crate::block::Part> {
    crate::block::parts().into_iter().find(|p| p.num == num as usize)
}

/// 区画の名前 (vda1、mmcblk0p1)
fn part_name(num: u16) -> String {
    crate::block::part_name(&crate::block::Part { start: 0, len: 0, num: num as usize }).trim_start_matches("/dev/").to_string()
}

/// /sys/block/*/stat の 11 列 (読んだ回数 まとめた数 セクタ ms 書いた回数 まとめた数 セクタ ms 動いている数 ms 重みつき ms)
fn stat(num: usize) -> String {
    let c = crate::block::io_stats(num);
    format!("{:8} {:8} {:8} {:8} {:8} {:8} {:8} {:8} {:8} {:8} {:8}\n", c[0], 0, c[1], c[2], c[3], 0, c[4], c[5], 0, c[2] + c[5], c[2] + c[5])
}

impl SysInode {
    fn child(&self, node: Node) -> InodeRef {
        Rc::new(SysInode { fs: self.fs, node })
    }

    fn ino(&self) -> u64 {
        match self.node {
            Node::Root => 1,
            Node::Block => 2,
            Node::Disk => 3,
            Node::Queue => 4,
            Node::Class => 5,
            Node::ClassNet => 6,
            Node::Devices => 7,
            Node::System => 8,
            Node::Cpu => 9,
            Node::Kernel => 10,
            Node::Security => 11,
            Node::Module => 12,
            Node::ModAa => 13,
            Node::ModAaParams => 14,
            Node::ModAaEnabled => 15,
            Node::Firmware => 16,
            Node::Efi => 17,
            Node::EfiVars => 18,
            Node::EfiFile(i) => 0x400 + i as u64,
            Node::DiskFile(i) => 0x100 + i as u64,
            Node::QueueFile(i) => 0x200 + i as u64,
            Node::CpuFile(i) => 0x300 + i as u64,
            Node::Part(n) => 0x1_0000 + ((n as u64) << 8),
            Node::PartFile(n, i) => 0x1_0000 + ((n as u64) << 8) + 1 + i as u64,
            Node::Net(n) => 0x2_0000 + ((n as u64) << 8),
            Node::NetFile(n, i) => 0x2_0000 + ((n as u64) << 8) + 1 + i as u64,
            Node::NetStats(n) => 0x2_0000 + ((n as u64) << 8) + 0x40,
            Node::NetStat(n, i) => 0x2_0000 + ((n as u64) << 8) + 0x41 + i as u64,
            Node::CpuN(n) => 0x3_0000 + ((n as u64) << 4),
            Node::CpuOnline(n) => 0x3_0000 + ((n as u64) << 4) + 1,
            Node::Vulns => 19,
            Node::Vuln(i) => 0x500 + i as u64,
            Node::Dev => 20,
            Node::DevChar => 21,
            Node::Bus => 22,
            Node::BusPlatform => 23,
            Node::Platform => 24,
            Node::DevCharLink(i) => 0x6_0000 + ((i as u64) << 5),
            Node::PlatDev(i) => 0x6_0000 + ((i as u64) << 5) + 1,
            Node::PlatUevent(i) => 0x6_0000 + ((i as u64) << 5) + 2,
            Node::PlatSubsys(i) => 0x6_0000 + ((i as u64) << 5) + 3,
            Node::PlatClass(i) => 0x6_0000 + ((i as u64) << 5) + 4,
            Node::Chr(i) => 0x6_0000 + ((i as u64) << 5) + 5,
            Node::ChrUevent(i) => 0x6_0000 + ((i as u64) << 5) + 6,
            Node::ChrDev(i) => 0x6_0000 + ((i as u64) << 5) + 7,
            Node::ChrDevice(i) => 0x6_0000 + ((i as u64) << 5) + 8,
            Node::ChrSubsys(i) => 0x6_0000 + ((i as u64) << 5) + 9,
            Node::ClassOf(i) => 0x6_0000 + ((i as u64) << 5) + 10,
            Node::ClassLink(i) => 0x6_0000 + ((i as u64) << 5) + 11,
        }
    }

    fn is_dir(&self) -> bool {
        matches!(
            self.node,
            Node::Root | Node::Block | Node::Disk | Node::Queue | Node::Part(_) | Node::Class | Node::ClassNet | Node::Net(_) | Node::NetStats(_) | Node::Devices | Node::System | Node::Cpu | Node::Vulns | Node::CpuN(_) | Node::Kernel | Node::Security | Node::Module | Node::ModAa | Node::ModAaParams | Node::Firmware | Node::Efi | Node::EfiVars
                | Node::Dev | Node::DevChar | Node::Bus | Node::BusPlatform | Node::Platform | Node::PlatDev(_) | Node::PlatClass(_) | Node::Chr(_) | Node::ClassOf(_)
        )
    }

    /// 親のディレクトリ (/sys は /sys)
    fn parent(&self) -> Node {
        let l = || chrdevs();
        match self.node {
            Node::Root | Node::Block | Node::Class | Node::Devices | Node::Kernel | Node::Module | Node::Firmware | Node::Dev | Node::Bus => Node::Root,
            Node::Disk => Node::Block,
            Node::DiskFile(_) | Node::Queue | Node::Part(_) => Node::Disk,
            Node::QueueFile(_) => Node::Queue,
            Node::PartFile(n, _) => Node::Part(n),
            Node::ClassNet | Node::ClassOf(_) => Node::Class,
            Node::Net(_) => Node::ClassNet,
            Node::NetFile(n, _) | Node::NetStats(n) => Node::Net(n),
            Node::NetStat(n, _) => Node::NetStats(n),
            Node::System | Node::Platform => Node::Devices,
            Node::Cpu => Node::System,
            Node::CpuFile(_) | Node::CpuN(_) | Node::Vulns => Node::Cpu,
            Node::CpuOnline(n) => Node::CpuN(n),
            Node::Vuln(_) => Node::Vulns,
            Node::Security => Node::Kernel,
            Node::ModAa => Node::Module,
            Node::ModAaParams => Node::ModAa,
            Node::ModAaEnabled => Node::ModAaParams,
            Node::Efi => Node::Firmware,
            Node::EfiFile(_) | Node::EfiVars => Node::Efi,
            Node::DevChar => Node::Dev,
            Node::DevCharLink(_) => Node::DevChar,
            Node::BusPlatform => Node::Bus,
            Node::PlatDev(_) => Node::Platform,
            Node::PlatUevent(i) | Node::PlatSubsys(i) => Node::PlatDev(i),
            Node::PlatClass(i) => {
                let l = l();
                let p = l.get(i as usize).map(|d| d.parent.clone());
                Node::PlatDev((0..l.len()).find(|&j| first_parent(&l, j) && Some(&l[j].parent) == p.as_ref()).unwrap_or(0) as u8)
            }
            Node::Chr(i) => {
                let l = l();
                let c = l.get(i as usize).map(|d| (d.parent.clone(), d.class));
                Node::PlatClass((0..l.len()).find(|&j| first_class_in(&l, j) && Some((l[j].parent.clone(), l[j].class)) == c).unwrap_or(0) as u8)
            }
            Node::ChrUevent(i) | Node::ChrDev(i) | Node::ChrDevice(i) | Node::ChrSubsys(i) => Node::Chr(i),
            Node::ClassLink(i) => {
                let l = l();
                let c = l.get(i as usize).map(|d| d.class);
                Node::ClassOf((0..l.len()).find(|&j| first_class(&l, j) && Some(l[j].class) == c).unwrap_or(0) as u8)
            }
        }
    }

    /// シンボリックリンクの先 (リンクでなければ None)
    fn link(&self) -> Option<String> {
        let d = match self.node {
            Node::DevCharLink(i) | Node::PlatSubsys(i) | Node::ChrDevice(i) | Node::ChrSubsys(i) | Node::ClassLink(i) => chrdev(i).ok()?,
            _ => return None,
        };
        Some(match self.node {
            Node::DevCharLink(_) => format!("../../devices/platform/{}/{}/{}", d.parent, d.class, d.name),
            Node::PlatSubsys(_) => "../../../bus/platform".into(),
            Node::ChrDevice(_) => format!("../../../{}", d.parent),
            Node::ChrSubsys(_) => format!("../../../../../class/{}", d.class),
            _ => format!("../../devices/platform/{}/{}/{}", d.parent, d.class, d.name),
        })
    }

    fn content(&self) -> Result<String, i64> {
        let ncpu = crate::smp::online().max(1);
        let dev = |num: usize| {
            let (ma, mi) = crate::block::dev_of_part(&crate::block::Part { start: 0, len: 0, num });
            format!("{}:{}\n", ma, mi)
        };
        Ok(match self.node {
            Node::DiskFile(i) => match DISK_FILES[i as usize] {
                "size" => format!("{}\n", part(0).map_or(0, |p| p.len)),
                "stat" => stat(0),
                "dev" => dev(0),
                _ => "0\n".into(),
            },
            Node::QueueFile(i) => match QUEUE_FILES[i as usize] {
                "rotational" => "0\n".into(),
                _ => format!("{}\n", crate::block::SECTOR),
            },
            Node::PartFile(n, i) => {
                let p = part(n).ok_or(-ENOENT)?;
                match PART_FILES[i as usize] {
                    "size" => format!("{}\n", p.len),
                    "start" => format!("{}\n", p.start),
                    "stat" => stat(p.num),
                    "dev" => dev(p.num),
                    "partition" => format!("{}\n", p.num),
                    _ => "0\n".into(),
                }
            }
            Node::NetFile(n, i) => {
                let lo = NETS[n as usize] == "lo";
                match NET_FILES[i as usize] {
                    "address" => {
                        let m = if lo { [0; 6] } else { crate::net::mac().unwrap_or([0; 6]) };
                        format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}\n", m[0], m[1], m[2], m[3], m[4], m[5])
                    }
                    "mtu" => if lo { "65536\n".into() } else { "1500\n".into() },
                    "operstate" => if lo { "unknown\n".into() } else { "up\n".into() },
                    "carrier" => "1\n".into(),
                    "ifindex" => format!("{}\n", if lo { crate::netlink::LO_INDEX } else { crate::netlink::ETH_INDEX }),
                    // ARPHRD_LOOPBACK と ARPHRD_ETHER
                    "type" => if lo { "772\n".into() } else { "1\n".into() },
                    // dev->flags (Linux は IFF_RUNNING を入れない): IFF_UP | IFF_LOOPBACK と IFF_UP | IFF_BROADCAST | IFF_MULTICAST
                    _ => if lo { "0x9\n".into() } else { "0x1003\n".into() },
                }
            }
            Node::NetStat(n, i) => {
                let s = crate::net::stats(NETS[n as usize] == "lo");
                let v = match NET_STATS[i as usize] {
                    "rx_bytes" => s[0],
                    "rx_packets" => s[1],
                    "tx_bytes" => s[2],
                    "tx_packets" => s[3],
                    _ => 0,
                };
                format!("{}\n", v)
            }
            Node::PlatUevent(i) => chrdev(i)?.parent_uevent,
            Node::ChrUevent(i) => {
                let d = chrdev(i)?;
                format!("MAJOR={}\nMINOR={}\nDEVNAME={}\nDEVTYPE={}\n", d.major, d.minor, d.devname, d.devtype)
            }
            Node::ChrDev(i) => {
                let d = chrdev(i)?;
                format!("{}:{}\n", d.major, d.minor)
            }
            Node::CpuFile(_) => if ncpu == 1 { "0\n".into() } else { format!("0-{}\n", ncpu - 1) },
            Node::CpuOnline(_) => "1\n".into(),
            Node::Vuln(i) => crate::cpuvuln::show(VULN_FILES[i as usize]),
            Node::ModAaEnabled => if crate::parts::off(crate::parts::Part::Apparmor) { "N\n".into() } else { "Y\n".into() },
            Node::EfiFile(0) => "64\n".into(),
            // 設定テーブルの ACPI と SMBIOS (Linux の systab_show の順)
            Node::EfiFile(_) => {
                if !crate::cred::current().capable(crate::cred::CAP_DAC_READ_SEARCH) {
                    return Err(-EACCES);
                }
                let rt = crate::efi::runtime();
                let mut t = String::new();
                for (k, v) in [("ACPI20", rt.acpi20), ("ACPI", rt.acpi), ("SMBIOS3", rt.smbios3), ("SMBIOS", rt.smbios)] {
                    if v != 0 {
                        t.push_str(&format!("{}={:#x}\n", k, v));
                    }
                }
                t
            }
            _ => return Err(-EISDIR),
        })
    }
}

impl Inode for SysInode {
    fn id(&self) -> (usize, u64) {
        (self.fs, self.ino())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn meta(&self) -> Meta {
        let link = self.link();
        let mode = match self.node {
            _ if self.is_dir() => S_IFDIR | 0o555,
            _ if link.is_some() => S_IFLNK | 0o777,
            Node::EfiFile(1) => S_IFREG | 0o400,
            // uevent は root が書ける (Linux と同じ形。書いても何もしない)
            Node::PlatUevent(_) | Node::ChrUevent(_) => S_IFREG | 0o644,
            _ => S_IFREG | 0o444,
        };
        let size = match &link {
            _ if self.is_dir() => 0,
            Some(l) => l.len() as u64,
            None => 4096,
        };
        let now = crate::timer::epoch_ns();
        Meta { ino: self.ino(), mode, nlink: 1, uid: 0, gid: 0, size, rdev: 0, blocks: 0, mtime: now, ctime: now }
    }

    fn read_at(&self, off: usize, buf: &mut [u8]) -> Result<usize, i64> {
        let s = self.content()?;
        let b = s.as_bytes();
        if off >= b.len() {
            return Ok(0);
        }
        let n = buf.len().min(b.len() - off);
        buf[..n].copy_from_slice(&b[off..off + n]);
        Ok(n)
    }

    fn write_at(&self, _: usize, b: &[u8]) -> Result<usize, i64> {
        match self.node {
            // uevent に書くと Linux はその装置のできごとをもう一度出す (udev の trigger)。aios には udev がないので受けとるだけ
            Node::PlatUevent(_) | Node::ChrUevent(_) => Ok(b.len()),
            _ => Err(-EACCES),
        }
    }

    fn truncate(&self, _: usize) -> Result<(), i64> {
        Err(-EACCES)
    }

    fn readlink(&self) -> Result<String, i64> {
        self.link().ok_or_else(crate::why::inval)
    }

    fn lookup(&self, name: &str) -> Result<InodeRef, i64> {
        let pos = |list: &[&str]| list.iter().position(|x| *x == name).map(|i| i as u8).ok_or(-ENOENT);
        let node = match self.node {
            // リンク (/sys/dev/char/MA:MI など) の .. を、たどるところで引く
            _ if self.is_dir() && name == "." => self.node,
            _ if self.is_dir() && name == ".." => self.parent(),
            Node::Root => match name {
                "block" => Node::Block,
                "bus" => Node::Bus,
                "class" => Node::Class,
                "dev" => Node::Dev,
                "devices" => Node::Devices,
                "kernel" => Node::Kernel,
                "module" => Node::Module,
                "firmware" => Node::Firmware,
                _ => return Err(-ENOENT),
            },
            Node::Dev if name == "char" => Node::DevChar,
            Node::DevChar => {
                let l = chrdevs();
                let i = l.iter().position(|d| format!("{}:{}", d.major, d.minor) == name).ok_or(-ENOENT)?;
                Node::DevCharLink(i as u8)
            }
            Node::Bus if name == "platform" => Node::BusPlatform,
            Node::Devices if name == "platform" && !chrdevs().is_empty() => Node::Platform,
            Node::Platform => {
                let l = chrdevs();
                let i = (0..l.len()).find(|&i| first_parent(&l, i) && l[i].parent == name).ok_or(-ENOENT)?;
                Node::PlatDev(i as u8)
            }
            Node::PlatDev(i) => match name {
                "uevent" => Node::PlatUevent(i),
                "subsystem" => Node::PlatSubsys(i),
                _ => {
                    let l = chrdevs();
                    let p = &l.get(i as usize).ok_or(-ENOENT)?.parent;
                    let j = (0..l.len()).find(|&j| first_class_in(&l, j) && &l[j].parent == p && l[j].class == name).ok_or(-ENOENT)?;
                    Node::PlatClass(j as u8)
                }
            },
            Node::PlatClass(i) => {
                let l = chrdevs();
                let c = l.get(i as usize).ok_or(-ENOENT)?;
                let j = l.iter().position(|d| d.parent == c.parent && d.class == c.class && d.name == name).ok_or(-ENOENT)?;
                Node::Chr(j as u8)
            }
            Node::Chr(i) => match name {
                "uevent" => Node::ChrUevent(i),
                "dev" => Node::ChrDev(i),
                "device" => Node::ChrDevice(i),
                "subsystem" => Node::ChrSubsys(i),
                _ => return Err(-ENOENT),
            },
            Node::ClassOf(i) => {
                let l = chrdevs();
                let c = l.get(i as usize).ok_or(-ENOENT)?.class;
                let j = l.iter().position(|d| d.class == c && d.name == name).ok_or(-ENOENT)?;
                Node::ClassLink(j as u8)
            }
            Node::Firmware if name == "efi" && crate::efirt::available() => Node::Efi,
            Node::Efi if name == "efivars" => Node::EfiVars,
            Node::Efi => Node::EfiFile(pos(&EFI_FILES)?),
            Node::Kernel if name == "security" => Node::Security,
            Node::Module if name == "apparmor" => Node::ModAa,
            Node::ModAa if name == "parameters" => Node::ModAaParams,
            Node::ModAaParams if name == "enabled" => Node::ModAaEnabled,
            Node::Block if name == disk_name() => Node::Disk,
            Node::Disk if name == "queue" => Node::Queue,
            Node::Disk => match DISK_FILES.iter().position(|x| *x == name) {
                Some(i) => Node::DiskFile(i as u8),
                None => {
                    let p = crate::block::parts().into_iter().find(|p| p.num != 0 && part_name(p.num as u16) == name).ok_or(-ENOENT)?;
                    Node::Part(p.num as u16)
                }
            },
            Node::Queue => Node::QueueFile(pos(&QUEUE_FILES)?),
            Node::Part(n) => Node::PartFile(n, pos(&PART_FILES)?),
            Node::Class if name == "net" => Node::ClassNet,
            Node::Class => {
                let l = chrdevs();
                let i = (0..l.len()).find(|&i| first_class(&l, i) && l[i].class == name).ok_or(-ENOENT)?;
                Node::ClassOf(i as u8)
            }
            Node::ClassNet => Node::Net(pos(&NETS)?),
            Node::Net(n) if name == "statistics" => Node::NetStats(n),
            Node::Net(n) => Node::NetFile(n, pos(&NET_FILES)?),
            Node::NetStats(n) => Node::NetStat(n, pos(&NET_STATS)?),
            Node::Devices if name == "system" => Node::System,
            Node::System if name == "cpu" => Node::Cpu,
            Node::Cpu if name == "vulnerabilities" => Node::Vulns,
            Node::Vulns => Node::Vuln(pos(&VULN_FILES)?),
            Node::Cpu => match pos(&CPU_FILES) {
                Ok(i) => Node::CpuFile(i),
                Err(_) => {
                    let n: usize = name.strip_prefix("cpu").and_then(|n| n.parse().ok()).ok_or(-ENOENT)?;
                    if n >= crate::smp::online().max(1) {
                        return Err(-ENOENT);
                    }
                    Node::CpuN(n as u16)
                }
            },
            Node::CpuN(n) if name == "online" => Node::CpuOnline(n),
            _ if !self.is_dir() => return Err(-ENOTDIR),
            _ => return Err(-ENOENT),
        };
        Ok(self.child(node))
    }

    fn readdir(&self) -> Result<Vec<DirEntry>, i64> {
        let mut v = Vec::new();
        let mut add = |name: String, node: Node| {
            let c = SysInode { fs: self.fs, node };
            v.push(DirEntry { name, ino: c.ino(), mode: c.meta().mode });
        };
        match self.node {
            Node::Root => {
                add("block".into(), Node::Block);
                add("bus".into(), Node::Bus);
                add("class".into(), Node::Class);
                add("dev".into(), Node::Dev);
                add("devices".into(), Node::Devices);
                add("kernel".into(), Node::Kernel);
                add("module".into(), Node::Module);
                add("firmware".into(), Node::Firmware);
            }
            Node::Firmware => {
                if crate::efirt::available() {
                    add("efi".into(), Node::Efi);
                }
            }
            Node::Efi => {
                for (i, f) in EFI_FILES.iter().enumerate() {
                    add((*f).into(), Node::EfiFile(i as u8));
                }
                add("efivars".into(), Node::EfiVars);
            }
            Node::EfiVars => {}
            Node::Kernel => add("security".into(), Node::Security),
            Node::Security => {}
            Node::Module => add("apparmor".into(), Node::ModAa),
            Node::ModAa => add("parameters".into(), Node::ModAaParams),
            Node::ModAaParams => add("enabled".into(), Node::ModAaEnabled),
            Node::Block => {
                if !crate::block::parts().is_empty() {
                    add(disk_name(), Node::Disk);
                }
            }
            Node::Disk => {
                for (i, f) in DISK_FILES.iter().enumerate() {
                    add((*f).into(), Node::DiskFile(i as u8));
                }
                add("queue".into(), Node::Queue);
                for p in crate::block::parts().into_iter().filter(|p| p.num != 0) {
                    add(part_name(p.num as u16), Node::Part(p.num as u16));
                }
            }
            Node::Queue => {
                for (i, f) in QUEUE_FILES.iter().enumerate() {
                    add((*f).into(), Node::QueueFile(i as u8));
                }
            }
            Node::Part(n) => {
                for (i, f) in PART_FILES.iter().enumerate() {
                    add((*f).into(), Node::PartFile(n, i as u8));
                }
            }
            Node::Class => {
                let l = chrdevs();
                for i in (0..l.len()).filter(|&i| first_class(&l, i)) {
                    add(l[i].class.into(), Node::ClassOf(i as u8));
                }
                add("net".into(), Node::ClassNet);
            }
            Node::ClassOf(i) => {
                let l = chrdevs();
                let c = l.get(i as usize).map(|d| d.class);
                for (j, d) in l.iter().enumerate().filter(|(_, d)| Some(d.class) == c) {
                    add(d.name.clone(), Node::ClassLink(j as u8));
                }
            }
            Node::Dev => add("char".into(), Node::DevChar),
            Node::DevChar => {
                for (i, d) in chrdevs().iter().enumerate() {
                    add(format!("{}:{}", d.major, d.minor), Node::DevCharLink(i as u8));
                }
            }
            Node::Bus => add("platform".into(), Node::BusPlatform),
            Node::BusPlatform => {}
            Node::Platform => {
                let l = chrdevs();
                for i in (0..l.len()).filter(|&i| first_parent(&l, i)) {
                    add(l[i].parent.clone(), Node::PlatDev(i as u8));
                }
            }
            Node::PlatDev(i) => {
                let l = chrdevs();
                if let Some(p) = l.get(i as usize).map(|d| d.parent.clone()) {
                    for j in (0..l.len()).filter(|&j| first_class_in(&l, j) && l[j].parent == p) {
                        add(l[j].class.into(), Node::PlatClass(j as u8));
                    }
                }
                add("subsystem".into(), Node::PlatSubsys(i));
                add("uevent".into(), Node::PlatUevent(i));
            }
            Node::PlatClass(i) => {
                let l = chrdevs();
                if let Some(c) = l.get(i as usize).cloned() {
                    for (j, d) in l.iter().enumerate().filter(|(_, d)| d.parent == c.parent && d.class == c.class) {
                        add(d.name.clone(), Node::Chr(j as u8));
                    }
                }
            }
            Node::Chr(i) => {
                add("dev".into(), Node::ChrDev(i));
                add("device".into(), Node::ChrDevice(i));
                add("subsystem".into(), Node::ChrSubsys(i));
                add("uevent".into(), Node::ChrUevent(i));
            }
            Node::ClassNet => {
                for (i, n) in NETS.iter().enumerate() {
                    add((*n).into(), Node::Net(i as u8));
                }
            }
            Node::Net(n) => {
                for (i, f) in NET_FILES.iter().enumerate() {
                    add((*f).into(), Node::NetFile(n, i as u8));
                }
                add("statistics".into(), Node::NetStats(n));
            }
            Node::NetStats(n) => {
                for (i, f) in NET_STATS.iter().enumerate() {
                    add((*f).into(), Node::NetStat(n, i as u8));
                }
            }
            Node::Devices => {
                if !chrdevs().is_empty() {
                    add("platform".into(), Node::Platform);
                }
                add("system".into(), Node::System);
            }
            Node::System => add("cpu".into(), Node::Cpu),
            Node::Cpu => {
                for (i, f) in CPU_FILES.iter().enumerate() {
                    add((*f).into(), Node::CpuFile(i as u8));
                }
                for n in 0..crate::smp::online().max(1) {
                    add(format!("cpu{}", n), Node::CpuN(n as u16));
                }
                add("vulnerabilities".into(), Node::Vulns);
            }
            Node::Vulns => {
                for (i, f) in VULN_FILES.iter().enumerate() {
                    add((*f).into(), Node::Vuln(i as u8));
                }
            }
            Node::CpuN(n) => add("online".into(), Node::CpuOnline(n)),
            _ => return Err(-ENOTDIR),
        }
        Ok(v)
    }

    fn create(&self, _: &str, _: u32, _: NewNode) -> Result<InodeRef, i64> {
        Err(-EACCES)
    }

    fn link(&self, _: &str, _: &InodeRef) -> Result<(), i64> {
        Err(-EACCES)
    }

    fn unlink(&self, _: &str, _: bool) -> Result<(), i64> {
        Err(-EACCES)
    }

    fn rename(&self, _: &str, _: &InodeRef, _: &str) -> Result<(), i64> {
        Err(-EACCES)
    }

    fn set_mode(&self, _: u32) -> Result<(), i64> {
        Err(-EPERM)
    }

    fn set_owner(&self, _: Option<u32>, _: Option<u32>) -> Result<(), i64> {
        Err(-EPERM)
    }

    fn set_mtime(&self, _: u64) -> Result<(), i64> {
        Err(-EPERM)
    }

    fn statfs(&self) -> [u8; 120] {
        crate::tmpfs::statfs_bytes(SYSFS_MAGIC, 4096, 0, 0, 0, 0)
    }
}

/// /sys/devices/system/cpu/vulnerabilities/ のファイル (Linux 7.3 の drivers/base/cpu.c の順)
const VULN_FILES: [&str; 19] = [
    "meltdown",
    "spectre_v1",
    "spectre_v2",
    "spec_store_bypass",
    "l1tf",
    "mds",
    "tsx_async_abort",
    "itlb_multihit",
    "srbds",
    "mmio_stale_data",
    "retbleed",
    "spec_rstack_overflow",
    "gather_data_sampling",
    "reg_file_data_sampling",
    "ghostwrite",
    "old_microcode",
    "indirect_target_selection",
    "tsa",
    "vmscape",
];

// /sys: デバイスの様子を見せる小さなファイルシステム (Linux の sysfs の、プログラムがよく読むところだけ)
//
//   /sys/block/vda/{size,stat,dev,ro,removable}      ディスク全体 (psutil はここがあるものを数える)
//   /sys/block/vda/queue/{hw_sector_size,logical_block_size,physical_block_size,rotational}
//   /sys/block/vda/vda1/{size,start,stat,dev,partition,ro}   区画
//   /sys/class/net/{eth0,lo}/{address,mtu,operstate,carrier,ifindex,type,flags}
//   /sys/class/net/IF/statistics/{rx,tx}_{bytes,packets,errors,dropped}
//   /sys/devices/system/cpu/{online,possible,present} と cpuN/online
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
/// ネットワークのインターフェース: eth0 (ioctl の SIOCGIFINDEX と同じ 1) と lo
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
        }
    }

    fn is_dir(&self) -> bool {
        matches!(self.node, Node::Root | Node::Block | Node::Disk | Node::Queue | Node::Part(_) | Node::Class | Node::ClassNet | Node::Net(_) | Node::NetStats(_) | Node::Devices | Node::System | Node::Cpu | Node::CpuN(_))
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
                    "ifindex" => if lo { "2\n".into() } else { "1\n".into() },
                    // ARPHRD_LOOPBACK と ARPHRD_ETHER
                    "type" => if lo { "772\n".into() } else { "1\n".into() },
                    // IFF_UP | IFF_LOOPBACK | IFF_RUNNING と IFF_UP | IFF_BROADCAST | IFF_RUNNING | IFF_MULTICAST
                    _ => if lo { "0x49\n".into() } else { "0x1043\n".into() },
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
            Node::CpuFile(_) => if ncpu == 1 { "0\n".into() } else { format!("0-{}\n", ncpu - 1) },
            Node::CpuOnline(_) => "1\n".into(),
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
        let mode = if self.is_dir() { S_IFDIR | 0o555 } else { S_IFREG | 0o444 };
        let now = crate::timer::epoch_ns();
        Meta { ino: self.ino(), mode, nlink: 1, uid: 0, gid: 0, size: if self.is_dir() { 0 } else { 4096 }, rdev: 0, blocks: 0, mtime: now, ctime: now }
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

    fn write_at(&self, _: usize, _: &[u8]) -> Result<usize, i64> {
        Err(-EACCES)
    }

    fn truncate(&self, _: usize) -> Result<(), i64> {
        Err(-EACCES)
    }

    fn readlink(&self) -> Result<String, i64> {
        Err(-EINVAL)
    }

    fn lookup(&self, name: &str) -> Result<InodeRef, i64> {
        let pos = |list: &[&str]| list.iter().position(|x| *x == name).map(|i| i as u8).ok_or(-ENOENT);
        let node = match self.node {
            Node::Root => match name {
                "block" => Node::Block,
                "class" => Node::Class,
                "devices" => Node::Devices,
                _ => return Err(-ENOENT),
            },
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
            Node::ClassNet => Node::Net(pos(&NETS)?),
            Node::Net(n) if name == "statistics" => Node::NetStats(n),
            Node::Net(n) => Node::NetFile(n, pos(&NET_FILES)?),
            Node::NetStats(n) => Node::NetStat(n, pos(&NET_STATS)?),
            Node::Devices if name == "system" => Node::System,
            Node::System if name == "cpu" => Node::Cpu,
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
                add("class".into(), Node::Class);
                add("devices".into(), Node::Devices);
            }
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
            Node::Class => add("net".into(), Node::ClassNet),
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
            Node::Devices => add("system".into(), Node::System),
            Node::System => add("cpu".into(), Node::Cpu),
            Node::Cpu => {
                for (i, f) in CPU_FILES.iter().enumerate() {
                    add((*f).into(), Node::CpuFile(i as u8));
                }
                for n in 0..crate::smp::online().max(1) {
                    add(format!("cpu{}", n), Node::CpuN(n as u16));
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

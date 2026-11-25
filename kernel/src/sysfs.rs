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
//   /sys/kernel/cgroup/{delegate,features}           cgroup2 の任せてよいファイルとマウントのオプション (cgroup.rs)
//   /sys/fs/cgroup                                   cgroup2 をかぶせるところ (中は空)
//   /sys/firmware/efi/{fw_platform_size,systab}      UEFI から起動したとき (arm64 の Linux と同じもの。systab は root だけ)
//   /sys/firmware/efi/efivars                        efivarfs をかぶせるところ (efivarfs.rs)
//   /sys/firmware/acpi/tables/SIG                    ACPI で起こしたとき、表そのもの (同じ名前がいくつかなら SSDT1、SSDT2。root だけ)
//   /sys/firmware/acpi/{pm_profile,hotplug/}         Linux と同じ (hotplug/force_remove、container・pci_root・processor/enabled)
//   /sys/module/apparmor/parameters/enabled          Y (aios.off=apparmor なら N。libapparmor の aa_is_enabled が読む)
//   /sys/dev/char/MA:MI、/sys/class/CLASS/NAME       ドライバが add_chrdev で置いた文字デバイス (DRM の card0 と renderD128:
//                                                    libdrm の drmGetDevices2 が読む)。どちらも次へのリンク:
//   /sys/devices/platform/PARENT/{uevent,subsystem}  それをつなぐ装置 (a003e00.virtio_mmio: DTB の名前と、Linux と同じ uevent)
//   /sys/devices/platform/PARENT/CLASS/NAME/{uevent,dev,device,subsystem}
//   /sys/bus/platform                                subsystem のリンクの先 (中は空)
//   /sys/bus/pci/{devices/NAME,drivers,slots,...}    PCI の装置 (pci.rs が見つけたもの。Linux の pci-sysfs.c)。装置は
//   /sys/devices/pci0000:00/0000:00:00.0/...         ACPI ならここ、DTB はホストブリッジの platform の装置の下
//                                                    (/sys/devices/platform/4010000000.pcie/pci0000:00)。ブリッジの下は
//                                                    その中に入れ子。pci_bus/0000:BB と /sys/class/pci_bus も
// 大きさはセクタ (512 バイト)、stat は /proc/diskstats の 4 列目からの 11 列と同じ
use crate::vfs::*;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::any::Any;

const EACCES: i64 = 13;
const EOPNOTSUPP: i64 = 95;
const SYSFS_MAGIC: u64 = 0x6265_6572;

const DISK_FILES: [&str; 5] = ["size", "stat", "dev", "ro", "removable"];
const QUEUE_FILES: [&str; 4] = ["hw_sector_size", "logical_block_size", "physical_block_size", "rotational"];
const PART_FILES: [&str; 6] = ["size", "start", "stat", "dev", "partition", "ro"];
const NET_FILES: [&str; 7] = ["address", "mtu", "operstate", "carrier", "ifindex", "type", "flags"];
const NET_STATS: [&str; 8] = ["rx_bytes", "tx_bytes", "rx_packets", "tx_packets", "rx_errors", "tx_errors", "rx_dropped", "tx_dropped"];
const CPU_FILES: [&str; 3] = ["online", "possible", "present"];
const EFI_FILES: [&str; 2] = ["fw_platform_size", "systab"];
/// /sys/kernel/cgroup のファイル
const CG_FILES: [&str; 2] = ["delegate", "features"];
/// /sys/firmware/acpi/hotplug の下 (Linux の container、pci_root、processor のドライバのもの) と、その enabled
/// (Linux の既定: container は 0、ほかは 1。root が 0 か 1 を書ける。aios はそのまま覚えるだけ)
const ACPI_HOTPLUG: [&str; 3] = ["container", "pci_root", "processor"];
static HOTPLUG_ON: [core::sync::atomic::AtomicBool; 3] =
    [core::sync::atomic::AtomicBool::new(false), core::sync::atomic::AtomicBool::new(true), core::sync::atomic::AtomicBool::new(true)];
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
    /// /sys/fs、/sys/fs/cgroup、/sys/kernel/cgroup とその中のファイル (CG_FILES の番号)
    Fs,
    FsCgroup,
    KernelCgroup,
    KernelCgroupFile(u8),
    Module,
    ModAa,
    ModAaParams,
    ModAaEnabled,
    Firmware,
    Efi,
    EfiFile(u8),
    EfiVars,
    /// /sys/firmware/acpi と tables、その中の表 (acpi::sysfs_tables の番号)
    Acpi,
    AcpiTables,
    AcpiTable(u8),
    /// tables/data と tables/dynamic (Linux が作る空のディレクトリ。0 と 1) と /sys/firmware/acpi/pm_profile
    AcpiSub(u8),
    AcpiPmProfile,
    /// /sys/firmware/acpi/hotplug と force_remove (Linux はもう 1 にできない: 0 だけ)
    AcpiHotplug,
    AcpiForceRemove,
    /// hotplug/container、pci_root、processor (Linux の acpi_sysfs_add_hotplug_profile: ドライバの名前) と enabled
    AcpiHp(u8),
    AcpiHpEnabled(u8),
    /// /sys/dev、/sys/dev/char と、その中のリンク (CHRDEVS の番号)
    Dev,
    DevChar,
    DevCharLink(u8),
    /// /sys/bus と /sys/bus/platform
    Bus,
    BusPlatform,
    /// /sys/bus/event_source (perf の種類: perf.rs の SOURCES) と devices、drivers (空)、ファイル (EV_BUS_FILES)、
    /// devices の中のリンク。/sys/devices/NAME と、そのファイル (EV_DEV_FILES)
    EvBus,
    EvBusDevs,
    EvBusDrivers,
    EvBusFile(u8),
    EvBusLink(u8),
    EvDev(u8),
    EvDevFile(u8, u8),
    /// CPU の PMU (pmu.rs) の cpus と、caps (0)・events (1)・format (2) のディレクトリとその中
    EvPmuCpus,
    EvPmuDir(u8),
    EvPmuEnt(u8, u8),
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
    /// /sys/bus/pci と devices、drivers と slots (0、1: 空)、ファイル (pci::BUS_FILES)、devices の中のリンク (pci::devs の番号)
    BusPci,
    BusPciDevs,
    BusPciSub(u8),
    BusPciFile(u8),
    BusPciLink(u8),
    /// DTB のホストブリッジの platform の装置 (4010000000.pcie) と、そのファイル (HOST_FILES) と subsystem
    PciHost,
    PciHostFile(u8),
    PciHostSubsys,
    /// pci0000:00 と、そのファイル (uevent と、ACPI なら waiting_for_supplier)
    PciRoot,
    PciRootFile(u8),
    /// 装置 (pci::devs の番号)、そのファイル (pci::DEV_FILES)、subsystem、link (ASPM。空のディレクトリ)
    PciDev(u8),
    PciFile(u8, u8),
    PciSubsys(u8),
    PciLinkDir(u8),
    /// BAR のファイル (resourceN、resourceN_wc、rom: pci::bar_file の番号) と、link/ の ASPM (pci::ASPM_FILES の番号)
    PciBar(u8, u8),
    PciAspm(u8, u8),
    /// pci_bus (ルートのバスは ROOT_BUS、ほかはブリッジの装置の番号) と、その中の 0000:BB、ファイル (PCI_BUS_FILES)、
    /// リンク (0 device、1 subsystem)
    PciBusDir(u8),
    PciBus(u8),
    PciBusFile(u8, u8),
    PciBusLink(u8, u8),
    /// /sys/class/pci_bus と、その中のリンク
    ClassPciBus,
    ClassPciBusLink(u8),
}

/// pci_bus の番号でルートのバスを表すもの (装置は 255 より少ない: pci.rs)
const ROOT_BUS: u8 = 255;
/// DTB のホストブリッジの platform の装置のファイル (Linux の driver と of_node のリンクはない: その先を見せていない)
/// /sys/bus/event_source のファイル (名前、mode): Linux の bus_attrs と uevent
const EV_BUS_FILES: [(&str, u32); 3] = [("drivers_autoprobe", 0o644), ("drivers_probe", 0o200), ("uevent", 0o200)];
/// /sys/devices/NAME (perf の種類) のもの: perf_event_mux_interval_ms、subsystem (リンク)、type、uevent
/// perf の種類 k が CPU の PMU か
fn is_pmu(k: u8) -> bool {
    crate::perf::sources().get(k as usize).is_some_and(|s| s.0 == crate::pmu::name() && crate::pmu::present())
}

const EV_DEV_FILES: [(&str, u32); 4] = [("perf_event_mux_interval_ms", 0o644), ("subsystem", 0o777), ("type", 0o444), ("uevent", 0o644)];
const HOST_FILES: [(&str, u32); 3] = [("driver_override", 0o644), ("modalias", 0o444), ("uevent", 0o644)];
/// pci_bus/0000:BB のファイル (waiting_for_supplier は DTB のルートのバスだけ)
const PCI_BUS_FILES: [(&str, u32); 5] = [("cpuaffinity", 0o444), ("cpulistaffinity", 0o444), ("rescan", 0o200), ("uevent", 0o644), ("waiting_for_supplier", 0o444)];

fn pci_dev(i: u8) -> Result<&'static crate::pci::Dev, i64> {
    crate::pci::devs().get(i as usize).ok_or(-ENOENT)
}

/// /sys/devices から pci0000:00 までの道と、その段の数
fn pci_root_path() -> (String, usize) {
    match crate::pci::host() {
        Some(h) => (format!("platform/{}/pci0000:00", h.dir), 3),
        None => ("pci0000:00".into(), 1),
    }
}

/// pci_bus/0000:BB の道 (/sys/devices から) と、/sys からの段の数
fn pci_bus_path(b: u8) -> Option<(String, usize)> {
    let (root, n) = pci_root_path();
    if b == ROOT_BUS {
        return Some((format!("{}/pci_bus/0000:{:02x}", root, crate::pci::root_bus()?), 1 + n + 2));
    }
    let d = pci_dev(b).ok()?;
    let p = crate::pci::path(b as usize);
    Some((format!("{}/pci_bus/0000:{:02x}", p, d.below?.0), 1 + n + crate::pci::depth(b as usize) + 1 + 2))
}

/// その pci_bus にある、または上がその装置の装置 (None はルートのバス)
fn pci_children(up: Option<usize>) -> impl Iterator<Item = usize> {
    crate::pci::devs().iter().enumerate().filter(move |(_, d)| d.up == up).map(|(i, _)| i)
}

fn pci_bus_files(b: u8) -> impl Iterator<Item = usize> {
    (0..PCI_BUS_FILES.len()).filter(move |&k| k != 4 || (b == ROOT_BUS && crate::pci::host().is_some()))
}

fn pci_root_files() -> impl Iterator<Item = (usize, &'static str)> {
    [(0, "uevent"), (1, "waiting_for_supplier")].into_iter().filter(|&(k, _)| k == 0 || crate::pci::host().is_none())
}

/// DTB のノードの uevent の行 (Linux の of_device_uevent)
fn of_uevent(h: &crate::pci::Host) -> String {
    let mut s = format!("OF_NAME={}\nOF_FULLNAME={}\nOF_TYPE={}\n", h.of_name, h.of_full, h.of_type);
    for (i, c) in h.compat.iter().enumerate() {
        s.push_str(&format!("OF_COMPATIBLE_{}={}\n", i, c));
    }
    s.push_str(&format!("OF_COMPATIBLE_N={}\n", h.compat.len()));
    s
}

/// of:NnameTtypeCcompat... (Linux の of_modalias)
fn of_modalias(h: &crate::pci::Host) -> String {
    let mut s = format!("of:N{}T{}", h.of_name, h.of_type);
    for c in &h.compat {
        s.push('C');
        s.push_str(&c.replace(' ', "_"));
    }
    s
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
            Node::Fs => 32,
            Node::FsCgroup => 33,
            Node::KernelCgroup => 34,
            Node::KernelCgroupFile(k) => 35 + k as u64,
            Node::Module => 12,
            Node::ModAa => 13,
            Node::ModAaParams => 14,
            Node::ModAaEnabled => 15,
            Node::Firmware => 16,
            Node::Efi => 17,
            Node::EfiVars => 18,
            Node::Acpi => 25,
            Node::AcpiTables => 26,
            Node::AcpiTable(i) => 0x700 + i as u64,
            Node::AcpiSub(i) => 27 + i as u64,
            Node::AcpiPmProfile => 29,
            Node::AcpiHotplug => 30,
            Node::AcpiForceRemove => 31,
            Node::AcpiHp(i) => 0x780 + 2 * i as u64,
            Node::AcpiHpEnabled(i) => 0x781 + 2 * i as u64,
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
            Node::EvBus => 0xa_0000,
            Node::EvBusDevs => 0xa_0001,
            Node::EvBusDrivers => 0xa_0002,
            Node::EvBusFile(k) => 0xa_0004 + k as u64,
            Node::EvBusLink(k) => 0xa_0010 + k as u64,
            Node::EvDev(k) => 0xa_0100 + ((k as u64) << 4),
            Node::EvDevFile(k, f) => 0xa_0100 + ((k as u64) << 4) + 1 + f as u64,
            Node::EvPmuCpus => 0xa_0f00,
            Node::EvPmuDir(d) => 0xa_0f10 + d as u64,
            Node::EvPmuEnt(d, i) => 0xa_1000 + ((d as u64) << 8) + i as u64,
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
            Node::BusPci => 0x8_0000,
            Node::BusPciDevs => 0x8_0001,
            Node::BusPciSub(k) => 0x8_0002 + k as u64,
            Node::BusPciFile(k) => 0x8_0004 + k as u64,
            Node::PciHost => 0x8_0010,
            Node::PciHostFile(k) => 0x8_0011 + k as u64,
            Node::PciHostSubsys => 0x8_001a,
            Node::PciRoot => 0x8_0020,
            Node::PciRootFile(k) => 0x8_0021 + k as u64,
            Node::ClassPciBus => 0x8_0030,
            Node::PciDev(i) => 0x9_0000 + ((i as u64) << 8),
            Node::PciSubsys(i) => 0x9_0000 + ((i as u64) << 8) + 1,
            Node::PciLinkDir(i) => 0x9_0000 + ((i as u64) << 8) + 2,
            Node::BusPciLink(i) => 0x9_0000 + ((i as u64) << 8) + 3,
            Node::PciBusDir(i) => 0x9_0000 + ((i as u64) << 8) + 4,
            Node::PciBus(i) => 0x9_0000 + ((i as u64) << 8) + 5,
            Node::ClassPciBusLink(i) => 0x9_0000 + ((i as u64) << 8) + 6,
            Node::PciBusLink(i, k) => 0x9_0000 + ((i as u64) << 8) + 8 + k as u64,
            Node::PciBusFile(i, k) => 0x9_0000 + ((i as u64) << 8) + 0x10 + k as u64,
            Node::PciFile(i, k) => 0x9_0000 + ((i as u64) << 8) + 0x40 + k as u64,
            Node::PciBar(i, k) => 0x9_0000 + ((i as u64) << 8) + 0x80 + k as u64,
            Node::PciAspm(i, k) => 0x9_0000 + ((i as u64) << 8) + 0xa0 + k as u64,
        }
    }

    fn is_dir(&self) -> bool {
        matches!(
            self.node,
            Node::Root | Node::Block | Node::Disk | Node::Queue | Node::Part(_) | Node::Class | Node::ClassNet | Node::Net(_) | Node::NetStats(_) | Node::Devices | Node::System | Node::Cpu | Node::Vulns | Node::CpuN(_) | Node::Kernel | Node::Security | Node::Fs | Node::FsCgroup | Node::KernelCgroup | Node::Module | Node::ModAa | Node::ModAaParams | Node::Firmware | Node::Efi | Node::EfiVars | Node::Acpi | Node::AcpiTables | Node::AcpiSub(_) | Node::AcpiHotplug | Node::AcpiHp(_)
                | Node::Dev | Node::DevChar | Node::Bus | Node::BusPlatform | Node::Platform | Node::PlatDev(_) | Node::PlatClass(_) | Node::Chr(_) | Node::ClassOf(_)
                | Node::BusPci | Node::BusPciDevs | Node::BusPciSub(_) | Node::PciHost | Node::PciRoot | Node::PciDev(_) | Node::PciLinkDir(_) | Node::PciBusDir(_) | Node::PciBus(_) | Node::ClassPciBus
                | Node::EvBus | Node::EvBusDevs | Node::EvBusDrivers | Node::EvDev(_) | Node::EvPmuDir(_)
        )
    }

    /// 親のディレクトリ (/sys は /sys)
    fn parent(&self) -> Node {
        let l = || chrdevs();
        match self.node {
            Node::Root | Node::Block | Node::Class | Node::Devices | Node::Kernel | Node::Module | Node::Firmware | Node::Dev | Node::Bus | Node::Fs => Node::Root,
            Node::FsCgroup => Node::Fs,
            Node::KernelCgroup => Node::Kernel,
            Node::KernelCgroupFile(_) => Node::KernelCgroup,
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
            Node::Efi | Node::Acpi => Node::Firmware,
            Node::AcpiTables | Node::AcpiPmProfile | Node::AcpiHotplug => Node::Acpi,
            Node::AcpiForceRemove | Node::AcpiHp(_) => Node::AcpiHotplug,
            Node::AcpiHpEnabled(i) => Node::AcpiHp(i),
            Node::AcpiTable(_) | Node::AcpiSub(_) => Node::AcpiTables,
            Node::EfiFile(_) | Node::EfiVars => Node::Efi,
            Node::DevChar => Node::Dev,
            Node::DevCharLink(_) => Node::DevChar,
            Node::BusPlatform | Node::EvBus => Node::Bus,
            Node::EvBusDevs | Node::EvBusDrivers | Node::EvBusFile(_) => Node::EvBus,
            Node::EvBusLink(_) => Node::EvBusDevs,
            Node::EvDev(_) => Node::Devices,
            Node::EvDevFile(k, _) => Node::EvDev(k),
            // PMU は種類の一覧のはじめ (perf::sources)
            Node::EvPmuCpus | Node::EvPmuDir(_) => Node::EvDev(0),
            Node::EvPmuEnt(d, _) => Node::EvPmuDir(d),
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
            Node::BusPci => Node::Bus,
            Node::BusPciDevs | Node::BusPciSub(_) | Node::BusPciFile(_) => Node::BusPci,
            Node::BusPciLink(_) => Node::BusPciDevs,
            Node::PciHost => Node::Platform,
            Node::PciHostFile(_) | Node::PciHostSubsys => Node::PciHost,
            Node::PciRoot => if crate::pci::host().is_some() { Node::PciHost } else { Node::Devices },
            Node::PciRootFile(_) => Node::PciRoot,
            Node::PciDev(i) => match pci_dev(i).ok().and_then(|d| d.up) {
                Some(u) => Node::PciDev(u as u8),
                None => Node::PciRoot,
            },
            Node::PciFile(i, _) | Node::PciSubsys(i) | Node::PciLinkDir(i) | Node::PciBar(i, _) => Node::PciDev(i),
            Node::PciAspm(i, _) => Node::PciLinkDir(i),
            Node::PciBusDir(b) => if b == ROOT_BUS { Node::PciRoot } else { Node::PciDev(b) },
            Node::PciBus(b) => Node::PciBusDir(b),
            Node::PciBusFile(b, _) | Node::PciBusLink(b, _) => Node::PciBus(b),
            Node::ClassPciBus => Node::Class,
            Node::ClassPciBusLink(_) => Node::ClassPciBus,
            Node::ClassLink(i) => {
                let l = l();
                let c = l.get(i as usize).map(|d| d.class);
                Node::ClassOf((0..l.len()).find(|&j| first_class(&l, j) && Some(l[j].class) == c).unwrap_or(0) as u8)
            }
        }
    }

    /// シンボリックリンクの先 (リンクでなければ None)
    fn link(&self) -> Option<String> {
        let up = |n: usize| "../".repeat(n);
        match self.node {
            Node::BusPciLink(i) => return Some(format!("../../../devices/{}", crate::pci::path(i as usize))),
            Node::EvBusLink(k) => return Some(format!("../../../devices/{}", crate::perf::sources()[k as usize].0)),
            Node::EvDevFile(_, 1) => return Some("../../bus/event_source".into()),
            Node::PciHostSubsys => return Some("../../../bus/platform".into()),
            // /sys/devices/ROOT/装置.../subsystem
            Node::PciSubsys(i) => return Some(format!("{}bus/pci", up(1 + pci_root_path().1 + crate::pci::depth(i as usize) + 1))),
            Node::PciBusLink(b, 0) => {
                return Some(if b == ROOT_BUS { format!("../../../pci0000:{:02x}", crate::pci::root_bus()?) } else { format!("../../../{}", pci_dev(b).ok()?.name()) });
            }
            Node::PciBusLink(b, _) => return Some(format!("{}class/pci_bus", up(pci_bus_path(b)?.1))),
            Node::ClassPciBusLink(b) => return Some(format!("../../devices/{}", pci_bus_path(b)?.0)),
            _ => {}
        }
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
            Node::EvBusFile(0) => "1\n".into(),
            Node::EvBusFile(_) | Node::EvDevFile(_, 3) => String::new(),
            Node::EvDevFile(_, 0) => format!("{}\n", crate::perf::mux_interval_ms()),
            Node::EvDevFile(k, _) => format!("{}\n", crate::perf::sources()[k as usize].1),
            Node::EvPmuCpus => format!("{}\n", crate::pmu::cpus()),
            Node::EvPmuEnt(0, i) => format!("{}\n", crate::pmu::cap(i as usize)),
            Node::EvPmuEnt(1, i) => format!("event=0x{:04x}\n", crate::pmu::events().get(i as usize).ok_or(-ENOENT)?.1),
            Node::EvPmuEnt(_, i) => format!("{}\n", crate::pmu::FORMAT[i as usize].1),
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
            Node::BusPciFile(k) => crate::pci::bus_file(k as usize),
            Node::PciFile(i, f) => pci_dev(i)?.file(f as usize),
            Node::PciAspm(i, k) => crate::pci::aspm_file(i as usize, k as usize).ok_or(-ENOENT)?,
            Node::PciRootFile(k) => if k == 0 { String::new() } else { "0\n".into() },
            Node::PciBusFile(b, k) => match PCI_BUS_FILES[k as usize].0 {
                "cpuaffinity" => crate::pci::cpus(false),
                "cpulistaffinity" => crate::pci::cpus(true),
                "waiting_for_supplier" => "0\n".into(),
                // DTB のルートのバスは、ホストブリッジのノード (Linux の of_device_uevent)
                "uevent" => match crate::pci::host().filter(|_| b == ROOT_BUS) {
                    Some(h) => of_uevent(h),
                    None => String::new(),
                },
                _ => String::new(),
            },
            Node::PciHostFile(k) => {
                let h = crate::pci::host().ok_or(-ENOENT)?;
                match HOST_FILES[k as usize].0 {
                    "driver_override" => "(null)\n".into(),
                    "modalias" => format!("{}\n", of_modalias(h)),
                    _ => format!("DRIVER=pci-host-generic\n{}MODALIAS={}\n", of_uevent(h), of_modalias(h)),
                }
            }
            Node::Vuln(i) => crate::cpuvuln::show(VULN_FILES[i as usize]),
            Node::KernelCgroupFile(0) => crate::cgroup::delegate_text().into(),
            Node::KernelCgroupFile(_) => crate::cgroup::features_text().into(),
            Node::ModAaEnabled => if crate::parts::off(crate::parts::Part::Apparmor) { "N\n".into() } else { "Y\n".into() },
            Node::EfiFile(0) => "64\n".into(),
            // FADT の Preferred_PM_Profile (Linux の acpi_show_profile)
            Node::AcpiPmProfile => format!("{}\n", crate::acpi::pm_profile()),
            Node::AcpiForceRemove => "0\n".into(),
            Node::AcpiHpEnabled(i) => format!("{}\n", HOTPLUG_ON[i as usize].load(core::sync::atomic::Ordering::Relaxed) as u8),
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
            // Linux の /sys のディレクトリは 0755 (kobject のディレクトリ。/proc は 0555)
            _ if self.is_dir() => S_IFDIR | 0o755,
            _ if link.is_some() => S_IFLNK | 0o777,
            Node::EfiFile(1) | Node::AcpiTable(_) => S_IFREG | 0o400,
            // uevent は root が書ける (Linux と同じ形。書いても何もしない)
            Node::PlatUevent(_) | Node::ChrUevent(_) | Node::AcpiForceRemove | Node::AcpiHpEnabled(_) | Node::PciRootFile(0) => S_IFREG | 0o644,
            Node::BusPciFile(k) => S_IFREG | crate::pci::BUS_FILES[k as usize].1,
            Node::PciFile(_, f) => S_IFREG | crate::pci::DEV_FILES[f as usize].1,
            Node::PciBar(..) => S_IFREG | 0o600,
            Node::PciAspm(..) => S_IFREG | 0o644,
            Node::PciHostFile(k) => S_IFREG | HOST_FILES[k as usize].1,
            Node::PciBusFile(_, k) => S_IFREG | PCI_BUS_FILES[k as usize].1,
            Node::EvBusFile(k) => S_IFREG | EV_BUS_FILES[k as usize].1,
            Node::EvDevFile(_, f) => S_IFREG | EV_DEV_FILES[f as usize].1,
            _ => S_IFREG | 0o444,
        };
        let size = match &link {
            _ if self.is_dir() => 0,
            Some(l) => l.len() as u64,
            // BAR のファイルは BAR の大きさ
            None if matches!(self.node, Node::PciBar(..)) => {
                let Node::PciBar(i, k) = self.node else { unreachable!() };
                pci_dev(i).ok().and_then(|d| crate::pci::bar_file(d, k as usize)).map_or(0, |b| b.1)
            }
            // config は設定空間の大きさ (Linux の pci_dev_config_attr_bin_size)
            None if matches!(self.node, Node::PciFile(_, f) if crate::pci::DEV_FILES[f as usize].0 == "config") => {
                let Node::PciFile(i, _) = self.node else { unreachable!() };
                pci_dev(i).map_or(0, |d| d.cfg_size as u64)
            }
            None if matches!(self.node, Node::AcpiTable(_)) => {
                let Node::AcpiTable(i) = self.node else { unreachable!() };
                crate::acpi::sysfs_tables().get(i as usize).map_or(0, |t| t.1.len() as u64)
            }
            None => 4096,
        };
        let now = crate::timer::epoch_ns();
        Meta { ino: self.ino(), mode, nlink: 1, uid: 0, gid: 0, size, rdev: 0, blocks: 0, mtime: now, ctime: now, atime: now }
    }

    fn read_at(&self, off: usize, buf: &mut [u8]) -> Result<usize, i64> {
        if let Node::PciBar(i, k) = self.node {
            return crate::pci::bar_rw(pci_dev(i)?, k as usize, off, buf, false);
        }
        // PCI の設定空間 (Linux の pci_read_config: CAP_SYS_ADMIN がなければ頭の 64 バイト、CardBus は 128)
        if let Node::PciFile(i, f) = self.node
            && crate::pci::DEV_FILES[f as usize].0 == "config"
        {
            let d = pci_dev(i)?;
            let size = if crate::cred::current().capable(crate::cred::CAP_SYS_ADMIN) {
                d.cfg_size
            } else if d.hdr == 2 {
                128
            } else {
                64
            };
            if off >= size {
                return Ok(0);
            }
            let n = buf.len().min(size - off);
            return Ok(d.read_config(off, &mut buf[..n]));
        }
        // ACPI の表はそのままのバイト (Linux の acpi_table_show。root だけ)
        let s;
        let b = if let Node::AcpiTable(i) = self.node {
            if !crate::cred::current().capable(crate::cred::CAP_DAC_READ_SEARCH) {
                return Err(-EACCES);
            }
            crate::acpi::sysfs_tables().get(i as usize).map(|t| t.1).ok_or(-ENOENT)?
        } else {
            s = self.content()?;
            s.as_bytes()
        };
        if off >= b.len() {
            return Ok(0);
        }
        let n = buf.len().min(b.len() - off);
        buf[..n].copy_from_slice(&b[off..off + n]);
        Ok(n)
    }

    fn write_at(&self, off: usize, b: &[u8]) -> Result<usize, i64> {
        match self.node {
            // uevent に書くと Linux はその装置のできごとをもう一度出す (udev の trigger)。aios には udev がないので受けとるだけ
            Node::PlatUevent(_) | Node::ChrUevent(_) | Node::PciRootFile(0) | Node::PciHostFile(2) | Node::PciBusFile(_, 3) => Ok(b.len()),
            // /sys/bus/pci の uevent と rescan、pci_bus と装置の rescan (たどりなおしても同じものしかない: 受けとるだけ)
            Node::BusPciFile(2 | 4) | Node::PciBusFile(_, 2) => Ok(b.len()),
            // /sys/bus/event_source の drivers_autoprobe、drivers_probe、uevent と装置の uevent: 受けとるだけ
            Node::EvBusFile(_) | Node::EvDevFile(_, 3) => Ok(b.len()),
            // perf_event_mux_interval_ms (Linux の perf_event_mux_interval_ms_store: 1 より小さいと EINVAL。まわす
            // ものがないので覚えない)
            Node::EvDevFile(_, 0) => match core::str::from_utf8(b).ok().and_then(|s| s.trim().parse::<i32>().ok()) {
                Some(v) if v >= 1 => Ok(b.len()),
                _ => Err(-EINVAL),
            },
            Node::PciBar(i, k) => {
                let mut v = b.to_vec();
                crate::pci::bar_rw(pci_dev(i)?, k as usize, off, &mut v, true)
            }
            Node::PciAspm(i, k) => crate::pci::aspm_write(i as usize, k as usize, b).map(|_| b.len()).map_err(|_| -EINVAL),
            Node::PciFile(i, f) => match crate::pci::DEV_FILES[f as usize].0 {
                "uevent" | "rescan" => Ok(b.len()),
                // 設定空間に書く (Linux の pci_write_config: CAP_SYS_ADMIN。mode 0644 で root だけが開ける)
                "config" => {
                    if !crate::cred::current().capable(crate::cred::CAP_SYS_ADMIN) {
                        return Err(-EPERM);
                    }
                    Ok(pci_dev(i)?.write_config(off, b))
                }
                _ => Err(-EOPNOTSUPP),
            },
            // Linux の force_remove_store: 0 (false) だけ受けとる
            // Linux の hotplug_enabled_store: kstrtouint (10 進。終わりの改行はよい)、0 でなければ有効
            Node::AcpiHpEnabled(i) => match core::str::from_utf8(b).ok().map(|s| s.strip_suffix('\n').unwrap_or(s)).map(|s| s.strip_prefix('+').unwrap_or(s)).and_then(|s| s.parse::<u32>().ok()) {
                Some(v) => {
                    HOTPLUG_ON[i as usize].store(v != 0, core::sync::atomic::Ordering::Relaxed);
                    Ok(b.len())
                }
                None => Err(-EINVAL),
            },
            Node::AcpiForceRemove => match core::str::from_utf8(b).map(|s| s.trim()) {
                Ok("0" | "n" | "N" | "no" | "off" | "false") => Ok(b.len()),
                _ => Err(-EINVAL),
            },
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
                "fs" => Node::Fs,
                _ => return Err(-ENOENT),
            },
            Node::Fs if name == "cgroup" => Node::FsCgroup,
            Node::Kernel if name == "cgroup" => Node::KernelCgroup,
            Node::KernelCgroup => Node::KernelCgroupFile(pos(&CG_FILES)?),
            Node::Dev if name == "char" => Node::DevChar,
            Node::DevChar => {
                let l = chrdevs();
                let i = l.iter().position(|d| format!("{}:{}", d.major, d.minor) == name).ok_or(-ENOENT)?;
                Node::DevCharLink(i as u8)
            }
            Node::Bus if name == "platform" => Node::BusPlatform,
            Node::Bus if name == "event_source" => Node::EvBus,
            Node::EvBus => match name {
                "devices" => Node::EvBusDevs,
                "drivers" => Node::EvBusDrivers,
                _ => Node::EvBusFile(EV_BUS_FILES.iter().position(|f| f.0 == name).ok_or(-ENOENT)? as u8),
            },
            Node::EvBusDevs => Node::EvBusLink(crate::perf::sources().iter().position(|s| s.0 == name).ok_or(-ENOENT)? as u8),
            Node::Devices if crate::perf::sources().iter().any(|s| s.0 == name) => Node::EvDev(crate::perf::sources().iter().position(|s| s.0 == name).unwrap() as u8),
            Node::EvDev(k) => match name {
                "cpus" if is_pmu(k) => Node::EvPmuCpus,
                "caps" if is_pmu(k) => Node::EvPmuDir(0),
                "events" if is_pmu(k) => Node::EvPmuDir(1),
                "format" if is_pmu(k) => Node::EvPmuDir(2),
                _ => Node::EvDevFile(k, EV_DEV_FILES.iter().position(|f| f.0 == name).ok_or(-ENOENT)? as u8),
            },
            Node::EvPmuDir(0) => Node::EvPmuEnt(0, crate::pmu::CAPS.iter().position(|c| *c == name).ok_or(-ENOENT)? as u8),
            Node::EvPmuDir(1) => Node::EvPmuEnt(1, crate::pmu::events().iter().position(|e| e.0 == name).ok_or(-ENOENT)? as u8),
            Node::EvPmuDir(_) => Node::EvPmuEnt(2, crate::pmu::FORMAT.iter().position(|f| f.0 == name).ok_or(-ENOENT)? as u8),
            Node::Bus if name == "pci" => Node::BusPci,
            Node::BusPci => match name {
                "devices" => Node::BusPciDevs,
                "drivers" => Node::BusPciSub(0),
                "slots" => Node::BusPciSub(1),
                _ => Node::BusPciFile(crate::pci::BUS_FILES.iter().position(|f| f.0 == name).ok_or(-ENOENT)? as u8),
            },
            Node::BusPciDevs => Node::BusPciLink(crate::pci::devs().iter().position(|d| d.name() == name).ok_or(-ENOENT)? as u8),
            Node::Devices if name == "platform" && (!chrdevs().is_empty() || crate::pci::host().is_some()) => Node::Platform,
            Node::Devices if crate::pci::host().is_none() && crate::pci::root_bus().is_some_and(|b| name == format!("pci0000:{:02x}", b)) => Node::PciRoot,
            Node::Platform if crate::pci::host().is_some_and(|h| h.dir == name) => Node::PciHost,
            Node::PciHost => match name {
                "subsystem" => Node::PciHostSubsys,
                _ if crate::pci::root_bus().is_some_and(|b| name == format!("pci0000:{:02x}", b)) => Node::PciRoot,
                _ => Node::PciHostFile(HOST_FILES.iter().position(|f| f.0 == name).ok_or(-ENOENT)? as u8),
            },
            Node::PciRoot | Node::PciDev(_) => {
                let (up, bus) = match self.node {
                    Node::PciDev(i) => (Some(i as usize), i),
                    _ => (None, ROOT_BUS),
                };
                if let Some(j) = pci_children(up).find(|&j| crate::pci::devs()[j].name() == name) {
                    Node::PciDev(j as u8)
                } else if name == "pci_bus" && (up.is_none() || pci_dev(bus)?.below.is_some()) {
                    Node::PciBusDir(bus)
                } else if let Node::PciDev(i) = self.node {
                    let d = pci_dev(i)?;
                    match name {
                        "subsystem" => Node::PciSubsys(i),
                        "link" => Node::PciLinkDir(i),
                        _ if (0..13).any(|k| crate::pci::bar_file(d, k).is_some_and(|b| b.0 == name)) => {
                            Node::PciBar(i, (0..13).find(|&k| crate::pci::bar_file(d, k).is_some_and(|b| b.0 == name)).unwrap_or(0) as u8)
                        }
                        _ => Node::PciFile(i, (0..crate::pci::DEV_FILES.len()).find(|&f| crate::pci::DEV_FILES[f].0 == name && crate::pci::visible(i as usize, f)).ok_or(-ENOENT)? as u8),
                    }
                } else {
                    Node::PciRootFile(pci_root_files().find(|f| f.1 == name).ok_or(-ENOENT)?.0 as u8)
                }
            }
            Node::PciLinkDir(i) => {
                let k = crate::pci::ASPM_FILES.iter().position(|f| f.0 == name).ok_or(-ENOENT)?;
                crate::pci::aspm_file(i as usize, k).ok_or(-ENOENT)?;
                Node::PciAspm(i, k as u8)
            }
            Node::PciBusDir(b) if pci_bus_path(b).is_some_and(|p| p.0.ends_with(&format!("/{}", name))) => Node::PciBus(b),
            Node::PciBus(b) => match name {
                "device" => Node::PciBusLink(b, 0),
                "subsystem" => Node::PciBusLink(b, 1),
                _ => Node::PciBusFile(b, pci_bus_files(b).find(|&k| PCI_BUS_FILES[k].0 == name).ok_or(-ENOENT)? as u8),
            },
            Node::Class if name == "pci_bus" && crate::pci::root_bus().is_some() => Node::ClassPciBus,
            Node::ClassPciBus => {
                let b = if crate::pci::root_bus().is_some_and(|r| name == format!("0000:{:02x}", r)) {
                    ROOT_BUS
                } else {
                    crate::pci::devs().iter().position(|d| d.below.is_some_and(|(sec, _)| name == format!("0000:{:02x}", sec))).ok_or(-ENOENT)? as u8
                };
                Node::ClassPciBusLink(b)
            }
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
            Node::Firmware if name == "acpi" && crate::acpi::present() => Node::Acpi,
            Node::Acpi if name == "tables" => Node::AcpiTables,
            Node::Acpi if name == "pm_profile" => Node::AcpiPmProfile,
            Node::Acpi if name == "hotplug" => Node::AcpiHotplug,
            Node::AcpiHotplug if name == "force_remove" => Node::AcpiForceRemove,
            Node::AcpiHotplug => Node::AcpiHp(pos(&ACPI_HOTPLUG)?),
            Node::AcpiHp(i) if name == "enabled" => Node::AcpiHpEnabled(i),
            Node::AcpiTables if name == "data" => Node::AcpiSub(0),
            Node::AcpiTables if name == "dynamic" => Node::AcpiSub(1),
            Node::AcpiTables => Node::AcpiTable(crate::acpi::sysfs_tables().iter().position(|(n, _)| n == name).ok_or(-ENOENT)? as u8),
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
                add("fs".into(), Node::Fs);
            }
            Node::Fs => add("cgroup".into(), Node::FsCgroup),
            Node::FsCgroup => {}
            Node::KernelCgroup => {
                for (k, n) in CG_FILES.iter().enumerate() {
                    add((*n).into(), Node::KernelCgroupFile(k as u8));
                }
            }
            Node::Firmware => {
                if crate::acpi::present() {
                    add("acpi".into(), Node::Acpi);
                }
                if crate::efirt::available() {
                    add("efi".into(), Node::Efi);
                }
            }
            Node::Acpi => {
                add("hotplug".into(), Node::AcpiHotplug);
                add("pm_profile".into(), Node::AcpiPmProfile);
                add("tables".into(), Node::AcpiTables);
            }
            Node::AcpiHotplug => {
                for (i, n) in ACPI_HOTPLUG.iter().enumerate() {
                    add((*n).into(), Node::AcpiHp(i as u8));
                }
                add("force_remove".into(), Node::AcpiForceRemove);
            }
            Node::AcpiHp(i) => add("enabled".into(), Node::AcpiHpEnabled(i)),
            Node::AcpiSub(_) => {}
            Node::AcpiTables => {
                add("data".into(), Node::AcpiSub(0));
                add("dynamic".into(), Node::AcpiSub(1));
                for (i, (n, _)) in crate::acpi::sysfs_tables().into_iter().enumerate() {
                    add(n, Node::AcpiTable(i as u8));
                }
            }
            Node::Efi => {
                for (i, f) in EFI_FILES.iter().enumerate() {
                    add((*f).into(), Node::EfiFile(i as u8));
                }
                add("efivars".into(), Node::EfiVars);
            }
            Node::EfiVars => {}
            Node::Kernel => {
                add("security".into(), Node::Security);
                add("cgroup".into(), Node::KernelCgroup);
            }
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
                if crate::pci::root_bus().is_some() {
                    add("pci_bus".into(), Node::ClassPciBus);
                }
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
            Node::Bus => {
                add("event_source".into(), Node::EvBus);
                add("pci".into(), Node::BusPci);
                add("platform".into(), Node::BusPlatform);
            }
            Node::EvBus => {
                add("devices".into(), Node::EvBusDevs);
                add("drivers".into(), Node::EvBusDrivers);
                for (k, f) in EV_BUS_FILES.iter().enumerate() {
                    add(f.0.into(), Node::EvBusFile(k as u8));
                }
            }
            Node::EvBusDrivers => {}
            Node::EvBusDevs => {
                for (k, s) in crate::perf::sources().iter().enumerate() {
                    add(s.0.into(), Node::EvBusLink(k as u8));
                }
            }
            Node::EvDev(k) => {
                if is_pmu(k) {
                    add("caps".into(), Node::EvPmuDir(0));
                    add("cpus".into(), Node::EvPmuCpus);
                    add("events".into(), Node::EvPmuDir(1));
                    add("format".into(), Node::EvPmuDir(2));
                }
                for (f, e) in EV_DEV_FILES.iter().enumerate() {
                    add(e.0.into(), Node::EvDevFile(k, f as u8));
                }
            }
            Node::EvPmuDir(0) => {
                for (i, c) in crate::pmu::CAPS.iter().enumerate() {
                    add((*c).into(), Node::EvPmuEnt(0, i as u8));
                }
            }
            Node::EvPmuDir(1) => {
                for (i, e) in crate::pmu::events().iter().enumerate() {
                    add(e.0.into(), Node::EvPmuEnt(1, i as u8));
                }
            }
            Node::EvPmuDir(_) => {
                for (i, f) in crate::pmu::FORMAT.iter().enumerate() {
                    add(f.0.into(), Node::EvPmuEnt(2, i as u8));
                }
            }
            Node::BusPlatform | Node::BusPciSub(_) => {}
            Node::PciLinkDir(i) => {
                for (k, f) in crate::pci::ASPM_FILES.iter().enumerate() {
                    if crate::pci::aspm_file(i as usize, k).is_some() {
                        add(f.0.into(), Node::PciAspm(i, k as u8));
                    }
                }
            }
            Node::BusPci => {
                add("devices".into(), Node::BusPciDevs);
                add("drivers".into(), Node::BusPciSub(0));
                add("slots".into(), Node::BusPciSub(1));
                for (k, f) in crate::pci::BUS_FILES.iter().enumerate() {
                    add(f.0.into(), Node::BusPciFile(k as u8));
                }
            }
            Node::BusPciDevs => {
                for (i, d) in crate::pci::devs().iter().enumerate() {
                    add(d.name(), Node::BusPciLink(i as u8));
                }
            }
            Node::PciHost => {
                for (k, f) in HOST_FILES.iter().enumerate() {
                    add(f.0.into(), Node::PciHostFile(k as u8));
                }
                add("subsystem".into(), Node::PciHostSubsys);
                if let Some(b) = crate::pci::root_bus() {
                    add(format!("pci0000:{:02x}", b), Node::PciRoot);
                }
            }
            Node::PciRoot | Node::PciDev(_) => {
                let up = match self.node {
                    Node::PciDev(i) => Some(i as usize),
                    _ => None,
                };
                for j in pci_children(up) {
                    add(crate::pci::devs()[j].name(), Node::PciDev(j as u8));
                }
                match self.node {
                    Node::PciDev(i) => {
                        let d = pci_dev(i)?;
                        for f in (0..crate::pci::DEV_FILES.len()).filter(|&f| crate::pci::visible(i as usize, f)) {
                            add(crate::pci::DEV_FILES[f].0.into(), Node::PciFile(i, f as u8));
                        }
                        for k in 0..13 {
                            if let Some((n, _)) = crate::pci::bar_file(d, k) {
                                add(n, Node::PciBar(i, k as u8));
                            }
                        }
                        add("subsystem".into(), Node::PciSubsys(i));
                        add("link".into(), Node::PciLinkDir(i));
                        if d.below.is_some() {
                            add("pci_bus".into(), Node::PciBusDir(i));
                        }
                    }
                    _ => {
                        for (k, f) in pci_root_files() {
                            add(f.into(), Node::PciRootFile(k as u8));
                        }
                        add("pci_bus".into(), Node::PciBusDir(ROOT_BUS));
                    }
                }
            }
            Node::PciBusDir(b) => {
                if let Some((p, _)) = pci_bus_path(b) {
                    add(p.rsplit('/').next().unwrap_or("").into(), Node::PciBus(b));
                }
            }
            Node::PciBus(b) => {
                for k in pci_bus_files(b) {
                    add(PCI_BUS_FILES[k].0.into(), Node::PciBusFile(b, k as u8));
                }
                add("device".into(), Node::PciBusLink(b, 0));
                add("subsystem".into(), Node::PciBusLink(b, 1));
            }
            Node::ClassPciBus => {
                if let Some(r) = crate::pci::root_bus() {
                    add(format!("0000:{:02x}", r), Node::ClassPciBusLink(ROOT_BUS));
                }
                for (i, d) in crate::pci::devs().iter().enumerate() {
                    if let Some((sec, _)) = d.below {
                        add(format!("0000:{:02x}", sec), Node::ClassPciBusLink(i as u8));
                    }
                }
            }
            Node::Platform => {
                let l = chrdevs();
                for i in (0..l.len()).filter(|&i| first_parent(&l, i)) {
                    add(l[i].parent.clone(), Node::PlatDev(i as u8));
                }
                if let Some(h) = crate::pci::host() {
                    add(h.dir.clone(), Node::PciHost);
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
                // perf の種類 (Linux の perf_pmu_register が /sys/devices/NAME を作る)
                for (k, s) in crate::perf::sources().iter().enumerate() {
                    add(s.0.into(), Node::EvDev(k as u8));
                }
                if !chrdevs().is_empty() || crate::pci::host().is_some() {
                    add("platform".into(), Node::Platform);
                }
                // ACPI: ルートのバスは /sys/devices のすぐ下
                if let Some(b) = crate::pci::root_bus().filter(|_| crate::pci::host().is_none()) {
                    add(format!("pci0000:{:02x}", b), Node::PciRoot);
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

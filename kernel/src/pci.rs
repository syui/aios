// PCI と PCIe: ホストブリッジの設定空間 (ECAM) を写し、バスをたどって装置を見つける (Linux の drivers/pci/probe.c と
// ecam.c。ECAM の場所は DTB の pci-host-ecam-generic (pci-host-generic) か、ACPI の MCFG)。ブリッジの下のバスもたどり、
// 番号がなければつける (DTB は Linux の PCI_REASSIGN_ALL_BUS と同じくいつも。ACPI はファームウェアのものを使う)。
// 見つけたものは /sys/bus/pci と /sys/devices/pci0000:00 (sysfs.rs) に。BAR の割り当てとドライバはまだない
// (aios の virtio は virtio-mmio)
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// 見つけた装置 (見つけた順: Linux と同じく、バスごとに装置の番号の順、ブリッジの下はそのバスのあと)
pub struct Dev {
    pub bus: u8,
    pub slot: u8,
    pub func: u8,
    /// 設定空間 (写した仮想アドレス)
    cfg: usize,
    pub vendor: u16,
    pub device: u16,
    /// 3 バイト (基本の種類、下の種類、プログラムの形)
    pub class: u32,
    pub revision: u8,
    /// 頭の形 (0 ふつう、1 PCI ブリッジ、2 CardBus。多機能の印は除く)
    pub hdr: u8,
    pub subvendor: u16,
    pub subdevice: u16,
    /// 設定空間の大きさ (256 か 4096: Linux の pci_cfg_space_size)
    pub cfg_size: usize,
    /// PCIe なら、その種類 (PCIe の capability の 4..7 ビット)
    pub pcie: Option<u8>,
    /// 上のブリッジ (DEVS の番号)。ルートのバスなら None
    pub up: Option<usize>,
    /// ブリッジなら、下のバスの番号 (secondary, subordinate)
    pub below: Option<(u8, u8)>,
}

/// ECAM の窓 (bus start..=end)
#[derive(Clone, Copy)]
struct Ecam {
    va: usize,
    start: u8,
    end: u8,
}

impl Ecam {
    fn at(&self, bus: u8, dev: u8, func: u8) -> Option<usize> {
        (self.start..=self.end).contains(&bus).then(|| self.va + (((bus - self.start) as usize) << 20 | (dev as usize) << 15 | (func as usize) << 12))
    }
}

static DEVS: crate::cell::Once<Vec<Dev>> = crate::cell::Once::new();
/// ルートのバスの番号 (pci0000:BB)。ホストブリッジがなければ None
static ROOT: crate::cell::Once<u8> = crate::cell::Once::new();

/// DTB のホストブリッジ (Linux はその platform の装置の下に pci0000:00 を置く: /sys/devices/platform/4010000000.pcie)
pub struct Host {
    /// 4010000000.pcie
    pub dir: String,
    /// DTB のノードの名前 (pcie)、道 (/pcie@10000000)、device_type (pci)、compatible
    pub of_name: String,
    pub of_full: String,
    pub of_type: String,
    pub compat: Vec<String>,
}

static HOST: crate::cell::Once<Host> = crate::cell::Once::new();

/// DTB で見つけたホストブリッジ (ACPI なら None: pci0000:00 は /sys/devices のすぐ下)
pub fn host() -> Option<&'static Host> {
    HOST.get()
}

pub fn devs() -> &'static [Dev] {
    DEVS.get().map_or(&[], |v| v.as_slice())
}

/// ルートのバス (ホストブリッジを見つけたときだけ)
pub fn root_bus() -> Option<u8> {
    ROOT.get().copied()
}

fn rd32(va: usize, off: usize) -> u32 {
    unsafe { core::ptr::read_volatile((va + (off & !3)) as *const u32) }
}

fn rd16(va: usize, off: usize) -> u16 {
    (rd32(va, off) >> ((off & 2) * 8)) as u16
}

fn rd8(va: usize, off: usize) -> u8 {
    (rd32(va, off) >> ((off & 3) * 8)) as u8
}

fn wr32(va: usize, off: usize, v: u32) {
    unsafe { core::ptr::write_volatile((va + (off & !3)) as *mut u32, v) }
}

/// capability の並びから id のものの位置 (Linux の pci_find_capability)
fn find_cap(cfg: usize, hdr: u8, id: u8) -> Option<usize> {
    if rd16(cfg, 0x06) & (1 << 4) == 0 {
        return None;
    }
    let mut pos = rd8(cfg, if hdr == 2 { 0x14 } else { 0x34 }) as usize;
    // 輪になったものでも止まるように (Linux の PCI_FIND_CAP_TTL)
    for _ in 0..48 {
        if pos < 0x40 {
            return None;
        }
        pos &= !3;
        let v = rd16(cfg, pos);
        if v as u8 == 0xff {
            return None;
        }
        if v as u8 == id {
            return Some(pos);
        }
        pos = (v >> 8) as usize;
    }
    None
}

/// 256 から上 (PCIe の広げた設定空間) が読めるか (Linux の pci_cfg_space_size_ext と pci_ext_cfg_is_aliased)
fn ext_ok(cfg: usize) -> bool {
    let v = rd32(cfg, 0x100);
    v != 0xffff_ffff && !(0..0x100).step_by(4).all(|o| rd32(cfg, o) == rd32(cfg, 0x100 + o))
}

fn read_dev(cfg: usize, bus: u8, slot: u8, func: u8, up: Option<usize>, ext: bool) -> Dev {
    let id = rd32(cfg, 0);
    let cr = rd32(cfg, 0x08);
    let hdr = rd8(cfg, 0x0e) & 0x7f;
    let pcie = find_cap(cfg, hdr, 0x10).map(|p| (rd16(cfg, p + 2) >> 4) as u8 & 0xf);
    let (subvendor, subdevice) = match hdr {
        0 => (rd16(cfg, 0x2c), rd16(cfg, 0x2e)),
        // ブリッジは SSVID の capability から (Linux の pci_setup_device)
        1 => find_cap(cfg, hdr, 0x0d).map_or((0, 0), |p| (rd16(cfg, p + 4), rd16(cfg, p + 6))),
        2 => (rd16(cfg, 0x40), rd16(cfg, 0x42)),
        _ => (0, 0),
    };
    let class = cr >> 8;
    let cfg_size = if ext && (class >> 8 == 0x0600 || pcie.is_some()) && ext_ok(cfg) { 4096 } else { 256 };
    Dev {
        bus,
        slot,
        func,
        cfg,
        vendor: id as u16,
        device: (id >> 16) as u16,
        class,
        revision: cr as u8,
        hdr,
        subvendor,
        subdevice,
        cfg_size,
        pcie,
        up,
        below: None,
    }
}

/// Linux の pci_type_str (見つけたときの kmsg)
fn type_str(d: &Dev) -> &'static str {
    const PCIE: [&str; 11] = [
        "PCIe Endpoint",
        "PCIe Legacy Endpoint",
        "PCIe unknown",
        "PCIe unknown",
        "PCIe Root Port",
        "PCIe Switch Upstream Port",
        "PCIe Switch Downstream Port",
        "PCIe to PCI/PCI-X bridge",
        "PCI/PCI-X to PCIe bridge",
        "PCIe Root Complex Integrated Endpoint",
        "PCIe Root Complex Event Collector",
    ];
    match (d.pcie, d.hdr) {
        (Some(t), _) => PCIE.get(t as usize).copied().unwrap_or("PCIe unknown"),
        (None, 0) => "conventional PCI endpoint",
        (None, 1) => "conventional PCI bridge",
        (None, 2) => "CardBus bridge",
        _ => "conventional PCI",
    }
}

/// bus の装置をみな見て、そのあとブリッジの下をたどる (Linux の pci_scan_child_bus_extend)。いちばん大きなバスの番号を返す。
/// ext: このバスで 256 から上が読めるか (上がふつうの PCI ブリッジなら読めない)、only0: 装置 0 だけ (PCIe の
/// ルートポートと下りのポートの下)、assign: ブリッジの番号をつけなおす
fn scan_bus(e: &Ecam, bus: u8, up: Option<usize>, ext: bool, only0: bool, assign: bool, out: &mut Vec<Dev>) -> u8 {
    let first = out.len();
    for d in 0..if only0 { 1 } else { 32 } {
        for f in 0..8 {
            let Some(cfg) = e.at(bus, d, f) else { return bus };
            let id = rd32(cfg, 0);
            // ないもの (Linux の pci_bus_generic_read_dev_vendor_id: すべて 1 か 0、片方だけ 1 か 0)
            if matches!(id, 0xffff_ffff | 0 | 0x0000_ffff | 0xffff_0000) {
                if f == 0 {
                    break;
                }
                continue;
            }
            let multi = rd8(cfg, 0x0e) & 0x80 != 0;
            let dev = read_dev(cfg, bus, d, f, up, ext);
            println!("pci 0000:{:02x}:{:02x}.{}: [{:04x}:{:04x}] type {:02x} class {:#08x} {}", bus, d, f, dev.vendor, dev.device, dev.hdr, dev.class, type_str(&dev));
            out.push(dev);
            // sysfs の番号は u8 (255 はルートのバス)
            if out.len() >= 255 {
                return bus;
            }
            if f == 0 && !multi {
                break;
            }
        }
    }
    let mut max = bus;
    for i in first..out.len() {
        if out[i].hdr != 1 {
            continue;
        }
        let cfg = out[i].cfg;
        let buses = rd32(cfg, 0x18);
        let (sec, sub) = ((buses >> 8) as u8, (buses >> 16) as u8);
        // ファームウェアがつけた番号をそのまま使うか、いちばん大きなものの次をつける
        let keep = !assign && sec > bus && sub >= sec;
        let sec = if keep {
            sec
        } else {
            let Some(s) = max.checked_add(1) else { break };
            // 下をたどるあいだは subordinate を 0xff に (Linux の pci_scan_bridge_extend)
            wr32(cfg, 0x18, (buses & 0xff00_0000) | bus as u32 | (s as u32) << 8 | 0xff << 16);
            s
        };
        let child_ext = ext && out[i].pcie.is_some_and(|t| t != 7);
        let only0 = out[i].pcie.is_some_and(|t| t == 4 || t == 6);
        let m = scan_bus(e, sec, Some(i), child_ext, only0, assign, out);
        let sub = if keep {
            sub.max(m)
        } else {
            wr32(cfg, 0x18, (buses & 0xff00_0000) | bus as u32 | (sec as u32) << 8 | (m as u32) << 16);
            m
        };
        out[i].below = Some((sec, sub));
        max = max.max(sub);
    }
    max
}

/// ECAM の場所: DTB の pci-host-ecam-generic (reg と bus-range) か、ACPI の MCFG (区分 0 の最初のもの)。
/// (物理アドレス, 大きさ, はじめのバス, 終わりのバス, どこから)。DTB なら HOST も
fn find_ecam() -> Option<(u64, u64, u8, u8, &'static str)> {
    if crate::dtb::present() {
        return crate::dtb::scan(|n| {
            if !n.is_compatible("pci-host-ecam-generic") || !n.enabled() {
                return None;
            }
            let (pa, size) = n.reg(0)?;
            let (s, e) = match n.prop("bus-range") {
                Some(b) if b.len() >= 8 => (u32::from_be_bytes([b[0], b[1], b[2], b[3]]), u32::from_be_bytes([b[4], b[5], b[6], b[7]])),
                _ => (0, 0xff),
            };
            // Linux の of_device_make_bus_id: 番地と、@ の前の名前
            let name = n.name().split('@').next().unwrap_or("").into();
            let compat = n.prop("compatible").unwrap_or(&[]).split(|&c| c == 0).filter(|c| !c.is_empty()).map(|c| String::from_utf8_lossy(c).into_owned()).collect();
            HOST.set(Host { dir: format!("{:x}.{}", pa, name), of_name: name, of_full: n.full_name(), of_type: n.str("device_type").unwrap_or("<NULL>").into(), compat });
            Some((pa, size, s.min(0xff) as u8, e.min(0xff) as u8, "pci-host-generic"))
        });
    }
    let tables = crate::acpi::sysfs_tables();
    let t = tables.iter().find(|(n, _)| n == "MCFG")?.1;
    let u64_at = |o: usize| t.get(o..o + 8).map(|b| u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]));
    (44..t.len()).step_by(16).find_map(|o| {
        let base = u64_at(o)?;
        let seg = u16::from_le_bytes([*t.get(o + 8)?, *t.get(o + 9)?]);
        let (s, e) = (*t.get(o + 10)?, *t.get(o + 11)?);
        // MCFG の番地はバス 0 のもの
        (seg == 0 && e >= s).then(|| (base + ((s as u64) << 20), ((e - s) as u64 + 1) << 20, s, e, "MCFG"))
    })
}

pub fn init() {
    let Some((pa, size, start, end, from)) = find_ecam() else { return };
    // 窓に入るバスだけ (QEMU の下のほうの ECAM は 16 MiB: 16 本)
    let end = end.min(start.saturating_add(((size >> 20).max(1) - 1).min(0xff) as u8));
    let Some(va) = crate::memlayout::map_device(pa as usize, (((end - start) as usize) + 1) << 20) else {
        println!("pci: {}: cannot map ECAM at {:#x}", from, pa);
        return;
    };
    println!("{}: ECAM at [mem {:#x}-{:#x}] for [bus {:02x}-{:02x}]", from, pa, pa + ((((end - start) as u64) + 1) << 20) - 1, start, end);
    let e = Ecam { va, start, end };
    let mut out = Vec::new();
    // DTB は Linux の pci-host-generic と同じく、ブリッジの番号をいつもつけなおす (ACPI はファームウェアのもの)
    scan_bus(&e, start, None, true, false, crate::dtb::present(), &mut out);
    ROOT.set(start);
    DEVS.set(out);
}

impl Dev {
    /// 0000:BB:DD.F
    pub fn name(&self) -> String {
        format!("0000:{:02x}:{:02x}.{}", self.bus, self.slot, self.func)
    }

    /// 設定空間を読む (off から。cfg_size まで)
    pub fn read_config(&self, off: usize, buf: &mut [u8]) -> usize {
        let n = buf.len().min(self.cfg_size.saturating_sub(off));
        for (i, b) in buf[..n].iter_mut().enumerate() {
            *b = rd8(self.cfg, off + i);
        }
        n
    }
}

/// /sys/devices からの道 (pci0000:00/0000:00:01.0/0000:01:00.0。DTB は platform/4010000000.pcie/pci0000:00/...)
pub fn path(i: usize) -> String {
    let d = &devs()[i];
    match (d.up, host()) {
        (Some(u), _) => format!("{}/{}", path(u), d.name()),
        (None, Some(h)) => format!("platform/{}/pci0000:{:02x}/{}", h.dir, d.bus, d.name()),
        (None, None) => format!("pci0000:{:02x}/{}", d.bus, d.name()),
    }
}

/// 上のブリッジの数 (ルートのバスなら 0)
pub fn depth(i: usize) -> usize {
    devs()[i].up.map_or(0, |u| 1 + depth(u))
}

// ---- /sys に見せるもの (sysfs.rs が木の形を、ここが中身を。Linux の drivers/pci/pci-sysfs.c と bus.c) ----

/// /sys/bus/pci のファイルと、その許可 (Linux の pci_bus_type の bus_groups と drivers_autoprobe など)
pub const BUS_FILES: [(&str, u32); 5] = [("drivers_autoprobe", 0o644), ("drivers_probe", 0o200), ("rescan", 0o200), ("resource_alignment", 0o644), ("uevent", 0o200)];

pub fn bus_file(i: usize) -> String {
    if i == 0 { "1\n".into() } else { String::new() }
}

/// 装置のディレクトリのファイルと許可 (Linux 7.4 の pci_dev_attrs ほか。CONFIG_PM と CONFIG_NUMA のないもの)。
/// visible で、その装置にあるか
pub const DEV_FILES: [(&str, u32); 31] = [
    ("ari_enabled", 0o444),
    ("broken_parity_status", 0o644),
    ("class", 0o444),
    ("config", 0o644),
    ("consistent_dma_mask_bits", 0o444),
    ("current_link_speed", 0o444),
    ("current_link_width", 0o444),
    ("device", 0o444),
    ("devspec", 0o444),
    ("dma_mask_bits", 0o444),
    ("driver_override", 0o644),
    ("enable", 0o644),
    ("irq", 0o444),
    ("local_cpulist", 0o444),
    ("local_cpus", 0o444),
    ("max_link_speed", 0o444),
    ("max_link_width", 0o444),
    ("modalias", 0o444),
    ("msi_bus", 0o644),
    ("power_state", 0o444),
    ("remove", 0o220),
    ("rescan", 0o200),
    ("reset_subordinate", 0o200),
    ("resource", 0o444),
    ("revision", 0o444),
    ("secondary_bus_number", 0o444),
    ("subordinate_bus_number", 0o444),
    ("subsystem_device", 0o444),
    ("subsystem_vendor", 0o444),
    ("uevent", 0o644),
    ("vendor", 0o444),
];

/// その装置にそのファイルがあるか (Linux の is_visible: リンクのものは PCIe だけ、バスの番号はブリッジだけ)
pub fn visible(d: &Dev, f: usize) -> bool {
    match DEV_FILES[f].0 {
        "current_link_speed" | "current_link_width" | "max_link_speed" | "max_link_width" => d.pcie.is_some(),
        "secondary_bus_number" | "subordinate_bus_number" | "reset_subordinate" => d.below.is_some(),
        _ => true,
    }
}

/// オンラインの CPU の印 (Linux の %*pb と %*pbl: NUMA がないので、みな)
pub fn cpus(list: bool) -> String {
    let n = crate::smp::online().max(1);
    if list {
        if n == 1 { "0\n".into() } else { format!("0-{}\n", n - 1) }
    } else {
        format!("{:x}\n", (1u64 << n) - 1)
    }
}

/// PCIe のリンクの速さ (Linux の pci_speed_string)
fn speed(v: u32) -> &'static str {
    match v {
        1 => "2.5 GT/s PCIe",
        2 => "5.0 GT/s PCIe",
        3 => "8.0 GT/s PCIe",
        4 => "16.0 GT/s PCIe",
        5 => "32.0 GT/s PCIe",
        6 => "64.0 GT/s PCIe",
        _ => "Unknown",
    }
}

impl Dev {
    pub fn modalias(&self) -> String {
        format!("pci:v{:08X}d{:08X}sv{:08X}sd{:08X}bc{:02X}sc{:02X}i{:02X}", self.vendor, self.device, self.subvendor, self.subdevice, self.class >> 16, (self.class >> 8) as u8, self.class as u8)
    }

    /// PCIe の capability の位置
    fn pcie_cap(&self) -> Option<usize> {
        find_cap(self.cfg, self.hdr, 0x10)
    }

    /// 装置のファイルの中身 (config と書くだけのものは別)
    pub fn file(&self, f: usize) -> String {
        let lnk = |o: usize| self.pcie_cap().map_or(0, |p| rd32(self.cfg, p + o));
        match DEV_FILES[f].0 {
            "class" => format!("{:#08x}\n", self.class),
            "vendor" => format!("{:#06x}\n", self.vendor),
            "device" => format!("{:#06x}\n", self.device),
            "subsystem_vendor" => format!("{:#06x}\n", self.subvendor),
            "subsystem_device" => format!("{:#06x}\n", self.subdevice),
            "revision" => format!("{:#04x}\n", self.revision),
            "modalias" => format!("{}\n", self.modalias()),
            "uevent" => format!(
                "PCI_CLASS={:X}\nPCI_ID={:04X}:{:04X}\nPCI_SUBSYS_ID={:04X}:{:04X}\nPCI_SLOT_NAME={}\nMODALIAS={}\n",
                self.class,
                self.vendor,
                self.device,
                self.subvendor,
                self.subdevice,
                self.name(),
                self.modalias()
            ),
            // ドライバがないので割り込みはまだ (Linux の pci_read_irq: ピンがあれば INTERRUPT_LINE の値)
            "irq" => format!("{}\n", if rd8(self.cfg, 0x3d) != 0 { rd8(self.cfg, 0x3c) } else { 0 }),
            "local_cpus" => cpus(false),
            "local_cpulist" => cpus(true),
            // ドライバが決めるまでの既定 (Linux の pci_setup_device: 32 ビット)
            "dma_mask_bits" | "consistent_dma_mask_bits" => "32\n".into(),
            "driver_override" => "(null)\n".into(),
            "msi_bus" => "1\n".into(),
            "power_state" => "D0\n".into(),
            "devspec" => String::new(),
            // LNKCAP2 の速さの並びがあればその上 (Linux の pcie_get_speed_cap)、なければ LNKCAP
            "max_link_speed" => {
                let cap2 = lnk(0x2c) >> 1 & 0x7f;
                format!("{}\n", speed(if cap2 != 0 { 32 - cap2.leading_zeros() } else { lnk(0x0c) & 0xf }))
            }
            "max_link_width" => format!("{}\n", lnk(0x0c) >> 4 & 0x3f),
            "current_link_speed" => format!("{}\n", speed(lnk(0x10) >> 16 & 0xf)),
            "current_link_width" => format!("{}\n", lnk(0x10) >> 20 & 0x3f),
            "secondary_bus_number" => format!("{}\n", self.below.map_or(0, |b| b.0)),
            "subordinate_bus_number" => format!("{}\n", self.below.map_or(0, |b| b.1)),
            "resource" => {
                // BAR はまだ割り当てない: みな 0 (Linux の resource_show。ブリッジは窓の 4 つも)
                let n = if self.below.is_some() { 11 } else { 7 };
                "0x0000000000000000 0x0000000000000000 0x0000000000000000\n".repeat(n)
            }
            // ari_enabled、broken_parity_status、enable
            _ => "0\n".into(),
        }
    }
}

fn wr8(va: usize, off: usize, v: u8) {
    unsafe { core::ptr::write_volatile((va + off) as *mut u8, v) }
}

impl Dev {
    /// 設定空間に書く (root だけ: Linux の pci_write_config。1 バイトずつ。ステータスの書いて消すビットをまきこまないように)
    pub fn write_config(&self, off: usize, b: &[u8]) -> usize {
        let n = b.len().min(self.cfg_size.saturating_sub(off));
        for (i, &v) in b[..n].iter().enumerate() {
            wr8(self.cfg, off + i, v);
        }
        n
    }
}

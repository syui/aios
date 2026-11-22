// PCI と PCIe: ホストブリッジの設定空間 (ECAM) を写し、バスをたどって装置を見つける (Linux の drivers/pci/probe.c と
// ecam.c。ECAM の場所は DTB の pci-host-ecam-generic (pci-host-generic) か、ACPI の MCFG)。ブリッジの下のバスもたどり、
// 番号がなければつける (DTB は Linux の PCI_REASSIGN_ALL_BUS と同じくいつも。ACPI はファームウェアのものを使う)。
// BAR とブリッジの窓は、DTB なら Linux の pci_bus_size_bridges と pci_bus_assign_resources と同じ並べかたで割り当てて
// 書き、ACPI はファームウェアのものをそのまま使う。PCIe のリンクの ASPM は、DTB なら両はしにあるもの (L0s、L1) を
// 入れる (Linux 6.18 からの devicetree の既定)。見つけたものは /sys/bus/pci と /sys/devices/pci0000:00 (sysfs.rs) に。
// ドライバはまだない (aios の virtio は virtio-mmio)
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
    /// 資源 (BAR 0..5、ROM、ブリッジの窓)。MEM は CPU の番地、IO は Linux と同じ口の番号 (バスの番地)
    pub res: [Res; NRES],
    /// ブリッジの窓の並べる単位 (Linux の IORESOURCE_STARTALIGN: 割り当てるまでだけ)
    walign: [u64; 3],
    /// PCIe のスロットがホットプラグできるか (Linux の is_hotplug_bridge: 窓に余りをとる)
    hotplug: bool,
    /// ブリッジの窓のあるもの (IO、プリフェッチ、64 ビットのプリフェッチ)
    io_win: bool,
    pref_win: bool,
    pref64: bool,
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
    /// ranges の窓 (バスの番地。CPU とちがう MEM の窓があれば空: 割り当てない)
    wins: Vec<Res>,
    /// interrupt-map-mask (子の番地の上のセル、ピン)
    imask: (u32, u32),
}

/// PCI の IO の口 0 の CPU の番地 (resourceN で IO の BAR を読み書きする。DTB の ranges か、QEMU の ACPI の表)
static IO_CPU: crate::cell::Once<u64> = crate::cell::Once::new();

static HOST: crate::cell::Once<Host> = crate::cell::Once::new();
/// DTB の interrupt-map のそのままのバイト (GIC のセルの数は phandle の先を見てから読む)
static RAW_IMAP: crate::cell::Once<Vec<u8>> = crate::cell::Once::new();
/// INTx のつなぎ先: DTB の interrupt-map を読んだもの (子の番地の上のセル、ピン、INTID) か ACPI の _PRT
static IMAP: crate::cell::Once<Vec<(u32, u32, u32)>> = crate::cell::Once::new();

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
    let mut res = read_bars(cfg, hdr, class);
    let (mut io_win, mut pref_win, mut pref64) = (false, false, false);
    if hdr == 1 {
        (io_win, pref_win, pref64) = bridge_windows(cfg);
        read_windows(cfg, &mut res, io_win, pref_win);
        // 下の装置の ERR_ のしらせを上へ通す (Linux の pci_configure_serr: ブリッジ制御の SERR#)
        let ctl = rd16(cfg, 0x3e);
        if ctl & 2 == 0 {
            wr16(cfg, 0x3e, ctl | 2);
        }
    }
    // スロットのある下りのポートで、ホットプラグの印 (Linux の set_pcie_hotplug_bridge)
    let hotplug = find_cap(cfg, hdr, 0x10).is_some_and(|p| {
        let t = rd16(cfg, p + 2) >> 4 & 0xf;
        matches!(t, 4 | 6) && rd16(cfg, p + 2) & (1 << 8) != 0 && rd32(cfg, p + 0x14) & (1 << 6) != 0
    });
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
        res,
        walign: [0; 3],
        hotplug,
        io_win,
        pref_win,
        pref64,
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
        let mut root_ac = 2;
        return crate::dtb::scan(|n| {
            if n.depth() == 0 {
                root_ac = n.prop("#address-cells").and_then(|b| b.get(..4)).map_or(2, |b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize);
            }
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
            // ranges の CPU の番地のセルの数は親 (ルート) の #address-cells
            let (wins, io_cpu) = n.prop("ranges").and_then(|r| dt_windows(r, root_ac)).unwrap_or_default();
            if let Some(io) = io_cpu {
                IO_CPU.set(io);
            }
            let cell = |b: &[u8], o: usize| b.get(o..o + 4).map_or(0, |c| u32::from_be_bytes([c[0], c[1], c[2], c[3]]));
            let m = n.prop("interrupt-map-mask").unwrap_or(&[]);
            let imask = (cell(m, 0), cell(m, 12));
            let imap = n.prop("interrupt-map").map(|b| b.to_vec()).unwrap_or_default();
            HOST.set(Host {
                dir: format!("{:x}.{}", pa, name),
                of_name: name,
                of_full: n.full_name(),
                of_type: n.str("device_type").unwrap_or("<NULL>").into(),
                compat,
                wins,
                imask,
            });
            RAW_IMAP.set(imap);
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
    match host() {
        // DTB: Linux の pci_host_probe と同じく、窓の大きさを決めて、みな割り当てなおす
        Some(h) if !h.wins.is_empty() => {
            size_bridges(&mut out, None);
            let mut wins: Vec<Win> = h.wins.iter().map(|&r| Win { res: r, used: Vec::new() }).collect();
            assign_bus(&mut out, None, &mut wins);
        }
        Some(_) => println!("pci: no host bridge windows in ranges (or translated memory windows); BARs are left as they are"),
        // ACPI: ホストブリッジの _CRS の窓で、やはりみな割り当てなおす (Linux の pci_acpi_scan_root: _DSM の
        // 「ファームウェアのものを残す」がなければ。aios は _DSM (Method) を読めないので、いつも)
        None => {
            let mut wins = Vec::new();
            let mut ok = true;
            for (kind, min, len, tra, pref) in crate::acpi::pci_windows() {
                let mut flags = if kind == 1 { IORESOURCE_IO } else { IORESOURCE_MEM };
                if pref {
                    flags |= IORESOURCE_PREFETCH;
                }
                if kind == 0 && min + len > 1 << 32 {
                    flags |= IORESOURCE_MEM_64;
                }
                if kind == 1 {
                    IO_CPU.set(tra);
                } else if tra != 0 {
                    ok = false;
                }
                wins.push(Win { res: Res { start: min, end: min + len - 1, flags }, used: Vec::new() });
            }
            if ok && !wins.is_empty() {
                size_bridges(&mut out, None);
                assign_bus(&mut out, None, &mut wins);
            } else {
                println!("pci: no usable host bridge windows in _CRS; BARs are left as firmware set them");
            }
        }
    }
    aspm_init(&out, host().is_some());
    IMAP.set(if host().is_some() { dt_imap() } else { crate::acpi::pci_prt().into_iter().map(|(slot, pin, gsi)| (slot << 11, pin as u32 + 1, gsi)).collect() });
    ROOT.set(start);
    DEVS.set(out);
}

/// DTB の interrupt-map を読む: 1 つは子の番地 (3)、ピン (1)、親の phandle (1)、親の番地 (親の #address-cells)、
/// 親の割り込み (親の #interrupt-cells。GIC は種類 (0 SPI、1 PPI)、番号、印)
fn dt_imap() -> Vec<(u32, u32, u32)> {
    let raw = RAW_IMAP.get().map(|v| v.as_slice()).unwrap_or(&[]);
    let cell = |o: usize| raw.get(o * 4..o * 4 + 4).map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]]));
    let mut v = Vec::new();
    let mut o = 0;
    while let (Some(hi), Some(pin), Some(ph)) = (cell(o), cell(o + 3), cell(o + 4)) {
        let (ac, ic) = crate::dtb::scan(|n| {
            let c = |p: &str| n.prop(p).and_then(|b| b.get(..4)).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize);
            (c("phandle") == Some(ph as usize)).then(|| (c("#address-cells").unwrap_or(0), c("#interrupt-cells").unwrap_or(3)))
        })
        .unwrap_or((0, 3));
        let p = o + 5 + ac;
        if ic >= 2
            && let (Some(kind), Some(num)) = (cell(p), cell(p + 1))
        {
            v.push((hi, pin, num + if kind == 1 { 16 } else { 32 }));
        }
        o = p + ic;
    }
    v
}

/// i の装置の INTx の GIC の INTID (Linux の pci_assign_irq: ブリッジをこえるたびにピンをまわし (pci_swizzle_interrupt_pin)、
/// ルートのバスの装置の番地で interrupt-map か _PRT を引く)。ピンがなければ None
pub fn intx(i: usize) -> Option<u32> {
    let out = devs();
    let mut pin = rd8(out[i].cfg, 0x3d) as u32;
    if pin == 0 || pin > 4 {
        return None;
    }
    let mut d = i;
    while let Some(u) = out[d].up {
        pin = (pin - 1 + out[d].slot as u32) % 4 + 1;
        d = u;
    }
    let hi = (out[d].bus as u32) << 16 | (out[d].slot as u32) << 11 | (out[d].func as u32) << 8;
    let (mhi, mpin) = match host() {
        Some(h) => h.imask,
        // _PRT は装置の番号とピンだけを見る
        None => (0x1f << 11, 7),
    };
    IMAP.get()?.iter().find(|&&(chi, cpin, _)| chi & mhi == hi & mhi && cpin & mpin == pin & mpin).map(|e| e.2)
}

/// virtio の装置 (Linux の virtio_pci の id の表: 1af4:1000..107f。modern は 0x1040 + 種類、transitional は
/// subsystem の装置の番号が種類): (devs の番号, virtio の種類)
pub fn virtio_devices() -> Vec<(usize, u32)> {
    devs()
        .iter()
        .enumerate()
        .filter(|(_, d)| d.vendor == 0x1af4 && (0x1000..=0x107f).contains(&d.device))
        .map(|(i, d)| (i, if d.device >= 0x1040 { (d.device - 0x1040) as u32 } else { d.subdevice as u32 }))
        .collect()
}

/// vendor の capability (id 9) のうち virtio のもの (struct virtio_pci_cap): 種類、BAR、BAR の中の場所、長さ、
/// 知らせるものなら notify_off_multiplier
pub struct VendorCap {
    pub kind: u8,
    pub bar: u8,
    pub offset: u32,
    pub length: u32,
    pub extra: u32,
}

pub fn vendor_caps(i: usize) -> Vec<VendorCap> {
    let d = &devs()[i];
    let mut v = Vec::new();
    if rd16(d.cfg, 0x06) & (1 << 4) == 0 {
        return v;
    }
    let mut pos = rd8(d.cfg, 0x34) as usize;
    for _ in 0..48 {
        if pos < 0x40 {
            break;
        }
        pos &= !3;
        let h = rd16(d.cfg, pos);
        if h as u8 == 0x09 {
            v.push(VendorCap { kind: rd8(d.cfg, pos + 3), bar: rd8(d.cfg, pos + 4), offset: rd32(d.cfg, pos + 8), length: rd32(d.cfg, pos + 12), extra: rd32(d.cfg, pos + 16) });
        }
        pos = (h >> 8) as usize;
    }
    v
}

/// BAR bar の中の [off, off + len) の仮想番地 (MEM で、割り当てたものだけ。Linux の pci_iomap_range)
pub fn bar_va(i: usize, bar: usize, off: u64, len: usize) -> Option<usize> {
    let r = devs()[i].res.get(bar)?;
    if r.flags & IORESOURCE_MEM == 0 || r.size() == 0 || off + len as u64 > r.size() {
        return None;
    }
    crate::memlayout::map_device((r.start + off) as usize, len.max(1))
}

/// 装置を使えるようにする: MEM の読み書きと bus master を入れ、INTx を止めない (Linux の pci_enable_device と
/// pci_set_master、pci_intx)
pub fn enable(i: usize) {
    let d = &devs()[i];
    let cmd = rd16(d.cfg, 4);
    wr16(d.cfg, 4, (cmd | 0x6) & !(1 << 10));
    ENABLED[i.min(ENABLED.len() - 1)].store(true, core::sync::atomic::Ordering::Relaxed);
}

/// 使えるようにした装置 (/sys の enable: Linux の enable_cnt)
static ENABLED: [core::sync::atomic::AtomicBool; 255] = [const { core::sync::atomic::AtomicBool::new(false) }; 255];

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
pub const DEV_FILES: [(&str, u32); 33] = [
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
    ("reset", 0o200),
    ("reset_method", 0o644),
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

/// i の装置にそのファイルがあるか (Linux の is_visible: リンクのものは PCIe だけ、バスの番号はブリッジだけ、
/// reset と reset_method はリセットのやりかたがあるときだけ)
pub fn visible(i: usize, f: usize) -> bool {
    let d = &devs()[i];
    match DEV_FILES[f].0 {
        "current_link_speed" | "current_link_width" | "max_link_speed" | "max_link_width" => d.pcie.is_some(),
        "secondary_bus_number" | "subordinate_bus_number" | "reset_subordinate" => d.below.is_some(),
        "reset" | "reset_method" => !reset_methods(devs(), i).is_empty(),
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
            "enable" => {
                let i = devs().iter().position(|o| core::ptr::eq(o, self)).unwrap_or(0);
                format!("{}\n", ENABLED[i.min(ENABLED.len() - 1)].load(core::sync::atomic::Ordering::Relaxed) as u8)
            }
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
            // BAR と ROM、ブリッジなら窓の 4 つも (Linux の resource_show: 使わない窓は 0)
            "resource" => {
                let mut t = String::new();
                for (i, r) in self.res.iter().enumerate().take(if self.below.is_some() { NRES } else { WIN_IO }) {
                    let z = r.flags == 0 || (i >= WIN_IO && r.size() == 0);
                    let (a, b, c) = if z { (0, 0, 0) } else { (r.start, r.end, r.flags) };
                    t.push_str(&format!("{:#018x} {:#018x} {:#018x}\n", a, b, c));
                }
                t
            }
            "reset_method" => {
                let i = devs().iter().position(|o| core::ptr::eq(o, self)).unwrap_or(0);
                format!("{}\n", reset_methods(devs(), i).join(" "))
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

// ---- BAR とブリッジの窓 (Linux の drivers/pci/probe.c の __pci_read_base、setup-bus.c、setup-res.c) ----

/// Linux の struct resource の flags のうち、PCI が使うもの
const IORESOURCE_IO: u64 = 0x100;
const IORESOURCE_MEM: u64 = 0x200;
const IORESOURCE_TYPE_BITS: u64 = 0x1f00;
const IORESOURCE_PREFETCH: u64 = 0x2000;
const IORESOURCE_READONLY: u64 = 0x4000;
const IORESOURCE_SIZEALIGN: u64 = 0x4_0000;
const IORESOURCE_MEM_64: u64 = 0x10_0000;
/// 資源の番号: BAR 0..5、ROM、ブリッジの窓 (IO、MEM、PREF と、使わない 1 つ)。Linux の PCI_NUM_RESOURCES (SR-IOV なし)
pub const ROM: usize = 6;
const WIN_IO: usize = 7;
const WIN_MEM: usize = 8;
const WIN_PREF: usize = 9;
pub const NRES: usize = 11;
/// Linux の arm64 の PCIBIOS_MIN_IO と、ホットプラグのスロットの窓の余り (pci_hotplug_io_size など)
const MIN_IO: u64 = 0x1000;
const HP_IO: u64 = 256;
const HP_MEM: u64 = 2 << 20;
const HP_PREF: u64 = 2 << 20;

/// 資源 (Linux の struct resource: start、end、flags。flags が 0 ならない)
#[derive(Clone, Copy, Default)]
pub struct Res {
    pub start: u64,
    pub end: u64,
    pub flags: u64,
}

impl Res {
    pub fn size(&self) -> u64 {
        if self.flags == 0 || self.end < self.start { 0 } else { self.end - self.start + 1 }
    }
}

/// Linux の %pR ([mem 0x10000000-0x101fffff 64bit pref])
fn res_str(r: &Res) -> String {
    let mut s = if r.flags & IORESOURCE_IO != 0 { format!("[io  {:#06x}-{:#06x}", r.start, r.end) } else { format!("[mem {:#010x}-{:#010x}", r.start, r.end) };
    if r.flags & IORESOURCE_MEM_64 != 0 {
        s.push_str(" 64bit");
    }
    if r.flags & IORESOURCE_PREFETCH != 0 {
        s.push_str(" pref");
    }
    s.push(']');
    s
}

fn res_name(i: usize) -> String {
    match i {
        ROM => "ROM".into(),
        WIN_IO.. => "bridge window".into(),
        _ => format!("BAR {}", i),
    }
}

fn wr16(va: usize, off: usize, v: u16) {
    unsafe { core::ptr::write_volatile((va + (off & !1)) as *mut u16, v) }
}

/// Linux の pci_size: 1 を書いて読んだ値から、いちばん下の立っているビットが大きさ
fn bar_size(base: u64, maxbase: u64, mask: u64) -> u64 {
    let size = mask & maxbase;
    if size == 0 {
        return 0;
    }
    let size = size & size.wrapping_neg();
    if base == maxbase && ((base | (size - 1)) & mask) != mask {
        return 0;
    }
    size
}

/// BAR (か ROM) の大きさと種類をはかる (Linux の __pci_read_base): 1 を書いて読み、もとにもどす。(資源, 64 ビットか)
fn read_bar(cfg: usize, off: usize, rom: bool) -> (Res, bool) {
    let mut l = rd32(cfg, off);
    wr32(cfg, off, l | if rom { 0xffff_f800 } else { !0 });
    let mut sz = rd32(cfg, off);
    wr32(cfg, off, l);
    if sz == 0xffff_ffff {
        sz = 0;
    }
    if l == 0xffff_ffff {
        l = 0;
    }
    let (mut flags, mut l64, mut sz64, mut mask) = if rom {
        let m = 0xffff_f800u32;
        (IORESOURCE_MEM | IORESOURCE_PREFETCH | IORESOURCE_READONLY | IORESOURCE_SIZEALIGN | (l & 1) as u64, (l & m) as u64, (sz & m) as u64, m as u64)
    } else if l & 1 != 0 {
        // IO (arm64 の IO_SPACE_LIMIT は 16 MiB)
        (IORESOURCE_IO | IORESOURCE_SIZEALIGN | (l & 3) as u64, (l & !3) as u64, (sz & !3) as u64, (!3u32 & 0x00ff_ffff) as u64)
    } else {
        let mut f = IORESOURCE_MEM | IORESOURCE_SIZEALIGN | (l & 0xf) as u64;
        if l & 8 != 0 {
            f |= IORESOURCE_PREFETCH;
        }
        if l & 6 == 4 {
            f |= IORESOURCE_MEM_64;
        }
        (f, (l & !0xf) as u64, (sz & !0xf) as u64, (!0xfu32) as u64)
    };
    let is64 = flags & IORESOURCE_MEM_64 != 0;
    if is64 {
        let h = rd32(cfg, off + 4);
        wr32(cfg, off + 4, !0);
        let szh = rd32(cfg, off + 4);
        wr32(cfg, off + 4, h);
        l64 |= (h as u64) << 32;
        sz64 |= (szh as u64) << 32;
        mask |= 0xffff_ffff << 32;
    }
    let size = if sz64 == 0 { 0 } else { bar_size(l64, sz64, mask) };
    if size == 0 {
        flags = 0;
    }
    (Res { start: l64, end: l64.wrapping_add(size).wrapping_sub(1), flags }, is64)
}

/// 装置の BAR と ROM (ふつうは 6 つと 0x30、ブリッジは 2 つと 0x38)。はかるあいだは読み書きを止める (ホストブリッジのほか)
fn read_bars(cfg: usize, hdr: u8, class: u32) -> [Res; NRES] {
    let mut res = [Res::default(); NRES];
    let (n, rom) = match hdr {
        0 => (6, 0x30),
        1 => (2, 0x38),
        _ => (0, 0),
    };
    let cmd = rd16(cfg, 4);
    let quiet = class >> 8 != 0x0600 && cmd & 3 != 0;
    if quiet {
        wr16(cfg, 4, cmd & !3);
    }
    let mut i = 0;
    while i < n {
        let (r, is64) = read_bar(cfg, 0x10 + 4 * i, false);
        res[i] = r;
        i += if is64 { 2 } else { 1 };
    }
    if rom != 0 {
        res[ROM] = read_bar(cfg, rom, true).0;
    }
    if quiet {
        wr16(cfg, 4, cmd);
    }
    res
}

/// ブリッジにある窓 (Linux の pci_read_bridge_windows): IO、プリフェッチ、64 ビットのプリフェッチ
fn bridge_windows(cfg: usize) -> (bool, bool, bool) {
    let mut io = rd16(cfg, 0x1c);
    if io == 0 {
        wr16(cfg, 0x1c, 0xe0f0);
        io = rd16(cfg, 0x1c);
        wr16(cfg, 0x1c, 0);
    }
    let mut pm = rd16(cfg, 0x24);
    if pm == 0 {
        wr16(cfg, 0x24, 0xe0f0);
        pm = rd16(cfg, 0x24);
        wr16(cfg, 0x24, 0);
    }
    let mut pref64 = false;
    if pm != 0 && pm & 0xf == 1 {
        let u = rd32(cfg, 0x28);
        wr32(cfg, 0x28, !0);
        pref64 = rd32(cfg, 0x28) != 0;
        wr32(cfg, 0x28, u);
    }
    (io != 0, pm != 0, pref64)
}

/// いまの窓 (Linux の pci_read_bridge_bases。ACPI ならファームウェアがつけたもの)。base > limit なら大きさ 0
fn read_windows(cfg: usize, res: &mut [Res; NRES], io_win: bool, pref_win: bool) {
    let win = |base: u64, limit: u64, gran: u64, flags: u64| if base <= limit { Res { start: base, end: limit + gran - 1, flags } } else { Res { start: 1, end: 0, flags } };
    if io_win {
        let (b, l) = (rd8(cfg, 0x1c) as u64, rd8(cfg, 0x1d) as u64);
        let (mut base, mut limit) = ((b & 0xf0) << 8, (l & 0xf0) << 8);
        if b & 0xf == 1 {
            base |= (rd16(cfg, 0x30) as u64) << 16;
            limit |= (rd16(cfg, 0x32) as u64) << 16;
        }
        res[WIN_IO] = win(base, limit, 0x1000, IORESOURCE_IO | (b & 0xf));
    }
    let (b, l) = (rd16(cfg, 0x20) as u64, rd16(cfg, 0x22) as u64);
    res[WIN_MEM] = win((b & 0xfff0) << 16, (l & 0xfff0) << 16, 1 << 20, IORESOURCE_MEM | (b & 0xf));
    if pref_win {
        let (b, l) = (rd16(cfg, 0x24) as u64, rd16(cfg, 0x26) as u64);
        let (mut base, mut limit) = ((b & 0xfff0) << 16, (l & 0xfff0) << 16);
        let mut flags = IORESOURCE_MEM | IORESOURCE_PREFETCH | (b & 0xf);
        if b & 0xf == 1 {
            base |= (rd32(cfg, 0x28) as u64) << 32;
            limit |= (rd32(cfg, 0x2c) as u64) << 32;
            flags |= IORESOURCE_MEM_64;
        }
        res[WIN_PREF] = win(base, limit, 1 << 20, flags);
    }
}

/// 資源の並べる単位 (Linux の pci_resource_alignment: BAR は大きさ、窓は決めた単位)
fn alignment(d: &Dev, i: usize) -> u64 {
    match i {
        WIN_IO..=WIN_PREF => d.walign[i - WIN_IO],
        _ if d.res[i].flags & IORESOURCE_SIZEALIGN != 0 => d.res[i].size(),
        _ => 0,
    }
}

fn align_up(v: u64, a: u64) -> u64 {
    if a <= 1 { v } else { v.div_ceil(a) * a }
}

/// Linux 7.4 の calculate_head_align: 1 MiB からの単位ごとの合計から、すきまなく並ぶ窓の頭の単位
fn mem_align(aligns: &[u64; 28], max_order: usize) -> u64 {
    let mut head = 1u64 << (max_order + 20);
    let mut rest = 0u64;
    for order in (0..max_order).rev() {
        let align1 = 1u64 << (order + 20);
        rest += aligns[order];
        while head > align1 && rest >= head / 2 {
            head /= 2;
            rest -= head;
        }
    }
    head
}

/// ブリッジ b の下の装置 (ROM と窓をふくむ) の資源のうち、sel にあうもの: (装置, 番号)
fn child_res(out: &[Dev], b: usize, sel: impl Fn(u64) -> bool) -> Vec<(usize, usize)> {
    let mut v = Vec::new();
    for (k, d) in out.iter().enumerate().filter(|(_, d)| d.up == Some(b)) {
        for i in 0..NRES {
            if d.res[i].flags != 0 && d.res[i].size() != 0 && sel(d.res[i].flags) {
                v.push((k, i));
            }
        }
    }
    v
}

/// ブリッジの窓の大きさを、下から決める (Linux の __pci_bus_size_bridges と pbus_size_io、pbus_size_mem。
/// realloc_head なしなので、ホットプラグの余りはいちばん小さな大きさとして)
fn size_bridges(out: &mut [Dev], up: Option<usize>) {
    let kids: Vec<usize> = (0..out.len()).filter(|&k| out[k].up == up && out[k].below.is_some()).collect();
    for k in kids {
        size_bridges(out, Some(k));
    }
    let Some(b) = up else { return };
    let hp = out[b].hotplug;
    // IO: 1 KiB より小さなものと大きなものをたし、4 KiB (か、下のいちばん大きな単位) にそろえる
    if out[b].io_win {
        let (mut size, mut size1, mut align) = (0u64, 0u64, 0x1000u64);
        for (k, i) in child_res(out, b, |f| f & IORESOURCE_IO != 0) {
            let s = out[k].res[i].size();
            if s < 0x400 {
                size += s;
            } else {
                size1 += s;
            }
            align = align.max(alignment(&out[k], i));
        }
        // いまの窓の大きさ (old_size) は使わない: arm64 の Linux は窓を読まずに大きさを決める (pci_read_bridge_bases なし)
        let size0 = align_up(size.max(if hp { HP_IO } else { 0 }) + size1, align);
        let flags = IORESOURCE_IO | (out[b].res[WIN_IO].flags & 0xf);
        out[b].res[WIN_IO] = if size0 == 0 { Res::default() } else { Res { start: 0, end: size0 - 1, flags } };
        out[b].walign[0] = align;
    }
    // プリフェッチの窓 (64 ビットなら 64 ビットのプリフェッチだけ) と、ほかの MEM の窓
    let pm = IORESOURCE_MEM | IORESOURCE_PREFETCH | IORESOURCE_MEM_64;
    let pref_sel = move |f: u64, p64: bool| if p64 { f & pm == pm } else { f & (IORESOURCE_MEM | IORESOURCE_PREFETCH) == IORESOURCE_MEM | IORESOURCE_PREFETCH };
    let (pref, p64) = (out[b].pref_win, out[b].pref64);
    for w in [WIN_PREF, WIN_MEM] {
        if w == WIN_PREF && !pref {
            continue;
        }
        let sel = move |f: u64| f & IORESOURCE_MEM != 0 && if w == WIN_PREF { pref_sel(f, p64) } else { !(pref && pref_sel(f, p64)) };
        let (mut aligns, mut max_order, mut size) = ([0u64; 28], 0usize, 0u64);
        for (k, i) in child_res(out, b, sel) {
            let (s, a) = (out[k].res[i].size(), alignment(&out[k], i));
            let order = (a.trailing_zeros() as usize).saturating_sub(20);
            if order >= 28 {
                continue;
            }
            size += s.max(a);
            if s <= a {
                aligns[order] += a;
            }
            max_order = max_order.max(order);
        }
        let min_align = mem_align(&aligns, max_order).max(1 << 20);
        let min = match (hp, w) {
            (true, WIN_PREF) => HP_PREF,
            (true, _) => HP_MEM,
            _ => 0,
        };
        let size0 = align_up(size.max(min), 1 << 20);
        let mut flags = IORESOURCE_MEM | (out[b].res[w].flags & 0xf);
        if w == WIN_PREF {
            flags |= IORESOURCE_PREFETCH | if p64 { IORESOURCE_MEM_64 | 1 } else { 0 };
        }
        out[b].res[w] = if size0 == 0 { Res::default() } else { Res { start: 0, end: size0 - 1, flags } };
        out[b].walign[w - WIN_IO] = min_align;
    }
}

/// 割り当て先の窓と、その中にもう置いたもの
struct Win {
    res: Res,
    used: Vec<(u64, u64)>,
}

/// 窓の中の [lo, hi] で、align にそろえた size のいちばん下のすきま (Linux の allocate_resource: 下から)
fn first_fit(used: &[(u64, u64)], lo: u64, hi: u64, size: u64, align: u64) -> Option<u64> {
    let mut v = used.to_vec();
    v.sort();
    let mut s = align_up(lo, align);
    for (a, b) in v {
        if s.checked_add(size - 1)? < a {
            break;
        }
        if b >= s {
            s = align_up(b + 1, align);
        }
    }
    (s.checked_add(size - 1)? <= hi).then_some(s)
}

/// 資源を窓に置く (Linux の __pci_assign_resource): まずプリフェッチと 64 ビットがあう窓、64 ビットのプリフェッチは
/// プリフェッチの窓、さいごはどの MEM の窓でも。64 ビットのものは 4 GiB より上を先に (pci_bus_alloc_resource)
fn alloc(wins: &mut [Win], r: &Res, align: u64) -> Option<u64> {
    let size = r.size();
    let min = if r.flags & IORESOURCE_IO != 0 { MIN_IO } else { 0 };
    let pm = IORESOURCE_PREFETCH | IORESOURCE_MEM_64;
    let mut masks = Vec::from([pm]);
    if r.flags & pm == pm {
        masks.push(IORESOURCE_PREFETCH);
    }
    if r.flags & pm != 0 {
        masks.push(0);
    }
    let regions: &[(u64, u64)] = if r.flags & IORESOURCE_MEM_64 != 0 { &[(1 << 32, u64::MAX), (0, u64::MAX)] } else { &[(0, 0xffff_ffff)] };
    for mask in masks {
        for &(rlo, rhi) in regions {
            for w in wins.iter_mut() {
                if (r.flags ^ w.res.flags) & (mask | IORESOURCE_TYPE_BITS) != 0 || (w.res.flags & IORESOURCE_PREFETCH != 0 && r.flags & IORESOURCE_PREFETCH == 0) {
                    continue;
                }
                let (lo, hi) = (w.res.start.max(rlo), w.res.end.min(rhi));
                if lo > hi {
                    continue;
                }
                let lo = if lo != 0 { lo } else { min };
                if lo > hi || size - 1 > hi - lo {
                    continue;
                }
                if let Some(s) = first_fit(&w.used, lo, hi, size, align) {
                    w.used.push((s, s + size - 1));
                    return Some(s);
                }
            }
        }
    }
    None
}

/// BAR に書く (Linux の pci_std_update_resource。ROM は使えるようにしたときだけ書く: しない)
fn write_bar(d: &Dev, i: usize) {
    let r = d.res[i];
    if i >= ROM {
        return;
    }
    let off = 0x10 + 4 * i;
    let new = if r.flags & IORESOURCE_IO != 0 { (r.start as u32 & !3) | (r.flags as u32 & 3) } else { (r.start as u32 & !0xf) | (r.flags as u32 & 0xf) };
    let is64 = r.flags & IORESOURCE_MEM_64 != 0;
    // 64 ビットの BAR は半分だけ書いたところでほかとぶつからないよう、MEM の読み書きを止めて (ホストブリッジのほか)
    let cmd = rd16(d.cfg, 4);
    let quiet = is64 && d.class >> 8 != 0x0600;
    if quiet {
        wr16(d.cfg, 4, cmd & !2);
    }
    wr32(d.cfg, off, new);
    if is64 {
        wr32(d.cfg, off + 4, (r.start >> 32) as u32);
    }
    if quiet {
        wr16(d.cfg, 4, cmd);
    }
}

/// ブリッジの窓を書く (Linux の pci_setup_bridge_io、_mmio、_mmio_pref)。使わない窓は base > limit
fn setup_bridge(d: &Dev) {
    let (cfg, io, mem, pref) = (d.cfg, d.res[WIN_IO], d.res[WIN_MEM], d.res[WIN_PREF]);
    if let Some((sec, sub)) = d.below {
        if sec == sub { println!("pci {}: PCI bridge to [bus {:02x}]", d.name(), sec) } else { println!("pci {}: PCI bridge to [bus {:02x}-{:02x}]", d.name(), sec, sub) }
    }
    let (l, up16) = if io.flags & IORESOURCE_IO != 0 && io.size() != 0 {
        println!("pci {}:   bridge window {}", d.name(), res_str(&io));
        ((((io.end >> 8) & 0xf0) << 8 | ((io.start >> 8) & 0xf0)) as u16, ((io.end & 0xffff_0000) | (io.start >> 16)) as u32)
    } else {
        (0x00f0, 0)
    };
    wr32(cfg, 0x30, 0x0000_ffff);
    wr16(cfg, 0x1c, l);
    wr32(cfg, 0x30, up16);
    let l = if mem.flags & IORESOURCE_MEM != 0 && mem.size() != 0 {
        println!("pci {}:   bridge window {}", d.name(), res_str(&mem));
        (((mem.start >> 16) & 0xfff0) | (mem.end & 0xfff0_0000)) as u32
    } else {
        0x0000_fff0
    };
    wr32(cfg, 0x20, l);
    wr32(cfg, 0x2c, 0);
    let (l, bu, lu) = if pref.flags & IORESOURCE_PREFETCH != 0 && pref.size() != 0 {
        println!("pci {}:   bridge window {}", d.name(), res_str(&pref));
        let hi = pref.flags & IORESOURCE_MEM_64 != 0;
        ((((pref.start >> 16) & 0xfff0) | (pref.end & 0xfff0_0000)) as u32, if hi { (pref.start >> 32) as u32 } else { 0 }, if hi { (pref.end >> 32) as u32 } else { 0 })
    } else {
        (0x0000_fff0, 0, 0)
    };
    wr32(cfg, 0x24, l);
    wr32(cfg, 0x28, bu);
    wr32(cfg, 0x2c, lu);
}

/// バスの装置の資源を、並べる単位の大きい順 (同じなら装置と番号の順) に窓へ置き、そのあとブリッジの下へ
/// (Linux の __pci_bus_assign_resources と pbus_assign_resources_sorted)
fn assign_bus(out: &mut [Dev], up: Option<usize>, wins: &mut [Win]) {
    let kids: Vec<usize> = (0..out.len()).filter(|&k| out[k].up == up).collect();
    let mut list: Vec<(u64, usize, usize)> = Vec::new();
    for &k in &kids {
        // 種類のないものとホストブリッジにはさわらない (Linux の __dev_sort_resources)
        if matches!(out[k].class >> 8, 0x0000 | 0x0600) {
            continue;
        }
        for i in 0..NRES {
            let a = alignment(&out[k], i);
            if out[k].res[i].flags == 0 || out[k].res[i].size() == 0 || a == 0 {
                continue;
            }
            let pos = list.iter().position(|e| a > e.0).unwrap_or(list.len());
            list.insert(pos, (a, k, i));
        }
    }
    for (a, k, i) in list {
        let r = out[k].res[i];
        match alloc(wins, &r, a) {
            Some(s) => {
                out[k].res[i].start = s;
                out[k].res[i].end = s + r.size() - 1;
                println!("pci {}: {} {}: assigned", out[k].name(), res_name(i), res_str(&out[k].res[i]));
                write_bar(&out[k], i);
            }
            None => {
                println!("pci {}: {} {}: can't assign; no space", out[k].name(), res_name(i), res_str(&r));
                out[k].res[i] = Res::default();
            }
        }
    }
    for k in kids {
        if out[k].below.is_none() {
            continue;
        }
        let mut child: Vec<Win> = [WIN_IO, WIN_MEM, WIN_PREF].iter().filter(|&&w| out[k].res[w].size() != 0).map(|&w| Win { res: out[k].res[w], used: Vec::new() }).collect();
        assign_bus(out, Some(k), &mut child);
        setup_bridge(&out[k]);
    }
}

/// DTB のホストブリッジの ranges (Linux の of_pci_range): IO、32 ビットの MEM、64 ビットの MEM の窓 (バスの番地)。
/// CPU の番地とバスの番地がちがう MEM の窓があれば None (aios はそれを書きかえない)
fn dt_windows(ranges: &[u8], cpu_cells: usize) -> Option<(Vec<Res>, Option<u64>)> {
    let cell = |o: usize| ranges.get(o..o + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as u64);
    let step = (3 + cpu_cells + 2) * 4;
    let (mut wins, mut io_cpu) = (Vec::new(), None);
    for o in (0..ranges.len() / step).map(|e| e * step) {
        let hi = cell(o)?;
        let pci = cell(o + 4)? << 32 | cell(o + 8)?;
        let cpu = (0..cpu_cells).try_fold(0u64, |a, c| Some(a << 32 | cell(o + 12 + c * 4)?))?;
        let size = cell(o + 12 + cpu_cells * 4)? << 32 | cell(o + 16 + cpu_cells * 4)?;
        let mut flags = match hi >> 24 & 3 {
            1 => IORESOURCE_IO,
            2 => IORESOURCE_MEM,
            3 => IORESOURCE_MEM | IORESOURCE_MEM_64,
            _ => continue,
        };
        if hi & 0x4000_0000 != 0 {
            flags |= IORESOURCE_PREFETCH;
        }
        if flags & IORESOURCE_IO != 0 {
            io_cpu = Some(cpu - pci);
        } else if cpu != pci {
            return None;
        }
        wins.push(Res { start: pci, end: pci + size - 1, flags });
    }
    Some((wins, io_cpu))
}

// ---- PCIe のリンクの ASPM (Linux の drivers/pci/pcie/aspm.c) と、リセットのやりかた (drivers/pci/pci.c) ----

/// ASPM の状態のビット (Linux の PCIE_LINK_STATE_L0S_UP、_L0S_DW、_L1)
const ASPM_L0S_UP: u8 = 1;
const ASPM_L0S_DW: u8 = 2;
const ASPM_L1: u8 = 4;

/// リンクの入れた状態 (上のポートの番号ごと。Linux の link->aspm_enabled: 書いたものを覚える。QEMU は ASPM の
/// ビットを書けないので、読みなおすと 0 のまま)
static ASPM_ON: [core::sync::atomic::AtomicU8; 255] = [const { core::sync::atomic::AtomicU8::new(0) }; 255];

/// i の装置がつながる PCIe のリンク: (上のポート, リンクの下のはじめの装置)。上が PCIe の下りのポートのときだけ
fn link_of(out: &[Dev], i: usize) -> Option<(usize, usize)> {
    let u = out[i].up?;
    if !matches!(out[u].pcie, Some(4 | 6 | 7)) || out[i].pcie.is_none() {
        return None;
    }
    Some((u, (0..out.len()).find(|&k| out[k].up == Some(u))?))
}

/// リンクの (できるもの, いま入っているもの) (Linux の pcie_aspm_cap_init と pcie_aspm_check_latency)。
/// 下に PCIe でないものや 1.1 より前のもの (DEVCAP の RBER がない) があれば None (pcie_aspm_sanity_check)
fn aspm_state(out: &[Dev], u: usize, first: usize) -> Option<(u8, u8)> {
    let kids: Vec<usize> = (0..out.len()).filter(|&k| out[k].up == Some(u)).collect();
    if kids.iter().any(|&k| out[k].pcie_cap().is_none_or(|p| rd32(out[k].cfg, p + 4) & (1 << 15) == 0)) {
        return None;
    }
    let (pp, cp) = (out[u].pcie_cap()?, out[first].pcie_cap()?);
    let (pcap, ccap) = (rd32(out[u].cfg, pp + 0x0c), rd32(out[first].cfg, cp + 0x0c));
    let mut capable = 0;
    if pcap & ccap & (1 << 10) != 0 {
        capable |= ASPM_L0S_UP | ASPM_L0S_DW;
    }
    if pcap & ccap & (1 << 11) != 0 {
        capable |= ASPM_L1;
    }
    // 出るまでの時間 (LNKCAP) が、端の装置のがまんできる時間 (DEVCAP) をこえるものはできない (ns)
    let l0s = |cap: u32| match cap >> 12 & 7 {
        7 => 5000,
        e => 64 << e,
    };
    let l1 = |cap: u32| match cap >> 15 & 7 {
        7 => 65000,
        e => 1000 << e,
    };
    for &k in kids.iter().filter(|&&k| matches!(out[k].pcie, Some(0 | 1))) {
        let devcap = rd32(out[k].cfg, out[k].pcie_cap()? + 4);
        let ok_l0s = match devcap >> 6 & 7 {
            7 => u32::MAX,
            e => 64 << e,
        };
        let ok_l1 = match devcap >> 9 & 7 {
            7 => u32::MAX,
            e => 1000 << e,
        };
        if l0s(pcap) > ok_l0s {
            capable &= !ASPM_L0S_UP;
        }
        if l0s(ccap) > ok_l0s {
            capable &= !ASPM_L0S_DW;
        }
        if l1(pcap).max(l1(ccap)) > ok_l1 {
            capable &= !ASPM_L1;
        }
    }
    let (pctl, cctl) = (rd16(out[u].cfg, pp + 0x10), rd16(out[first].cfg, cp + 0x10));
    let mut on = 0;
    if cctl & 1 != 0 {
        on |= ASPM_L0S_UP;
    }
    if pctl & 1 != 0 {
        on |= ASPM_L0S_DW;
    }
    if pctl & cctl & 2 != 0 {
        on |= ASPM_L1;
    }
    Some((capable, on))
}

/// リンクの ASPM を state にする (Linux の pcie_config_aspm_link: 下の装置は L0s_UP と L1、上のポートは L0s_DW と L1)
fn aspm_set(out: &[Dev], u: usize, state: u8) {
    let set = |d: &Dev, v: u16| {
        if let Some(p) = d.pcie_cap() {
            wr16(d.cfg, p + 0x10, (rd16(d.cfg, p + 0x10) & !3) | v);
        }
    };
    if let Some(a) = ASPM_ON.get(u) {
        a.store(state, core::sync::atomic::Ordering::Relaxed);
    }
    let l1 = if state & ASPM_L1 != 0 { 2 } else { 0 };
    set(&out[u], l1 | if state & ASPM_L0S_DW != 0 { 1 } else { 0 });
    for d in out.iter().filter(|d| d.up == Some(u)) {
        set(d, l1 | if state & ASPM_L0S_UP != 0 { 1 } else { 0 });
    }
}

/// DTB なら、どのリンクもできるもの (L0s と L1) をみな入れる (Linux 6.18 からの pcie_aspm_override_default_link_state)。
/// ACPI はファームウェアのまま
fn aspm_init(out: &[Dev], dt: bool) {
    for u in 0..out.len().min(ASPM_ON.len()) {
        let Some(first) = (0..out.len()).find(|&k| out[k].up == Some(u)) else { continue };
        let Some((capable, on)) = link_of(out, first).and_then(|(u, f)| aspm_state(out, u, f)) else { continue };
        ASPM_ON[u].store(on, core::sync::atomic::Ordering::Relaxed);
        if !dt {
            continue;
        }
        if capable & !on != 0 {
            println!("pci {}: ASPM: default states{}{}", out[first].name(), if capable & 3 != 0 { " L0s" } else { "" }, if capable & ASPM_L1 != 0 { " L1" } else { "" });
        }
        aspm_set(out, u, capable);
    }
}

/// link/ のファイル (Linux の aspm_ctrl_attrs: できるものだけ見える): (名前, ビット)
pub const ASPM_FILES: [(&str, u8); 2] = [("l0s_aspm", ASPM_L0S_UP | ASPM_L0S_DW), ("l1_aspm", ASPM_L1)];

/// i の装置の link/ のファイル k の中身 (見えなければ None)
pub fn aspm_file(i: usize, k: usize) -> Option<String> {
    let out = devs();
    let (u, f) = link_of(out, i)?;
    let (capable, _) = aspm_state(out, u, f)?;
    let on = ASPM_ON.get(u)?.load(core::sync::atomic::Ordering::Relaxed);
    let bits = ASPM_FILES.get(k)?.1;
    (capable & bits != 0).then(|| if on & bits != 0 { "1\n".into() } else { "0\n".into() })
}

/// link/ のファイルに書く (0 で切る、1 で入れる。入れるのは DTB のときだけ: ACPI の既定はファームウェアのもの)
pub fn aspm_write(i: usize, k: usize, b: &[u8]) -> Result<(), ()> {
    let out = devs();
    let (u, f) = link_of(out, i).ok_or(())?;
    let (capable, _) = aspm_state(out, u, f).ok_or(())?;
    let on = ASPM_ON.get(u).ok_or(())?.load(core::sync::atomic::Ordering::Relaxed);
    let bits = ASPM_FILES.get(k).ok_or(())?.1;
    let v = match core::str::from_utf8(b).map(|s| s.trim()) {
        Ok("1" | "y" | "Y" | "on" | "true") => true,
        Ok("0" | "n" | "N" | "off" | "false") => false,
        _ => return Err(()),
    };
    let state = if !v { on & !bits } else if host().is_some() { on | (capable & bits) } else { on };
    aspm_set(out, u, state);
    Ok(())
}

/// リセットのやりかた (Linux の pci_reset_fn_methods の順: flr、af_flr、pm、bus)
fn reset_methods(out: &[Dev], i: usize) -> Vec<&'static str> {
    let d = &out[i];
    let mut v = Vec::new();
    if d.pcie_cap().is_some_and(|p| rd32(d.cfg, p + 4) & (1 << 28) != 0) {
        v.push("flr");
    }
    if find_cap(d.cfg, d.hdr, 0x13).is_some_and(|p| rd8(d.cfg, p + 3) & 3 == 3) {
        v.push("af_flr");
    }
    if find_cap(d.cfg, d.hdr, 0x01).is_some_and(|p| rd16(d.cfg, p + 4) & (1 << 3) == 0) {
        v.push("pm");
    }
    // 上のブリッジのバスのリセット: ブリッジでなく、そのバスにひとりだけ (pci_parent_bus_reset)
    if let Some(u) = d.up
        && d.below.is_none()
        && out.iter().filter(|o| o.up == Some(u)).count() == 1
    {
        v.push("bus");
    }
    v
}

/// BAR のファイル (Linux の pci_create_resource_files と rom): k は 0..5 が resourceK、6..11 が resourceK_wc
/// (プリフェッチの MEM)、12 が rom。(名前, 大きさ)
pub fn bar_file(d: &Dev, k: usize) -> Option<(String, u64)> {
    match k {
        0..6 => (d.res[k].size() != 0).then(|| (format!("resource{}", k), d.res[k].size())),
        6..12 => {
            let r = &d.res[k - 6];
            (r.size() != 0 && r.flags & IORESOURCE_MEM != 0 && r.flags & IORESOURCE_PREFETCH != 0).then(|| (format!("resource{}_wc", k - 6), r.size()))
        }
        12 => (d.res[ROM].size() != 0).then(|| ("rom".into(), d.res[ROM].size())),
        _ => None,
    }
}

/// BAR のファイルを読み書きする。IO の BAR は 1、2、4 バイトずつ口を (Linux の pci_resource_io)、MEM の BAR は
/// mmap だけ (読み書きは EIO)、rom は使えるようにしていないので EINVAL
pub fn bar_rw(d: &Dev, k: usize, off: usize, buf: &mut [u8], write: bool) -> Result<usize, i64> {
    const EIO: i64 = 5;
    const EINVAL: i64 = 22;
    const ENXIO: i64 = 6;
    let r = match k {
        0..6 => d.res[k],
        12 => return Err(-EINVAL),
        _ => return Err(-EIO),
    };
    if r.flags & IORESOURCE_IO == 0 {
        return Err(-EIO);
    }
    let size = r.size() as usize;
    if off >= size {
        return Ok(0);
    }
    let n = buf.len().min(size - off);
    let Some(&io) = IO_CPU.get() else { return Err(-ENXIO) };
    let va = crate::memlayout::map_device((io + r.start) as usize + off, n).ok_or(-ENXIO)?;
    unsafe {
        match (n, write) {
            (1, false) => buf[0] = core::ptr::read_volatile(va as *const u8),
            (2, false) => buf[..2].copy_from_slice(&core::ptr::read_volatile(va as *const u16).to_le_bytes()),
            (4, false) => buf[..4].copy_from_slice(&core::ptr::read_volatile(va as *const u32).to_le_bytes()),
            (1, true) => core::ptr::write_volatile(va as *mut u8, buf[0]),
            (2, true) => core::ptr::write_volatile(va as *mut u16, u16::from_le_bytes([buf[0], buf[1]])),
            (4, true) => core::ptr::write_volatile(va as *mut u32, u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]])),
            _ => return Err(-EINVAL),
        }
    }
    Ok(n)
}

// ACPI の表を読む (UEFI の機械で DTB のかわり。Linux の drivers/acpi/tables.c と arch/arm64/kernel/acpi.c の、起動に
// 要るところ): EFI の設定テーブルの RSDP → XSDT → MADT (GIC の番地と版、CPU の MPIDR)、SPCR (シリアル)、FADT (PSCI の
// 呼び方 (hvc / smc) と DSDT)、DSDT の AML のうち Device の Name(_HID) と Name(_CRS) (virtio-mmio の LNRO0005、PL011 の
// ARMH0011、PL061 の ARMH0061 など)。AML はインタプリタなしで、Device の中の名前だけをたどる (QEMU の virt と、
// ふつうの機械の静的な表ならこれで足りる。メソッドで決まる _CRS (_PRS や Method(_CRS)) は読まない)。
// 読むのは起動のはじめ (UART と kalloc の前) で、要るものを決まった大きさの表に写す。表そのものは EFI の ACPI Reclaim の
// RAM に残る (efi.rs の reserve_firmware が kalloc からよける。/sys/firmware/acpi/tables のため)。
// dtb.rs の口 (reg_of、irq_cell、each_virtio、each_cpu、psci_method) は、DTB がないときここに聞く (Linux の fwnode が
// DT と ACPI をならべるのと同じ向き)
use crate::memlayout::p2v;

const MAX_CPUS: usize = 64;
const MAX_DEVS: usize = 64;
const MAX_TABLES: usize = 32;

#[derive(Clone, Copy)]
pub struct Dev {
    /// _HID (ASCII。EISA ID の数は PNP0A08 などの文字にする)
    pub hid: [u8; 8],
    pub uid: u32,
    /// _CRS のはじめの番地の範囲と割り込み (GSIV。GIC の INTID)
    pub base: u64,
    pub size: u64,
    pub gsiv: u32,
}

const NO_DEV: Dev = Dev { hid: [0; 8], uid: 0, base: 0, size: 0, gsiv: 0 };

pub struct Info {
    pub rsdp: u64,
    /// (名前, 物理アドレス, 長さ): XSDT にあったものと DSDT
    pub tables: [([u8; 4], u64, u32); MAX_TABLES],
    pub ntables: usize,
    pub oem: [u8; 6],
    pub oem_table: [u8; 8],
    pub revision: u8,
    /// MADT の GICD (番地、版 1〜4。0 は「書いていない」) と、GICv2 の CPU インターフェイス、GICv3 の再配布器
    pub gicd: u64,
    pub gic_version: u8,
    pub gicc: u64,
    pub gicr: u64,
    /// 使える CPU の MPIDR
    pub cpus: [u64; MAX_CPUS],
    pub ncpus: usize,
    /// FADT の ARM_BOOT_ARCH: PSCI があるか、hvc で呼ぶか
    pub psci: bool,
    pub psci_hvc: bool,
    /// FADT の Preferred_PM_Profile (45)
    pub pm_profile: u8,
    /// SPCR: シリアルの番地と割り込み (PL011 か SBSA の generic UART のときだけ)
    pub uart: u64,
    pub uart_gsiv: u32,
    pub devs: [Dev; MAX_DEVS],
    pub ndevs: usize,
}

static INFO: crate::cell::Early<Info> = crate::cell::Early::new(Info {
    rsdp: 0,
    tables: [([0; 4], 0, 0); MAX_TABLES],
    ntables: 0,
    oem: [0; 6],
    oem_table: [0; 8],
    revision: 0,
    gicd: 0,
    gic_version: 0,
    gicc: 0,
    gicr: 0,
    cpus: [0; MAX_CPUS],
    ncpus: 0,
    psci: false,
    psci_hvc: false,
    pm_profile: 0,
    uart: 0,
    uart_gsiv: 0,
    devs: [NO_DEV; MAX_DEVS],
    ndevs: 0,
});

/// ACPI の表を読めたか (DTB のかわりに使うか)
pub fn present() -> bool {
    INFO.get().rsdp != 0
}

/// 物理アドレスの表を読む (RAM として写してあるところだけ)
fn phys(pa: u64, len: usize) -> Option<&'static [u8]> {
    if pa == 0 || len == 0 || !crate::memlayout::is_mapped_ram(pa as usize, len) {
        return None;
    }
    Some(unsafe { core::slice::from_raw_parts(p2v(pa as usize) as *const u8, len) })
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    b.get(o..o + 2).map_or(0, |x| u16::from_le_bytes([x[0], x[1]]))
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    b.get(o..o + 4).map_or(0, |x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]))
}

fn u64_at(b: &[u8], o: usize) -> u64 {
    u32_at(b, o) as u64 | (u32_at(b, o + 4) as u64) << 32
}

/// 表 (頭 36 バイトの長さまで)
fn table(pa: u64) -> Option<&'static [u8]> {
    let head = phys(pa, 36)?;
    let len = u32_at(head, 4) as usize;
    if !(36..=16 << 20).contains(&len) {
        return None;
    }
    phys(pa, len)
}

fn sum(b: &[u8]) -> u8 {
    b.iter().fold(0u8, |a, &c| a.wrapping_add(c))
}

/// ACPI 2.0 の RSDP (EFI の設定テーブル: main が efi.rs からもらってわたす) から読む。DTB がないときに、何よりも先に
pub fn init(rsdp: u64) -> bool {
    let Some(r) = phys(rsdp, 36) else { return false };
    // "RSD PTR "、版 2 から XSDT。はじめの 20 バイトと全体の和は 0
    if &r[..8] != b"RSD PTR " || r[15] < 2 || sum(&r[..20]) != 0 || sum(&r[..36]) != 0 {
        return false;
    }
    let Some(xsdt) = table(u64_at(r, 24)).filter(|t| &t[..4] == b"XSDT") else { return false };
    let i = INFO.get_mut();
    i.rsdp = rsdp;
    i.revision = r[15];
    i.oem.copy_from_slice(&r[9..15]);
    i.oem_table.copy_from_slice(&xsdt[16..24]);
    add_table(i, u64_at(r, 24), xsdt);
    for o in (36..xsdt.len()).step_by(8) {
        let pa = u64_at(xsdt, o);
        let Some(t) = table(pa) else { continue };
        add_table(i, pa, t);
        match &t[..4] {
            b"APIC" => madt(i, t),
            b"SPCR" => spcr(i, t),
            b"FACP" => {
                // ARM_BOOT_ARCH (129): bit 0 PSCI がある、bit 1 hvc で呼ぶ。X_DSDT (140)、なければ DSDT (40)
                let flags = u16_at(t, 129);
                i.pm_profile = t.get(45).copied().unwrap_or(0);
                i.psci = flags & 1 != 0;
                i.psci_hvc = flags & 2 != 0;
                let dsdt = if t.len() >= 148 && u64_at(t, 140) != 0 { u64_at(t, 140) } else { u32_at(t, 40) as u64 };
                if let Some(d) = table(dsdt).filter(|d| &d[..4] == b"DSDT") {
                    add_table(i, dsdt, d);
                    aml(i, &d[36..]);
                }
            }
            b"SSDT" => aml(i, &t[36..]),
            _ => {}
        }
    }
    true
}

fn add_table(i: &mut Info, pa: u64, t: &[u8]) {
    if i.ntables < MAX_TABLES {
        let mut sig = [0; 4];
        sig.copy_from_slice(&t[..4]);
        i.tables[i.ntables] = (sig, pa, t.len() as u32);
        i.ntables += 1;
    }
}

/// MADT: GICC (0x0b: CPU ごと)、GICD (0x0c)、GICR (0x0e)
fn madt(i: &mut Info, t: &[u8]) {
    let mut o = 44;
    while o + 2 <= t.len() {
        let (ty, len) = (t[o], t[o + 1] as usize);
        if len < 2 || o + len > t.len() {
            break;
        }
        let e = &t[o..o + len];
        match ty {
            0x0b if len >= 76 => {
                // flags (12): bit 0 使える、bit 3 あとで使える (online capable)
                let flags = u32_at(e, 12);
                if flags & 1 != 0 && i.ncpus < MAX_CPUS {
                    i.cpus[i.ncpus] = u64_at(e, 68) & 0xff_00ff_ffff;
                    i.ncpus += 1;
                }
                if i.gicc == 0 {
                    i.gicc = u64_at(e, 32);
                }
                if i.gicr == 0 && u64_at(e, 60) != 0 {
                    i.gicr = u64_at(e, 60);
                }
            }
            0x0c if len >= 24 => {
                i.gicd = u64_at(e, 8);
                i.gic_version = e[20];
            }
            0x0e if len >= 16 => i.gicr = u64_at(e, 4),
            _ => {}
        }
        o += len;
    }
}

/// SPCR: interface type (36) が PL011 (3) か SBSA の generic UART (0x0e、0x0f) なら、番地 (GAS の 44) と割り込み (54)
fn spcr(i: &mut Info, t: &[u8]) {
    if t.len() >= 58 && matches!(t[36], 0x03 | 0x0e | 0x0f) && t[40] == 0 {
        i.uart = u64_at(t, 44);
        i.uart_gsiv = u32_at(t, 54);
    }
}

// ---- AML (DSDT と SSDT): Device の _HID、_UID、_CRS だけ ----

/// PkgLength: (値, 自分のバイト数)。値は PkgLength 自身から数える
fn pkglen(b: &[u8], i: usize) -> Option<(usize, usize)> {
    let lead = *b.get(i)?;
    let n = (lead >> 6) as usize;
    if n == 0 {
        return Some(((lead & 0x3f) as usize, 1));
    }
    let mut v = (lead & 0x0f) as usize;
    for k in 0..n {
        v |= (*b.get(i + 1 + k)? as usize) << (4 + 8 * k);
    }
    Some((v, n + 1))
}

fn is_seg(b: &[u8]) -> bool {
    b.len() == 4 && (b[0].is_ascii_uppercase() || b[0] == b'_') && b[1..].iter().all(|&c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
}

/// NameString の長さ (\ と ^ のあとの 1 つ、2 つ (0x2e)、いくつか (0x2f) の NameSeg)
fn name_len(b: &[u8], mut i: usize) -> Option<usize> {
    let s = i;
    while matches!(b.get(i), Some(b'\\') | Some(b'^')) {
        i += 1;
    }
    let segs = match *b.get(i)? {
        0x2e => {
            i += 1;
            2
        }
        0x2f => {
            i += 2;
            *b.get(i - 1)? as usize
        }
        0 => return Some(i + 1 - s),
        _ => 1,
    };
    for _ in 0..segs {
        if !is_seg(b.get(i..i + 4)?) {
            return None;
        }
        i += 4;
    }
    Some(i - s)
}

/// 数 (ZeroOp、OneOp、OnesOp、BytePrefix、WordPrefix、DWordPrefix、QWordPrefix): (値, バイト数)
fn integer(b: &[u8], i: usize) -> Option<(u64, usize)> {
    Some(match *b.get(i)? {
        0x00 => (0, 1),
        0x01 => (1, 1),
        0xff => (u64::MAX, 1),
        0x0a => (*b.get(i + 1)? as u64, 2),
        0x0b => (u16_at(b.get(i + 1..i + 3)?, 0) as u64, 3),
        0x0c => (u32_at(b.get(i + 1..i + 5)?, 0) as u64, 5),
        0x0e => (u64_at(b.get(i + 1..i + 9)?, 0), 9),
        _ => return None,
    })
}

/// 圧縮した EISA ID (PNP0A08 など) を文字に
fn eisa(v: u32) -> [u8; 8] {
    let v = v.swap_bytes();
    let c = |s: u32| b'@' + ((v >> s) & 0x1f) as u8;
    let h = |s: u32| b"0123456789ABCDEF"[((v >> s) & 0xf) as usize];
    [c(26), c(21), c(16), h(12), h(8), h(4), h(0), 0]
}

/// Device の中身 (入れ子の Device の前まで) から Name(NAME, ...) の値のはじめ
fn name_in(body: &[u8], name: &[u8; 4]) -> Option<usize> {
    let mut i = 0;
    while i + 5 < body.len() {
        if body[i] == 0x5b && body[i + 1] == 0x82 {
            return None;
        }
        if body[i] == 0x08 && &body[i + 1..i + 5] == name {
            return Some(i + 5);
        }
        i += 1;
    }
    None
}

/// Device(...) を見つけて、_HID と _CRS を Dev に
fn aml(i: &mut Info, b: &[u8]) {
    let mut p = 0;
    while p + 2 < b.len() {
        if b[p] != 0x5b || b[p + 1] != 0x82 {
            p += 1;
            continue;
        }
        let Some((len, n)) = pkglen(b, p + 2) else { break };
        let end = p + 2 + len;
        let Some(nl) = name_len(b, p + 2 + n) else {
            p += 1;
            continue;
        };
        if end > b.len() || len < n + nl {
            p += 1;
            continue;
        }
        let body = &b[p + 2 + n + nl..end];
        if let Some(d) = device(body)
            && i.ndevs < MAX_DEVS
        {
            i.devs[i.ndevs] = d;
            i.ndevs += 1;
        }
        // 入れ子の Device も見るので、中身の頭から続ける
        p += 2;
    }
}

fn device(body: &[u8]) -> Option<Dev> {
    let h = name_in(body, b"_HID")?;
    let mut d = NO_DEV;
    match *body.get(h)? {
        0x0d => {
            let s = &body[h + 1..];
            let n = s.iter().position(|&c| c == 0)?.min(8);
            d.hid[..n].copy_from_slice(&s[..n]);
        }
        0x0c => d.hid = eisa(u32_at(body, h + 1)),
        _ => return None,
    }
    if let Some(u) = name_in(body, b"_UID") {
        d.uid = integer(body, u).map_or(0, |(v, _)| v as u32);
    }
    if let Some(c) = name_in(body, b"_CRS")
        && body.get(c) == Some(&0x11)
    {
        let (len, n) = pkglen(body, c + 1)?;
        let (_, sz) = integer(body, c + 1 + n)?;
        let s = c + 1 + n + sz;
        if let Some(res) = body.get(s..c + 1 + len) {
            resources(res, &mut d);
        }
    }
    Some(d)
}

/// ResourceTemplate: はじめの番地の範囲 (Memory32Fixed、Memory32、DWord / QWord の Memory) と割り込み (Extended Interrupt)
fn resources(r: &[u8], d: &mut Dev) {
    let mut o = 0;
    while o < r.len() {
        let t = r[o];
        if t & 0x80 == 0 {
            // 小さいもの: End Tag (0x79) で終わり
            if (t >> 3) & 0xf == 0xf {
                break;
            }
            o += 1 + (t & 7) as usize;
            continue;
        }
        let len = u16_at(r, o + 1) as usize;
        let e = match r.get(o + 3..o + 3 + len) {
            Some(e) => e,
            None => break,
        };
        match t & 0x7f {
            0x06 if d.size == 0 && len >= 9 => {
                d.base = u32_at(e, 1) as u64;
                d.size = u32_at(e, 5) as u64;
            }
            0x05 if d.size == 0 && len >= 17 => {
                d.base = u32_at(e, 1) as u64;
                d.size = u32_at(e, 13) as u64;
            }
            0x07 if d.size == 0 && len >= 23 && e[0] == 0 => {
                d.base = u32_at(e, 7) as u64 + u32_at(e, 15) as u64;
                d.size = u32_at(e, 19) as u64;
            }
            0x0a if d.size == 0 && len >= 43 && e[0] == 0 => {
                d.base = u64_at(e, 11).wrapping_add(u64_at(e, 27));
                d.size = u64_at(e, 35);
            }
            0x09 if d.gsiv == 0 && len >= 6 && e[1] >= 1 => d.gsiv = u32_at(e, 2),
            _ => {}
        }
        o += 3 + len;
    }
}

// ---- dtb.rs の口のかわり ----

fn hid_is(d: &Dev, hid: &str) -> bool {
    let n = d.hid.iter().position(|&c| c == 0).unwrap_or(8);
    &d.hid[..n] == hid.as_bytes()
}

/// DT の compatible に当たる ACPI の _HID (Linux のドライバの acpi_match_table と同じもの)
fn hid_of(compatible: &str) -> Option<&'static str> {
    Some(match compatible {
        "arm,pl011" => "ARMH0011",
        "arm,pl061" => "ARMH0061",
        "virtio,mmio" => "LNRO0005",
        "qemu,fw-cfg-mmio" => "QEMU0002",
        _ => return None,
    })
}

/// compatible のものの i 番目の番地の範囲 (dtb::reg_of と同じ形)
pub fn reg_of(compatible: &str, idx: usize) -> Option<(u64, u64)> {
    let i = INFO.get();
    match (compatible, idx) {
        ("arm,gic-v3", 0) if i.gic_version >= 3 => return Some((i.gicd, 0x1_0000)),
        ("arm,gic-v3", 1) if i.gic_version >= 3 && i.gicr != 0 => return Some((i.gicr, 0x2_0000 * i.ncpus.max(1) as u64)),
        ("arm,cortex-a15-gic" | "arm,gic-400", 0) if i.gicd != 0 && i.gic_version < 3 => return Some((i.gicd, 0x1000)),
        ("arm,cortex-a15-gic" | "arm,gic-400", 1) if i.gicc != 0 && i.gic_version < 3 => return Some((i.gicc, 0x2000)),
        ("arm,pl011", 0) if i.uart != 0 => return Some((i.uart, 0x1000)),
        _ => {}
    }
    let hid = hid_of(compatible)?;
    i.devs[..i.ndevs].iter().filter(|d| hid_is(d, hid) && d.size != 0).nth(idx).map(|d| (d.base, d.size))
}

/// interrupts の i 番目のセル (DT の GIC の形: <種類 (0 = SPI、1 = PPI) 番号 flags>)
pub fn irq_cell(compatible: &str, idx: usize) -> Option<u32> {
    let i = INFO.get();
    let gsiv = match compatible {
        "arm,pl011" if i.uart != 0 => i.uart_gsiv,
        c => {
            let hid = hid_of(c)?;
            i.devs[..i.ndevs].iter().find(|d| hid_is(d, hid))?.gsiv
        }
    };
    match idx {
        0 => Some(if gsiv < 32 { 1 } else { 0 }),
        1 => Some(if gsiv < 32 { gsiv.saturating_sub(16) } else { gsiv - 32 }),
        2 => Some(4),
        _ => None,
    }
}

/// virtio-mmio (LNRO0005) ごとに f(番地, SPI の番号)
pub fn each_virtio(mut f: impl FnMut(u64, u32)) {
    let i = INFO.get();
    for d in &i.devs[..i.ndevs] {
        if hid_is(d, "LNRO0005") && d.size != 0 && d.gsiv >= 32 {
            f(d.base, d.gsiv - 32);
        }
    }
}

pub fn psci_method() -> Option<&'static str> {
    let i = INFO.get();
    i.psci.then_some(if i.psci_hvc { "hvc" } else { "smc" })
}

/// CPU ごとに f(MPIDR, "psci", None) (ACPI の arm64 は PSCI で起こす。parking protocol は使わない)
pub fn each_cpu(mut f: impl FnMut(u64, Option<&'static str>, Option<u64>)) {
    let i = INFO.get();
    for &m in &i.cpus[..i.ncpus] {
        f(m, Some("psci"), None);
    }
}

/// /sys/firmware/acpi/tables の名前と中身: XSDT が指す表と DSDT (XSDT そのものは入れない)。同じ名前がいくつかあれば
/// みな 1 から番号をつける (Linux の acpi_table_attr_init)
pub fn sysfs_tables() -> alloc::vec::Vec<(alloc::string::String, &'static [u8])> {
    let i = INFO.get();
    let list: alloc::vec::Vec<&([u8; 4], u64, u32)> = i.tables[..i.ntables].iter().filter(|t| &t.0 != b"XSDT").collect();
    let mut out = alloc::vec::Vec::new();
    for (k, &&(sig, pa, len)) in list.iter().enumerate() {
        let Some(b) = phys(pa, len as usize) else { continue };
        let name = core::str::from_utf8(&sig).unwrap_or("????");
        let same = list.iter().filter(|t| t.0 == sig).count();
        if same > 1 {
            let nth = list[..=k].iter().filter(|t| t.0 == sig).count();
            out.push((alloc::format!("{}{}", name, nth), b));
        } else {
            out.push((name.into(), b));
        }
    }
    out
}

pub fn pm_profile() -> u8 {
    INFO.get().pm_profile
}

/// QEMU の表か (OEM ID が BOCHS: ACPI に書いていない QEMU virt の PL031 を使ってよい)
pub fn is_qemu() -> bool {
    &INFO.get().oem == b"BOCHS "
}

/// 起動のときに見つけたものを出す
pub fn summary() {
    let i = INFO.get();
    fn s(b: &[u8]) -> &str {
        core::str::from_utf8(b).unwrap_or("?").trim_end_matches(['\0', ' '])
    }
    print!("acpi: RSDP {:#x} (rev {}) {} {}:", i.rsdp, i.revision, s(&i.oem), s(&i.oem_table));
    for (sig, _, _) in &i.tables[..i.ntables] {
        print!(" {}", s(sig));
    }
    println!();
    println!(
        "acpi:   gic v{} dist {:#x} {} {:#x}, {} cpus, psci {}, uart {:#x} irq {}",
        i.gic_version,
        i.gicd,
        if i.gic_version >= 3 { "redist" } else { "cpu" },
        if i.gic_version >= 3 { i.gicr } else { i.gicc },
        i.ncpus,
        psci_method().unwrap_or("none"),
        i.uart,
        i.uart_gsiv
    );
    let mut virtio = 0;
    each_virtio(|_, _| virtio += 1);
    println!("acpi:   {} devices in the DSDT, virtio-mmio x{}", i.ndevs, virtio);
}

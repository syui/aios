// デバイスツリー (Flattened Device Tree, DTB) を読む
//
// カーネルを Linux の arm64 Image として起動すると (bin/run.sh)、QEMU やラズパイの
// ファームウェアは x0 に DTB の物理アドレスを入れてくれる (boot.rs が boot_dtb に置く)。
// 機械ごとの違い (メモリ、UART / 割り込みコントローラ / タイマ / virtio の場所、起動の引数)
// はここから読む。起動のすぐ後 (ヒープより前) にも使うので、読むところはメモリを確保しない。
//
// ラズパイのように、周辺機器がバスのアドレス (0x7e20_1000 など) で書かれていれば、
// 親の ranges をたどって CPU から見た物理アドレス (0x3f20_1000) に直す。
use crate::memlayout::p2v;

const MAGIC: u32 = 0xd00d_feed;
const BEGIN_NODE: u32 = 1;
const END_NODE: u32 = 2;
const PROP: u32 = 3;
const NOP: u32 = 4;

static mut BLOB: &[u8] = &[];
/// DTB の写し (DTB は RAM のどこにあるかわからず、kalloc が上書きするかもしれないので)
const MAX: usize = 1024 * 1024;
static mut COPY: [u8; MAX] = [0; MAX];

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
}

fn blob() -> &'static [u8] {
    unsafe { *(&raw const BLOB) }
}

/// 0 で終わる文字列 (終わりの 0 は含めない)
fn cstr(b: &'static [u8], o: usize) -> &'static [u8] {
    let end = b[o..].iter().position(|&c| c == 0).map_or(b.len(), |e| o + e);
    &b[o..end]
}

/// 起動のときにもらった DTB (物理アドレス) を写して覚える。何よりも先に呼ぶ
pub fn init() -> bool {
    unsafe extern "C" {
        static boot_dtb: u64;
    }
    let pa = unsafe { core::ptr::read_volatile(&raw const boot_dtb) } as usize;
    if pa == 0 || pa % 8 != 0 || !crate::memlayout::is_mapped_ram(pa, 40) {
        return false;
    }
    let head = unsafe { core::slice::from_raw_parts(p2v(pa) as *const u8, 40) };
    if be32(head, 0) != MAGIC {
        return false;
    }
    // totalsize には後ろの空き (ブートローダーが書き足すための) も入るので、
    // 中身 (構造ブロックと文字列ブロックの終わり) だけを写す
    let total = be32(head, 4) as usize;
    let used = (be32(head, 8) + be32(head, 36)).max(be32(head, 12) + be32(head, 32)).max(be32(head, 16)) as usize;
    let size = used.min(total);
    if !(40..=MAX).contains(&size) || !crate::memlayout::is_mapped_ram(pa, size) {
        return false;
    }
    unsafe {
        let src = core::slice::from_raw_parts(p2v(pa) as *const u8, size);
        let dst = &mut *(&raw mut COPY);
        dst[..size].copy_from_slice(src);
        *(&raw mut BLOB) = &dst[..size];
    }
    true
}

pub fn present() -> bool {
    !blob().is_empty()
}

const DEPTH: usize = 16;

/// たどっている途中の 1 段 (そのノードの子のための #address-cells / #size-cells と ranges)
#[derive(Clone, Copy)]
struct Level {
    name: &'static [u8],
    ac: u32,
    sc: u32,
    ranges: Option<&'static [u8]>,
    /// このノードのプロパティの並び (構造ブロックの中の位置)
    props: (usize, usize),
}

const EMPTY: Level = Level { name: &[], ac: 2, sc: 1, ranges: None, props: (0, 0) };

/// 1 つのノード。親たちの情報も持つ (reg の読みかえに使う)
pub struct Node<'a> {
    levels: &'a [Level],
}

impl Node<'_> {
    fn me(&self) -> &Level {
        self.levels.last().unwrap()
    }

    /// ノードの名前 (例: serial@7e201000)
    pub fn name(&self) -> &'static str {
        core::str::from_utf8(self.me().name).unwrap_or("")
    }

    /// 深さ (ルートが 0)
    pub fn depth(&self) -> usize {
        self.levels.len() - 1
    }

    pub fn prop(&self, name: &str) -> Option<&'static [u8]> {
        let b = blob();
        let strs = be32(b, 12) as usize;
        let (mut o, end) = self.me().props;
        while o < end {
            if be32(b, o) != PROP {
                o += 4;
                continue;
            }
            let len = be32(b, o + 4) as usize;
            let n = cstr(b, strs + be32(b, o + 8) as usize);
            let v = &b[o + 12..o + 12 + len];
            if n == name.as_bytes() {
                return Some(v);
            }
            o = (o + 12 + len + 3) & !3;
        }
        None
    }

    /// 文字列のプロパティ (最初の 1 つ)
    pub fn str(&self, name: &str) -> Option<&'static str> {
        let v = self.prop(name)?;
        let end = v.iter().position(|&c| c == 0).unwrap_or(v.len());
        core::str::from_utf8(&v[..end]).ok()
    }

    pub fn is_compatible(&self, c: &str) -> bool {
        self.prop("compatible").is_some_and(|v| v.split(|&x| x == 0).any(|s| s == c.as_bytes()))
    }

    /// status が "okay" か、書いてない
    pub fn enabled(&self) -> bool {
        self.str("status").is_none_or(|s| s == "okay" || s == "ok")
    }

    /// reg の i 番目 (CPU から見た物理アドレス, 大きさ)
    pub fn reg(&self, i: usize) -> Option<(u64, u64)> {
        let v = self.prop("reg")?;
        let parent = self.levels[self.levels.len().checked_sub(2)?];
        let (ac, sc) = (parent.ac as usize, parent.sc as usize);
        let step = (ac + sc) * 4;
        if step == 0 || v.len() < (i + 1) * step {
            return None;
        }
        let addr = cells(v, i * step, ac);
        let size = cells(v, i * step + ac * 4, sc);
        Some((self.translate(addr)?, size))
    }

    /// 親たちの ranges をたどって、ルートのアドレスに直す
    fn translate(&self, mut addr: u64) -> Option<u64> {
        // levels[k] (バス) の子のアドレスを、levels[k - 1] の空間へ
        for k in (1..self.levels.len() - 1).rev() {
            let bus = self.levels[k];
            let up = self.levels[k - 1];
            let Some(r) = bus.ranges else { continue };
            if r.is_empty() {
                continue; // 空の ranges は 1 対 1
            }
            let (ca, pa, sz) = (bus.ac as usize, up.ac as usize, bus.sc as usize);
            let step = (ca + pa + sz) * 4;
            let mut hit = None;
            for e in 0..r.len() / step {
                let child = cells(r, e * step, ca);
                let parent = cells(r, e * step + ca * 4, pa);
                let size = cells(r, e * step + (ca + pa) * 4, sz);
                if addr >= child && addr < child + size {
                    hit = Some(addr - child + parent);
                    break;
                }
            }
            addr = hit?;
        }
        Some(addr)
    }

    /// interrupts の i 番目のセル
    pub fn interrupt_cell(&self, i: usize) -> Option<u32> {
        let v = self.prop("interrupts")?;
        (v.len() >= (i + 1) * 4).then(|| be32(v, i * 4))
    }
}

fn cells(v: &[u8], o: usize, n: usize) -> u64 {
    (0..n).fold(0u64, |a, i| (a << 32) | be32(v, o + i * 4) as u64)
}

/// すべてのノードを見る (子を見終えたところで f)。f が Some を返したらそこで終わる
pub fn scan<T>(mut f: impl FnMut(&Node) -> Option<T>) -> Option<T> {
    let b = blob();
    if b.is_empty() {
        return None;
    }
    let strs = be32(b, 12) as usize;
    let mut stack = [EMPTY; DEPTH];
    let mut depth = 0usize;
    let mut o = be32(b, 8) as usize;
    while o + 4 <= b.len() {
        match be32(b, o) {
            BEGIN_NODE => {
                let name = cstr(b, o + 4);
                o = (o + 4 + name.len() + 1 + 3) & !3;
                if depth >= DEPTH {
                    return None;
                }
                stack[depth] = Level { name, ac: 2, sc: 1, ranges: None, props: (o, o) };
                depth += 1;
            }
            PROP => {
                if depth == 0 {
                    return None;
                }
                let len = be32(b, o + 4) as usize;
                let name = cstr(b, strs + be32(b, o + 8) as usize);
                let val = &b[o + 12..o + 12 + len];
                let lv = &mut stack[depth - 1];
                match name {
                    b"#address-cells" if len == 4 => lv.ac = be32(val, 0),
                    b"#size-cells" if len == 4 => lv.sc = be32(val, 0),
                    b"ranges" => lv.ranges = Some(val),
                    _ => {}
                }
                o = (o + 12 + len + 3) & !3;
                lv.props.1 = o;
            }
            END_NODE => {
                if depth == 0 {
                    return None;
                }
                if let Some(r) = f(&Node { levels: &stack[..depth] }) {
                    return Some(r);
                }
                depth -= 1;
                o += 4;
            }
            NOP => o += 4,
            _ => break,
        }
    }
    None
}

/// compatible が c で、使えるようになっている最初のノードの reg[i]
pub fn reg_of(c: &str, i: usize) -> Option<(u64, u64)> {
    scan(|n| (n.is_compatible(c) && n.enabled()).then(|| n.reg(i)).flatten())
}

/// compatible が c のノードの interrupts の i 番目のセル
pub fn irq_cell(c: &str, i: usize) -> Option<u32> {
    scan(|n| (n.is_compatible(c) && n.enabled()).then(|| n.interrupt_cell(i)).flatten())
}

/// /memory の最初の範囲
pub fn memory() -> Option<(u64, u64)> {
    scan(|n| (n.depth() == 1 && (n.name() == "memory" || n.name().starts_with("memory@"))).then(|| n.reg(0)).flatten())
}

/// /chosen/bootargs (カーネルのコマンドライン)
pub fn bootargs() -> Option<&'static str> {
    scan(|n| (n.depth() == 1 && n.name() == "chosen").then(|| n.str("bootargs")).flatten()).filter(|s| !s.is_empty())
}

/// コマンドラインの key=value の value
pub fn arg(key: &str) -> Option<&'static str> {
    bootargs()?.split_whitespace().find_map(|w| w.strip_prefix(key)?.strip_prefix('='))
}

/// PSCI の呼び方 ("hvc" / "smc")。DTB がなければ qemu virt の hvc
pub fn psci_method() -> Option<&'static str> {
    if !present() {
        return Some("hvc");
    }
    scan(|n| (n.is_compatible("arm,psci") || n.is_compatible("arm,psci-0.2") || n.is_compatible("arm,psci-1.0")).then(|| n.str("method")).flatten())
}

/// ルートの model
pub fn model() -> &'static str {
    scan(|n| (n.depth() == 0).then(|| n.str("model")).flatten()).unwrap_or("")
}

/// virtio,mmio のノードを見る (base, SPI の番号)
pub fn each_virtio(mut f: impl FnMut(u64, u32)) {
    scan(|n| {
        if n.is_compatible("virtio,mmio") && n.enabled() {
            if let (Some((base, _)), Some(spi)) = (n.reg(0), n.interrupt_cell(1)) {
                f(base, spi);
            }
        }
        None::<()>
    });
}

/// 起動のときに見つけたものを出す
pub fn summary() {
    if !present() {
        println!("dtb: none (using QEMU virt defaults)");
        return;
    }
    print!("dtb: {} bytes, {}", blob().len(), model());
    if let Some((base, size)) = memory() {
        print!(", memory {:#x} {} MiB", base, size / (1024 * 1024));
    }
    println!();
    for c in ["arm,pl011", "arm,cortex-a15-gic", "arm,gic-400", "brcm,bcm2836-l1-intc", "brcm,bcm2836-armctrl-ic", "arm,pl031", "brcm,bcm2835-sdhost"] {
        if let Some((a, _)) = reg_of(c, 0) {
            println!("dtb:   {} at {:#x}", c, a);
        }
    }
    let mut virtio = 0;
    each_virtio(|_, _| virtio += 1);
    if virtio > 0 {
        println!("dtb:   virtio,mmio x{}", virtio);
    }
    if let Some(a) = bootargs() {
        println!("dtb: cmdline: {}", a);
    }
}

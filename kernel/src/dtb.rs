// デバイスツリー (Flattened Device Tree, DTB) を読む
//
// カーネルを Linux の arm64 Image として起動すると (bin/run.sh)、QEMU やラズパイの
// ファームウェアは x0 に DTB の物理アドレスを入れてくれる (boot.rs が boot_dtb に置く)。
// 機械ごとの違い (メモリの大きさ、UART や GIC の場所、起動の引数) はここから読むようにしていく。
// DTB は RAM のどこにあるかわからない (kalloc が上書きするかもしれない) ので、最初に写しておく。
use crate::memlayout::{p2v, PHYSBASE};
use alloc::string::String;
use alloc::vec::Vec;

const MAGIC: u32 = 0xd00d_feed;
const BEGIN_NODE: u32 = 1;
const END_NODE: u32 = 2;
const PROP: u32 = 3;
const NOP: u32 = 4;
const END: u32 = 9;

static mut BLOB: &[u8] = &[];
/// DTB の写し (QEMU virt の DTB は 1 MiB に収まる)
const MAX: usize = 1024 * 1024;
static mut COPY: [u8; MAX] = [0; MAX];

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
}

fn blob() -> &'static [u8] {
    unsafe { *(&raw const BLOB) }
}

/// 起動のときにもらった DTB を写して覚える (kalloc を使う前に呼ぶ)
pub fn init() -> bool {
    unsafe extern "C" {
        static boot_dtb: u64;
    }
    let pa = unsafe { core::ptr::read_volatile(&raw const boot_dtb) } as usize;
    // 写像してあるのは RAM (PHYSBASE から 1 GiB) だけ
    if pa < PHYSBASE || pa >= PHYSBASE + 0x4000_0000 - 40 || pa % 8 != 0 {
        return false;
    }
    let head = unsafe { core::slice::from_raw_parts(p2v(pa) as *const u8, 40) };
    if be32(head, 0) != MAGIC {
        return false;
    }
    let size = be32(head, 4) as usize;
    if !(40..=MAX).contains(&size) {
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

pub struct Node {
    /// "/" からのパス (例: /pl011@9000000)
    pub path: String,
    pub props: Vec<(&'static str, &'static [u8])>,
    /// 親の #address-cells / #size-cells (reg を読むのに要る)
    pub addr_cells: u32,
    pub size_cells: u32,
}

impl Node {
    pub fn prop(&self, name: &str) -> Option<&'static [u8]> {
        self.props.iter().find(|(n, _)| *n == name).map(|(_, v)| *v)
    }

    /// 0 区切りの文字列のリスト (compatible など)
    pub fn strings(&self, name: &str) -> Vec<&'static str> {
        self.prop(name).map_or(Vec::new(), |v| v.split(|&c| c == 0).filter(|s| !s.is_empty()).filter_map(|s| core::str::from_utf8(s).ok()).collect())
    }

    pub fn is_compatible(&self, c: &str) -> bool {
        self.strings("compatible").contains(&c)
    }

    /// reg の (アドレス, 大きさ) の並び
    pub fn reg(&self) -> Vec<(u64, u64)> {
        let Some(v) = self.prop("reg") else { return Vec::new() };
        let cell = |o: usize, n: u32| (0..n as usize).fold(0u64, |a, i| (a << 32) | be32(v, o + i * 4) as u64);
        let step = ((self.addr_cells + self.size_cells) * 4) as usize;
        if step == 0 {
            return Vec::new();
        }
        (0..v.len() / step).map(|i| (cell(i * step, self.addr_cells), cell(i * step + self.addr_cells as usize * 4, self.size_cells))).collect()
    }
}

/// すべてのノードを順に見る
pub fn walk(mut f: impl FnMut(&Node)) {
    let b = blob();
    if b.is_empty() {
        return;
    }
    let st = be32(b, 8) as usize;
    let strs = be32(b, 12) as usize;
    let cstr = |o: usize| -> &'static str {
        let end = b[o..].iter().position(|&c| c == 0).map_or(b.len(), |e| o + e);
        core::str::from_utf8(&b[o..end]).unwrap_or("")
    };
    // (ノード, そのノードの #address-cells, #size-cells)
    let mut stack: Vec<(Node, u32, u32)> = Vec::new();
    let mut o = st;
    loop {
        if o + 4 > b.len() {
            break;
        }
        let tok = be32(b, o);
        o += 4;
        match tok {
            BEGIN_NODE => {
                let name = cstr(o);
                o = (o + name.len() + 1 + 3) & !3;
                let (ac, sc) = stack.last().map_or((2, 1), |(_, a, s)| (*a, *s));
                let path = match stack.last() {
                    None => String::from("/"),
                    Some((p, _, _)) if p.path == "/" => alloc::format!("/{}", name),
                    Some((p, _, _)) => alloc::format!("{}/{}", p.path, name),
                };
                // 子のための既定値 (#address-cells = 2, #size-cells = 1)
                stack.push((Node { path, props: Vec::new(), addr_cells: ac, size_cells: sc }, 2, 1));
            }
            PROP => {
                let len = be32(b, o) as usize;
                let nameoff = be32(b, o + 4) as usize;
                let val = &b[o + 8..o + 8 + len];
                o = (o + 8 + len + 3) & !3;
                let name = cstr(strs + nameoff);
                if let Some((n, ac, sc)) = stack.last_mut() {
                    match name {
                        "#address-cells" if len == 4 => *ac = be32(val, 0),
                        "#size-cells" if len == 4 => *sc = be32(val, 0),
                        _ => {}
                    }
                    n.props.push((name, val));
                }
            }
            END_NODE => {
                if let Some((n, _, _)) = stack.pop() {
                    f(&n);
                }
            }
            NOP => {}
            END | _ => break,
        }
    }
}

/// compatible に c を持つノード
pub fn find_compatible(c: &str) -> Vec<Node> {
    let mut v = Vec::new();
    walk(|n| {
        if n.is_compatible(c) {
            v.push(Node { path: n.path.clone(), props: n.props.clone(), addr_cells: n.addr_cells, size_cells: n.size_cells });
        }
    });
    v
}

/// /memory の最初の範囲。ヒープの準備より前に呼ぶので、メモリを確保せずに読む
pub fn memory() -> Option<(u64, u64)> {
    let b = blob();
    if b.is_empty() {
        return None;
    }
    let st = be32(b, 8) as usize;
    let strs = be32(b, 12) as usize;
    let name_at = |o: usize| -> &[u8] {
        let end = b[o..].iter().position(|&c| c == 0).map_or(b.len(), |e| o + e);
        &b[o..end]
    };
    let (mut ac, mut sc) = (2u32, 1u32);
    let mut depth = 0;
    let mut in_memory = false;
    let mut o = st;
    while o + 4 <= b.len() {
        let tok = be32(b, o);
        o += 4;
        match tok {
            BEGIN_NODE => {
                let name = name_at(o);
                o = (o + name.len() + 1 + 3) & !3;
                depth += 1;
                in_memory = depth == 2 && (name == b"memory" || name.starts_with(b"memory@"));
            }
            PROP => {
                let len = be32(b, o) as usize;
                let name = name_at(strs + be32(b, o + 4) as usize);
                let val = &b[o + 8..o + 8 + len];
                o = (o + 8 + len + 3) & !3;
                if depth == 1 && len == 4 {
                    match name {
                        b"#address-cells" => ac = be32(val, 0),
                        b"#size-cells" => sc = be32(val, 0),
                        _ => {}
                    }
                }
                if in_memory && name == b"reg" && len >= ((ac + sc) * 4) as usize {
                    let cell = |off: usize, n: u32| (0..n as usize).fold(0u64, |a, i| (a << 32) | be32(val, off + i * 4) as u64);
                    return Some((cell(0, ac), cell(ac as usize * 4, sc)));
                }
            }
            END_NODE => {
                depth -= 1;
                in_memory = false;
            }
            NOP => {}
            _ => break,
        }
    }
    None
}

/// /chosen/bootargs (カーネルのコマンドライン)
pub fn bootargs() -> Option<&'static str> {
    let mut a = None;
    walk(|n| {
        if n.path == "/chosen" {
            a = n.prop("bootargs").and_then(|v| core::str::from_utf8(v.strip_suffix(&[0]).unwrap_or(v)).ok());
        }
    });
    a.filter(|s| !s.is_empty())
}

/// コマンドラインの key=value の value
pub fn arg(key: &str) -> Option<&'static str> {
    bootargs()?.split_whitespace().find_map(|w| w.strip_prefix(key)?.strip_prefix('='))
}

/// 起動のときに見つけたものを出す
pub fn summary() {
    let b = blob();
    if b.is_empty() {
        println!("dtb: none");
        return;
    }
    let mut model = "";
    walk(|n| {
        if n.path == "/" {
            model = n.prop("model").and_then(|v| core::str::from_utf8(v.strip_suffix(&[0]).unwrap_or(v)).ok()).unwrap_or("");
        }
    });
    print!("dtb: {} bytes, {}", b.len(), model);
    if let Some((base, size)) = memory() {
        print!(", memory {:#x} {} MiB", base, size / (1024 * 1024));
    }
    println!();
    for (c, what) in [("arm,pl011", "uart"), ("arm,cortex-a15-gic", "gic"), ("arm,gic-400", "gic"), ("arm,pl031", "rtc")] {
        for n in find_compatible(c) {
            if let Some(&(a, _)) = n.reg().first() {
                println!("dtb:   {} {} at {:#x}", what, c, a);
            }
        }
    }
    let virtio = find_compatible("virtio,mmio").len();
    if virtio > 0 {
        println!("dtb:   virtio,mmio x{}", virtio);
    }
    if let Some(a) = bootargs() {
        println!("dtb: cmdline: {}", a);
    }
}

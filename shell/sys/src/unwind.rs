// スレッドの呼び出しの並びをたどる (aish-sys の hang の stack)。カーネルの /proc/ai/stack/TID (レジスタ、
// スタックの中身、ファイルを写している地図) と、写しているファイルの .eh_frame (DWARF の CFI) で:
//   1. pc の関数の CFI で CFA (呼んだ側の sp) を出し、保存されたレジスタ (x29、x30 ...) をスタックから戻す
//   2. CFI のないところ (musl の libc はつけていない): いちばん上のフレームなら lr、ほかはスタックの中から
//      「戻り先 (前の命令が bl / blr) になっている値」を探し、そこから CFI で 3 段以上つながる sp を当てる (scan)。
//      つながるものがなければ、戻り先らしい値をそのまま 1 つずつ (guess)
// 名前は .symtab か .dynsym から。名前のないもの (libxul のように strip したもの) は、その関数のあたりで
// 使っている文字列 (adrp + add で指すもの) を手がかりに出す
use object::{Object, ObjectSection, ObjectSegment, ObjectSymbol};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// 写したファイル 1 つ
struct Module {
    name: String,
    data: &'static [u8],
    /// PT_LOAD: (vaddr, ファイルの中の位置, 長さ)
    segs: Vec<(u64, u64, u64)>,
    eh_frame: Option<(u64, &'static [u8])>,
    eh_frame_hdr: Option<(u64, &'static [u8])>,
    text: u64,
    /// (アドレス, 大きさ, 名前) をアドレスの順に
    syms: Vec<(u64, u64, String)>,
}

/// ファイルを読むだけで写す (libxul は 170 MB あるので、読むのは触ったページだけ)。終わるまで外さない
fn map_file(path: &str) -> Option<&'static [u8]> {
    use std::os::unix::io::AsRawFd;
    let f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len() as usize;
    if len == 0 {
        return None;
    }
    let p = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_PRIVATE, f.as_raw_fd(), 0) };
    if p == libc::MAP_FAILED {
        return None;
    }
    Some(unsafe { std::slice::from_raw_parts(p as *const u8, len) })
}

impl Module {
    fn load(path: &str) -> Option<Module> {
        let data = map_file(path)?;
        let f = object::File::parse(data).ok()?;
        let segs = f.segments().filter_map(|s| s.file_range().1.gt(&0).then(|| (s.address(), s.file_range().0, s.file_range().1))).collect();
        let sec = |n: &str| f.section_by_name(n).and_then(|s| Some((s.address(), s.data().ok()?)));
        let mut syms: Vec<(u64, u64, String)> = f.symbols().chain(f.dynamic_symbols()).filter(|s| s.kind() == object::SymbolKind::Text && s.address() != 0).filter_map(|s| Some((s.address(), s.size(), s.name().ok()?.to_string()))).collect();
        syms.sort();
        syms.dedup_by_key(|s| s.0);
        Some(Module {
            name: path.rsplit('/').next().unwrap_or(path).to_string(),
            data,
            segs,
            eh_frame: sec(".eh_frame"),
            eh_frame_hdr: sec(".eh_frame_hdr"),
            text: f.section_by_name(".text").map_or(0, |s| s.address()),
            syms,
        })
    }

    /// ファイルの中の位置 → vaddr
    fn vaddr_of(&self, foff: u64) -> Option<u64> {
        self.segs.iter().find(|(_, o, n)| (*o..o + n).contains(&foff)).map(|(v, o, _)| v + (foff - o))
    }

    /// vaddr の 4 バイト (命令)
    fn insn(&self, va: u64) -> Option<u32> {
        let (v, o, _) = self.segs.iter().find(|(v, _, n)| (*v..v + n).contains(&va))?;
        let at = (o + (va - v)) as usize;
        Some(u32::from_le_bytes(self.data.get(at..at + 4)?.try_into().ok()?))
    }

    /// 戻り先らしいか: 前の命令が bl か blr
    fn is_return(&self, va: u64) -> bool {
        va >= 4 && self.insn(va - 4).is_some_and(|i| i >> 26 == 0b100101 || i & 0xffff_fc1f == 0xd63f_0000)
    }

    /// vaddr の NUL 終わりの文字列 (印字できる 4 文字以上)
    fn string_at(&self, va: u64) -> Option<String> {
        let (v, o, n) = self.segs.iter().find(|(v, _, n)| (*v..v + n).contains(&va))?;
        let at = (o + (va - v)) as usize;
        let end = (o + n) as usize;
        let b = self.data.get(at..end.min(at + 120))?;
        let z = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        let s = &b[..z];
        (s.len() >= 4 && s.iter().all(|&c| (0x20..0x7f).contains(&c))).then(|| String::from_utf8_lossy(s).into_owned())
    }

    /// va の関数の名前 (名前+ずれ)。大きさのわからない名前 (asm のものなど) からなら、あいだに隠れた関数が
    /// あるかもしれないので ? をつける
    fn symbol(&self, va: u64) -> Option<String> {
        let i = self.syms.partition_point(|s| s.0 <= va).checked_sub(1)?;
        let (a, size, n) = &self.syms[i];
        if *size == 0 {
            return Some(format!("{}+{:#x}?", n, va - a));
        }
        (va < a + size).then(|| format!("{}+{:#x}", n, va - a))
    }

    /// 名前のないときの手がかり: va のまわり (前 0x600、後ろ 0x100) で adrp + add が指す文字列の終わりのいくつか
    fn strings_near(&self, va: u64) -> Vec<String> {
        let mut regs = [None::<u64>; 32];
        let mut out: Vec<String> = Vec::new();
        let mut a = va.saturating_sub(0x600) & !3;
        while a < va + 0x100 {
            if let Some(i) = self.insn(a) {
                let rd = (i & 31) as usize;
                if i & 0x9f00_0000 == 0x9000_0000 {
                    // adrp: ページ (pc & !0xfff) + imm << 12
                    let imm = (((i >> 5) & 0x7ffff) << 2 | (i >> 29) & 3) as i64;
                    let imm = (imm << 43) >> 43;
                    regs[rd] = Some(((a & !0xfff) as i64 + (imm << 12)) as u64);
                } else if i & 0xff80_0000 == 0x9100_0000 {
                    // add (64 ビット、即値、シフトなし)
                    let rn = ((i >> 5) & 31) as usize;
                    if let Some(base) = regs[rn] {
                        let t = base + ((i >> 10) & 0xfff) as u64;
                        if let Some(s) = self.string_at(t)
                            && out.last() != Some(&s)
                        {
                            out.push(s);
                        }
                    }
                    regs[rd] = None;
                }
            }
            a += 4;
        }
        let n = out.len();
        out.into_iter().skip(n.saturating_sub(4)).collect()
    }

    /// va の CFI の行 (CFA と保存されたレジスタ): (CFA の基のレジスタ, ずれ, [(レジスタ, CFA からのずれ)])
    fn cfi(&self, va: u64) -> Option<(u16, i64, Vec<(u16, i64)>)> {
        use gimli::{BaseAddresses, CfaRule, EhFrame, EhFrameHdr, LittleEndian, RegisterRule, UnwindContext, UnwindSection};
        let (eh_addr, eh) = self.eh_frame?;
        let eh_frame = EhFrame::new(eh, LittleEndian);
        let mut bases = BaseAddresses::default().set_eh_frame(eh_addr).set_text(self.text);
        let mut ctx = UnwindContext::new();
        let fde = match self.eh_frame_hdr {
            Some((hdr_addr, hdr)) => {
                bases = bases.set_eh_frame_hdr(hdr_addr);
                let h = EhFrameHdr::new(hdr, LittleEndian).parse(&bases, 8).ok()?;
                h.table()?.fde_for_address(&eh_frame, &bases, va, EhFrame::cie_from_offset).ok()?
            }
            None => eh_frame.fde_for_address(&bases, va, EhFrame::cie_from_offset).ok()?,
        };
        let row = fde.unwind_info_for_address(&eh_frame, &bases, &mut ctx, va).ok()?;
        let (reg, off) = match row.cfa() {
            CfaRule::RegisterAndOffset { register, offset } => (register.0, *offset),
            _ => return None,
        };
        let saved = row.registers().filter_map(|(r, rule)| match rule {
            RegisterRule::Offset(o) => Some((r.0, *o)),
            _ => None,
        });
        Some((reg, off, saved.collect()))
    }
}

/// スタックの中身 (sp から) と、地図と、読みこんだファイル
struct Ctx {
    sp0: u64,
    stack: Vec<u8>,
    /// (lo, hi, ファイルの中の位置, パス)
    maps: Vec<(u64, u64, u64, String)>,
    mods: BTreeMap<String, Option<Module>>,
}

impl Ctx {
    fn word(&self, va: u64) -> Option<u64> {
        let at = va.checked_sub(self.sp0)? as usize;
        Some(u64::from_le_bytes(self.stack.get(at..at + 8)?.try_into().ok()?))
    }

    /// va の (ファイル, vaddr)
    fn locate(&mut self, va: u64) -> Option<(String, u64)> {
        let (lo, _, off, path) = self.maps.iter().find(|(lo, hi, _, _)| (*lo..*hi).contains(&va))?.clone();
        let m = self.mods.entry(path.clone()).or_insert_with(|| Module::load(&path)).as_ref()?;
        Some((path, m.vaddr_of(va - lo + off)?))
    }

    fn module(&self, path: &str) -> Option<&Module> {
        self.mods.get(path)?.as_ref()
    }

    /// pc から CFI で 1 段戻る。regs は x0..x30 と 31 (sp)。first はいちばん上 (pc そのものの命令で)
    fn step(&mut self, pc: u64, regs: &mut [Option<u64>; 32], first: bool) -> Option<u64> {
        let (path, va) = self.locate(pc)?;
        let (reg, off, saved) = self.module(&path)?.cfi(if first { va } else { va - 1 })?;
        let cfa = (regs[reg as usize]? as i64 + off) as u64;
        let mut ra = if first { regs[30] } else { None };
        for (r, o) in saved {
            let v = self.word((cfa as i64 + o) as u64)?;
            if (r as usize) < 31 {
                regs[r as usize] = Some(v);
            }
            if r == 30 {
                ra = Some(v);
            }
        }
        regs[31] = Some(cfa);
        let ra = ra?;
        // 戻り先は、写したファイルの中で、前の命令が bl / blr のところ
        let (p2, va2) = self.locate(ra)?;
        self.module(&p2)?.is_return(va2).then_some(ra)
    }

    /// CFI だけで何段つながるか (当てた sp がよいかを見る)
    fn chain_len(&mut self, pc: u64, regs: [Option<u64>; 32], max: usize) -> usize {
        let (mut pc, mut regs) = (pc, regs);
        for n in 0..max {
            match self.step(pc, &mut regs, false) {
                Some(ra) => pc = ra,
                None => return n,
            }
        }
        max
    }

    /// CFI のないところから: 今の sp より上で戻り先になっている値を探し、そこから 3 段以上つながる sp を当てる。
    /// (戻り先の場所, 当てた sp, その戻り先, 確かめられたか)。つながるものがなければ、はじめに見つけた戻り先を
    /// 確かめずに (sp はその次の場所)
    fn scan(&mut self, from: u64, regs: &[Option<u64>; 32]) -> Option<(u64, u64, u64, bool)> {
        let end = self.sp0 + self.stack.len() as u64;
        let mut first: Option<(u64, u64, u64, bool)> = None;
        let mut s = (from + 7) & !7;
        while s + 8 <= end {
            if let Some(v) = self.word(s)
                && let Some((p, va)) = self.locate(v)
                && self.module(&p).is_some_and(|m| m.is_return(va))
            {
                first.get_or_insert((s, s + 8, v, false));
                let mut best = None;
                let mut v_sp = s + 8;
                while v_sp < (s + 0x800).min(end) {
                    let mut r = *regs;
                    r[31] = Some(v_sp);
                    r[29] = s.checked_sub(8).and_then(|a| self.word(a));
                    if self.chain_len(v, r, 3) >= 3 {
                        best = Some(v_sp);
                        break;
                    }
                    v_sp += 8;
                }
                if let Some(b) = best {
                    return Some((s, b, v, true));
                }
            }
            s += 8;
        }
        first
    }

    fn describe(&mut self, pc: u64, how: &str) -> Value {
        match self.locate(pc) {
            Some((path, va)) => {
                let m = self.module(&path).unwrap();
                let look = if how == "pc" { va } else { va - 4 };
                let mut f = json!({ "pc": format!("{:#x}", pc), "file": m.name, "off": format!("{:#x}", va), "how": how });
                match m.symbol(look) {
                    Some(sym) => f["sym"] = json!(sym),
                    None => {
                        let near = m.strings_near(look);
                        if !near.is_empty() {
                            f["strings_near"] = json!(near);
                        }
                    }
                }
                f
            }
            None => json!({ "pc": format!("{:#x}", pc), "how": how }),
        }
    }
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).filter_map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()).collect()
}

/// /proc/ai/stack/TID の JSON から、呼び出しの並び (いちばん上から)
pub fn frames(st: &Value, max: usize) -> Vec<Value> {
    let num = |v: &Value| v.as_u64().or_else(|| v.as_str().and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok()));
    let mut regs = [None; 32];
    if let Some(x) = st["x"].as_array() {
        for (i, v) in x.iter().enumerate().take(31) {
            regs[i] = v.as_u64();
        }
    }
    let sp0 = st["sp"].as_u64().unwrap_or(0);
    regs[31] = Some(sp0);
    let maps = st["maps"]
        .as_array()
        .map(|m| {
            m.iter()
                .filter_map(|e| {
                    let lo = u64::from_str_radix(e[0].as_str()?, 16).ok()?;
                    let hi = u64::from_str_radix(e[1].as_str()?, 16).ok()?;
                    let off = u64::from_str_radix(e[2].as_str()?, 16).ok()?;
                    let path = e[4].as_str()?;
                    Some((lo, hi, off, if path.starts_with('/') { path.to_string() } else { format!("/{}", path) }))
                })
                .collect()
        })
        .unwrap_or_default();
    let mut c = Ctx { sp0, stack: hex(st["stack"].as_str().unwrap_or("")), maps, mods: BTreeMap::new() };
    let mut pc = num(&st["pc"]).unwrap_or(0);
    let mut out = vec![c.describe(pc, "pc")];
    let mut first = true;
    while out.len() < max {
        if let Some(ra) = c.step(pc, &mut regs, first) {
            pc = ra;
            out.push(c.describe(pc, "cfi"));
        } else if first && c.locate(regs[30].unwrap_or(0)).is_some() {
            // いちばん上で CFI がない (libc の中のシステムコール): 葉の関数として lr へ
            pc = regs[30].unwrap();
            out.push(c.describe(pc, "lr"));
        } else if let Some((slot, sp, ra, sure)) = c.scan(regs[31].unwrap_or(sp0), &regs) {
            // CFI のないところ: スタックから戻り先を探して、つながる sp を当てる (x29 はその前に保存されたもの)。
            // つながるものがなければ、戻り先らしい値を 1 つずつ (guess: たまたまスタックに残った古い値かもしれない)
            regs[31] = Some(sp);
            regs[29] = slot.checked_sub(8).and_then(|a| c.word(a));
            pc = ra;
            out.push(c.describe(pc, if sure { "scan" } else { "guess" }));
        } else {
            break;
        }
        first = false;
    }
    out
}

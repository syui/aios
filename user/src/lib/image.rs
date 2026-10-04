#![allow(dead_code)] // aios (作る) と aiosd (確かめる) で使うものが違う
// カーネルの ELF から、起動できる Image (Linux の arm64 Image 形式。aiboot と UEFI が読む) を作る。
// llvm-objcopy -O binary と同じ: 中身のある PT_LOAD を物理アドレスの順に並べ、すき間は 0 で埋める
// (aios には objcopy がないので、aios build kernel が自分でする)

/// Image のヘッダーの印 (先頭から 0x38 に "ARM\x64")
pub fn is_image(b: &[u8]) -> bool {
    b.len() > 0x40 && &b[0x38..0x3c] == b"ARM\x64"
}

pub fn elf_to_image(elf: &[u8]) -> Result<Vec<u8>, String> {
    let u16_at = |o: usize| elf.get(o..o + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize);
    let u64_at = |o: usize| elf.get(o..o + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()));
    if elf.get(0..4) != Some(b"\x7fELF") || elf.get(4) != Some(&2) {
        return Err("not a 64-bit ELF".into());
    }
    let (phoff, phentsize, phnum) = (u64_at(0x20).ok_or("bad ELF")? as usize, u16_at(0x36).ok_or("bad ELF")?, u16_at(0x38).ok_or("bad ELF")?);
    // (物理アドレス, ファイルの中の場所, 大きさ)
    let mut segs = Vec::new();
    for i in 0..phnum {
        let p = phoff + i * phentsize;
        let typ = elf.get(p..p + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).ok_or("bad ELF")?;
        let (off, paddr, filesz) = (u64_at(p + 8).ok_or("bad ELF")?, u64_at(p + 24).ok_or("bad ELF")?, u64_at(p + 32).ok_or("bad ELF")?);
        if typ == 1 && filesz > 0 {
            segs.push((paddr, off as usize, filesz as usize));
        }
    }
    let base = segs.iter().map(|s| s.0).min().ok_or("no loadable segment")?;
    let end = segs.iter().map(|s| s.0 + s.2 as u64).max().unwrap_or(base);
    let mut out = vec![0u8; (end - base) as usize];
    for (paddr, off, len) in segs {
        let src = elf.get(off..off + len).ok_or("segment outside the file")?;
        let at = (paddr - base) as usize;
        out[at..at + len].copy_from_slice(src);
    }
    if !is_image(&out) {
        return Err("the result has no arm64 Image header (is this the aios kernel?)".into());
    }
    Ok(out)
}

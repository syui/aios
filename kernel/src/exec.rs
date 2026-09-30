// ELF (静的リンク, ET_EXEC) を新しいアドレス空間に読み込み、
// Linux と同じ形 (argc, argv, envp, auxv) のスタックを作る
use crate::initrd;
use crate::memlayout::PGSIZE;
use crate::vm::{pg_up, PageTable, Perm};
use alloc::vec::Vec;

pub const USER_STACK_TOP: usize = 0x40_0000_0000;
const USER_STACK_SIZE: usize = 256 * 1024;
const ARG_MAX: usize = 64 * 1024;

const ENOENT: i64 = 2;
const ENOEXEC: i64 = 8;
const ENOMEM: i64 = 12;
const E2BIG: i64 = 7;

const PT_LOAD: u32 = 1;
const PF_X: u32 = 1;
const PF_W: u32 = 2;

const AT_NULL: u64 = 0;
const AT_PHDR: u64 = 3;
const AT_PHENT: u64 = 4;
const AT_PHNUM: u64 = 5;
const AT_PAGESZ: u64 = 6;
const AT_ENTRY: u64 = 9;
const AT_UID: u64 = 11;
const AT_EUID: u64 = 12;
const AT_GID: u64 = 13;
const AT_EGID: u64 = 14;
const AT_CLKTCK: u64 = 17;
const AT_SECURE: u64 = 23;
const AT_RANDOM: u64 = 25;
const AT_EXECFN: u64 = 31;

pub struct Image {
    pub pagetable: PageTable,
    pub entry: usize,
    pub sp: usize,
    pub brk: usize,
}

fn u16_at(b: &[u8], o: usize) -> usize {
    u16::from_le_bytes([b[o], b[o + 1]]) as usize
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], o: usize) -> usize {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap()) as usize
}

pub fn exec(path: &str, argv: &[Vec<u8>], envp: &[Vec<u8>]) -> Result<Image, i64> {
    let elf = initrd::read(path).ok_or(-ENOENT)?;
    if elf.len() < 64 || &elf[..4] != b"\x7fELF" || elf[4] != 2 || u16_at(elf, 16) != 2 || u16_at(elf, 18) != 183 {
        return Err(-ENOEXEC);
    }
    let entry = u64_at(elf, 24);
    let phoff = u64_at(elf, 32);
    let phentsize = u16_at(elf, 54);
    let phnum = u16_at(elf, 56);
    if phoff + phentsize * phnum > elf.len() {
        return Err(-ENOEXEC);
    }

    let mut pt = PageTable::new().ok_or(-ENOMEM)?;
    let mut brk = 0;
    let mut phdr_va = 0;
    for i in 0..phnum {
        let ph = &elf[phoff + i * phentsize..];
        if u32_at(ph, 0) != PT_LOAD {
            continue;
        }
        let flags = u32_at(ph, 4);
        let off = u64_at(ph, 8);
        let va = u64_at(ph, 16);
        let filesz = u64_at(ph, 32);
        let memsz = u64_at(ph, 40);
        if filesz > memsz || off + filesz > elf.len() || va.checked_add(memsz).is_none() {
            return Err(-ENOEXEC);
        }
        let perm = Perm { write: flags & PF_W != 0, exec: flags & PF_X != 0 };
        pt.alloc_range(va, va + memsz, perm).ok_or(-ENOMEM)?;
        pt.copy_out(va, &elf[off..off + filesz]).ok_or(-ENOEXEC)?;
        if off <= phoff && phoff < off + filesz {
            phdr_va = va + (phoff - off);
        }
        brk = brk.max(pg_up(va + memsz));
    }

    pt.alloc_range(USER_STACK_TOP - USER_STACK_SIZE, USER_STACK_TOP, Perm::RW).ok_or(-ENOMEM)?;

    // 文字列を天辺から積む
    let mut sp = USER_STACK_TOP;
    let mut push_bytes = |pt: &PageTable, b: &[u8], nul: bool| -> Result<usize, i64> {
        let n = b.len() + nul as usize;
        if sp - n < USER_STACK_TOP - ARG_MAX {
            return Err(-E2BIG);
        }
        sp -= n;
        pt.copy_out(sp, b).ok_or(-ENOMEM)?;
        if nul {
            pt.copy_out(sp + b.len(), &[0]).ok_or(-ENOMEM)?;
        }
        Ok(sp)
    };
    let (argc, envc) = (argv.len(), envp.len());
    let mut ptrs = Vec::with_capacity(argc + envc);
    for s in argv.iter().chain(envp.iter()) {
        ptrs.push(push_bytes(&pt, s, true)?);
    }
    let execfn = push_bytes(&pt, path.as_bytes(), true)?;
    let random = push_bytes(&pt, &crate::rand::bytes16(), false)?;

    let auxv = [
        (AT_PHDR, phdr_va as u64),
        (AT_PHENT, phentsize as u64),
        (AT_PHNUM, phnum as u64),
        (AT_PAGESZ, PGSIZE as u64),
        (AT_ENTRY, entry as u64),
        (AT_UID, 0),
        (AT_EUID, 0),
        (AT_GID, 0),
        (AT_EGID, 0),
        (AT_CLKTCK, crate::timer::HZ),
        (AT_SECURE, 0),
        (AT_RANDOM, random as u64),
        (AT_EXECFN, execfn as u64),
        (AT_NULL, 0),
    ];
    let words = 1 + argc + 1 + envc + 1 + auxv.len() * 2;
    let sp = (sp - words * 8) & !15;
    let mut w = sp;
    let mut put = |v: u64| -> Result<(), i64> {
        pt.copy_out(w, &v.to_le_bytes()).ok_or(-ENOMEM)?;
        w += 8;
        Ok(())
    };
    put(argc as u64)?;
    for p in &ptrs[..argc] {
        put(*p as u64)?;
    }
    put(0)?;
    for p in &ptrs[argc..argc + envc] {
        put(*p as u64)?;
    }
    put(0)?;
    for (k, v) in auxv {
        put(k)?;
        put(v)?;
    }

    Ok(Image { pagetable: pt, entry, sp, brk })
}



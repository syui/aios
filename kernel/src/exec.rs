// ELF を新しいアドレス空間に読み込み、Linux と同じ形 (argc, argv, envp, auxv) のスタックを作る。
//   静的リンク (ET_EXEC) はそのまま、位置によらないもの (ET_DYN、PIE) は PIE_BASE に置く。
//   PT_INTERP (動的リンク) があれば、そのインタプリタ (ld.so) も INTERP_BASE に置いて、そちらから始める
//   (AT_BASE にその場所、AT_ENTRY にプログラムの入口を渡す。あとは ld.so が共有ライブラリを読む)
use crate::memlayout::PGSIZE;
use crate::vm::{pg_down, pg_up, Backing, PageTable, PROT_EXEC, PROT_READ, PROT_RW, PROT_WRITE};
use alloc::string::String;
use alloc::vec::Vec;

pub const USER_STACK_TOP: usize = 0x40_0000_0000;
/// スタックの領域 (RLIMIT_STACK と同じ 8 MiB。触れたところだけページを作る)
const USER_STACK_SIZE: usize = 8 * 1024 * 1024;
/// 引数と環境の合計の上限 (Linux の既定と同じく、スタックの上限 8 MiB の 1/4)。
/// スタックは引数の分だけ広げて確保する
const ARG_MAX: usize = 2 * 1024 * 1024;

const EACCES: i64 = 13;
const ENOEXEC: i64 = 8;
const ENOENT: i64 = 2;
const ENOMEM: i64 = 12;
const E2BIG: i64 = 7;

const PT_LOAD: u32 = 1;
const PT_INTERP: u32 = 3;
const PT_PHDR: u32 = 6;
const ET_EXEC: usize = 2;
const ET_DYN: usize = 3;
/// PIE を置くところ (brk はそのうしろから)
const PIE_BASE: usize = 0x5555_0000;
/// ld.so を置くところ (vDSO の下)
const INTERP_BASE: usize = 0x3e_0000_0000;
const PF_X: u32 = 1;
const PF_W: u32 = 2;

const AT_NULL: u64 = 0;
const AT_PHDR: u64 = 3;
const AT_PHENT: u64 = 4;
const AT_PHNUM: u64 = 5;
const AT_HWCAP: u64 = 16;
const AT_PAGESZ: u64 = 6;
const AT_BASE: u64 = 7;
const AT_ENTRY: u64 = 9;
const AT_UID: u64 = 11;
const AT_EUID: u64 = 12;
const AT_GID: u64 = 13;
const AT_EGID: u64 = 14;
const AT_CLKTCK: u64 = 17;
const AT_SECURE: u64 = 23;
const AT_RANDOM: u64 = 25;
const AT_EXECFN: u64 = 31;
const AT_SYSINFO_EHDR: u64 = 33;

pub struct Image {
    /// setuid / setgid のビットで変わる euid / egid
    pub setuid: Option<u32>,
    pub setgid: Option<u32>,
    pub pagetable: PageTable,
    pub entry: usize,
    pub sp: usize,
    pub brk: usize,
    /// 動かしているプログラムの絶対パス (/proc/PID/exe。先頭 / なし)
    pub exe: String,
    /// 引数と環境の文字列の場所 (arg_start, env_start, env_end)。/proc/PID/cmdline と environ はここを読む
    pub args: (usize, usize, usize),
}

/// "#!" の後ろの 1 行を (インタプリタ, 引数) に分ける。
/// Linux と同じく、引数は空白で分けず残り全部を 1 つとして渡す
fn parse_shebang(b: &[u8]) -> Option<(Vec<u8>, Option<Vec<u8>>)> {
    let line = &b[..b.iter().position(|&c| c == b'\n')?];
    let is_sp = |c: &u8| *c == b' ' || *c == b'\t';
    let line = trim(line, is_sp);
    let (interp, rest) = match line.iter().position(is_sp) {
        Some(i) => (&line[..i], trim(&line[i..], is_sp)),
        None => (line, &line[..0]),
    };
    if interp.is_empty() {
        return None;
    }
    Some((interp.to_vec(), (!rest.is_empty()).then(|| rest.to_vec())))
}

fn trim(mut s: &[u8], sp: impl Fn(&u8) -> bool) -> &[u8] {
    while s.first().is_some_and(&sp) {
        s = &s[1..];
    }
    while s.last().is_some_and(&sp) {
        s = &s[..s.len() - 1];
    }
    s
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
    exec_depth(path, argv, envp, 0)
}

fn exec_depth(path: &str, argv: &[Vec<u8>], envp: &[Vec<u8>], depth: usize) -> Result<Image, i64> {
    let cwd = crate::proc::current_cwd();
    let (exe, ino) = crate::vfs::lookup(&cwd, path, true)?;
    crate::landlock::check_fs(&exe, crate::landlock::EXECUTE)?;
    let m = ino.meta();
    if m.mode & crate::vfs::S_IFMT != crate::vfs::S_IFREG || !crate::cred::current().may(&m, crate::cred::X, false) {
        return Err(-EACCES);
    }
    // #! で始まるスクリプトは、書かれたインタプリタにスクリプトのパスを渡して動かす
    let mut head = [0u8; 256];
    let n = ino.read_at(0, &mut head)?;
    if n >= 2 && &head[..2] == b"#!" {
        const ELOOP: i64 = 40;
        if depth >= 4 {
            return Err(-ELOOP);
        }
        let (interp, arg) = parse_shebang(&head[2..n]).ok_or(-ENOEXEC)?;
        let interp_s = core::str::from_utf8(&interp).map_err(|_| -ENOEXEC)?;
        let mut nargv = alloc::vec![interp.clone()];
        nargv.extend(arg);
        nargv.push(path.as_bytes().to_vec());
        nargv.extend(argv.iter().skip(1).cloned());
        return exec_depth(interp_s, &nargv, envp, depth + 1);
    }
    let setuid = (m.mode & crate::cred::S_ISUID != 0).then_some(m.uid);
    let setgid = (m.mode & crate::cred::S_ISGID != 0 && m.mode & 0o010 != 0).then_some(m.gid);
    let size = m.size as usize;
    let mut pt = PageTable::new().ok_or(-ENOMEM)?;
    let main = load_elf(&mut pt, &ino, size, PIE_BASE, &exe)?;
    let brk = main.end;
    // 動的リンク: インタプリタ (ld.so) も読み、そちらから始める
    let (entry, interp_base) = match &main.interp {
        Some(ip) => {
            let (ipath, iino) = crate::vfs::lookup(&cwd, ip, true).map_err(|_| -ENOENT)?;
            crate::landlock::check_fs(&ipath, crate::landlock::EXECUTE)?;
            let isize = iino.meta().size as usize;
            let i = load_elf(&mut pt, &iino, isize, INTERP_BASE, &ipath)?;
            if i.interp.is_some() {
                return Err(-ENOEXEC);
            }
            (i.entry, INTERP_BASE)
        }
        None => (main.entry, 0),
    };
    let (phdr_va, phentsize, phnum) = (main.phdr_va, main.phentsize, main.phnum);

    // 引数と環境の文字列、ポインタの並び (argc, argv, envp, auxv) の分を足す
    let strs: usize = argv.iter().chain(envp.iter()).map(|s| s.len() + 1).sum::<usize>() + path.len() + 1 + 16;
    let ptrs_size = (argv.len() + envp.len() + 3 + 2 * 32) * 8;
    if strs > ARG_MAX {
        return Err(-E2BIG);
    }
    let args_area = pg_up(strs + ptrs_size + 64);
    crate::vdso::map(&mut pt).ok_or(-ENOMEM)?;
    pt.map(USER_STACK_TOP - USER_STACK_SIZE - args_area, USER_STACK_TOP, PROT_RW, false, Backing::Anon).ok_or(-ENOMEM)?;

    // 文字列は Linux と同じ並び: 天辺の 8 バイトは 0 のまま空け、その下に argv[0] argv[1] ... envp ... execfn を
    // 低いほうから続けて置く (setproctitle などは argv と envp の文字列が続いて並ぶものとして、その終わりを数える)
    let total: usize = argv.iter().chain(envp.iter()).map(|s| s.len() + 1).sum::<usize>() + path.len() + 1;
    if total + 8 > ARG_MAX {
        return Err(-E2BIG);
    }
    let base = USER_STACK_TOP - 8 - total;
    let mut at = base;
    let mut put_str = |pt: &mut PageTable, b: &[u8]| -> Result<usize, i64> {
        let start = at;
        pt.copy_out(at, b).ok_or(-ENOMEM)?;
        pt.copy_out(at + b.len(), &[0]).ok_or(-ENOMEM)?;
        at += b.len() + 1;
        Ok(start)
    };
    let (argc, envc) = (argv.len(), envp.len());
    let mut ptrs = Vec::with_capacity(argc + envc);
    for s in argv.iter().chain(envp.iter()) {
        ptrs.push(put_str(&mut pt, s)?);
    }
    let env_start = base + argv.iter().map(|s| s.len() + 1).sum::<usize>();
    let args = (base, env_start, env_start + envp.iter().map(|s| s.len() + 1).sum::<usize>());
    let execfn = put_str(&mut pt, path.as_bytes())?;
    // AT_RANDOM の 16 バイトは文字列の下に
    let mut sp = base;
    let mut push_bytes = |pt: &mut PageTable, b: &[u8], nul: bool| -> Result<usize, i64> {
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
    let random = push_bytes(&mut pt, &crate::rand::bytes16(), false)?;

    let auxv = [
        (AT_PHDR, phdr_va as u64),
        (AT_PHENT, phentsize as u64),
        (AT_PHNUM, phnum as u64),
        (AT_HWCAP, hwcap()),
        (AT_PAGESZ, PGSIZE as u64),
        (AT_BASE, interp_base as u64),
        (AT_ENTRY, main.entry as u64),
        (AT_UID, 0),
        (AT_EUID, 0),
        (AT_GID, 0),
        (AT_EGID, 0),
        (AT_CLKTCK, crate::timer::HZ),
        (AT_SECURE, 0),
        (AT_RANDOM, random as u64),
        (AT_EXECFN, execfn as u64),
        (AT_SYSINFO_EHDR, crate::vdso::VDSO_VA as u64),
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

    Ok(Image { setuid, setgid, pagetable: pt, entry, sp, brk, exe, args })
}



/// 読み込んだ ELF: 入口、プログラムヘッダの場所、終わり (brk の始まり)、PT_INTERP のパス
struct Loaded {
    entry: usize,
    phdr_va: usize,
    phentsize: usize,
    phnum: usize,
    end: usize,
    interp: Option<String>,
}

/// ino の ELF の PT_LOAD を pt に置く。ET_DYN なら base をずらして (いちばん低いところが base に来る)
fn load_elf(pt: &mut PageTable, ino: &crate::vfs::InodeRef, size: usize, base: usize, path: &str) -> Result<Loaded, i64> {
    let mut ehdr = [0u8; 64];
    if ino.read_at(0, &mut ehdr)? < 64 {
        return Err(-ENOEXEC);
    }
    let elf = &ehdr[..];
    let etype = u16_at(elf, 16);
    if &elf[..4] != b"\x7fELF" || elf[4] != 2 || !(etype == ET_EXEC || etype == ET_DYN) || u16_at(elf, 18) != 183 {
        return Err(-ENOEXEC);
    }
    let phoff = u64_at(elf, 32);
    let phentsize = u16_at(elf, 54);
    let phnum = u16_at(elf, 56);
    if phentsize < 56 || phnum > 64 || phoff + phentsize * phnum > size {
        return Err(-ENOEXEC);
    }
    let mut phdrs = alloc::vec![0u8; phentsize * phnum];
    ino.read_at(phoff, &mut phdrs)?;
    let ph = |i: usize| &phdrs[i * phentsize..(i + 1) * phentsize];
    // ずらす量: ET_DYN なら、いちばん低い PT_LOAD を base に
    let bias = if etype == ET_DYN {
        let low = (0..phnum).filter(|&i| u32_at(ph(i), 0) == PT_LOAD).map(|i| pg_down(u64_at(ph(i), 16))).min().ok_or(-ENOEXEC)?;
        base.checked_sub(low).ok_or(-ENOEXEC)?
    } else {
        0
    };
    let mut end = 0;
    let mut phdr_va = 0;
    let mut interp = None;
    for i in 0..phnum {
        let ph = ph(i);
        match u32_at(ph, 0) {
            PT_INTERP => {
                let (off, n) = (u64_at(ph, 8), u64_at(ph, 32));
                if n == 0 || n > 256 || off + n > size {
                    return Err(-ENOEXEC);
                }
                let mut b = alloc::vec![0u8; n];
                ino.read_at(off, &mut b)?;
                let p = core::str::from_utf8(&b).map_err(|_| -ENOEXEC)?.trim_end_matches('\0');
                interp = Some(String::from(p));
                continue;
            }
            PT_PHDR => {
                phdr_va = u64_at(ph, 16) + bias;
                continue;
            }
            PT_LOAD => {}
            _ => continue,
        }
        let flags = u32_at(ph, 4);
        let off = u64_at(ph, 8);
        let va = u64_at(ph, 16).checked_add(bias).ok_or(-ENOEXEC)?;
        let filesz = u64_at(ph, 32);
        let memsz = u64_at(ph, 40);
        if filesz > memsz || off + filesz > size || va.checked_add(memsz).is_none_or(|e| e > USER_STACK_TOP) {
            return Err(-ENOEXEC);
        }
        let prot = PROT_READ | if flags & PF_W != 0 { PROT_WRITE } else { 0 } | if flags & PF_X != 0 { PROT_EXEC } else { 0 };
        let (start, seg_end) = (pg_down(va), pg_up(va + memsz));
        if (va - start) > off {
            return Err(-ENOEXEC);
        }
        if pt.find(start).is_none() && pt.find(seg_end - 1).is_none() {
            // ふつう: ページはファイルから、触れたときに読む (filesz の先は 0)
            let back = Backing::File { ino: ino.clone(), off: off - (va - start), fend: va + filesz, ver: crate::vm::file_ver(ino) };
            pt.map(start, seg_end, prot, false, back).ok_or(-ENOMEM)?;
            pt.set_name(start, path);
        } else {
            // 前のセグメントとページを分けあう: 残りを無名にして、中身を今読む
            let mut s = start;
            while pt.find(s).is_some() {
                s += PGSIZE;
            }
            if s < seg_end {
                pt.map(s, seg_end, prot, false, Backing::Anon).ok_or(-ENOMEM)?;
            }
            let mut buf = alloc::vec![0u8; 64 * 1024];
            let mut done = 0;
            while done < filesz {
                let n = buf.len().min(filesz - done);
                if ino.read_at(off + done, &mut buf[..n])? != n {
                    return Err(-ENOEXEC);
                }
                pt.copy_out_force(va + done, &buf[..n]).ok_or(-ENOEXEC)?;
                done += n;
            }
        }
        if phdr_va == 0 && off <= phoff && phoff < off + filesz {
            phdr_va = va + (phoff - off);
        }
        end = end.max(seg_end);
    }
    Ok(Loaded { entry: u64_at(elf, 24) + bias, phdr_va, phentsize, phnum, end, interp })
}

/// AT_HWCAP: この CPU で使える命令 (Linux の arm64 と同じ印)。ID レジスタから
fn hwcap() -> u64 {
    let (isar0, pfr0): (u64, u64);
    unsafe {
        core::arch::asm!("mrs {}, id_aa64isar0_el1", out(reg) isar0);
        core::arch::asm!("mrs {}, id_aa64pfr0_el1", out(reg) pfr0);
    }
    let f = |r: u64, shift: u32| (r >> shift) & 0xf;
    let mut h = 0;
    // FP と AdvSIMD (0xf は「ない」)
    if f(pfr0, 16) != 0xf {
        h |= 1 << 0; // FP
    }
    if f(pfr0, 20) != 0xf {
        h |= 1 << 1; // ASIMD
    }
    if f(isar0, 4) >= 1 {
        h |= 1 << 3; // AES
    }
    if f(isar0, 4) >= 2 {
        h |= 1 << 4; // PMULL
    }
    if f(isar0, 8) >= 1 {
        h |= 1 << 5; // SHA1
    }
    if f(isar0, 12) >= 1 {
        h |= 1 << 6; // SHA2
    }
    if f(isar0, 16) >= 1 {
        h |= 1 << 7; // CRC32
    }
    if f(isar0, 20) >= 2 {
        h |= 1 << 8; // ATOMICS (LSE)
    }
    h
}

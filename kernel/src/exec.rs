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
const AT_IGNORE: u64 = 1;
const AT_PHDR: u64 = 3;
const AT_PHENT: u64 = 4;
const AT_PHNUM: u64 = 5;
const AT_HWCAP: u64 = 16;
const AT_HWCAP2: u64 = 26;
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
    /// 動かしているプログラム (ELF) の inode。書くために開くと ETXTBSY (proc::executing: ファイルのページは 1 枚なので、
    /// 書けば動いているプログラムのコードが変わる。Linux と同じ)
    pub exe_id: (usize, u64),
    /// 引数と環境の文字列の場所 (arg_start, env_start, env_end)。/proc/PID/cmdline と environ はここを読む
    pub args: (usize, usize, usize),
    /// AppArmor: この exec でつくプロファイル (apparmor::exec_transition)
    pub apparmor: crate::apparmor::Exec,
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

/// ファイルを読む (ext4 ならページキャッシュを通す: 何度も動かすプログラムのヘッダを読みなおさない)
fn read_file(ino: &crate::vfs::InodeRef, off: usize, buf: &mut [u8]) -> Result<usize, i64> {
    if ino.page_cacheable() { crate::vm::read_cached(ino, off, buf) } else { ino.read_at(off, buf) }
}

pub fn exec(path: &str, argv: &[Vec<u8>], envp: &[Vec<u8>]) -> Result<Image, i64> {
    exec_depth(path, argv, envp, 0, None)
}

/// aa: はじめのプログラムで決めた AppArmor の答え (#! のインタプリタへもちこむ: Linux もスクリプトのパスで 1 度だけ)
fn exec_depth(path: &str, argv: &[Vec<u8>], envp: &[Vec<u8>], depth: usize, aa: Option<crate::apparmor::Exec>) -> Result<Image, i64> {
    let cwd = crate::proc::current_cwd();
    let (exe, ino) = crate::vfs::lookup(&cwd, path, true)?;
    crate::landlock::check_fs(&exe, crate::landlock::EXECUTE)?;
    let m = ino.meta();
    if m.mode & crate::vfs::S_IFMT != crate::vfs::S_IFREG || !crate::cred::current().may(&m, crate::cred::X, false) {
        return Err(-EACCES);
    }
    let mut aa = match aa {
        Some(a) => a,
        None => crate::apparmor::exec_transition(&exe, m.uid)?,
    };
    // #! で始まるスクリプトは、書かれたインタプリタにスクリプトのパスを渡して動かす
    let mut head = [0u8; 256];
    let n = read_file(&ino, 0, &mut head)?;
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
        return exec_depth(interp_s, &nargv, envp, depth + 1, Some(aa));
    }
    // AppArmor: 新しいプロファイルでこのプログラムを写せるか (だめなら exec のあと SIGSEGV: Linux と同じ)
    aa.fatal |= !crate::apparmor::exec_map_ok(&aa, &exe, m.uid);
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
            aa.fatal |= !crate::apparmor::exec_map_ok(&aa, &ipath, iino.meta().uid);
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
    // vDSO を切っていれば (aios.off=vdso) 写さず、auxv にも書かない (libc はシステムコールで clock_gettime する)
    let vdso = !crate::parts::off(crate::parts::Part::Vdso);
    if vdso {
        crate::vdso::map(&mut pt).ok_or(-ENOMEM)?;
    }
    pt.map(USER_STACK_TOP - USER_STACK_SIZE - args_area, USER_STACK_TOP, PROT_RW, false, Backing::Anon).ok_or(-ENOMEM)?;

    // 文字列は Linux と同じ並び: 天辺の 8 バイトは 0 のまま空け、その下に argv[0] argv[1] ... envp ... execfn を
    // 低いほうから続けて置く (setproctitle などは argv と envp の文字列が続いて並ぶものとして、その終わりを数える)。
    // その下に AT_RANDOM の 16 バイト、さらに下に (argc, argv, envp, auxv) の並び。
    // 全部をカーネルの中で組んでから、1 回で写す (8 バイトずつ写すと、そのたびにページ表を引くので)
    let total: usize = argv.iter().chain(envp.iter()).map(|s| s.len() + 1).sum::<usize>() + path.len() + 1;
    if total + 8 > ARG_MAX {
        return Err(-E2BIG);
    }
    let base = USER_STACK_TOP - 8 - total;
    let (argc, envc) = (argv.len(), envp.len());
    let random = base - 16;
    // auxv の数 (下の並びの型もこれで、ずれないように。前は AT_HWCAP2 を足して 18 になったのにここが 17 のままで、
    // おしまいの AT_NULL が AT_RANDOM の 16 バイトに重なり、そのほとんどを 0 にしていた: glibc のスタックの番人も)
    const NAUXV: usize = 18;
    let words = 1 + argc + 1 + envc + 1 + NAUXV * 2;
    let sp = (random - words * 8) & !15;
    if sp < USER_STACK_TOP - ARG_MAX {
        return Err(-E2BIG);
    }
    // img[i] は va = sp + i
    let mut img = crate::heap::try_vec(USER_STACK_TOP - sp, true)?;
    let mut at = base;
    let mut ptrs = Vec::with_capacity(argc + envc + 1);
    for s in argv.iter().chain(envp.iter()).chain(core::iter::once(&path.as_bytes().to_vec())) {
        ptrs.push(at);
        img[at - sp..at - sp + s.len()].copy_from_slice(s);
        at += s.len() + 1;
    }
    let execfn = ptrs[argc + envc];
    let env_start = base + argv.iter().map(|s| s.len() + 1).sum::<usize>();
    let args = (base, env_start, env_start + envp.iter().map(|s| s.len() + 1).sum::<usize>());
    crate::rand::fill(&mut img[random - sp..random - sp + 16]);

    let hw = hwcaps();
    let auxv: [(u64, u64); NAUXV] = [
        (AT_PHDR, phdr_va as u64),
        (AT_PHENT, phentsize as u64),
        (AT_PHNUM, phnum as u64),
        (AT_HWCAP, hw.0),
        (AT_HWCAP2, hw.1),
        (AT_PAGESZ, PGSIZE as u64),
        (AT_BASE, interp_base as u64),
        (AT_ENTRY, main.entry as u64),
        (AT_UID, 0),
        (AT_EUID, 0),
        (AT_GID, 0),
        (AT_EGID, 0),
        (AT_CLKTCK, crate::timer::HZ),
        // AppArmor の Px など (環境を消す移りかた) なら 1: ld.so が LD_* を見ない
        (AT_SECURE, aa.secure as u64),
        (AT_RANDOM, random as u64),
        (AT_EXECFN, execfn as u64),
        if vdso { (AT_SYSINFO_EHDR, crate::vdso::VDSO_VA as u64) } else { (AT_IGNORE, 0) },
        (AT_NULL, 0),
    ];
    let mut w = 0;
    let mut put = |v: u64| {
        img[w..w + 8].copy_from_slice(&v.to_le_bytes());
        w += 8;
    };
    put(argc as u64);
    for p in &ptrs[..argc] {
        put(*p as u64);
    }
    put(0);
    for p in &ptrs[argc..argc + envc] {
        put(*p as u64);
    }
    put(0);
    for (k, v) in auxv {
        put(k);
        put(v);
    }
    pt.copy_out(sp, &img).ok_or(-ENOMEM)?;

    Ok(Image { setuid, setgid, pagetable: pt, entry, sp, brk, exe, exe_id: ino.id(), args, apparmor: aa })
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
    if read_file(&ino, 0, &mut ehdr)? < 64 {
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
    read_file(&ino, phoff, &mut phdrs)?;
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
                read_file(&ino, off, &mut b)?;
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
            let mut buf = crate::heap::try_vec(64 * 1024, true)?;
            let mut done = 0;
            while done < filesz {
                let n = buf.len().min(filesz - done);
                if read_file(&ino, off + done, &mut buf[..n])? != n {
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

/// ID レジスタ (hwcaps と trap.rs の emulate_mrs が読むもの)
#[derive(Clone, Copy)]
enum Id {
    Pfr0,
    Isar0,
    Isar1,
    Isar2,
    Mmfr0,
    Mmfr1,
    Mmfr2,
}

/// 印 1 つ: (/proc/cpuinfo の名前, AT_HWCAP2 か, ビット, レジスタ, 欄の位置, 最小の値, 符号つきの欄か)。Linux の
/// arch/arm64/kernel/cpufeature.c (arm64_elf_hwcaps) と同じ決まりで、ビットの順 (cpuinfo の Features もこの順)。
/// カーネルが使わせていないもの (SVE と SME: Z と ZA のレジスタを切りかえで残さない、MTE、BTI、ポインタ認証、GCS、
/// MOPS、WFxT) と、仕掛けのないもの (evtstrm: タイマのイベントの流れ、SSBS) は出さない
const CAPS: &[(&str, bool, u32, Id, u32, u64, bool)] = &[
    ("fp", false, 0, Id::Pfr0, 16, 0, true),
    ("asimd", false, 1, Id::Pfr0, 20, 0, true),
    ("aes", false, 3, Id::Isar0, 4, 1, false),
    ("pmull", false, 4, Id::Isar0, 4, 2, false),
    ("sha1", false, 5, Id::Isar0, 8, 1, false),
    ("sha2", false, 6, Id::Isar0, 12, 1, false),
    ("crc32", false, 7, Id::Isar0, 16, 1, false),
    ("atomics", false, 8, Id::Isar0, 20, 2, false),
    ("fphp", false, 9, Id::Pfr0, 16, 1, true),
    ("asimdhp", false, 10, Id::Pfr0, 20, 1, true),
    // cpuid (11) はいつも (EL0 の MRS で ID レジスタを読める: trap.rs の emulate_mrs)
    ("asimdrdm", false, 12, Id::Isar0, 28, 1, false),
    ("jscvt", false, 13, Id::Isar1, 12, 1, false),
    ("fcma", false, 14, Id::Isar1, 16, 1, false),
    ("lrcpc", false, 15, Id::Isar1, 20, 1, false),
    ("dcpop", false, 16, Id::Isar1, 0, 1, false),
    ("sha3", false, 17, Id::Isar0, 32, 1, false),
    ("sm3", false, 18, Id::Isar0, 36, 1, false),
    ("sm4", false, 19, Id::Isar0, 40, 1, false),
    ("asimddp", false, 20, Id::Isar0, 44, 1, false),
    ("sha512", false, 21, Id::Isar0, 12, 2, false),
    ("asimdfhm", false, 23, Id::Isar0, 48, 1, false),
    ("dit", false, 24, Id::Pfr0, 48, 1, false),
    ("uscat", false, 25, Id::Mmfr2, 32, 1, false),
    ("ilrcpc", false, 26, Id::Isar1, 20, 2, false),
    ("flagm", false, 27, Id::Isar0, 52, 1, false),
    ("sb", false, 29, Id::Isar1, 36, 1, false),
    ("dcpodp", true, 0, Id::Isar1, 0, 2, false),
    ("flagm2", true, 7, Id::Isar0, 52, 2, false),
    ("frint", true, 8, Id::Isar1, 32, 1, false),
    ("i8mm", true, 13, Id::Isar1, 52, 1, false),
    ("bf16", true, 14, Id::Isar1, 44, 1, false),
    ("dgh", true, 15, Id::Isar1, 48, 1, false),
    ("rng", true, 16, Id::Isar0, 60, 1, false),
    ("ecv", true, 19, Id::Mmfr0, 60, 1, false),
    ("afp", true, 20, Id::Mmfr1, 44, 1, false),
    ("rpres", true, 21, Id::Isar2, 4, 1, false),
    ("ebf16", true, 32, Id::Isar1, 44, 2, false),
    ("cssc", true, 34, Id::Isar2, 52, 1, false),
    ("rprfm", true, 35, Id::Isar2, 48, 1, false),
    ("hbc", true, 44, Id::Isar2, 20, 1, false),
    ("lrcpc3", true, 46, Id::Isar1, 20, 3, false),
    ("lse128", true, 47, Id::Isar0, 20, 3, false),
];

/// ID レジスタの値 (この CPU の)
fn id_reg(r: Id) -> u64 {
    let v: u64;
    unsafe {
        match r {
            Id::Pfr0 => core::arch::asm!("mrs {}, id_aa64pfr0_el1", out(reg) v),
            Id::Isar0 => core::arch::asm!("mrs {}, id_aa64isar0_el1", out(reg) v),
            Id::Isar1 => core::arch::asm!("mrs {}, id_aa64isar1_el1", out(reg) v),
            // 古いアセンブラの名前のない ID_AA64ISAR2_EL1 (op0 3, op1 0, CRn 0, CRm 6, op2 2)
            Id::Isar2 => core::arch::asm!("mrs {}, s3_0_c0_c6_2", out(reg) v),
            Id::Mmfr0 => core::arch::asm!("mrs {}, id_aa64mmfr0_el1", out(reg) v),
            Id::Mmfr1 => core::arch::asm!("mrs {}, id_aa64mmfr1_el1", out(reg) v),
            Id::Mmfr2 => core::arch::asm!("mrs {}, id_aa64mmfr2_el1", out(reg) v),
        }
    }
    v
}

fn has(c: &(&str, bool, u32, Id, u32, u64, bool)) -> bool {
    let v = (id_reg(c.3) >> c.4) & 0xf;
    // 符号つきの欄 (FP と AdvSIMD) の 0xf は「ない」(-1)
    if c.6 && v == 0xf {
        return false;
    }
    v >= c.5
}

/// (AT_HWCAP, AT_HWCAP2): この CPU で使える命令 (Linux の arm64 と同じ印)。前は FP、ASIMD、AES、PMULL、SHA1、SHA2、
/// CRC32、ATOMICS と CPUID だけで、dotprod (asimddp) や fp16 (asimdhp)、i8mm を見るもの (llama.cpp の ggml、zstd、dav1d)
/// が、ある CPU でもおそい道を選んでいた
pub fn hwcaps() -> (u64, u64) {
    let mut h = (1u64 << 11, 0u64); // CPUID
    for c in CAPS.iter().filter(|c| has(c)) {
        if c.1 {
            h.1 |= 1 << c.2;
        } else {
            h.0 |= 1 << c.2;
        }
    }
    h
}

/// /proc/cpuinfo の Features (Linux と同じ名前と順)
pub fn cpu_features() -> alloc::string::String {
    let mut s = alloc::string::String::new();
    for c in CAPS.iter() {
        if c.2 == 12 && !c.1 {
            // cpuid は asimdhp と asimdrdm のあいだ (ビット 11)
            s.push_str("cpuid ");
        }
        if has(c) {
            s.push_str(c.0);
            s.push(' ');
        }
    }
    s.pop();
    s
}

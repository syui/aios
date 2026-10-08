// seccomp: プロセスが使えるシステムコールをしぼる (Linux と同じ番号と形)
//   seccomp(SECCOMP_SET_MODE_FILTER, flags, &sock_fprog) / prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &sock_fprog)
//     フィルタ (classic BPF。seccomp_data を読んで答えを返す) を足す。足したものは外せず、fork と exec で引き継ぐ
//   seccomp(SECCOMP_SET_MODE_STRICT) / prctl(PR_SET_SECCOMP, SECCOMP_MODE_STRICT)
//     read / write / exit / rt_sigreturn だけ (ほかは SIGKILL)
//   フィルタをかけるには prctl(PR_SET_NO_NEW_PRIVS) か root が要る (setuid のプログラムをだませないように。landlock と同じ)
// システムコールのたびに、かかっているフィルタをぜんぶ動かし、いちばんきびしい答えにしたがう:
//   KILL_PROCESS > KILL_THREAD > TRAP (SIGSYS) > ERRNO > USER_NOTIF > TRACE > LOG > ALLOW
// USER_NOTIF と TRACE は受け手 (ptrace、listener) がないので ENOSYS にする (Linux も受け手がなければそう)
use crate::cred::Cred;
use crate::proc;
use alloc::rc::Rc;
use alloc::vec::Vec;

const EINVAL: i64 = 22;
const EFAULT: i64 = 14;
const EACCES: i64 = 13;
const ENOMEM: i64 = 12;
const ENOSYS: i64 = 38;
const EOPNOTSUPP: i64 = 95;

pub const MODE_DISABLED: u64 = 0;
pub const MODE_STRICT: u64 = 1;
pub const MODE_FILTER: u64 = 2;

const SET_MODE_STRICT: u64 = 0;
const SET_MODE_FILTER: u64 = 1;
const GET_ACTION_AVAIL: u64 = 2;
const GET_NOTIF_SIZES: u64 = 3;
const FLAG_TSYNC: u64 = 1;
const FLAG_LOG: u64 = 2;
const FLAG_SPEC_ALLOW: u64 = 4;

const RET_KILL_PROCESS: u32 = 0x8000_0000;
const RET_KILL_THREAD: u32 = 0x0000_0000;
const RET_TRAP: u32 = 0x0003_0000;
const RET_ERRNO: u32 = 0x0005_0000;
const RET_USER_NOTIF: u32 = 0x7fc0_0000;
const RET_TRACE: u32 = 0x7ff0_0000;
const RET_LOG: u32 = 0x7ffc_0000;
const RET_ALLOW: u32 = 0x7fff_0000;
const RET_ACTION_FULL: u32 = 0xffff_0000;
const RET_DATA: u32 = 0x0000_ffff;

/// seccomp_data の arch (AUDIT_ARCH_AARCH64)
pub const AUDIT_ARCH_AARCH64: u32 = 0xc000_00b7;
/// seccomp_data の大きさ (nr 4、arch 4、instruction_pointer 8、args 8 × 6)
const DATA_LEN: u32 = 64;
const MAX_INSNS: usize = 4096;
/// 重ねたフィルタの命令の合計の上限 (Linux と同じ)
const MAX_TOTAL: usize = 32768;

/// BPF の命令 (struct sock_filter)
#[derive(Clone, Copy)]
pub struct Insn {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

/// かかっているもの: strict か、フィルタの重なり (新しいものが後ろ)
pub struct State {
    pub strict: bool,
    pub filters: Vec<Rc<Vec<Insn>>>,
}

pub type Seccomp = Rc<State>;

/// /proc/PID/status の Seccomp: (0 なし、1 strict、2 filter) と Seccomp_filters:
pub fn mode(c: &Cred) -> (u64, usize) {
    match &c.seccomp {
        None => (MODE_DISABLED, 0),
        Some(s) if s.strict => (MODE_STRICT, s.filters.len()),
        Some(s) => (MODE_FILTER, s.filters.len()),
    }
}

// ---- BPF ----

const LD: u16 = 0x00;
const LDX: u16 = 0x01;
const ST: u16 = 0x02;
const STX: u16 = 0x03;
const ALU: u16 = 0x04;
const JMP: u16 = 0x05;
const RET: u16 = 0x06;
const MISC: u16 = 0x07;
const W: u16 = 0x00;
const IMM: u16 = 0x00;
const ABS: u16 = 0x20;
const MEM: u16 = 0x60;
const LEN: u16 = 0x80;
const K: u16 = 0x00;
const X: u16 = 0x08;
const A: u16 = 0x10;

/// 読みこんだプログラムを確かめる (使える命令だけ、跳び先は中、読む場所は seccomp_data の中、終わりは RET)
fn validate(p: &[Insn]) -> Result<(), i64> {
    if p.is_empty() || p.len() > MAX_INSNS {
        return Err(-EINVAL);
    }
    for (i, n) in p.iter().enumerate() {
        let left = (p.len() - 1 - i) as u32;
        let ok = match n.code & 0x07 {
            LD => match n.code {
                c if c == LD | W | ABS => n.k % 4 == 0 && n.k < DATA_LEN,
                c if c == LD | W | LEN || c == LD | IMM => true,
                c if c == LD | MEM => n.k < 16,
                _ => false,
            },
            LDX => match n.code {
                c if c == LDX | W | IMM || c == LDX | W | LEN => true,
                c if c == LDX | MEM => n.k < 16,
                _ => false,
            },
            ST | STX => n.code & !0x07 == 0 && n.k < 16,
            ALU => {
                let op = n.code & 0xf0;
                let src_ok = n.code & !0xf8 == ALU;
                // DIV と MOD の 0 割り (K のとき) は読むときに断る
                src_ok && op <= 0xa0 && !(matches!(op, 0x30 | 0x90) && n.code & X == 0 && n.k == 0)
            }
            JMP => match n.code & 0xf0 {
                0x00 => n.code == JMP && n.k < left,
                0x10..=0x40 => n.code & !0xf8 == JMP && (n.jt as u32) < left && (n.jf as u32) < left,
                _ => false,
            },
            RET => n.code == RET | K || n.code == RET | A,
            MISC => n.code == MISC || n.code == MISC | 0x80,
            _ => false,
        };
        if !ok {
            return Err(-EINVAL);
        }
    }
    // 終わりまで流れ落ちないように、最後は RET
    if p[p.len() - 1].code & 0x07 != RET {
        return Err(-EINVAL);
    }
    Ok(())
}

/// プログラムを動かして答え (SECCOMP_RET_*) を出す
fn run(p: &[Insn], data: &[u8; DATA_LEN as usize]) -> u32 {
    let (mut a, mut x): (u32, u32) = (0, 0);
    let mut m = [0u32; 16];
    let mut pc = 0usize;
    let word = |k: u32| u32::from_le_bytes([data[k as usize], data[k as usize + 1], data[k as usize + 2], data[k as usize + 3]]);
    while pc < p.len() {
        let n = p[pc];
        pc += 1;
        match n.code & 0x07 {
            LD => {
                a = match n.code & 0xe0 {
                    ABS => word(n.k),
                    LEN => DATA_LEN,
                    MEM => m[n.k as usize],
                    _ => n.k,
                }
            }
            LDX => {
                x = match n.code & 0xe0 {
                    LEN => DATA_LEN,
                    MEM => m[n.k as usize],
                    _ => n.k,
                }
            }
            ST => m[n.k as usize] = a,
            STX => m[n.k as usize] = x,
            ALU => {
                let v = if n.code & X != 0 { x } else { n.k };
                a = match n.code & 0xf0 {
                    0x00 => a.wrapping_add(v),
                    0x10 => a.wrapping_sub(v),
                    0x20 => a.wrapping_mul(v),
                    0x30 => a.checked_div(v).unwrap_or(0),
                    0x40 => a | v,
                    0x50 => a & v,
                    0x60 => a.checked_shl(v).unwrap_or(0),
                    0x70 => a.checked_shr(v).unwrap_or(0),
                    0x80 => a.wrapping_neg(),
                    0x90 => a.checked_rem(v).unwrap_or(0),
                    _ => a ^ v,
                }
            }
            JMP => {
                let v = if n.code & X != 0 { x } else { n.k };
                let t = match n.code & 0xf0 {
                    0x00 => {
                        pc += n.k as usize;
                        continue;
                    }
                    0x10 => a == v,
                    0x20 => a > v,
                    0x30 => a >= v,
                    _ => a & v != 0,
                };
                pc += if t { n.jt } else { n.jf } as usize;
            }
            RET => return if n.code & A != 0 { a } else { n.k },
            _ => {
                if n.code & 0x80 != 0 {
                    a = x;
                } else {
                    x = a;
                }
            }
        }
    }
    RET_KILL_THREAD
}

/// strict: read 63、write 64、exit 93、rt_sigreturn 139 だけ
fn strict_allows(nr: u64) -> bool {
    matches!(nr, 63 | 64 | 93 | 139)
}

// ---- システムコールのたびに ----

/// したがうこと
pub enum Verdict {
    Allow,
    /// このエラーで返す (システムコールはしない)
    Errno(i64),
    /// SIGSYS を送る (付帯情報の errno)
    Trap(u16),
    /// SIGSYS で終わる
    Kill,
}

/// システムコール nr (引数 args、呼んだ場所 pc) をしてよいか
pub fn check(s: &State, nr: u64, args: &[u64; 6], pc: u64) -> Verdict {
    if s.strict && !strict_allows(nr) {
        return Verdict::Kill;
    }
    if s.filters.is_empty() {
        return Verdict::Allow;
    }
    let mut d = [0u8; DATA_LEN as usize];
    d[0..4].copy_from_slice(&(nr as u32).to_le_bytes());
    d[4..8].copy_from_slice(&AUDIT_ARCH_AARCH64.to_le_bytes());
    d[8..16].copy_from_slice(&pc.to_le_bytes());
    for (i, v) in args.iter().enumerate() {
        d[16 + i * 8..24 + i * 8].copy_from_slice(&v.to_le_bytes());
    }
    // いちばんきびしいもの (答えの上 16 ビットを符号つきでくらべて小さいほう)
    let mut best = RET_ALLOW;
    for f in s.filters.iter().rev() {
        let r = run(f, &d);
        if ((r & RET_ACTION_FULL) as i32) < ((best & RET_ACTION_FULL) as i32) {
            best = r;
        }
    }
    let data = (best & RET_DATA) as u16;
    match best & RET_ACTION_FULL {
        RET_ALLOW => Verdict::Allow,
        RET_LOG => {
            println!("seccomp: pid {} syscall {} (log)", proc::current().pid, nr);
            Verdict::Allow
        }
        RET_ERRNO => Verdict::Errno(-(data.min(4095) as i64)),
        RET_TRAP => Verdict::Trap(data),
        RET_TRACE | RET_USER_NOTIF => Verdict::Errno(-ENOSYS),
        // KILL_THREAD、KILL_PROCESS、知らない答え (Linux は KILL_PROCESS とみなす)
        _ => Verdict::Kill,
    }
}

// ---- かける ----

fn may_set(c: &Cred) -> Result<(), i64> {
    if !c.no_new_privs && c.euid != 0 {
        return Err(-EACCES);
    }
    Ok(())
}

/// ユーザーの sock_fprog (len u16、間 6 バイト、filter のアドレス u64) を読む
fn read_prog(addr: usize) -> Result<Vec<Insn>, i64> {
    let p = proc::current();
    let mut h = [0u8; 16];
    p.pt().copy_in(&mut h, addr).ok_or(-EFAULT)?;
    let len = u16::from_le_bytes([h[0], h[1]]) as usize;
    let ptr = u64::from_le_bytes(h[8..16].try_into().unwrap()) as usize;
    if len == 0 || len > MAX_INSNS {
        return Err(-EINVAL);
    }
    let mut raw = alloc::vec![0u8; len * 8];
    p.pt().copy_in(&mut raw, ptr).ok_or(-EFAULT)?;
    let prog: Vec<Insn> = raw
        .chunks(8)
        .map(|c| Insn { code: u16::from_le_bytes([c[0], c[1]]), jt: c[2], jf: c[3], k: u32::from_le_bytes([c[4], c[5], c[6], c[7]]) })
        .collect();
    validate(&prog)?;
    Ok(prog)
}

/// 今のもの (なければ空) に足した新しい State
fn add(old: &Option<Seccomp>, strict: bool, prog: Option<Rc<Vec<Insn>>>) -> Result<Seccomp, i64> {
    let (s0, mut f) = match old {
        Some(s) => (s.strict, s.filters.clone()),
        None => (false, Vec::new()),
    };
    if let Some(p) = prog {
        if f.iter().map(|x| x.len()).sum::<usize>() + p.len() > MAX_TOTAL {
            return Err(-ENOMEM);
        }
        f.push(p);
    }
    Ok(Rc::new(State { strict: s0 || strict, filters: f }))
}

/// 自分 (TSYNC ならスレッドグループのみな) にかける
fn install(strict: bool, prog: Option<Vec<Insn>>, tsync: bool) -> Result<i64, i64> {
    let p = proc::current();
    // フィルタは no_new_privs か root が要る。strict は要らない (できることが減るだけなので。Linux と同じ)
    if prog.is_some() {
        may_set(&p.cred)?;
    }
    let prog = prog.map(Rc::new);
    p.cred.seccomp = Some(add(&p.cred.seccomp, strict, prog.clone())?);
    if tsync {
        let s = p.cred.seccomp.clone();
        for t in p.siblings() {
            t.cred.seccomp = s.clone();
        }
    }
    Ok(0)
}

/// seccomp(2)
pub fn sys_seccomp(op: u64, flags: u64, args: usize) -> Result<i64, i64> {
    match op {
        SET_MODE_STRICT => {
            if flags != 0 || args != 0 {
                return Err(-EINVAL);
            }
            install(true, None, false)
        }
        SET_MODE_FILTER => {
            if flags & !(FLAG_TSYNC | FLAG_LOG | FLAG_SPEC_ALLOW) != 0 {
                // NEW_LISTENER (USER_NOTIF の受け手) などはない
                return Err(-EINVAL);
            }
            let prog = read_prog(args)?;
            install(false, Some(prog), flags & FLAG_TSYNC != 0)
        }
        GET_ACTION_AVAIL => {
            let mut b = [0u8; 4];
            proc::current().pt().copy_in(&mut b, args).ok_or(-EFAULT)?;
            match u32::from_le_bytes(b) {
                RET_KILL_PROCESS | RET_KILL_THREAD | RET_TRAP | RET_ERRNO | RET_TRACE | RET_LOG | RET_ALLOW => Ok(0),
                _ => Err(-EOPNOTSUPP),
            }
        }
        GET_NOTIF_SIZES => Err(-EOPNOTSUPP),
        _ => Err(-EINVAL),
    }
}

/// prctl(PR_SET_SECCOMP, mode, prog)
pub fn prctl_set(mode: u64, prog: usize) -> Result<i64, i64> {
    match mode {
        MODE_STRICT => install(true, None, false),
        MODE_FILTER => install(false, Some(read_prog(prog)?), false),
        _ => Err(-EINVAL),
    }
}

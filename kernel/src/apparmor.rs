// AppArmor (Linux の security/apparmor と同じ決まり)。プロファイル (文字で書いた決まり) は apparmor_parser がバイナリに
// して /sys/kernel/security/apparmor/.load (足す) か .replace (足すか入れかえる) に書き、.remove に名前を書くと外す
// (securityfs.rs)。.access には「このプロファイルでこの文字列は」を聞ける (libapparmor の aa_query_label)。
// ほどき方は Linux の policy_unpack.c と同じ: 型の印のついた値の並び (数は little endian)。ファイルの決まりは DFA
// (match.c。表は big endian) で、パスを通してたどりついた状態の許しを見る。名のる版は v5〜v7 (features。許しを DFA の
// accept の表にうめこんだ古い形で、policy_compat.c と同じく読むときに許しの表へ移す)。名前空間 (:ns:) はまだない
use crate::cell::Bkl;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

const EPERM: i64 = 1;
const ENOENT: i64 = 2;
const EACCES: i64 = 13;
const EEXIST: i64 = 17;
const EINVAL: i64 = 22;
const ECHILD: i64 = 10;
const EPROTO: i64 = 71;
const EPROTONOSUPPORT: i64 = 93;
const EOPNOTSUPP: i64 = 95;

// ---- 型の印のついた並び (policy_unpack.h の enum aa_code) ----

const AA_U8: u8 = 0;
const AA_U32: u8 = 2;
const AA_U64: u8 = 3;
const AA_NAME: u8 = 4;
const AA_STRING: u8 = 5;
const AA_BLOB: u8 = 6;
const AA_STRUCT: u8 = 7;
const AA_STRUCTEND: u8 = 8;
const AA_ARRAY: u8 = 11;
const AA_ARRAYEND: u8 = 12;

/// 版 (header の version の下 10 ビット)。FORCE_COMPLAIN_FLAG は、みな complain で読みこむ印
const K_ABI_MASK: u32 = 0x3ff;
const FORCE_COMPLAIN_FLAG: u32 = 0x800;

/// 読んでいるところ (Linux の struct aa_ext)。しくじった読み方は、読む位置を動かさない
struct Ext<'a> {
    d: &'a [u8],
    pos: usize,
    version: u32,
}

impl<'a> Ext<'a> {
    fn left(&self) -> usize {
        self.d.len() - self.pos
    }

    /// 長さ (u16) のついたかたまり
    fn chunk16(&mut self) -> Option<&'a [u8]> {
        let (d, p) = (self.d, self.pos);
        if self.left() < 2 {
            return None;
        }
        let n = u16::from_le_bytes([d[p], d[p + 1]]) as usize;
        if self.left() - 2 < n {
            return None;
        }
        self.pos = p + 2 + n;
        Some(&d[p + 2..p + 2 + n])
    }

    fn x(&mut self, code: u8) -> bool {
        if self.pos < self.d.len() && self.d[self.pos] == code {
            self.pos += 1;
            return true;
        }
        false
    }

    /// 次が型 code の値で、name があればその名札がついているか (name がなければ、名札はあってもなくても読みとばす)
    fn name_x(&mut self, code: u8, name: Option<&str>) -> bool {
        let p = self.pos;
        if self.x(AA_NAME) {
            let tag = self.chunk16();
            if let Some(name) = name {
                // 名札は NUL で終わる。C の strcmp と同じく、はじめの NUL までをくらべる
                let ok = tag.is_some_and(|t| !t.is_empty() && t[t.len() - 1] == 0 && t.split(|&b| b == 0).next() == Some(name.as_bytes()));
                if !ok {
                    self.pos = p;
                    return false;
                }
            }
        } else if name.is_some() {
            self.pos = p;
            return false;
        }
        if self.x(code) {
            return true;
        }
        self.pos = p;
        false
    }

    fn fixed<const N: usize>(&mut self, code: u8, name: Option<&str>) -> Option<[u8; N]> {
        let p = self.pos;
        if self.name_x(code, name) && self.left() >= N {
            let v: [u8; N] = self.d[self.pos..self.pos + N].try_into().unwrap();
            self.pos += N;
            return Some(v);
        }
        self.pos = p;
        None
    }

    fn u8(&mut self, name: Option<&str>) -> Option<u8> {
        self.fixed::<1>(AA_U8, name).map(|b| b[0])
    }

    fn u32(&mut self, name: Option<&str>) -> Option<u32> {
        self.fixed::<4>(AA_U32, name).map(u32::from_le_bytes)
    }

    fn u64(&mut self, name: Option<&str>) -> Option<u64> {
        self.fixed::<8>(AA_U64, name).map(u64::from_le_bytes)
    }

    fn array(&mut self, name: Option<&str>) -> Option<u16> {
        self.fixed::<2>(AA_ARRAY, name).map(u16::from_le_bytes)
    }

    /// (並びの頭からの位置, 中身)
    fn blob(&mut self, name: Option<&str>) -> Option<(usize, &'a [u8])> {
        let p = self.pos;
        if self.name_x(AA_BLOB, name) && self.left() >= 4 {
            let n = u32::from_le_bytes(self.d[self.pos..self.pos + 4].try_into().unwrap()) as usize;
            self.pos += 4;
            if self.left() >= n {
                let at = self.pos;
                self.pos += n;
                return Some((at, &self.d[at..at + n]));
            }
        }
        self.pos = p;
        None
    }

    /// NUL で終わる文字列 (終わりの NUL もふくめて。中に NUL があってもよい)
    fn str(&mut self, name: Option<&str>) -> Option<&'a [u8]> {
        let p = self.pos;
        if self.name_x(AA_STRING, name)
            && let Some(c) = self.chunk16()
            && c.last() == Some(&0)
        {
            return Some(c);
        }
        self.pos = p;
        None
    }

    fn string(&mut self, name: Option<&str>) -> Option<String> {
        self.str(name).map(cstr)
    }
}

/// はじめの NUL までを文字列に
fn cstr(b: &[u8]) -> String {
    String::from_utf8_lossy(b.split(|&c| c == 0).next().unwrap_or(&[])).into_owned()
}

// ---- DFA (match.c) ----

const YYTH_MAGIC: u32 = 0x1B5E_783D;
const YYTH_FLAGS: u16 = 1 | 2;
const YYTH_FLAG_DIFF_ENCODE: u16 = 1;
const YYTH_FLAG_OOB_TRANS: u16 = 2;
// 表の番号 (読んだ td_id から 1 をひいたもの)
const ID_ACCEPT: usize = 0;
const ID_BASE: usize = 1;
const ID_CHK: usize = 2;
const ID_DEF: usize = 3;
const ID_EC: usize = 4;
const ID_ACCEPT2: usize = 6;
const ID_NXT: usize = 7;
const ID_MAX: usize = 8;
const DATA8: u16 = 1;
const DATA16: u16 = 2;
const DATA32: u16 = 4;
const MATCH_FLAG_DIFF_ENCODE: u32 = 0x8000_0000;
const MARK_DIFF_ENCODE: u32 = 0x4000_0000;
const MATCH_FLAG_OOB_TRANSITION: u32 = 0x2000_0000;
const MARK_DIFF_ENCODE_VERIFIED: u32 = 0x1000_0000;
const MATCH_FLAGS_INVALID: u32 = 0xff00_0000 & !(MATCH_FLAG_DIFF_ENCODE | MATCH_FLAG_OOB_TRANSITION);
/// 状態 0 はどこへも行かない (落ちたところ)、1 がはじめ
const DFA_NOMATCH: u32 = 0;
const DFA_START: u32 = 1;

/// accept の表に要る型 (aa_dfa_unpack の flags: accept は下 6 ビット、accept2 はその上の 6 ビット)
fn accept1_flags(f: u32) -> u16 {
    (f & 0x3f) as u16
}

fn accept2_flags(f: u32) -> u16 {
    ((f >> ID_ACCEPT2) & 0x3f) as u16
}

fn be16(b: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([b[i], b[i + 1]])
}

fn be32(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

fn align8(n: usize) -> usize {
    (n + 7) & !7
}

/// 表をほどいた DFA (16 ビットの表は 32 ビットに広げる。accept の表は、ないときは空)
pub struct Dfa {
    flags: u16,
    accept: Vec<u32>,
    accept2: Vec<u32>,
    base: Vec<u32>,
    def: Vec<u32>,
    next: Vec<u32>,
    check: Vec<u32>,
    ec: Option<Vec<u32>>,
}

impl Dfa {
    fn states(&self) -> usize {
        self.base.len()
    }

    /// 1 文字すすむ (aa_dfa_next。差分で表した状態は default をたどる)
    fn step(&self, mut s: u32, c: u8) -> u32 {
        let c = match &self.ec {
            Some(e) => e[c as usize],
            None => c as u32,
        };
        loop {
            let b = self.base[s as usize];
            let pos = ((b & 0xff_ffff) + c) as usize;
            if self.check[pos] != s {
                s = self.def[s as usize];
                if b & MATCH_FLAG_DIFF_ENCODE != 0 {
                    continue;
                }
                return s;
            }
            return self.next[pos];
        }
    }

    /// start から s をみな通す (aa_dfa_match_len)
    pub fn match_len(&self, start: u32, s: &[u8]) -> u32 {
        if start == DFA_NOMATCH {
            return DFA_NOMATCH;
        }
        s.iter().fold(start, |st, &c| self.step(st, c))
    }

    /// blob の DFA をほどいて確かめる (aa_dfa_unpack。いつも paranoid_load: 状態と行き先が表の中か)
    fn unpack(blob: &[u8], flags: u32) -> Result<Dfa, i64> {
        if blob.len() < 16 || be32(blob, 0) != YYTH_MAGIC {
            return Err(-EPROTO);
        }
        let hsize = be32(blob, 4) as usize;
        if blob.len() < hsize {
            return Err(-EPROTO);
        }
        let dflags = be16(blob, 12);
        if dflags & !YYTH_FLAGS != 0 {
            return Err(-EPROTO);
        }
        let mut tables: [Option<Vec<u32>>; ID_MAX] = Default::default();
        let mut data = &blob[hsize..];
        while !data.is_empty() {
            let (id, tflags, v, size) = unpack_table(data)?;
            let ok = match id {
                ID_ACCEPT => tflags & accept1_flags(flags) != 0,
                ID_ACCEPT2 => tflags & accept2_flags(flags) != 0,
                ID_BASE => tflags == DATA32,
                ID_DEF | ID_NXT | ID_CHK => tflags == DATA16 || tflags == DATA32,
                ID_EC => tflags == DATA8,
                _ => false,
            };
            if !ok || tables[id].is_some() {
                return Err(-EPROTO);
            }
            tables[id] = Some(v);
            data = &data[size..];
        }
        let take = |t: &mut Option<Vec<u32>>| t.take().unwrap_or_default();
        // verify_table_headers: 要る表があって、大きさがそろっているか
        if tables[ID_DEF].is_none() || tables[ID_BASE].is_none() || tables[ID_NXT].is_none() || tables[ID_CHK].is_none() {
            return Err(-EPROTO);
        }
        let dfa = Dfa {
            flags: dflags,
            accept: take(&mut tables[ID_ACCEPT]),
            accept2: take(&mut tables[ID_ACCEPT2]),
            base: take(&mut tables[ID_BASE]),
            def: take(&mut tables[ID_DEF]),
            next: take(&mut tables[ID_NXT]),
            check: take(&mut tables[ID_CHK]),
            ec: tables[ID_EC].take(),
        };
        let n = dfa.states();
        if (accept1_flags(flags) != 0 && dfa.accept.len() != n)
            || (accept2_flags(flags) != 0 && dfa.accept2.len() != n)
            || dfa.def.len() != n
            || dfa.next.len() != dfa.check.len()
            || dfa.ec.as_ref().is_some_and(|e| e.len() != 256)
        {
            return Err(-EPROTO);
        }
        let mut dfa = dfa;
        dfa.verify()?;
        Ok(dfa)
    }

    /// verify_dfa: 状態と行き先が表の中、差分の鎖がまわっていない (step が止まる)
    fn verify(&mut self) -> Result<(), i64> {
        let (n, t) = (self.states(), self.next.len());
        if n < 2 {
            return Err(-EPROTO);
        }
        for i in 0..n {
            let b = self.base[i];
            if self.def[i] as usize >= n || b & MATCH_FLAGS_INVALID != 0 {
                return Err(-EPROTO);
            }
            if b & MATCH_FLAG_DIFF_ENCODE != 0 && self.flags & YYTH_FLAG_DIFF_ENCODE == 0 {
                return Err(-EPROTO);
            }
            // out of band の行き先は 1 つまで (MAX_OOB_SUPPORTED)
            if b & MATCH_FLAG_OOB_TRANSITION != 0 && ((b & 0xff_ffff) < 1 || self.flags & YYTH_FLAG_OOB_TRANS == 0) {
                return Err(-EPROTO);
            }
            if (b & 0xff_ffff) as usize + 255 >= t {
                return Err(-EPROTO);
            }
        }
        if self.next.iter().chain(&self.check).any(|&s| s as usize >= n) {
            return Err(-EPROTO);
        }
        for i in 0..n {
            let mut j = i;
            while self.base[j] & MATCH_FLAG_DIFF_ENCODE != 0 && self.base[j] & MARK_DIFF_ENCODE_VERIFIED == 0 {
                if self.base[j] & MARK_DIFF_ENCODE != 0 {
                    return Err(-EPROTO);
                }
                let k = self.def[j] as usize;
                if j == k {
                    return Err(-EPROTO);
                }
                self.base[j] |= MARK_DIFF_ENCODE;
                j = k;
            }
            let mut j = i;
            while self.base[j] & MATCH_FLAG_DIFF_ENCODE != 0 {
                let k = self.def[j] as usize;
                if j < i {
                    break;
                }
                self.base[j] = (self.base[j] & !MARK_DIFF_ENCODE) | MARK_DIFF_ENCODE_VERIFIED;
                j = k;
            }
        }
        Ok(())
    }
}

/// 表を 1 つ: (番号, 型, 中身, 次の表までの大きさ)
fn unpack_table(b: &[u8]) -> Result<(usize, u16, Vec<u32>, usize), i64> {
    if b.len() < 12 {
        return Err(-EPROTO);
    }
    let id = be16(b, 0).wrapping_sub(1) as usize;
    let tflags = be16(b, 2);
    let n = be32(b, 8) as usize;
    if id > ID_MAX || !matches!(tflags, DATA8 | DATA16 | DATA32) || n == 0 {
        return Err(-EPROTO);
    }
    let size = align8(12 + n * tflags as usize);
    if b.len() < size {
        return Err(-EPROTO);
    }
    let d = &b[12..];
    let v = match tflags {
        DATA8 => d[..n].iter().map(|&x| x as u32).collect(),
        DATA16 => (0..n).map(|i| be16(d, 2 * i) as u32).collect(),
        _ => (0..n).map(|i| be32(d, 4 * i)).collect(),
    };
    Ok((id, tflags, v, size))
}

// ---- 許し (perms.h、file.h) ----

pub const MAY_EXEC: u32 = 0x1;
pub const MAY_WRITE: u32 = 0x2;
pub const MAY_READ: u32 = 0x4;
pub const MAY_APPEND: u32 = 0x8;
pub const MAY_CREATE: u32 = 0x10;
pub const MAY_DELETE: u32 = 0x20;
pub const MAY_OPEN: u32 = 0x40;
pub const MAY_SETATTR: u32 = 0x100;
pub const MAY_GETATTR: u32 = 0x200;
pub const MAY_CHMOD: u32 = 0x1000;
pub const MAY_CHOWN: u32 = 0x2000;
pub const MAY_LOCK: u32 = 0x8000;
pub const EXEC_MMAP: u32 = 0x1_0000;
pub const MAY_LINK: u32 = 0x4_0000;
pub const MAY_ONEXEC: u32 = 0x2000_0000;
pub const MAY_CHANGE_PROFILE: u32 = 0x4000_0000;
pub const MAY_CHANGEHAT: u32 = 0x8000_0000;
// ソケットの許し (include/net.h)
pub const MAY_SEND: u32 = MAY_WRITE;
pub const MAY_RECEIVE: u32 = MAY_READ;
pub const MAY_SHUTDOWN: u32 = MAY_DELETE;
pub const MAY_CONNECT: u32 = MAY_OPEN;
pub const MAY_ACCEPT: u32 = 0x10_0000;
pub const MAY_BIND: u32 = 0x20_0000;
pub const MAY_LISTEN: u32 = 0x40_0000;
pub const MAY_SETOPT: u32 = 0x100_0000;
pub const MAY_GETOPT: u32 = 0x200_0000;
const NET_PERMS_MASK: u32 = MAY_SEND | MAY_RECEIVE | MAY_CREATE | MAY_SHUTDOWN | MAY_BIND | MAY_LISTEN | MAY_CONNECT | MAY_ACCEPT | MAY_SETATTR | MAY_GETATTR | MAY_SETOPT | MAY_GETOPT;
pub const NET_PEER_MASK: u32 = MAY_SEND | MAY_RECEIVE | MAY_CONNECT | MAY_ACCEPT;
/// まだ先 (protocol) を見ないと決まらない (AA_CONT_MATCH)
const CONT_MATCH: u32 = 0x800_0000;
const LINK_SUBSET: u32 = MAY_LOCK;
const ALL_PERMS: u32 = 0xffff_ffff;
// exec の行き先 (xindex)
pub const X_INDEX_MASK: u32 = 0x00ff_ffff;
pub const X_TYPE_MASK: u32 = 0x0c00_0000;
pub const X_NAME: u32 = 0x0400_0000;
pub const X_TABLE: u32 = 0x0800_0000;
pub const X_UNSAFE: u32 = 0x1000_0000;
pub const X_CHILD: u32 = 0x2000_0000;
pub const X_INHERIT: u32 = 0x4000_0000;
pub const X_UNCONFINED: u32 = 0x8000_0000;
/// accept2 の印: 許しは (持ち主のとき, ほかのとき) の 2 つ組
const ACCEPT_FLAG_OWNER: u32 = 1;
// 決まりの種類 (apparmor.h の AA_CLASS_*)
pub const CLASS_FILE: usize = 2;
const CLASS_XMATCH: usize = 11;
const CLASS_LAST: usize = 32;

#[derive(Clone, Copy, Default, PartialEq)]
pub struct Perms {
    pub allow: u32,
    pub deny: u32,
    pub subtree: u32,
    pub cond: u32,
    pub kill: u32,
    pub complain: u32,
    pub prompt: u32,
    pub audit: u32,
    pub quiet: u32,
    pub hide: u32,
    pub xindex: u32,
    pub tag: u32,
    pub label: u32,
}

/// 何もない (nullperms) と、なんでも (allperms: 重ねていくときのはじめ)
const ALLPERMS: Perms = Perms { allow: ALL_PERMS, deny: 0, subtree: 0, cond: 0, kill: 0, complain: 0, prompt: 0, audit: 0, quiet: ALL_PERMS, hide: ALL_PERMS, xindex: 0, tag: 0, label: 0 };

impl Perms {
    fn valid(&self) -> bool {
        self.allow & self.deny == 0
            && self.subtree & !self.allow == 0
            && self.cond & (self.allow | self.deny) == 0
            && self.kill & self.allow == 0
            && self.complain & (self.allow | self.deny) == 0
            && self.prompt & (self.allow | self.deny) == 0
            && self.complain & self.prompt == 0
            && self.hide & self.allow == 0
    }

    /// aa_perms_accum_raw: いくつものプロファイルの許しを重ねる
    fn accum_raw(&mut self, a: &Perms) {
        self.deny |= a.deny;
        self.allow &= a.allow & !a.deny;
        self.audit |= a.audit & a.allow;
        self.quiet &= a.quiet & !a.allow;
        self.kill |= a.kill & !a.allow;
        self.complain |= a.complain & !a.allow & !a.deny;
        self.cond |= a.cond & !a.allow & !a.deny;
        self.hide &= a.hide & !a.allow;
        self.prompt |= a.prompt & !a.allow & !a.deny;
        self.subtree |= a.subtree & !a.deny;
        if self.xindex == 0 {
            self.xindex = a.xindex;
        }
        if self.tag == 0 {
            self.tag = a.tag;
        }
        if self.label == 0 {
            self.label = a.label;
        }
    }
}

// 古い形 (accept と accept2 にうめこんだ許し) を許しの表へ (policy_compat.c)

fn map_xindex(mask: u32) -> u32 {
    let old = (mask >> 10) & 0xf;
    let mut index = 0;
    if mask & 0x100 != 0 {
        index |= X_UNSAFE;
    }
    if mask & 0x200 != 0 {
        index |= X_INHERIT;
    }
    if mask & 0x80 != 0 {
        index |= X_UNCONFINED;
    }
    match old {
        0 => {}
        1 => index |= X_UNCONFINED,
        2 => index |= X_NAME,
        3 => index |= X_NAME | X_CHILD,
        _ => index |= X_TABLE | (old - 4),
    }
    index
}

fn map_old_perms(old: u32) -> u32 {
    let mut new = old & 0xf;
    if old & MAY_READ != 0 {
        new |= MAY_GETATTR | MAY_OPEN;
    }
    if old & MAY_WRITE != 0 {
        new |= MAY_SETATTR | MAY_CREATE | MAY_DELETE | MAY_CHMOD | MAY_CHOWN | MAY_OPEN;
    }
    if old & 0x10 != 0 {
        new |= MAY_LINK;
    }
    // 古い形では lock と link の subset が同じビット (どちらかは組で決まる)
    if old & 0x20 != 0 {
        new |= MAY_LOCK | LINK_SUBSET;
    }
    if old & 0x40 != 0 {
        new |= EXEC_MMAP;
    }
    new
}

/// ファイルの許し: 持ち主のとき (other が false) と、ほかのとき
fn fperms(d: &Dfa, s: usize, other: bool) -> Perms {
    let (a, a2) = (d.accept[s], d.accept2[s]);
    let (sh, x) = if other { (14, 14) } else { (0, 0) };
    let mut p = Perms {
        allow: map_old_perms(((a >> sh) & 0x7f) | (a & 0x8000_0000)),
        audit: map_old_perms((a2 >> sh) & 0x7f),
        quiet: map_old_perms(((a2 >> 7) >> sh) & 0x7f),
        xindex: map_xindex((a >> x) & 0x3fff),
        ..Perms::default()
    };
    p.allow |= MAY_GETATTR;
    // change_profile は持ち主では分けない
    if a & 0x8000_0000 != 0 {
        p.allow |= MAY_CHANGE_PROFILE;
    }
    if a & 0x4000_0000 != 0 {
        p.allow |= MAY_ONEXEC;
    }
    p
}

fn map_other(x: u32) -> u32 {
    ((x & 0x3) << 8) | ((x & 0x1c) << 18) | ((x & 0x60) << 19)
}

fn map_xbits(x: u32) -> u32 {
    ((x & 0x1) << 7) | ((x & 0x7e) << 9)
}

/// policydb (ファイルのほかの決まり) の許し (compute_perms_entry)
fn policy_perms(d: &Dfa, s: usize, version: u32) -> Perms {
    let (a, a2) = (d.accept[s], d.accept2[s]);
    let mut p = Perms { allow: (a & 0x7f) | (a & 0x8000_0000), audit: a2 & 0x7f, quiet: (a2 >> 7) & 0x7f, ..Perms::default() };
    p.allow |= map_other(((a >> 14) & 0x7f) | (a & 0x8000_0000));
    if version & K_ABI_MASK <= 8 {
        p.allow |= MAY_LOCK;
    } else {
        p.allow |= map_xbits((a >> 7) & 0x7f);
    }
    p.audit |= map_other((a2 >> 14) & 0x7f);
    p.quiet |= map_other(((a2 >> 7) >> 14) & 0x7f);
    if version & K_ABI_MASK > 8 {
        p.quiet |= map_xbits(((a >> 7) >> 14) & 0x7f);
    }
    p
}

/// 許しの表を作り、accept を表の番号に置きかえる (remap_dfa_accept。factor 2 は持ち主とほかの 2 つ組)
fn remap(d: &mut Dfa, factor: u32) {
    for s in 0..d.states() {
        d.accept[s] = s as u32 * factor;
        d.accept2[s] = if factor > 1 { ACCEPT_FLAG_OWNER } else { 0 };
    }
}

// ---- 決まりの組 (struct aa_policydb) ----

pub struct Pdb {
    dfa: Option<Dfa>,
    perms: Vec<Perms>,
    /// 種類ごとのはじめの状態 (0 は policydb 全体、CLASS_FILE はファイル)
    start: [u32; CLASS_LAST + 1],
    /// exec の行き先の名前 (xtable。X_TABLE の番号で引く)
    trans: Vec<String>,
}

impl Pdb {
    const NULL: Pdb = Pdb { dfa: None, perms: Vec::new(), start: [0; CLASS_LAST + 1], trans: Vec::new() };

    /// 状態 s の許し (aa_lookup_condperms。owner: 持ち主のファイルか)
    fn lookup(&self, s: u32, owner: bool) -> Perms {
        let Some(d) = &self.dfa else { return Perms::default() };
        if self.perms.is_empty() {
            return Perms::default();
        }
        let index = d.accept[s as usize] as usize;
        if d.accept2[s as usize] & ACCEPT_FLAG_OWNER != 0 && !owner {
            return self.perms[index + 1];
        }
        self.perms[index]
    }

    /// verify_perms と verify_dfa_accept_index: 許しが矛盾せず、番号が表の中
    fn verify(&mut self) -> bool {
        if let Some(d) = &self.dfa
            && d.accept.iter().any(|&i| i as usize >= self.perms.len())
        {
            return false;
        }
        let mut xmax: Option<usize> = None;
        for p in &self.perms {
            if !p.valid() {
                return false;
            }
            if p.xindex & X_TYPE_MASK == X_TABLE {
                let x = (p.xindex & X_INDEX_MASK) as usize;
                if x >= self.trans.len() {
                    return false;
                }
                xmax = Some(xmax.map_or(x, |m| m.max(x)));
            }
            if p.tag != 0 || (p.label != 0 && p.label as usize >= self.trans.len()) {
                return false;
            }
        }
        // 使われていない名前は捨てる (Linux と同じ)
        self.trans.truncate(xmax.map_or(0, |m| m + 1));
        true
    }
}

/// unpack_pdb: 許しの表 (あれば) と DFA と、はじめの状態と exec の行き先の名前
fn unpack_pdb(e: &mut Ext, required_dfa: bool, required_trans: bool, info: &mut &'static str) -> Result<Pdb, i64> {
    let pos = e.pos;
    let r = unpack_pdb_inner(e, required_dfa, required_trans, info);
    if r.is_err() {
        e.pos = pos;
    }
    r
}

fn unpack_pdb_inner(e: &mut Ext, required_dfa: bool, required_trans: bool, info: &mut &'static str) -> Result<Pdb, i64> {
    let mut pdb = Pdb::NULL;
    // tags は新しい版のもの (features で名のっていない)
    if e.name_x(AA_STRUCT, Some("tags")) {
        *info = "invalid tags version";
        return Err(-EPROTO);
    }
    let has_perms = unpack_perms_table(e, &mut pdb.perms).map_err(|err| {
        *info = "failed to unpack - perms";
        err
    })?;
    // 許しの表があれば accept は番号、なければ accept と accept2 に許しをうめこんである
    let mut flags = DATA32 as u32;
    if has_perms {
        if e.u32(Some("permsv")).is_some_and(|v| v > 2) {
            flags |= (DATA32 as u32) << ID_ACCEPT2;
        }
    } else {
        flags |= (DATA32 as u32) << ID_ACCEPT2;
    }
    if let Some((at, blob)) = e.blob(Some("aadfa")) {
        // DFA は並びの頭から 8 バイトにそろえて置いてある (unpack_dfa と同じ数え方で)
        let sz = at.wrapping_sub(e.pos & 7);
        let pad = align8(sz) - sz;
        if pad > blob.len() {
            *info = "failed to unpack - dfa";
            return Err(-EPROTO);
        }
        let dfa = Dfa::unpack(&blob[pad..], flags).map_err(|err| {
            *info = "failed to unpack - dfa";
            err
        })?;
        pdb.start[0] = e.u32(Some("start")).unwrap_or(DFA_START);
        pdb.start[CLASS_FILE] = e.u32(Some("dfa_start")).unwrap_or(DFA_START);
        let n = dfa.states() as u32;
        if pdb.start[0] >= n || pdb.start[CLASS_FILE] >= n {
            *info = "invalid dfa start state";
            return Err(-EPROTO);
        }
        for i in CLASS_FILE + 1..=CLASS_LAST {
            pdb.start[i] = dfa.step(pdb.start[0], i as u8);
        }
        pdb.dfa = Some(dfa);
    } else if required_dfa {
        *info = "missing required dfa";
        return Err(-EPROTO);
    }
    // 許しの表があって accept2 がなければ、0 の表を足す (v2 の表)
    if let Some(d) = &mut pdb.dfa
        && has_perms
        && d.accept2.is_empty()
    {
        d.accept2 = vec![0; d.accept.len()];
    }
    match unpack_strs_table(e, "xtable") {
        Ok(t) => pdb.trans = t,
        Err(err) if required_trans => {
            *info = "failed to unpack profile transition table";
            return Err(err);
        }
        Err(_) => {}
    }
    if pdb.dfa.is_none() {
        pdb.trans.clear();
    }
    Ok(pdb)
}

/// 許しの表 (あれば true)
fn unpack_perms_table(e: &mut Ext, out: &mut Vec<Perms>) -> Result<bool, i64> {
    let pos = e.pos;
    if !e.name_x(AA_STRUCT, Some("perms")) {
        return Ok(false);
    }
    let fail = |e: &mut Ext| {
        e.pos = pos;
        Err(-EPROTO)
    };
    let (Some(version), Some(n)) = (e.u32(Some("version")), e.array(None)) else { return fail(e) };
    if version != 1 {
        return fail(e);
    }
    for _ in 0..n {
        // はじめの 1 つは、あとのためにとってある
        let mut v = [0u32; 14];
        for x in v.iter_mut() {
            match e.u32(None) {
                Some(n) => *x = n,
                None => return fail(e),
            }
        }
        out.push(Perms { allow: v[1], deny: v[2], subtree: v[3], cond: v[4], kill: v[5], complain: v[6], prompt: v[7], audit: v[8], quiet: v[9], hide: v[10], xindex: v[11], tag: v[12], label: v[13] });
    }
    if !e.name_x(AA_ARRAYEND, None) || !e.name_x(AA_STRUCTEND, None) {
        return fail(e);
    }
    Ok(true)
}

/// 名前の表 (xtable)。1 つずつ 1 つの名前 (":ns\0name" は ":ns:name" に。process_strs_entry と同じ確かめ)
fn unpack_strs_table(e: &mut Ext, name: &str) -> Result<Vec<String>, i64> {
    let pos = e.pos;
    let mut v = Vec::new();
    if !e.name_x(AA_STRUCT, Some(name)) {
        return Ok(v);
    }
    let fail = |e: &mut Ext| {
        e.pos = pos;
        Err(-EPROTO)
    };
    let Some(n) = e.array(None) else { return fail(e) };
    for _ in 0..n {
        let Some(s) = e.str(None) else { return fail(e) };
        let mut s = s.to_vec();
        let body = s.len() - 1;
        if body == 0 || s[0].is_ascii_whitespace() || (s[0] == b':' && s[1] == 0) {
            return fail(e);
        }
        let mut start = 0;
        for i in 1..body {
            if s[i] == 0 {
                // ":ns" のあとの NUL だけは ':' (ほかに NUL があれば、名前がいくつもある: xtable には置けない)
                if s[start] == b':' {
                    s[i] = b':';
                } else {
                    return fail(e);
                }
                start = i + 1;
            }
        }
        v.push(cstr(&s));
    }
    if !e.name_x(AA_ARRAYEND, None) || !e.name_x(AA_STRUCTEND, None) {
        return fail(e);
    }
    Ok(v)
}

// ---- プロファイル (struct aa_profile) ----

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Mode {
    Enforce,
    Complain,
    Kill,
    Unconfined,
    User,
}

const MODE_NAMES: [&str; 5] = ["enforce", "complain", "kill", "unconfined", "user"];

pub struct Caps {
    pub allow: u64,
    pub audit: u64,
    pub quiet: u64,
    pub extended: u64,
}

/// 読んだものをみな持つ (attach、disconnected、rlimits、data などは、それを効かせるところができるまで読むだけ)
#[allow(dead_code)]
pub struct Profile {
    /// 階層の名前 (子は "親//子")
    pub name: String,
    rename: Option<String>,
    /// 名前ではなくパスでつけるとき (attach、xmatch)
    pub attach: Option<String>,
    pub xmatch: Pdb,
    pub xmatch_len: u32,
    pub disconnected: Option<String>,
    pub signal: u32,
    pub flags: u32,
    pub mode: Mode,
    pub audit_all: bool,
    pub path_flags: u32,
    pub caps: Caps,
    pub xattrs: Vec<String>,
    pub rlimits: (u32, Vec<u64>),
    pub policy: Rc<Pdb>,
    pub file: Rc<Pdb>,
    pub data: Vec<(String, Vec<u8>)>,
    /// securityfs の policy/profiles/ のディレクトリの番号 (読みこんだときに。入れかえても同じ: Linux の ns の uniq_id)
    pub uniq: u64,
}

impl Profile {
    pub fn mode_name(&self) -> &'static str {
        MODE_NAMES[self.mode as usize]
    }

    /// hat (プロファイルの中の ^NAME。change_hat で入る子: PACKED_FLAG_HAT)
    pub fn is_hat(&self) -> bool {
        self.flags & 1 != 0
    }

    /// 文字列 s を、この決まりの種類 (s の 1 文字め) で問う (.access の profile_query_cb)。ファイルの持ち主は uid 0 と
    /// して (Linux も cond は 0): 問う人が root なら持ち主の許し (owner の決まり) を見る
    fn query(&self, s: &[u8], perms: &mut Perms) {
        let owner = crate::cred::current().euid == 0;
        if self.mode == Mode::Unconfined || s.is_empty() {
            return;
        }
        let mut tmp = Perms::default();
        if let Some(d) = self.file.dfa.as_ref().filter(|_| s[0] as usize == CLASS_FILE) {
            let st = d.match_len(self.file.start[CLASS_FILE], &s[1..]);
            if st != DFA_NOMATCH {
                tmp = self.file.lookup(st, owner);
            }
        } else if let Some(d) = &self.policy.dfa {
            // この種類の決まりがなければ変えない (RULE_MEDIATES)
            let class = s[0] as usize;
            let st = if class <= CLASS_LAST { self.policy.start[class] } else { d.match_len(self.policy.start[0], &s[..1]) };
            if st == DFA_NOMATCH {
                return;
            }
            let st = d.match_len(self.policy.start[0], s);
            if st != DFA_NOMATCH {
                tmp = self.policy.lookup(st, true);
            }
        }
        self.apply_modes(&mut tmp);
        perms.accum_raw(&tmp);
    }

    /// aa_apply_modes_to_perms
    pub fn apply_modes(&self, p: &mut Perms) {
        match self.mode {
            Mode::Kill => p.kill = !p.allow,
            Mode::Complain => p.complain |= !(p.allow | p.deny),
            Mode::User => p.prompt |= !(p.allow | p.deny),
            _ => {}
        }
        if self.audit_all {
            p.audit = ALL_PERMS;
            p.quiet = 0;
        }
    }
}

/// unpack_profile: 並びからプロファイルを 1 つ
fn unpack_profile(e: &mut Ext, info: &mut &'static str) -> Result<Profile, (i64, Option<String>)> {
    *info = "failed to unpack profile";
    let fail = |name: Option<String>| Err((-EPROTO, name));
    if !e.name_x(AA_STRUCT, Some("profile")) {
        return fail(None);
    }
    let Some(name) = e.string(None).filter(|n| !n.is_empty()) else { return fail(None) };
    let named = |err: i64| (err, Some(name.clone()));
    // 名前空間 (":ns:name") はまだない
    if name.starts_with(':') {
        *info = "namespaces are not supported";
        return Err(named(-EOPNOTSUPP));
    }
    let rename = e.string(Some("rename"));
    let attach = e.string(Some("attach"));
    let mut xmatch = unpack_pdb(e, false, false, info).map_err(|_| {
        *info = "bad xmatch";
        named(-EPROTO)
    })?;
    let mut xmatch_len = 0;
    if let Some(d) = &mut xmatch.dfa {
        let Some(n) = e.u32(None) else {
            *info = "missing xmatch len";
            return Err(named(-EPROTO));
        };
        xmatch_len = n;
        xmatch.start[CLASS_XMATCH] = DFA_START;
        if xmatch.perms.is_empty() {
            // compute_xmatch_perms: 持ち主のほうの許しだけ
            xmatch.perms = (0..d.states()).map(|s| if s == 0 { Perms::default() } else { Perms { allow: (d.accept[s] & 0x7f) | (d.accept[s] & 0x8000_0000), ..Perms::default() } }).collect();
            remap(d, 1);
        }
    }
    let disconnected = e.string(Some("disconnected"));
    let signal = e.u32(Some("kill")).unwrap_or(9);
    if !(1..=35).contains(&signal) {
        *info = "profile kill.signal invalid value";
        return Err(named(-EPROTO));
    }
    if !e.name_x(AA_STRUCT, Some("flags")) {
        *info = "profile missing flags";
        return Err(named(-EPROTO));
    }
    *info = "failed to unpack profile flags";
    let (Some(flags), Some(mode), Some(audit)) = (e.u32(None), e.u32(None), e.u32(None)) else { return Err(named(-EPROTO)) };
    let mode = if mode == 1 || e.version & FORCE_COMPLAIN_FLAG != 0 {
        Mode::Complain
    } else {
        match mode {
            0 => Mode::Enforce,
            2 => Mode::Kill,
            3 => Mode::Unconfined,
            4 => Mode::User,
            _ => return Err(named(-EPROTO)),
        }
    };
    if !e.name_x(AA_STRUCTEND, None) {
        return Err(named(-EPROTO));
    }
    // PATH_MEDIATE_DELETED (0x10000) は既定で
    let path_flags = e.u32(Some("path_flags")).unwrap_or(0x10000);
    *info = "failed to unpack profile capabilities";
    let mut caps = Caps { allow: 0, audit: 0, quiet: 0, extended: 0 };
    let (Some(a), Some(au), Some(q), Some(_deny)) = (e.u32(None), e.u32(None), e.u32(None), e.u32(None)) else { return Err(named(-EPROTO)) };
    (caps.allow, caps.audit, caps.quiet) = (a as u64, au as u64, q as u64);
    *info = "failed to unpack upper profile capabilities";
    if e.name_x(AA_STRUCT, Some("caps64")) {
        let (Some(a), Some(au), Some(q), Some(_deny)) = (e.u32(None), e.u32(None), e.u32(None), e.u32(None)) else { return Err(named(-EPROTO)) };
        caps.allow |= (a as u64) << 32;
        caps.audit |= (au as u64) << 32;
        caps.quiet |= (q as u64) << 32;
        if !e.name_x(AA_STRUCTEND, None) {
            return Err(named(-EPROTO));
        }
    }
    *info = "failed to unpack extended profile capabilities";
    if e.name_x(AA_STRUCT, Some("capsx")) {
        let (Some(lo), Some(hi)) = (e.u32(None), e.u32(None)) else { return Err(named(-EPROTO)) };
        caps.extended = lo as u64 | (hi as u64) << 32;
        if !e.name_x(AA_STRUCTEND, None) {
            return Err(named(-EPROTO));
        }
    }
    let xattrs = unpack_list(e, "xattrs", |e| e.string(None)).ok_or_else(|| {
        *info = "failed to unpack profile xattrs";
        named(-EPROTO)
    })?;
    let rlimits = unpack_rlimits(e).ok_or_else(|| {
        *info = "failed to unpack profile rlimits";
        named(-EPROTO)
    })?;
    unpack_list(e, "secmark", |e| Some((e.u8(None)?, e.u8(None)?, e.string(None)?))).ok_or_else(|| {
        *info = "failed to unpack profile secmark rules";
        named(-EPROTO)
    })?;
    let policy = if e.name_x(AA_STRUCT, Some("policydb")) {
        *info = "failed to unpack policydb";
        let mut p = unpack_pdb(e, true, false, info).map_err(named)?;
        if let Some(d) = &p.dfa {
            let f = d.step(p.start[0], CLASS_FILE as u8);
            if f != DFA_NOMATCH {
                p.start[CLASS_FILE] = f;
            }
        }
        if !e.name_x(AA_STRUCTEND, None) {
            return Err(named(-EPROTO));
        }
        if p.perms.is_empty()
            && let Some(d) = &mut p.dfa
        {
            p.perms = (0..d.states()).map(|s| if s == 0 { Perms::default() } else { policy_perms(d, s, e.version) }).collect();
            remap(d, 1);
        }
        p
    } else {
        Pdb::NULL
    };
    let policy = Rc::new(policy);
    let mut file = unpack_pdb(e, false, true, info).map_err(named)?;
    let file = if let Some(d) = &mut file.dfa {
        if file.perms.is_empty() {
            file.perms = (0..d.states()).flat_map(|s| [fperms(d, s, false), fperms(d, s, true)]).collect();
            remap(d, 2);
        }
        Rc::new(file)
    } else if policy.dfa.is_some() && policy.start[CLASS_FILE] != DFA_NOMATCH {
        policy.clone()
    } else {
        Rc::new(Pdb::NULL)
    };
    let mut data = Vec::new();
    if e.name_x(AA_STRUCT, Some("data")) {
        while let Some(k) = e.string(None) {
            let v = e.blob(None).map(|(_, b)| b.to_vec()).unwrap_or_default();
            data.push((k, v));
        }
        if !e.name_x(AA_STRUCTEND, None) {
            *info = "failed to unpack end of key, value data table";
            return Err(named(-EPROTO));
        }
    }
    if !e.name_x(AA_STRUCTEND, None) {
        *info = "failed to unpack end of profile";
        return Err(named(-EPROTO));
    }
    let mut p = Profile { name, rename, attach, xmatch, xmatch_len, disconnected, signal, flags, mode, audit_all: audit != 0, path_flags, caps, xattrs, rlimits, policy, file, data, uniq: 0 };
    // verify_profile
    let ok = Rc::get_mut(&mut p.file).is_none_or(|f| f.verify()) && Rc::get_mut(&mut p.policy).is_none_or(|f| f.verify()) && p.xmatch.verify();
    if !ok {
        *info = "Unpack: Invalid perm index";
        return Err((-EPROTO, Some(p.name)));
    }
    Ok(p)
}

/// STRUCT name { ARRAY [f ...] } (なければ空)
fn unpack_list<T>(e: &mut Ext, name: &str, mut f: impl FnMut(&mut Ext) -> Option<T>) -> Option<Vec<T>> {
    let pos = e.pos;
    let mut v = Vec::new();
    if !e.name_x(AA_STRUCT, Some(name)) {
        return Some(v);
    }
    let r = (|| {
        let n = e.array(None)?;
        for _ in 0..n {
            v.push(f(e)?);
        }
        (e.name_x(AA_ARRAYEND, None) && e.name_x(AA_STRUCTEND, None)).then_some(())
    })();
    if r.is_none() {
        e.pos = pos;
        return None;
    }
    Some(v)
}

/// rlimits: (決まりのある資源のビット, 上限)。番号は arm64 の RLIMIT_* と同じ
fn unpack_rlimits(e: &mut Ext) -> Option<(u32, Vec<u64>)> {
    let pos = e.pos;
    if !e.name_x(AA_STRUCT, Some("rlimits")) {
        return Some((0, Vec::new()));
    }
    let r = (|| {
        let mask = e.u32(None)?;
        let n = e.array(None)?;
        if n > 16 {
            return None;
        }
        let v = (0..n).map(|_| e.u64(None)).collect::<Option<Vec<u64>>>()?;
        (e.name_x(AA_ARRAYEND, None) && e.name_x(AA_STRUCTEND, None)).then_some((mask, v))
    })();
    if r.is_none() {
        e.pos = pos;
    }
    r
}

/// aa_unpack: 並びをみな (version の header と、プロファイルのくりかえし)
fn unpack(data: &[u8]) -> Result<Vec<Profile>, (i64, &'static str, Option<String>)> {
    let mut e = Ext { d: data, pos: 0, version: 0 };
    let mut v = Vec::new();
    while e.pos < data.len() {
        // verify_header: はじめのものは版が要る。v5〜v9 だけ
        match e.u32(Some("version")) {
            Some(ver) => e.version = ver,
            None if e.pos == 0 => return Err((-EPROTONOSUPPORT, "invalid profile format", None)),
            None => {}
        }
        let abi = e.version & K_ABI_MASK;
        if !(5..=9).contains(&abi) {
            return Err((-EPROTONOSUPPORT, "unsupported interface version", None));
        }
        if e.string(Some("namespace")).is_some() {
            return Err((-EOPNOTSUPP, "namespaces are not supported", None));
        }
        let mut info = "";
        match unpack_profile(&mut e, &mut info) {
            Ok(p) => v.push(p),
            Err((err, name)) => return Err((err, info, name)),
        }
    }
    Ok(v)
}

// ---- 読みこんだプロファイル (いまは根の名前空間だけ) ----

struct Ns {
    /// 読みこんだ順 (Linux はリストの頭に足すので、見せるのは新しいものから)
    profiles: Vec<Rc<Profile>>,
    revision: u64,
    /// 次のプロファイルの uniq
    next_uniq: u64,
}

static NS: Bkl<Ns> = Bkl::new(Ns { profiles: Vec::new(), revision: 1, next_uniq: 0 });

/// 名前でプロファイルを
pub fn find(name: &str) -> Option<Rc<Profile>> {
    NS.get().profiles.iter().find(|p| p.name == name).cloned()
}

/// .load (replace なし) と .replace: data のプロファイルをみな (ひとつでもしくじれば、どれも入れない)。書いた長さ
pub fn load(data: &[u8], replace: bool) -> Result<usize, i64> {
    may_manage()?;
    let op = if replace { "profile_replace" } else { "profile_load" };
    let new = unpack(data).map_err(|(err, info, name)| {
        audit_status(op, name.as_deref(), Some(info), err);
        err
    })?;
    let ns = NS.get();
    for p in &new {
        if !replace && ns.profiles.iter().any(|o| o.name == p.name) {
            audit_status(op, Some(&p.name), Some("profile can not be replaced"), -EEXIST);
            return Err(-EEXIST);
        }
    }
    ns.revision += 1;
    for mut p in new {
        let old = ns.profiles.iter().position(|o| o.name == p.name);
        p.uniq = match old {
            Some(i) => ns.profiles[i].uniq,
            None => {
                ns.next_uniq += 1;
                ns.next_uniq - 1
            }
        };
        audit_status(if old.is_some() { "profile_replace" } else { "profile_load" }, Some(&p.name), None, 0);
        let rename = p.rename.clone();
        match old {
            Some(i) => ns.profiles[i] = Rc::new(p),
            None => ns.profiles.push(Rc::new(p)),
        }
        // rename: 前の名前のものは外す
        if let Some(r) = rename {
            ns.profiles.retain(|o| o.name != r);
        }
    }
    Ok(data.len())
}

/// .remove: 名前のプロファイル (と、その子) を外す
pub fn remove(name: &[u8]) -> Result<usize, i64> {
    may_manage()?;
    let n = cstr(name);
    let n = n.trim_end_matches('\n');
    let ns = NS.get();
    if n.is_empty() || n.starts_with(':') || !ns.profiles.iter().any(|p| p.name == n) {
        audit_status("profile_remove", Some(n), Some("profile does not exist"), -ENOENT);
        return Err(-ENOENT);
    }
    let child = format!("{}//", n);
    ns.profiles.retain(|p| p.name != n && !p.name.starts_with(&child));
    ns.revision += 1;
    audit_status("profile_remove", Some(n), None, 0);
    Ok(name.len())
}

/// 決まりを変えてよいか (aa_may_manage_policy: namespace の外で CAP_MAC_ADMIN)
fn may_manage() -> Result<(), i64> {
    let c = crate::cred::current();
    if c.ns.user.is_some() || !c.capable(crate::cred::CAP_MAC_ADMIN) {
        audit_status("profile_manage", None, Some("not policy admin"), -EACCES);
        return Err(-EACCES);
    }
    Ok(())
}

/// /sys/kernel/security/apparmor/profiles: 1 行に「名前 (モード)」。新しく読んだものから、子は親のあと
pub fn profiles_text() -> Result<String, i64> {
    if crate::cred::current().euid != 0 {
        return Err(-EACCES);
    }
    let ns = NS.get();
    let mut out = String::new();
    fn walk(all: &[Rc<Profile>], parent: Option<&str>, out: &mut String) {
        for p in all.iter().rev() {
            let up = p.name.rfind("//").map(|i| &p.name[..i]);
            if up == parent {
                out.push_str(&format!("{} ({})\n", p.name, p.mode_name()));
                walk(all, Some(&p.name), out);
            }
        }
    }
    walk(&ns.profiles, None, &mut out);
    Ok(out)
}

pub fn revision() -> u64 {
    NS.get().revision
}

/// 階層の名前の終わり ("親//子" の子)
fn base_name(p: &Profile) -> &str {
    p.name.rsplit("//").next().unwrap_or(&p.name)
}

/// policy/profiles/ の中: parent (None は頭。番号) のすぐ下のプロファイルの (ディレクトリの名前, 番号)。名前は Linux の
/// mangle_name (頭の / と . を落とし、/ は .、空白は _、英数字と ._- のほかは消す) に ".番号"
pub fn profile_dirs(parent: Option<u64>) -> Vec<(String, u64)> {
    let ns = NS.get();
    let up = match parent {
        Some(u) => match ns.profiles.iter().find(|p| p.uniq == u) {
            Some(p) => Some(p.name.clone()),
            None => return Vec::new(),
        },
        None => None,
    };
    let mangle = |s: &str| -> String {
        s.trim_start_matches(['/', '.'])
            .chars()
            .filter_map(|c| match c {
                '/' => Some('.'),
                c if c.is_ascii_whitespace() => Some('_'),
                c if c.is_ascii_alphanumeric() || "._-".contains(c) => Some(c),
                _ => None,
            })
            .collect()
    };
    ns.profiles
        .iter()
        .rev()
        .filter(|p| p.name.rfind("//").map(|i| &p.name[..i]) == up.as_deref())
        .map(|p| (format!("{}.{}", mangle(base_name(p)), p.uniq), p.uniq))
        .collect()
}

/// policy/profiles/NAME.N/ のファイル: name (階層の名前の終わり)、mode、attach (つけるパス。DFA だけなら <unknown>、
/// なければ名前)。プロファイルがなければ None
pub fn profile_file(uniq: u64, file: &str) -> Option<String> {
    let p = NS.get().profiles.iter().find(|p| p.uniq == uniq).cloned()?;
    Some(match file {
        "name" => format!("{}\n", base_name(&p)),
        "mode" => format!("{}\n", p.mode_name()),
        _ => match (&p.attach, &p.xmatch.dfa) {
            (Some(a), _) => format!("{}\n", a),
            (None, Some(_)) => "<unknown>\n".into(),
            (None, None) => format!("{}\n", base_name(&p)),
        },
    })
}

/// 番号のプロファイルがあるか、子があるか
pub fn profile_exists(uniq: u64) -> Option<bool> {
    let p = NS.get().profiles.iter().find(|p| p.uniq == uniq).cloned()?;
    Some(has_children(&p))
}

/// .access の問い (aa_write_access): "label\0NAME\0Q"、"profile\0..."、"labelall\0..."。答えは
/// "allow 0x%08x\ndeny 0x%08x\naudit 0x%08x\nquiet 0x%08x\n"。data の問いはまだない
pub fn query(q: &[u8]) -> Result<String, i64> {
    let rest = ["profile\0", "label\0", "labelall\0"].iter().find_map(|c| (q.len() > c.len() && q.starts_with(c.as_bytes())).then(|| &q[c.len()..])).ok_or(-EINVAL)?;
    let n = rest.iter().position(|&b| b == 0).filter(|&n| n > 0).ok_or(-EINVAL)?;
    let name = String::from_utf8_lossy(&rest[..n]).into_owned();
    let s = &rest[n + 1..];
    let mut perms = ALLPERMS;
    // ラベルは "a//&b" で重ねられる (いまは 1 つずつ)。unconfined は決まりなし
    for one in name.split("//&") {
        if one == "unconfined" {
            continue;
        }
        find(one).ok_or(-ENOENT)?.query(s, &mut perms);
    }
    Ok(format!("allow 0x{:08x}\ndeny 0x{:08x}\naudit 0x{:08x}\nquiet 0x{:08x}\n", perms.allow, perms.deny, perms.audit, perms.quiet))
}

// ---- 記録 (Linux の audit と同じ形を kmsg へ。dmesg に出る) ----

static SERIAL: AtomicU64 = AtomicU64::new(1);

/// audit_log_untrustedstring: 引用符でかこむ。空白や " や 7bit の外の文字があれば 16 進で
fn untrusted(s: &str) -> String {
    if s.bytes().any(|c| c == b'"' || !(0x21..=0x7e).contains(&c)) {
        s.bytes().map(|c| format!("{:02X}", c)).collect()
    } else {
        format!("\"{}\"", s)
    }
}

/// "audit: type=1400 audit(秒.ミリ秒:番号): apparmor=..." の頭
fn audit_head(kind: &str) -> String {
    let ns = crate::timer::epoch_ns();
    format!("audit: type=1400 audit({}.{:03}:{}): apparmor=\"{}\"", ns / 1_000_000_000, ns / 1_000_000 % 1000, SERIAL.fetch_add(1, Relaxed), kind)
}

/// 終わりの pid と comm
fn audit_tail() -> String {
    let Some(p) = crate::proc::try_current() else { return " pid=0 comm=\"swapper\"".into() };
    let n = p.comm.iter().position(|&c| c == 0).unwrap_or(p.comm.len());
    format!(" pid={} comm={}", p.pid, untrusted(&String::from_utf8_lossy(&p.comm[..n])))
}

/// 決まりを読みこんだ・外した (audit_policy: apparmor="STATUS")
fn audit_status(op: &str, name: Option<&str>, info: Option<&str>, error: i64) {
    let mut s = format!("{} operation=\"{}\"", audit_head("STATUS"), op);
    if let Some(i) = info {
        s.push_str(&format!(" info=\"{}\"", i));
        if error != 0 {
            s.push_str(&format!(" error={}", error));
        }
    }
    s.push_str(" profile=\"unconfined\"");
    if let Some(n) = name {
        s.push_str(&format!(" name={}", untrusted(n)));
    }
    s.push_str(&audit_tail());
    crate::println!("{}", s);
}

// ---- プロセスについたプロファイル (cred の apparmor。fork で受けつぐ) ----

/// プロセスのラベル: プロファイルの名前。使うたびに名前で引く (入れかえれば新しいものに、外されれば unconfined に:
/// Linux の proxy と同じ)
#[derive(Clone, PartialEq)]
pub struct Label(alloc::sync::Arc<str>);

impl Label {
    fn of(p: &Profile) -> Label {
        Label(alloc::sync::Arc::from(p.name.as_str()))
    }

    pub fn profile(&self) -> Option<Rc<Profile>> {
        find(&self.0)
    }
}

fn off() -> bool {
    crate::parts::off(crate::parts::Part::Apparmor)
}

/// いまのプロセスの決まり (unconfined なら None。外されたプロファイルや、aios.off=apparmor も None)
pub fn current_profile() -> Option<Rc<Profile>> {
    if off() {
        return None;
    }
    let l = crate::cred::current().apparmor.as_ref()?;
    l.profile().filter(|p| p.mode != Mode::Unconfined)
}

/// 決まりのついたプロセスか (重いこと (inode を引くなど) をする前に。大きなロックの中で)
#[inline]
pub fn confined() -> bool {
    crate::cred::current().apparmor.is_some() && current_profile().is_some()
}

/// ラベルの見せ方 (/proc/PID/attr/current、aa_getprocattr): "name (mode)\n"、unconfined は "unconfined\n"
pub fn label_text(l: Option<&Label>) -> String {
    match l.and_then(|l| l.profile()).filter(|_| !off()) {
        Some(p) if p.mode != Mode::Unconfined => format!("{} ({})\n", p.name, p.mode_name()),
        Some(p) => format!("{}\n", p.name),
        None => "unconfined\n".into(),
    }
}

impl Profile {
    /// name (/ で始まるパス) のファイルの許しと、たどりついた状態 (aa_str_perms)。owner: 持ち主のファイルか
    fn file_perms(&self, start: u32, name: &[u8], owner: bool) -> (u32, Perms) {
        let Some(d) = &self.file.dfa else { return (DFA_NOMATCH, Perms::default()) };
        let st = d.match_len(start, name);
        (st, self.file.lookup(st, owner))
    }

    fn file_start(&self) -> u32 {
        self.file.start[CLASS_FILE]
    }

    /// 子のプロファイル ("この名前//name")
    fn child(&self, name: &str) -> Option<Rc<Profile>> {
        find(&format!("{}//{}", self.name, name))
    }
}

/// プロセスの fsuid (aios では euid)
fn fsuid() -> u32 {
    crate::cred::current().euid
}

/// パスの名前: 頭に /、ディレクトリなら終わりにも / (aa_path_name の PATH_IS_DIR)
pub fn path_name(full: &str, dir: bool) -> String {
    let full = full.trim_matches('/');
    if full.is_empty() {
        "/".into()
    } else if dir {
        format!("/{}/", full)
    } else {
        format!("/{}", full)
    }
}

// 操作の名前 (audit.h の OP_*)
pub const OP_OPEN: &str = "open";
pub const OP_MKDIR: &str = "mkdir";
pub const OP_MKNOD: &str = "mknod";
pub const OP_UNLINK: &str = "unlink";
pub const OP_RMDIR: &str = "rmdir";
pub const OP_SYMLINK: &str = "symlink";
pub const OP_TRUNC: &str = "truncate";
pub const OP_CHMOD: &str = "chmod";
pub const OP_CHOWN: &str = "chown";
pub const OP_GETATTR: &str = "getattr";
pub const OP_FLOCK: &str = "file_lock";
pub const OP_FMMAP: &str = "file_mmap";
const OP_EXEC: &str = "exec";
const OP_LINK: &str = "link";
const OP_CHANGE_PROFILE: &str = "change_profile";
const OP_CHANGE_ONEXEC: &str = "change_onexec";
const OP_CHANGE_HAT: &str = "change_hat";

/// ファイルの決まり (common_perm と aa_path_perm): full は頭の / のないパス、dir はディレクトリか、ouid は持ち主
pub fn path_perm(op: &str, full: &str, request: u32, dir: bool, ouid: u32) -> Result<(), i64> {
    let Some(p) = current_profile() else { return Ok(()) };
    let name = path_name(full, dir);
    let (_, perms) = p.file_perms(p.file_start(), name.as_bytes(), fsuid() == ouid);
    let e = if request & !perms.allow != 0 { -EACCES } else { 0 };
    audit_file(&p, &perms, op, request, &name, None, ouid, None, e)
}

/// rename (apparmor_path_rename): 前の名前は読んで消せること、新しい名前は作って書けること
pub fn rename_perm(ofull: &str, nfull: &str, dir: bool, ouid: u32) -> Result<(), i64> {
    path_perm("rename_src", ofull, MAY_READ | MAY_GETATTR | MAY_WRITE | MAY_SETATTR | MAY_DELETE, dir, ouid)?;
    path_perm("rename_dest", nfull, MAY_WRITE | MAY_SETATTR | MAY_CREATE, dir, ouid)
}

/// open (apparmor_file_open と aa_map_file_to_perms): 開くときの flags から要る許し
pub fn open_perm(full: &str, flags: u32, dir: bool, ouid: u32) -> Result<(), i64> {
    const O_ACCMODE: u32 = 3;
    const O_CREAT: u32 = 0o100;
    const O_TRUNC: u32 = 0o1000;
    const O_APPEND: u32 = 0o2000;
    let mut r = match flags & O_ACCMODE {
        0 => MAY_READ,
        1 => MAY_WRITE,
        _ => MAY_READ | MAY_WRITE,
    };
    if flags & O_APPEND != 0 && r & MAY_WRITE != 0 {
        r = (r & !MAY_WRITE) | MAY_APPEND;
    }
    if flags & O_TRUNC != 0 {
        r |= MAY_WRITE;
    }
    if flags & O_CREAT != 0 {
        r |= MAY_CREATE;
    }
    path_perm(OP_OPEN, full, r, dir, ouid)
}

/// mmap と mprotect の PROT_* から要る許し (common_mmap: MAP_PRIVATE の書きこみはファイルに戻らないので要らない)
pub fn mmap_perm(op: &str, full: &str, prot: u64, private: bool, ouid: u32) -> Result<(), i64> {
    let mut r = 0;
    if prot & 1 != 0 {
        r |= MAY_READ;
    }
    if prot & 2 != 0 && !private {
        r |= MAY_WRITE;
    }
    if prot & 4 != 0 {
        r |= EXEC_MMAP;
    }
    // 開いたときに許されたもの (読む・書く) のほかを頼むときだけ見なおす (aa_file_perm): 実行して写すとき
    if r & EXEC_MMAP == 0 {
        return Ok(());
    }
    path_perm(op, full, r, false, ouid)
}

/// link (profile_path_link): 新しい名前に l、ペアの古い名前にも l。subset なら、新しい名前の許しは古い名前の許しの中
pub fn link_perm(lfull: &str, tfull: &str, ouid: u32) -> Result<(), i64> {
    let Some(p) = current_profile() else { return Ok(()) };
    let (lname, tname) = (path_name(lfull, false), path_name(tfull, false));
    let owner = fsuid() == ouid;
    let mut request = MAY_LINK;
    let mut info = None;
    let (st, mut lperms) = p.file_perms(p.file_start(), lname.as_bytes(), owner);
    let mut error = -EACCES;
    'audit: {
        if lperms.allow & MAY_LINK == 0 {
            break 'audit;
        }
        // 古い名前とペアにできるか (link\0target)
        let st = p.file.dfa.as_ref().map_or(DFA_NOMATCH, |d| d.step(st, 0));
        let (_, perms) = p.file_perms(st, tname.as_bytes(), owner);
        lperms.audit = perms.audit;
        lperms.quiet = perms.quiet;
        lperms.kill = perms.kill;
        if perms.allow & MAY_LINK == 0 {
            info = Some("target restricted");
            lperms = perms;
            break 'audit;
        }
        if perms.allow & LINK_SUBSET != 0 {
            let (_, perms) = p.file_perms(p.file_start(), tname.as_bytes(), owner);
            request = lperms.allow & !MAY_LINK;
            lperms.allow &= perms.allow | MAY_LINK;
            request |= AUDIT_FILE_MASK & (lperms.allow & !perms.allow);
            if request & !lperms.allow != 0 {
                break 'audit;
            }
            let subset = |l: u32, t: u32| (l & !X_UNSAFE) == (t & !X_UNSAFE) && (l & X_UNSAFE == 0 || t & X_UNSAFE != 0);
            if lperms.allow & MAY_EXEC != 0 && !subset(lperms.xindex, perms.xindex) {
                lperms.allow &= !MAY_EXEC;
                request |= MAY_EXEC;
                info = Some("link not subset of target");
                break 'audit;
            }
        }
        error = 0;
    }
    audit_file(&p, &lperms, OP_LINK, request, &lname, Some(&tname), ouid, info, error)
}

/// 記録にのせるファイルの許し (AA_AUDIT_FILE_MASK)
const AUDIT_FILE_MASK: u32 = MAY_READ | MAY_WRITE | MAY_EXEC | MAY_APPEND | MAY_CREATE | MAY_DELETE | MAY_GETATTR | MAY_SETATTR | MAY_CHMOD | MAY_CHOWN | MAY_LOCK | EXEC_MMAP | MAY_LINK;

/// 許しの文字 (aa_file_perm_chrs と map_mask_to_chr_mask): x w r a c d ... k m . l
fn mask_chrs(mask: u32) -> String {
    const CHRS: &[u8] = b"xwracd         km l     ";
    let mut m = mask & (MAY_READ | MAY_WRITE | MAY_CREATE | MAY_DELETE | MAY_LINK | MAY_LOCK | MAY_EXEC | EXEC_MMAP | MAY_APPEND);
    if mask & MAY_GETATTR != 0 {
        m |= MAY_READ;
    }
    if mask & (MAY_SETATTR | MAY_CHMOD | MAY_CHOWN) != 0 {
        m |= MAY_WRITE;
    }
    CHRS.iter().enumerate().filter(|&(i, &c)| m & (1 << i) != 0 && c != b' ').map(|(_, &c)| c as char).collect()
}

/// ファイルの決まりの記録と答え (aa_audit_file と aa_audit)。complain なら許して ALLOWED、enforce なら DENIED。
/// 明かに deny した (quiet の) ものは、complain でも止めて記録しない。kill なら profile の signal を送る
#[allow(clippy::too_many_arguments)]
fn audit_file(p: &Profile, perms: &Perms, op: &str, request: u32, name: &str, target: Option<&str>, ouid: u32, info: Option<&str>, error: i64) -> Result<(), i64> {
    let mut req = request;
    let kind = if error == 0 {
        let mask = if p.audit_all { 0xffff } else { perms.audit };
        req &= mask;
        if req == 0 {
            return Ok(());
        }
        "AUDIT"
    } else {
        req &= !perms.allow;
        let kill = req & perms.kill != 0;
        if req & perms.quiet != 0 && !p.audit_all {
            req &= !perms.quiet;
        }
        if req == 0 {
            return Err(error);
        }
        if kill || p.mode == Mode::Kill {
            "KILLED"
        } else if p.mode == Mode::Complain {
            "ALLOWED"
        } else {
            "DENIED"
        }
    };
    let denied = req & !perms.allow;
    let mut s = format!("{} operation=\"{}\" class=\"file\"", audit_head(kind), op);
    if let Some(i) = info {
        s.push_str(&format!(" info=\"{}\"", i));
        if error != 0 {
            s.push_str(&format!(" error={}", error));
        }
    }
    s.push_str(&format!(" profile={}", untrusted(&p.name)));
    if !name.is_empty() {
        s.push_str(&format!(" name={}", untrusted(name)));
    }
    s.push_str(&audit_tail());
    if req & AUDIT_FILE_MASK != 0 {
        s.push_str(&format!(" requested_mask=\"{}\"", mask_chrs(req)));
    }
    if denied & AUDIT_FILE_MASK != 0 {
        s.push_str(&format!(" denied_mask=\"{}\"", mask_chrs(denied)));
    }
    if req & AUDIT_FILE_MASK != 0 {
        s.push_str(&format!(" fsuid={} ouid={}", fsuid(), ouid));
    }
    if let Some(t) = target {
        s.push_str(&format!(" target={}", untrusted(t)));
    }
    crate::println!("{}", s);
    match kind {
        "KILLED" => {
            let me = crate::proc::current();
            crate::signal::send_thread(me, p.signal as i32, crate::signal::SigInfo::from(crate::signal::SI_KERNEL));
            Err(error)
        }
        // complain: 止めない (complain_error)
        "ALLOWED" if error == -EACCES || error == -1 => Ok(()),
        _ if error == 0 => Ok(()),
        _ => Err(error),
    }
}

// ---- capability (capability.c) ----

/// 同じプロセスの同じ capability の記録は 1 秒に 1 度 (audit_caps の audit_cache): (pid, capability ごとの次に記録できる時刻)
static CAP_CACHE: Bkl<(u32, [u64; 41])> = Bkl::new((0, [0; 41]));

/// c (root) が cap を使ってよいか (aa_capable の古い形: プロファイルの caps.allow)。だめなら DENIED を記録して false
/// (complain なら許して ALLOWED)。audit が false なら記録しない (noaudit。complain だけ "optional: no audit" で記録)
pub fn capable(c: &crate::cred::Cred, cap: u32, audit: bool) -> bool {
    if off() || c.apparmor.is_none() {
        return true;
    }
    let Some(p) = c.apparmor.as_ref().and_then(|l| l.profile()).filter(|p| p.mode != Mode::Unconfined) else { return true };
    let bit = 1u64 << cap.min(63);
    let error = if p.caps.allow & bit != 0 { 0 } else { -EPERM };
    let mut info = None;
    if !audit {
        if p.mode != Mode::Complain {
            return error == 0;
        }
        info = Some("optional: no audit");
    }
    let kind = if error == 0 {
        if !p.audit_all && p.caps.audit & bit == 0 {
            return true;
        }
        "AUDIT"
    } else if p.mode == Mode::Kill {
        "KILLED"
    } else if p.caps.quiet & bit != 0 && !p.audit_all {
        // quiet: 記録しない (complain でも止める: Linux と同じ)
        return false;
    } else if p.mode == Mode::Complain {
        "ALLOWED"
    } else {
        "DENIED"
    };
    let allowed = error == 0 || kind == "ALLOWED";
    // 同じものを 1 秒のうちにもう一度なら記録しない
    let pid = crate::proc::try_current().map_or(0, |p| p.pid);
    let now = crate::timer::uptime_ns();
    let cache = CAP_CACHE.get();
    let i = (cap as usize).min(40);
    if cache.0 == pid && now <= cache.1[i] {
        return allowed;
    }
    if cache.0 != pid {
        *cache = (pid, [0; 41]);
    }
    cache.1[i] = now + 1_000_000_000;
    let mut s = format!("{} operation=\"capable\" class=\"cap\"", audit_head(kind));
    if let Some(i) = info {
        s.push_str(&format!(" info=\"{}\"", i));
        if error != 0 {
            s.push_str(&format!(" error={}", error));
        }
    }
    s.push_str(&format!(" profile={}", untrusted(&p.name)));
    s.push_str(&audit_tail());
    let name = crate::cred::CAP_NAMES.get(cap as usize).copied().unwrap_or("?");
    s.push_str(&format!(" capability={}  capname={}", cap, untrusted(name)));
    crate::println!("{}", s);
    if kind == "KILLED" {
        let me = crate::proc::current();
        crate::signal::send_thread(me, p.signal as i32, crate::signal::SigInfo::from(crate::signal::SI_KERNEL));
    }
    allowed
}

/// capget に見せる capability (apparmor_capget: プロファイルのある root は、その caps.allow だけ)
pub fn caps_of(c: &crate::cred::Cred) -> u64 {
    match c.apparmor.as_ref().and_then(|l| l.profile()).filter(|p| p.mode != Mode::Unconfined && !off()) {
        Some(p) if p.mode != Mode::Complain => p.caps.allow,
        _ => u64::MAX,
    }
}

// ---- exec で次のプロファイル (domain.c) ----

/// exec の答え: 次のラベル (None は unconfined) と、AT_SECURE にするか (環境を消す Px など)。fatal は、新しい
/// プロファイルでプログラムを写せない (exec_map_ok): exec のあと SIGSEGV
#[derive(Clone, Default)]
pub struct Exec {
    pub label: Option<Label>,
    pub secure: bool,
    pub fatal: bool,
}

/// exec で写すプログラム (と ld.so) を、新しいプロファイルで r と m が許されるか。Linux は、開いたとき (前の
/// ラベル) とラベルがちがえば binfmt_elf の mmap で見なおし (aa_file_perm)、だめなら戻れないところの先で SIGSEGV。
/// 同じラベル (ix)、行き先が unconfined、前が unconfined (開いたファイルはわたされたもの: __file_is_delegated) は
/// 見ない。だめなら記録して false
pub fn exec_map_ok(e: &Exec, full: &str, ouid: u32) -> bool {
    if off() || e.label == crate::cred::current().apparmor || current_profile().is_none() {
        return true;
    }
    let Some(p) = e.label.as_ref().and_then(|l| l.profile()).filter(|p| p.mode != Mode::Unconfined) else { return true };
    let name = path_name(full, false);
    let request = MAY_READ | EXEC_MMAP;
    let (_, perms) = p.file_perms(p.file_start(), name.as_bytes(), fsuid() == ouid);
    let error = if request & !perms.allow != 0 { -EACCES } else { 0 };
    audit_file(&p, &perms, OP_FMMAP, request, &name, None, ouid, None, error).is_ok()
}

/// is_loop: たどった状態の終わり 32 のうちに同じものがあれば、そこからは同じことのくりかえし
struct Wb {
    history: [u32; 32],
    pos: usize,
    len: usize,
}

/// aa_dfa_leftmatch: 名前をたどり、ループに入るまでの文字数 (どれだけ文字どおりに合ったか。長いほうを選ぶ)
fn leftmatch(d: &Dfa, start: u32, name: &[u8]) -> (u32, u32) {
    let mut wb = Wb { history: [0; 32], pos: 0, len: 0 };
    let mut st = start;
    let mut count: u32 = 0;
    if st == DFA_NOMATCH {
        return (DFA_NOMATCH, 0);
    }
    for (i, &c) in name.iter().enumerate() {
        wb.history[wb.pos] = st;
        st = d.step(st, c);
        // is_loop
        let mut adjust = None;
        if wb.history[wb.pos] >= st {
            let mut pos = wb.pos;
            for k in 0..wb.len {
                if wb.history[pos] == st {
                    adjust = Some(k as u32);
                    break;
                }
                pos = (pos + 31) & 31;
            }
        }
        if let Some(a) = adjust {
            st = d.match_len(st, &name[i + 1..]);
            count = count.saturating_sub(a);
            break;
        }
        wb.pos = (wb.pos + 1) & 31;
        wb.len = (wb.len + 1).min(32);
        count += 1;
    }
    if st == DFA_NOMATCH {
        count = 0;
    }
    (st, count)
}

/// find_attach: name (動かすプログラムのパス) につくプロファイル。xmatch (attach) のあるものはいちばん長く文字どおりに
/// 合ったもの (同じ長さが 2 つあればどれにもつけない)、ないものは名前がそのパスのもの。children なら parent の子から
fn find_attach(name: &str, parent: Option<&Profile>) -> (Option<Rc<Profile>>, Option<&'static str>) {
    let ns = NS.get();
    let mut cand: Option<Rc<Profile>> = None;
    let (mut cand_len, mut conflict) = (0u32, false);
    // Linux はリストの頭に足すので、新しいものから
    for p in ns.profiles.iter().rev() {
        let base = match parent {
            Some(par) => match p.name.strip_prefix(par.name.as_str()).and_then(|r| r.strip_prefix("//")) {
                Some(b) if !b.contains("//") => b,
                _ => continue,
            },
            None if p.name.contains("//") => continue,
            None => p.name.as_str(),
        };
        if let Some(d) = &p.xmatch.dfa {
            let (st, count) = leftmatch(d, p.xmatch.start[CLASS_XMATCH], name.as_bytes());
            if p.xmatch.lookup(st, true).allow & MAY_EXEC == 0 || count < cand_len {
                continue;
            }
            // xattr の条件は見られない (aios には拡張属性がない): 合わないものとする
            if !p.xattrs.is_empty() {
                continue;
            }
            // 同じ長さならどちらにもつけない (Linux と同じく、はじめのものが 0 文字でも)
            if count == cand_len {
                conflict = true;
                continue;
            }
            cand_len = count.max(p.xmatch_len);
            cand = Some(p.clone());
            conflict = false;
        } else if base == name {
            return (Some(p.clone()), None);
        }
    }
    if conflict {
        return (None, Some("conflicting profile attachments"));
    }
    (cand, None)
}

/// x_to_label: exec の許しの xindex から次のプロファイル。(次, 名前, info)。None の None は unconfined
fn x_to_label(p: &Rc<Profile>, name: &str, xindex: u32) -> (Option<Option<Label>>, Option<String>, Option<&'static str>) {
    let mut new: Option<Option<Label>> = None;
    let mut target = None;
    let mut info = None;
    match xindex & X_TYPE_MASK {
        X_TABLE => {
            let t = p.file.trans.get((xindex & X_INDEX_MASK) as usize).cloned().unwrap_or_default();
            let lookup = t.strip_prefix('&').unwrap_or(&t);
            let found = if xindex & X_CHILD != 0 { p.child(lookup) } else { find(lookup) };
            new = found.map(|f| Some(Label::of(&f)));
            target = Some(t);
        }
        X_NAME => {
            let (f, i) = find_attach(name, (xindex & X_CHILD != 0).then_some(&**p));
            info = i;
            new = f.map(|f| Some(Label::of(&f)));
            target = Some(name.into());
        }
        _ => {}
    }
    if new.is_none() {
        if xindex & X_INHERIT != 0 {
            info = Some(if info.is_some() { "conflicting profile attachments - ix fallback" } else { "ix fallback" });
            new = Some(Some(Label::of(p)));
        } else if xindex & X_UNCONFINED != 0 {
            info = Some(if info.is_some() { "conflicting profile attachments - ux fallback" } else { "ux fallback" });
            new = Some(None);
        }
    }
    (new, target, info)
}

/// 次のラベルの名前 (記録の target)
fn label_name(l: &Option<Label>) -> String {
    l.as_ref().map_or("unconfined".into(), |l| String::from(&*l.0))
}

/// exec (apparmor_bprm_creds_for_exec): full (頭の / のないパス) を動かすときの次のラベル。onexec が先
pub fn exec_transition(full: &str, ouid: u32) -> Result<Exec, i64> {
    let c = crate::cred::current();
    if off() {
        return Ok(Exec { label: c.apparmor.clone(), ..Default::default() });
    }
    let name = path_name(full, false);
    let cur = c.apparmor.as_ref().and_then(|l| l.profile()).filter(|p| p.mode != Mode::Unconfined);
    let mut secure = false;
    let new = if let Some(on) = c.aa_onexec.clone() {
        // change_onexec: 決まりがついていれば、このプログラムで onexec が許され、その組で行き先が許されること
        if let Some(p) = &cur {
            let (st, mut perms) = p.file_perms(p.file_start(), name.as_bytes(), fsuid() == ouid);
            let mut error = -EACCES;
            let mut info = Some("change_profile onexec");
            if perms.allow & MAY_ONEXEC == 0 {
                info = Some("no change_onexec valid for executable");
            } else {
                let st = p.file.dfa.as_ref().map_or(DFA_NOMATCH, |d| d.step(st, 0));
                match change_perms(p, st, &on.0, MAY_ONEXEC) {
                    Ok(_) => {
                        error = 0;
                        if perms.xindex & X_UNSAFE == 0 {
                            secure = true;
                        }
                    }
                    Err(np) => {
                        perms = np;
                        perms.allow &= !MAY_ONEXEC;
                    }
                }
            }
            audit_file(p, &perms, OP_EXEC, MAY_ONEXEC, &name, Some(&on.0), ouid, info.filter(|_| error != 0), error)?;
        }
        Some(on)
    } else {
        match &cur {
            // unconfined: パスでつくプロファイルがあればそれ
            None => find_attach(&name, None).0.map(|p| Label::of(&p)),
            Some(p) => {
                let (_, mut perms) = p.file_perms(p.file_start(), name.as_bytes(), fsuid() == ouid);
                let mut error = 0;
                let mut info = None;
                let mut target = None;
                let mut new = None;
                if perms.allow & MAY_EXEC != 0 {
                    let (n, t, i) = x_to_label(p, &name, perms.xindex);
                    target = t;
                    info = i;
                    match n {
                        Some(n) => {
                            // ix fallback (同じプロファイル) は AT_SECURE にしない
                            if !(n.as_ref() == Some(&Label::of(p)) && info.is_some()) && perms.xindex & X_UNSAFE == 0 {
                                secure = true;
                            }
                            new = Some(n);
                        }
                        None => {
                            info = Some("profile transition not found");
                            perms.allow &= !MAY_EXEC;
                            error = -EACCES;
                        }
                    }
                } else {
                    error = -EACCES;
                }
                // complain: Linux は学ぶための空のプロファイルに移る。ここではいまのプロファイルのまま (記録は ALLOWED)
                // 記録の target は、次のラベルがあればその名前、なければさがした名前
                let t = new.as_ref().map(label_name).or(target);
                audit_file(p, &perms, OP_EXEC, MAY_EXEC, &name, t.as_deref(), ouid, info.filter(|_| error != 0), error)?;
                new.unwrap_or_else(|| Some(Label::of(p)))
            }
        }
    };
    // no_new_privs: 決まりのついたプロセスは、同じプロファイルのほかへは移れない (Linux の subset と同じく、ほかの
    // プロファイルは「せまくなる」とは言えない)
    if c.no_new_privs && cur.is_some() && new.as_ref().map(|l| &*l.0) != cur.as_ref().map(|p| p.name.as_str()) {
        if let Some(p) = &cur {
            audit_file(p, &Perms::default(), OP_EXEC, MAY_EXEC, &name, Some(&label_name(&new)), ouid, Some("no new privs"), -EPERM)?;
        }
        return Err(-EPERM);
    }
    Ok(Exec { label: new, secure, fatal: false })
}

/// change_profile_perms: p のファイルの決まりで、start から target の名前をたどって request が許されるか
fn change_perms(p: &Profile, start: u32, target: &str, request: u32) -> Result<Perms, Perms> {
    let Some(d) = &p.file.dfa else { return Err(Perms::default()) };
    let st = d.match_len(start, target.as_bytes());
    if st == DFA_NOMATCH {
        return Err(Perms::default());
    }
    let mut tmp = p.file.lookup(st, fsuid() == 0);
    p.apply_modes(&mut tmp);
    let mut perms = ALLPERMS;
    perms.accum_raw(&tmp);
    if perms.allow & request != request {
        return Err(perms);
    }
    Ok(perms)
}

/// aa_change_profile: name のプロファイルにいま移る (onexec なら次の exec で)。/proc/self/attr/current の
/// "changeprofile NAME" と attr/exec の "exec NAME"
pub fn change_profile(name: &str, onexec: bool) -> Result<(), i64> {
    let (op, request) = if onexec { (OP_CHANGE_ONEXEC, MAY_ONEXEC) } else { (OP_CHANGE_PROFILE, MAY_CHANGE_PROFILE) };
    if name.is_empty() || name.starts_with('&') || name.starts_with(':') {
        return Err(-EINVAL);
    }
    let cur = current_profile();
    let Some(target) = find(name) else {
        if let Some(p) = &cur {
            audit_file(p, &Perms::default(), op, request, name, None, 0, Some("label not found"), -ENOENT)?;
        }
        return Err(-ENOENT);
    };
    if let Some(p) = &cur {
        let r = change_perms(p, p.file_start(), &target.name, request);
        let (perms, error) = match r {
            Ok(perms) => (perms, 0),
            Err(perms) => (perms, -EACCES),
        };
        audit_file(p, &perms, op, request, name, Some(&target.name), 0, None, error)?;
    }
    let c = &mut crate::proc::current().cred;
    if onexec {
        c.aa_onexec = Some(Label::of(&target));
        return Ok(());
    }
    // no_new_privs: 決まりのついたプロセスはほかのプロファイルへは移れない (記録しない: Linux と同じ)
    if c.no_new_privs && cur.as_ref().is_some_and(|p| p.name != target.name) {
        return Err(-EPERM);
    }
    c.apparmor = Some(Label::of(&target));
    Ok(())
}

// ---- change_hat (domain.c と task.c) ----

/// p の子 (階層の名前 "p//name": aa_find_child)
fn find_child(p: &Profile, name: &str) -> Option<Rc<Profile>> {
    find(&format!("{}//{}", p.name, name))
}

/// p に子 (hat か子のプロファイル) があるか
fn has_children(p: &Profile) -> bool {
    let pre = format!("{}//", p.name);
    NS.get().profiles.iter().any(|q| q.name.starts_with(&pre))
}

/// change_hat のしくじり: hat をいくつもためしたときか complain のときだけ記録する (ひとつだけさがすのは、あるかどうかを
/// ためすのにも使うので: Linux と同じ)
fn hat_fail(p: &Profile, count: usize, name: &str, info: &str, error: i64) -> Result<(), i64> {
    if count > 1 || p.mode == Mode::Complain {
        let _ = audit_file(p, &Perms::default(), OP_CHANGE_HAT, MAY_CHANGEHAT, name, None, 0, Some(info), error);
    }
    Err(error)
}

/// token がちがう: 当てずっぽうをふせぐため、記録してプロファイルの signal (SIGKILL) で止める
fn hat_kill(p: &Profile, target: &str) -> Result<(), i64> {
    let perms = Perms { kill: MAY_CHANGEHAT, ..Perms::default() };
    audit_file(p, &perms, OP_CHANGE_HAT, MAY_CHANGEHAT, "", Some(target), 0, Some("failed token match"), -EACCES)
}

/// aa_change_hat: hats のはじめにあるものに入る (いまのプロファイルの子の hat。hat の中からは兄弟の hat へ)。入るときに
/// token を覚え、hat から hat へ移るのも、hats なしで前のプロファイルに帰るのも、同じ token のときだけ。test (permhat) なら
/// 入れるかを見るだけ
fn change_hat(hats: &[&str], token: u64, test: bool) -> Result<(), i64> {
    let Some(p) = current_profile() else {
        // 決まりのないプロセスには入る hat がない (Linux も EPERM と記録)
        let mut s = format!("{} operation=\"{}\" class=\"file\" info=\"unconfined can not change_hat\" error={} profile={}", audit_head("DENIED"), OP_CHANGE_HAT, -EPERM, untrusted("unconfined"));
        s.push_str(&audit_tail());
        crate::println!("{}", s);
        return Err(-EPERM);
    };
    let c = &mut crate::proc::current().cred;
    if hats.is_empty() {
        // 帰る。前がなければ何もしない
        let Some(prev) = c.aa_prev.clone().filter(|_| !test) else { return Ok(()) };
        // no_new_privs: ほかのプロファイルへはもどれない (記録しない)
        if c.no_new_privs {
            return Err(-EPERM);
        }
        if c.aa_token != token {
            return hat_kill(&p, &prev.0);
        }
        c.apparmor = Some(prev);
        (c.aa_prev, c.aa_onexec, c.aa_token) = (None, None, 0);
        return Ok(());
    }
    let root = if p.is_hat() { p.name.rsplit_once("//").and_then(|(up, _)| find(up)) } else { Some(p.clone()) };
    let mut found = None;
    for &name in hats {
        match root.as_ref().and_then(|r| find_child(r, name)) {
            // complain なら、ないものにも入ったことにする (下で)
            None if p.mode != Mode::Complain => continue,
            Some(h) if !h.is_hat() => return hat_fail(&p, hats.len(), name, "target not hat", -EPERM),
            h => {
                found = Some((name, h));
                break;
            }
        }
    }
    let Some((name, hat)) = found else {
        let (info, error) = if has_children(&p) { ("hat not found", -ENOENT) } else { ("no hats defined", -ECHILD) };
        return hat_fail(&p, hats.len(), "", info, error);
    };
    // complain で hat がないとき、Linux は学ぶための空の hat を作って入る。aios は記録して ENOENT
    let error = if hat.is_some() { 0 } else { -ENOENT };
    audit_file(&p, &Perms::default(), OP_CHANGE_HAT, MAY_CHANGEHAT, name, hat.as_ref().map(|h| h.name.as_str()), 0, None, error)?;
    let Some(hat) = hat else { return Err(-ENOENT) };
    if c.no_new_privs {
        return Err(-EPERM);
    }
    if test {
        return Ok(());
    }
    // aa_set_current_hat: はじめて入るなら前のプロファイルと token を覚える
    match c.aa_prev {
        None => (c.aa_prev, c.aa_token) = (c.apparmor.clone(), token),
        Some(_) if c.aa_token == token => {}
        Some(_) => return hat_kill(&p, &hat.name),
    }
    c.apparmor = Some(Label::of(&hat));
    c.aa_onexec = None;
    Ok(())
}

/// "changehat TOKEN^HAT" の TOKEN^HAT (split_token_from_name と aa_setprocattr_changehat)。TOKEN は 16 進 (0x はあっても
/// よい)。HAT のあとに \0 で区切ってほかの hat を続けられる (more: 1 つめの文字列のあとの残り。終わりの \0 は 1 つ落として
/// ある)。HAT がなければ帰る (token が 0 なら EINVAL)
fn set_hat(arg: &str, more: Option<&[u8]>, test: bool) -> Result<(), i64> {
    let digits = arg.strip_prefix("0x").or_else(|| arg.strip_prefix("0X")).unwrap_or(arg);
    let n = digits.bytes().take_while(u8::is_ascii_hexdigit).count();
    let token = digits.bytes().take(n).fold(0u64, |t, d| t.wrapping_mul(16).wrapping_add((d as char).to_digit(16).unwrap_or(0) as u64));
    let Some(first) = digits[n..].strip_prefix('^').filter(|_| n > 0 || digits.len() != arg.len()) else { return Err(-EINVAL) };
    let mut hats = Vec::new();
    if !first.is_empty() {
        hats.push(first);
        for h in more.into_iter().flat_map(|m| m.split(|&b| b == 0)) {
            hats.push(core::str::from_utf8(h).unwrap_or(""));
        }
        hats.truncate(16);
    }
    if hats.is_empty() && token == 0 {
        return Err(-EINVAL);
    }
    change_hat(&hats, token, test)
}

/// /proc/PID/attr/{current,exec,prev} (と attr/apparmor/ の同じもの) を読む。ないもの (exec を決めていない、hat の中で
/// ないときの prev) は EINVAL
pub fn attr_read(c: &crate::cred::Cred, which: &str) -> Result<String, i64> {
    match which {
        "current" => Ok(label_text(c.apparmor.as_ref())),
        "exec" if c.aa_onexec.is_some() => Ok(label_text(c.aa_onexec.as_ref())),
        "prev" if c.aa_prev.is_some() => Ok(label_text(c.aa_prev.as_ref())),
        _ => Err(-EINVAL),
    }
}

/// /proc/self/attr/{current,exec} に書く (do_setattr): "changehat TOKEN^HAT"、"permhat TOKEN^HAT"、"changeprofile NAME"、
/// "permprofile NAME"、"exec NAME"
pub fn attr_write(which: &str, b: &[u8]) -> Result<usize, i64> {
    if b.is_empty() {
        return Err(-EINVAL);
    }
    // 1 つめの文字列 (\0 まで)。changehat の hat は、そのあとにも \0 で区切って続けられる
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    let s = String::from_utf8_lossy(&b[..end]);
    let s = s.trim();
    let (cmd, arg) = s.split_once(' ').ok_or(-EINVAL)?;
    let arg = arg.trim_start();
    if arg.is_empty() {
        return Err(-EINVAL);
    }
    match (which, cmd) {
        ("current", "changehat" | "permhat") => {
            let more = b.get(end + 1..).filter(|r| !r.is_empty()).map(|r| r.strip_suffix(&[0]).unwrap_or(r));
            set_hat(arg, more, cmd == "permhat")?
        }
        ("current", "changeprofile") => change_profile(arg, false)?,
        ("current", "permprofile") => {
            // ためすだけ (AA_CHANGE_TEST): 許されるかだけ見る
            let cur = current_profile();
            let t = find(arg).ok_or(-ENOENT)?;
            if let Some(p) = cur {
                change_perms(&p, p.file_start(), &t.name, MAY_CHANGE_PROFILE).map_err(|_| -EACCES)?;
            }
        }
        ("exec", "exec") => change_profile(arg, true)?,
        _ => {
            let me = current_profile().map_or("unconfined".into(), |p| p.name.clone());
            let mut s = format!("{} operation=\"setprocattr\" info=\"{}\" error=-22 profile={}", audit_head("DENIED"), which, untrusted(&me));
            s.push_str(&audit_tail());
            crate::println!("{}", s);
            return Err(-EINVAL);
        }
    }
    Ok(b.len())
}

// ---- ネットワーク (net.c、af_unix.c、af_inet.c の network_v8: ソケットの (family, type, protocol) を net の DFA で) ----

const CLASS_NET: usize = 14;
const CLASS_NETV9: usize = 15;

/// 記録の family (include/linux/socket.h の AF_ の順。AppArmor の address_family_names)
const AF_NAMES: [&str; 46] = [
    "unspec", "unix", "inet", "ax25", "ipx", "appletalk", "netrom", "bridge", "atmpvc", "x25", "inet6", "rose", "", "netbeui", "security", "key", "netlink", "packet", "ash", "econet",
    "atmsvc", "rds", "sna", "irda", "pppox", "wanpipe", "llc", "ib", "mpls", "can", "tipc", "bluetooth", "iucv", "rxrpc", "isdn", "phonet", "ieee802154", "caif", "alg", "nfc", "vsock",
    "kcm", "qipcrtr", "smc", "xdp", "mctp",
];
/// features の network_v8/af_mask (AF_NAMES の名前のあるもの)
pub const AF_MASK: &str = "unspec unix inet ax25 ipx appletalk netrom bridge atmpvc x25 inet6 rose netbeui security key netlink packet ash econet atmsvc rds sna irda pppox wanpipe llc ib mpls can tipc bluetooth iucv rxrpc isdn phonet ieee802154 caif alg nfc vsock kcm qipcrtr smc xdp mctp";
/// 記録の sock_type (sock_type_names)
const SOCK_TYPE_NAMES: [&str; 11] = ["", "stream", "dgram", "raw", "rdm", "seqpacket", "dccp", "", "", "", "packet"];
/// 記録の requested / denied (net_mask_names: ビットの順)
const NET_MASK_NAMES: [&str; 32] = [
    "unknown", "send", "receive", "unknown", "create", "shutdown", "connect", "unknown", "setattr", "getattr", "setcred", "getcred", "chmod", "chown", "chgrp", "lock",
    "mmap", "mprot", "unknown", "unknown", "accept", "bind", "listen", "unknown", "setopt", "getopt", "unknown", "unknown", "unknown", "unknown", "unknown", "unknown",
];

/// ソケットの記録に足すもの (aa_audit の audit_net_cb と dump_common_audit_data のうち、ソケットや番地によるもの)。
/// pre は comm のあと (sk の番地: inet の laddr / faddr、unix の path)、post は denied のあと (unix の addr= と peer_addr=)
#[derive(Default)]
pub struct NetAudit {
    pub pre: String,
    pub post: String,
}

/// aa_check_perms: 許しと記録。log は (種類, request, denied, error) で 1 行を出す
fn check_perms(perms: &Perms, request: u32, log: &dyn Fn(&str, u32, u32, i64)) -> Result<(), i64> {
    let mut denied = request & (!perms.allow | perms.deny);
    let mut req = request;
    let error = if denied == 0 {
        req &= perms.audit;
        if req == 0 {
            return Ok(());
        }
        0
    } else {
        let e = if denied == denied & perms.hide { -ENOENT } else { -EACCES };
        denied &= !perms.quiet;
        if denied == 0 {
            return Err(e);
        }
        e
    };
    // aa_select_audit_type
    let kind = if denied == 0 {
        "AUDIT"
    } else if denied & perms.kill != 0 {
        "KILLED"
    } else if denied == denied & perms.complain {
        "ALLOWED"
    } else {
        "DENIED"
    };
    log(kind, req, denied, error);
    if error == 0 || kind == "ALLOWED" { Ok(()) } else { Err(error) }
}

/// aa_audit_perm_mask: 名前を空白で区切って引用符で
fn net_mask(m: u32) -> String {
    let v: Vec<&str> = (0..32).filter(|i| m & NET_PERMS_MASK & (1 << i) != 0).map(|i| NET_MASK_NAMES[i]).collect();
    format!("\"{}\"", v.join(" "))
}

/// プロファイルが net を見るか (RULE_MEDIATES_NET: v9 がなければ v7/v8 の種類)
fn net_start(p: &Profile) -> u32 {
    let v9 = p.policy.start[CLASS_NETV9];
    if v9 != DFA_NOMATCH { v9 } else { p.policy.start[CLASS_NET] }
}

/// aa_profile_af_perm: p の net の決まりで (family, type, protocol) に request が許されるか (aa_match_to_prot と
/// aa_do_perms)。だめなら class="net" で記録
#[allow(clippy::too_many_arguments)]
fn af_perm(p: &Profile, op: &str, request: u32, family: u32, typ: u32, protocol: u32, extra: &dyn Fn() -> NetAudit) -> Result<(), i64> {
    let Some(d) = &p.policy.dfa else { return Ok(()) };
    let mut st = net_start(p);
    if st == DFA_NOMATCH {
        return Ok(());
    }
    let be = |v: u32| (v as u16).to_be_bytes();
    let mut info = None;
    st = d.match_len(st, &be(family));
    if st == DFA_NOMATCH {
        info = Some("failed af match");
    } else {
        st = d.match_len(st, &be(typ));
        if st == DFA_NOMATCH {
            info = Some("failed type match");
        } else {
            // early_match: type までで request がみなあるか、先がなければ protocol は見ない
            let e = p.policy.lookup(st, true);
            if e.allow & request != request && e.allow & CONT_MATCH != 0 {
                st = d.match_len(st, &be(protocol));
                if st == DFA_NOMATCH {
                    info = Some("failed protocol match");
                }
            }
        }
    }
    let mut perms = p.policy.lookup(st, true);
    p.apply_modes(&mut perms);
    check_perms(&perms, request, &|kind, req, denied, error| {
        let x = extra();
        let mut s = format!("{} operation=\"{}\" class=\"net\"", audit_head(kind), op);
        if let Some(i) = info {
            s.push_str(&format!(" info=\"{}\"", i));
            if error != 0 {
                s.push_str(&format!(" error={}", error));
            }
        }
        s.push_str(&format!(" profile={}", untrusted(&p.name)));
        s.push_str(&audit_tail());
        s.push_str(&x.pre);
        match AF_NAMES.get(family as usize).filter(|n| !n.is_empty()) {
            Some(n) => s.push_str(&format!(" family=\"{}\"", n)),
            None => s.push_str(&format!(" family=\"unknown({})\"", family)),
        }
        match SOCK_TYPE_NAMES.get(typ as usize).filter(|n| !n.is_empty()) {
            Some(n) => s.push_str(&format!(" sock_type=\"{}\"", n)),
            None => s.push_str(&format!(" sock_type=\"unknown({})\"", typ)),
        }
        s.push_str(&format!(" protocol={}", protocol as i32));
        if req & NET_PERMS_MASK != 0 {
            s.push_str(&format!(" requested={}", net_mask(req)));
            if denied & NET_PERMS_MASK != 0 {
                s.push_str(&format!(" denied={}", net_mask(denied)));
            }
        }
        s.push_str(&x.post);
        crate::println!("{}", s);
    })
}

/// いまのプロセス (決まりがあれば) のソケットの決まり。決まりのないものは見ない
pub fn net_perm(op: &str, request: u32, family: u32, typ: u32, protocol: u32, extra: &dyn Fn() -> NetAudit) -> Result<(), i64> {
    match current_profile() {
        Some(p) => af_perm(&p, op, request, family, typ, protocol, extra),
        None => Ok(()),
    }
}

/// label (ソケットの持ち主のラベル。None は unconfined) の決まりで (AF_UNIX の相手の側を見る: aa_unix_peer_perm)
pub fn label_net_perm(label: Option<&Label>, op: &str, request: u32, family: u32, typ: u32, protocol: u32, extra: &dyn Fn() -> NetAudit) -> Result<(), i64> {
    match label.and_then(|l| l.profile()).filter(|p| p.mode != Mode::Unconfined && !off()) {
        Some(p) => af_perm(&p, op, request, family, typ, protocol, extra),
        None => Ok(()),
    }
}

/// いまのプロセスに決まりがあり、net を見るか (af_inet.c の label_mediates(label, AA_CLASS_NET): 番地の確かめもこのときだけ)
pub fn net_mediated() -> bool {
    current_profile().is_some_and(|p| net_start(&p) != DFA_NOMATCH)
}

/// いまのプロセスのラベル (ソケットに持たせる: apparmor_socket_post_create)。決まりがなければ None
pub fn current_label() -> Option<Label> {
    if off() {
        return None;
    }
    crate::cred::current().apparmor.clone()
}

/// AF_UNIX の番地の記録 (audit_unix_addr): なし、パス、"@抽象名" (制御文字があれば 16 進)
pub fn unix_addr_audit(key: &str, a: Option<&[u8]>) -> String {
    let path = a.filter(|a| a.len() > 2).map(|a| &a[2..]);
    match path {
        None => format!(" {}=none", key),
        Some(p) if p[0] != 0 => {
            let end = p.iter().position(|&c| c == 0).unwrap_or(p.len());
            format!(" {}={}", key, untrusted(&String::from_utf8_lossy(&p[..end])))
        }
        Some(p) => {
            let name = &p[1..];
            if name.iter().any(|&c| c == b'"' || !(0x21..=0x7e).contains(&c)) {
                format!(" {}=\"@{}\"", key, name.iter().map(|c| format!("{:02X}", c)).collect::<String>())
            } else {
                format!(" {}=\"@{}\"", key, String::from_utf8_lossy(name))
            }
        }
    }
}

/// AF_INET の番地の記録 (print_ipv4_addr: 0 なら出さない)。a は sockaddr_in のバイト
pub fn inet_addr_audit(a: Option<&[u8]>, k_addr: &str, k_port: &str) -> String {
    let Some(a) = a.filter(|a| a.len() >= 8) else { return String::new() };
    let port = u16::from_be_bytes([a[2], a[3]]);
    let mut s = String::new();
    if a[4..8] != [0, 0, 0, 0] {
        s.push_str(&format!(" {}={}.{}.{}.{}", k_addr, a[4], a[5], a[6], a[7]));
    }
    if port != 0 {
        s.push_str(&format!(" {}={}", k_port, port));
    }
    s
}

// ---- 相手のラベル (label.c の aa_label_match。ラベルはいまはプロファイル 1 つ) ----

/// aa_perms_accum
fn perms_accum(a: &mut Perms, b: &Perms) {
    a.deny |= b.deny;
    a.allow &= b.allow & !a.deny;
    a.audit |= b.audit & a.allow;
    a.quiet &= b.quiet & !a.allow;
    a.kill |= b.kill & !a.allow;
    a.complain |= b.complain & !a.allow & !a.deny;
    a.cond |= b.cond & !a.allow & !a.deny;
    a.hide &= b.hide & !a.allow;
    a.prompt |= b.prompt & !a.allow & !a.deny;
    a.subtree |= b.subtree & !a.deny;
}

/// aa_label_match: state から相手のラベルの名前 peer をたどった許し (label_compound_match で足りなければ
/// label_components_match: どちらも 1 つのプロファイルでは同じ状態)。たどれなければ何も許さない
fn label_match(p: &Profile, state: u32, peer: &str, request: u32) -> Perms {
    let Some(d) = &p.policy.dfa else { return Perms::default() };
    let st = d.match_len(state, peer.as_bytes());
    if st == DFA_NOMATCH {
        return Perms::default();
    }
    let mut perms = p.policy.lookup(st, true);
    p.apply_modes(&mut perms);
    if perms.allow & request == request {
        return perms;
    }
    let mut all = ALLPERMS;
    perms_accum(&mut all, &perms);
    all
}

/// ラベルの名前 (決まりのないもの、外されたものは unconfined) と、決まりのあるプロファイル
fn label_profile(l: Option<&Label>) -> (String, Option<Rc<Profile>>) {
    match l.and_then(|l| l.profile()).filter(|p| p.mode != Mode::Unconfined && !off()) {
        Some(p) => (p.name.clone(), Some(p)),
        None => ("unconfined".into(), None),
    }
}

/// xcheck: 両方見て、後のしくじりのほうを
fn xcheck(a: Result<(), i64>, b: Result<(), i64>) -> Result<(), i64> {
    if b.is_err() { b } else { a }
}

// ---- シグナル (ipc.c) ----

const CLASS_PTRACE: usize = 9;
const CLASS_SIGNAL: usize = 10;
const CLASS_LABEL: usize = 16;
/// features の signal/mask (AA_SFS_SIG_MASK)
pub const SIG_MASK: &str = "hup int quit ill trap abrt bus fpe kill usr1 segv usr2 pipe alrm term stkflt chld cont stop stp ttin ttou urg xcpu xfsz vtalrm prof winch io pwr sys emt lost";
/// 決まりの番号の名前 (sig_names: 番号はシグナルの番号のまま、0 は 35 の exists、リアルタイムは 128 から)
const SIG_NAMES: [&str; 36] = [
    "unknown", "hup", "int", "quit", "ill", "trap", "abrt", "bus", "fpe", "kill", "usr1", "segv", "usr2", "pipe", "alrm", "term", "stkflt", "chld", "cont", "stop", "stp", "ttin",
    "ttou", "urg", "xcpu", "xfsz", "vtalrm", "prof", "winch", "io", "pwr", "sys", "emt", "lost", "unused", "exists",
];

/// map_signal_num (arm64: 1-31 はそのまま、0 は exists、32-64 は 128 から、ほかは unknown)
fn map_signal(sig: i32) -> u32 {
    match sig {
        0 => 35,
        1..=31 => sig as u32,
        32..=64 => (sig - 32 + 128) as u32,
        _ => 0,
    }
}

/// profile_signal_perm: p の決まりで、相手のラベル peer へ request (送る MAY_WRITE、受ける MAY_READ)
fn signal_perm(p: &Profile, peer: &str, request: u32, sig: i32) -> Result<(), i64> {
    let (Some(d), st) = (&p.policy.dfa, p.policy.start[CLASS_SIGNAL]) else { return Ok(()) };
    if st == DFA_NOMATCH {
        return Ok(());
    }
    let mapped = map_signal(sig);
    let st = d.step(st, mapped as u8);
    let mut perms = label_match(p, st, peer, request);
    p.apply_modes(&mut perms);
    let mask = |m: u32| if m & MAY_READ != 0 { "receive" } else if m & MAY_WRITE != 0 { "send" } else { "" };
    check_perms(&perms, request, &|kind, req, denied, _| {
        let mut s = format!("{} operation=\"signal\" class=\"signal\" profile={}", audit_head(kind), untrusted(&p.name));
        s.push_str(&audit_tail());
        if req & (MAY_READ | MAY_WRITE) != 0 {
            s.push_str(&format!(" requested_mask=\"{}\"", mask(req)));
            if denied & (MAY_READ | MAY_WRITE) != 0 {
                s.push_str(&format!(" denied_mask=\"{}\"", mask(denied)));
            }
        }
        // Linux の audit_signal_cb は unknown の前に空白を入れない
        match mapped {
            0 => s.push_str(&format!("signal=unknown({})", sig)),
            m if (m as usize) < SIG_NAMES.len() => s.push_str(&format!(" signal={}", SIG_NAMES[m as usize])),
            m => s.push_str(&format!(" signal=rtmin+{}", m - 128)),
        }
        s.push_str(&format!(" peer={}", untrusted(peer)));
        crate::println!("{}", s);
    })
}

/// aa_may_signal: いまのプロセスから target (資格情報) へ sig を送れるか。送る側の決まりで相手に send、受ける側の
/// 決まりで送る側から receive (両方見る)。だめなら EACCES
pub fn may_signal(target: &crate::cred::Cred, sig: i32) -> Result<(), i64> {
    if off() {
        return Ok(());
    }
    let me = crate::cred::current();
    if me.apparmor.is_none() && target.apparmor.is_none() {
        return Ok(());
    }
    let (sname, sp) = label_profile(me.apparmor.as_ref());
    let (tname, tp) = label_profile(target.apparmor.as_ref());
    let a = sp.map_or(Ok(()), |p| signal_perm(&p, &tname, MAY_WRITE, sig));
    let b = tp.map_or(Ok(()), |p| signal_perm(&p, &sname, MAY_READ, sig));
    xcheck(a, b)
}

// ---- ptrace (task.c。aios に ptrace はないので、/proc/PID の「追える人だけ」のもの: ptrace_may_access) ----

/// features の ptrace/mask
pub const PTRACE_MASK: &str = "read trace";

/// profile_ptrace_perm: p の決まりで相手のラベル peer へ request (aa_profile_match_label: LABEL の種類の中の PTRACE)
fn ptrace_perm(p: &Profile, peer: &str, request: u32) -> Result<(), i64> {
    let st = match &p.policy.dfa {
        Some(d) => d.step(p.policy.start[CLASS_LABEL], CLASS_PTRACE as u8),
        None => DFA_NOMATCH,
    };
    let mut perms = if st == DFA_NOMATCH { Perms::default() } else { label_match(p, st, peer, request) };
    p.apply_modes(&mut perms);
    check_perms(&perms, request, &|kind, req, denied, _| ptrace_audit(p, kind, req, denied, peer, None))
}

/// ptrace の記録 (audit_ptrace_cb: 1 つだけのビットの名前)
fn ptrace_audit(p: &Profile, kind: &str, req: u32, denied: u32, peer: &str, error: Option<i64>) {
    let mask = |m: u32| match m {
        MAY_READ => "read",
        MAY_WRITE => "trace",
        MAY_CREATE => "readby",
        MAY_APPEND => "tracedby",
        _ => "",
    };
    let all = MAY_READ | MAY_WRITE | MAY_CREATE | MAY_APPEND;
    let mut s = format!("{} operation=\"ptrace\" class=\"ptrace\"", audit_head(kind));
    if let Some(e) = error {
        s.push_str(&format!(" error={}", e));
    }
    s.push_str(&format!(" profile={}", untrusted(&p.name)));
    s.push_str(&audit_tail());
    if req & all != 0 {
        s.push_str(&format!(" requested_mask=\"{}\"", mask(req)));
        if denied & all != 0 {
            s.push_str(&format!(" denied_mask=\"{}\"", mask(denied)));
        }
    }
    s.push_str(&format!(" peer={}", untrusted(peer)));
    crate::println!("{}", s);
}

/// aa_may_ptrace: いまのプロセス (追う人) が tracee (資格情報) を見る (read) か追う (trace) か。追う人の決まりで相手に
/// read / trace、追われる人の決まりで追う人から readby / tracedby (両方見る)。ptrace の決まりのないプロファイルは
/// ほかのラベルを CAP_SYS_PTRACE がなければ追えない (古い形)
pub fn may_ptrace(tracee: &crate::cred::Cred, read: bool) -> Result<(), i64> {
    if off() {
        return Ok(());
    }
    let me = crate::cred::current();
    if me.apparmor.is_none() && tracee.apparmor.is_none() {
        return Ok(());
    }
    let request = if read { MAY_READ } else { MAY_WRITE };
    let (mname, mp) = label_profile(me.apparmor.as_ref());
    let (tname, tp) = label_profile(tracee.apparmor.as_ref());
    let a = match &mp {
        None => Ok(()),
        Some(p) if p.policy.start[CLASS_PTRACE] != DFA_NOMATCH => ptrace_perm(p, &tname, request),
        Some(_) if mname == tname => Ok(()),
        Some(p) => {
            if capable(me, crate::cred::CAP_SYS_PTRACE, true) {
                Ok(())
            } else {
                ptrace_audit(p, if p.mode == Mode::Complain { "ALLOWED" } else { "DENIED" }, 0, 0, &tname, Some(-EPERM));
                if p.mode == Mode::Complain { Ok(()) } else { Err(-EPERM) }
            }
        }
    };
    let b = match (&tp, &mp) {
        (Some(t), Some(_)) if t.policy.start[CLASS_PTRACE] != DFA_NOMATCH => ptrace_perm(t, &mname, request << 2),
        _ => Ok(()),
    };
    xcheck(a, b)
}

// ---- mount (mount.c: マウント先、元、fs の種類、フラグ、データの順に DFA をたどる) ----

const CLASS_MOUNT: usize = 7;
const MAY_MOUNT: u32 = 0x2;
const MAY_UMOUNT: u32 = 0x4;
/// フラグのあとにデータも見る (AA_MNT_CONT_MATCH)。audit なら記録にデータも (AA_AUDIT_DATA)
const MNT_CONT_MATCH: u32 = 0x40;
/// features の mount/mask
pub const MOUNT_MASK: &str = "mount umount pivot_root";
const MNT_INFO: [&str; 7] = ["match succeeded", "failed mntpnt match", "failed srcname match", "failed type match", "failed flags match", "failed data match", "failed perms check"];

/// 記録の flags= (audit_mnt_flags)
fn mnt_flags_text(f: u64) -> String {
    const REC: u64 = 1 << 14;
    let rec = f & REC != 0;
    let mut v = alloc::vec![if f & 1 != 0 { "ro" } else { "rw" }];
    let named: [(u64, &str, &str); 21] = [
        (2, "nosuid", ""), (4, "nodev", ""), (8, "noexec", ""), (16, "sync", ""), (32, "remount", ""), (64, "mand", ""), (128, "dirsync", ""),
        (256, "nosymfollow", ""), (1024, "noatime", ""), (2048, "nodiratime", ""), (1 << 12, "bind", "rbind"), (1 << 13, "move", ""), (1 << 15, "silent", ""),
        (1 << 16, "acl", ""), (1 << 17, "unbindable", "runbindable"), (1 << 18, "private", "rprivate"), (1 << 19, "slave", "rslave"),
        (1 << 20, "shared", "rshared"), (1 << 21, "relatime", ""), (1 << 23, "iversion", ""), (1 << 24, "strictatime", ""),
    ];
    for (bit, name, rname) in named {
        if f & bit != 0 {
            v.push(if rec && !rname.is_empty() { rname } else { name });
        }
    }
    if f & (1 << 31) != 0 {
        v.push("nouser");
    }
    v.join(", ")
}

/// mount の記録 (audit_cb)
#[allow(clippy::too_many_arguments)]
fn mount_audit(p: &Profile, kind: &str, op: &str, info: Option<&str>, error: i64, name: Option<&str>, fstype: Option<&str>, src: Option<&str>, flags: u64, data: Option<&str>) {
    let mut s = format!("{} operation=\"{}\" class=\"mount\"", audit_head(kind), op);
    if let Some(i) = info {
        s.push_str(&format!(" info=\"{}\"", i));
        if error != 0 {
            s.push_str(&format!(" error={}", error));
        }
    }
    s.push_str(&format!(" profile={}", untrusted(&p.name)));
    if let Some(n) = name {
        s.push_str(&format!(" name={}", untrusted(n)));
    }
    s.push_str(&audit_tail());
    if let Some(t) = fstype {
        s.push_str(&format!(" fstype={}", untrusted(t)));
    }
    if let Some(d) = src {
        s.push_str(&format!(" srcname={}", untrusted(d)));
    }
    if flags != 0 {
        s.push_str(&format!(" flags=\"{}\"", mnt_flags_text(flags)));
    }
    if let Some(d) = data {
        s.push_str(&format!(" options={}", untrusted(d)));
    }
    crate::println!("{}", s);
}

/// do_match_mnt: (どこでしくじったか (0 は合った), 許し)
fn match_mnt(d: &Dfa, start: u32, pdb: &Pdb, mntpnt: &str, dev: Option<&str>, fstype: Option<&str>, flags: u64, data: Option<&str>) -> (usize, Perms) {
    let mut st = d.step(d.match_len(start, mntpnt.as_bytes()), 0);
    if st == DFA_NOMATCH {
        return (1, Perms::default());
    }
    if let Some(v) = dev {
        st = d.match_len(st, v.as_bytes());
    }
    st = d.step(st, 0);
    if st == DFA_NOMATCH {
        return (2, Perms::default());
    }
    if let Some(t) = fstype {
        st = d.match_len(st, t.as_bytes());
    }
    st = d.step(st, 0);
    if st == DFA_NOMATCH {
        return (3, Perms::default());
    }
    for i in 0..32 {
        if flags & (1 << i) != 0 {
            st = d.step(st, i as u8 + 1);
        }
    }
    if st == DFA_NOMATCH {
        return (4, Perms::default());
    }
    let mut perms = pdb.lookup(st, true);
    if perms.allow & MAY_MOUNT != 0 {
        return (0, perms);
    }
    if let Some(v) = data.filter(|_| perms.allow & MNT_CONT_MATCH != 0) {
        st = d.step(st, 0);
        if st == DFA_NOMATCH {
            return (4, perms);
        }
        st = d.match_len(st, v.as_bytes());
        if st == DFA_NOMATCH {
            return (5, perms);
        }
        perms = pdb.lookup(st, true);
        if perms.allow & MAY_MOUNT != 0 {
            return (0, perms);
        }
    }
    (6, perms)
}

/// いまのプロセスの決まりで mount (match_mnt_path_str)。mntpnt と dev はパスの名前 (ディレクトリは / で終わる) か、新しい
/// マウントの元の文字列
pub fn mount_perm(mntpnt: &str, dev: Option<&str>, fstype: Option<&str>, flags: u64, data: Option<&str>) -> Result<(), i64> {
    let Some(p) = current_profile() else { return Ok(()) };
    let Some(d) = &p.policy.dfa else { return Ok(()) };
    let start = p.policy.start[CLASS_MOUNT];
    if start == DFA_NOMATCH {
        return Ok(());
    }
    let (pos, mut perms) = match_mnt(d, start, &p.policy, mntpnt, dev, fstype, flags, data);
    let info = (pos != 0).then_some(MNT_INFO[pos]);
    p.apply_modes(&mut perms);
    let shown = data.filter(|_| perms.audit & MNT_CONT_MATCH != 0);
    check_perms(&perms, MAY_MOUNT, &|kind, _, _, error| mount_audit(&p, kind, "mount", info, error, Some(mntpnt), fstype, dev, flags, shown))
}

/// mount の元を引けない (aa_audit_perm_error: 決まりのあるプロセスは DENIED を記録してそのエラー)
pub fn mount_error(flags: u64, error: i64) -> i64 {
    if let Some(p) = current_profile() {
        mount_audit(&p, "DENIED", "mount", None, error, None, None, None, flags, None);
    }
    error
}

/// いまのプロセスの決まりで umount (profile_umount: マウントの根のパスの名前)
pub fn umount_perm(name: &str) -> Result<(), i64> {
    let Some(p) = current_profile() else { return Ok(()) };
    let Some(d) = &p.policy.dfa else { return Ok(()) };
    let start = p.policy.start[CLASS_MOUNT];
    if start == DFA_NOMATCH {
        return Ok(());
    }
    let mut perms = p.policy.lookup(d.match_len(start, name.as_bytes()), true);
    p.apply_modes(&mut perms);
    check_perms(&perms, MAY_UMOUNT, &|kind, _, _, error| mount_audit(&p, kind, "umount", None, error, Some(name), None, None, 0, None))
}

// ---- LSM のシステムコール (Linux 6.8 から: security/lsm_syscalls.c と security.c。libapparmor は /proc/self/attr より先に使う) ----

const LSM_ID_CAPABILITY: u64 = 100;
const LSM_ID_APPARMOR: u64 = 104;
const LSM_ID_LANDLOCK: u64 = 110;
const LSM_ATTR_CURRENT: u32 = 100;
const LSM_ATTR_EXEC: u32 = 101;
const LSM_ATTR_PREV: u32 = 104;
const LSM_FLAG_SINGLE: u32 = 1;
const E2BIG: i64 = 7;
const EFAULT: i64 = 14;
/// struct lsm_ctx の頭 (id、flags、len、ctx_len。どれも u64)
const LSM_CTX: usize = 32;

fn get_user(va: usize, b: &mut [u8]) -> Result<(), i64> {
    crate::proc::current().pt().copy_in(b, va).ok_or(-EFAULT)
}

fn put_user(va: usize, b: &[u8]) -> Result<(), i64> {
    crate::proc::current().pt().copy_out(va, b).ok_or(-EFAULT)
}

fn get_u32(va: usize) -> Result<u32, i64> {
    let mut b = [0u8; 4];
    get_user(va, &mut b)?;
    Ok(u32::from_le_bytes(b))
}

/// lsm_list_modules(ids, size, flags): 動いている LSM の番号 (/sys/kernel/security/lsm と同じ順)
pub fn lsm_list_modules(ids: usize, size: usize, flags: u32) -> Result<i64, i64> {
    if flags != 0 {
        return Err(-EINVAL);
    }
    let have = get_u32(size)?;
    let mut v = alloc::vec![LSM_ID_CAPABILITY];
    if !crate::parts::off(crate::parts::Part::Landlock) {
        v.push(LSM_ID_LANDLOCK);
    }
    if !off() {
        v.push(LSM_ID_APPARMOR);
    }
    let total = (v.len() * 8) as u32;
    put_user(size, &total.to_le_bytes())?;
    if have < total {
        return Err(-E2BIG);
    }
    for (i, id) in v.iter().enumerate() {
        put_user(ids + i * 8, &id.to_le_bytes())?;
    }
    Ok(v.len() as i64)
}

/// lsm_get_self_attr(attr, ctx, size, flags): いまのプロセスの current / exec / prev を struct lsm_ctx で (答えるのは
/// AppArmor だけ: 中身は /proc/self/attr と同じ「名前 (モード)」の改行なし)。ctx が 0 か size が足りなければ、要る大きさを
/// size に書く (足りなければ E2BIG)。flags の LSM_FLAG_SINGLE なら ctx の id の LSM だけ。答えた LSM の数を返す
pub fn lsm_get_self_attr(attr: u32, ctx: usize, size: usize, flags: u32) -> Result<i64, i64> {
    if attr == 0 || size == 0 {
        return Err(-EINVAL);
    }
    let left = get_u32(size)?;
    let mut single = None;
    if flags != 0 {
        if flags != LSM_FLAG_SINGLE || ctx == 0 {
            return Err(-EINVAL);
        }
        let mut b = [0u8; 8];
        get_user(ctx, &mut b)?;
        let id = u64::from_le_bytes(b);
        if id == 0 {
            return Err(-EINVAL);
        }
        single = Some(id);
    }
    let (mut total, mut count, mut toobig) = (0u32, 0i64, false);
    if !off() && single.is_none_or(|id| id == LSM_ID_APPARMOR) {
        let c = crate::cred::current();
        // 決まりのないものは unconfined。exec と prev は、なければ ENOENT (apparmor_getselfattr)
        let label = match attr {
            LSM_ATTR_CURRENT => Some(c.apparmor.as_ref()),
            LSM_ATTR_EXEC => Some(Some(c.aa_onexec.as_ref().ok_or(-ENOENT)?)),
            LSM_ATTR_PREV => Some(Some(c.aa_prev.as_ref().ok_or(-ENOENT)?)),
            _ => None,
        };
        if let Some(l) = label {
            let text = label_text(l);
            let v = text.trim_end_matches('\n').as_bytes();
            // lsm_fill_user_ctx: 頭と中身を 8 バイトにそろえて
            let len = (LSM_CTX + v.len()).div_ceil(8) * 8;
            if len as u32 > left {
                toobig = true;
            } else {
                if ctx != 0 {
                    let mut b = alloc::vec![0u8; len];
                    for (i, x) in [LSM_ID_APPARMOR, 0, len as u64, v.len() as u64].iter().enumerate() {
                        b[i * 8..i * 8 + 8].copy_from_slice(&x.to_le_bytes());
                    }
                    b[LSM_CTX..LSM_CTX + v.len()].copy_from_slice(v);
                    put_user(ctx, &b)?;
                }
                count += 1;
            }
            total += len as u32;
        }
    }
    put_user(size, &total.to_le_bytes())?;
    if toobig {
        return Err(-E2BIG);
    }
    if count == 0 {
        return Err(-EOPNOTSUPP);
    }
    Ok(count)
}

/// lsm_set_self_attr(attr, ctx, size, flags): ctx の id の LSM に current か exec を書く (AppArmor なら
/// /proc/self/attr に書くのと同じ "changeprofile NAME" など)
pub fn lsm_set_self_attr(attr: u32, ctx: usize, size: u32, flags: u32) -> Result<i64, i64> {
    if flags != 0 || (size as usize) < LSM_CTX {
        return Err(-EINVAL);
    }
    if size > 4096 {
        return Err(-E2BIG);
    }
    let mut b = alloc::vec![0u8; size as usize];
    get_user(ctx, &mut b)?;
    let field = |i: usize| u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
    let (id, len, ctx_len) = (field(0), field(2), field(3));
    let need = (LSM_CTX as u64).checked_add(ctx_len);
    if (size as u64) < len || need.is_none_or(|n| len < n) {
        return Err(-EINVAL);
    }
    if id != LSM_ID_APPARMOR || off() {
        return Err(-EOPNOTSUPP);
    }
    let which = match attr {
        LSM_ATTR_CURRENT => "current",
        LSM_ATTR_EXEC => "exec",
        _ => return Err(-EOPNOTSUPP),
    };
    attr_write(which, &b[LSM_CTX..LSM_CTX + ctx_len as usize])?;
    Ok(0)
}

// ---- features (apparmor_parser が読んで、それに合う形のバイナリを作る) ----

/// 名のること (パス, 中身)。ディレクトリはパスの頭から。中身には改行を足して見せる
pub const FEATURES: &[(&str, &str)] = &[
    ("caps/mask", "chown dac_override dac_read_search fowner fsetid kill setgid setuid setpcap linux_immutable net_bind_service net_broadcast net_admin net_raw ipc_lock ipc_owner sys_module sys_rawio sys_chroot sys_ptrace sys_pacct sys_admin sys_boot sys_nice sys_resource sys_time sys_tty_config mknod lease audit_write audit_control setfcap mac_override mac_admin syslog wake_alarm block_suspend audit_read perfmon bpf checkpoint_restore"),
    ("domain/change_hat", "yes"),
    ("domain/change_hatv", "yes"),
    ("domain/change_onexec", "yes"),
    ("domain/change_profile", "yes"),
    ("domain/version", "1.2"),
    ("file/mask", "create read write exec append mmap_exec link lock"),
    ("network_v8/af_mask", AF_MASK),
    ("network_v8/tcp-fast-open", "yes"),
    ("policy/set_load", "yes"),
    ("policy/versions/v5", "yes"),
    ("policy/versions/v6", "yes"),
    ("policy/versions/v7", "yes"),
    ("mount/mask", MOUNT_MASK),
    ("ptrace/mask", PTRACE_MASK),
    ("signal/mask", SIG_MASK),
];


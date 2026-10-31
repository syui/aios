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
    let mut p = Profile { name, rename, attach, xmatch, xmatch_len, disconnected, signal, flags, mode, audit_all: audit != 0, path_flags, caps, xattrs, rlimits, policy, file, data };
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
}

static NS: Bkl<Ns> = Bkl::new(Ns { profiles: Vec::new(), revision: 1 });

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
    for p in new {
        let old = ns.profiles.iter().position(|o| o.name == p.name);
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
pub struct Label(Rc<str>);

impl Label {
    fn of(p: &Profile) -> Label {
        Label(Rc::from(p.name.as_str()))
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
fn path_name(full: &str, dir: bool) -> String {
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
    ("policy/set_load", "yes"),
    ("policy/versions/v5", "yes"),
    ("policy/versions/v6", "yes"),
    ("policy/versions/v7", "yes"),
];


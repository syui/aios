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

const ENOENT: i64 = 2;
const EACCES: i64 = 13;
const EEXIST: i64 = 17;
const EINVAL: i64 = 22;
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
pub const MAY_RENAME: u32 = 0x80;
pub const MAY_SETATTR: u32 = 0x100;
pub const MAY_GETATTR: u32 = 0x200;
pub const MAY_CHMOD: u32 = 0x1000;
pub const MAY_CHOWN: u32 = 0x2000;
pub const MAY_LOCK: u32 = 0x8000;
pub const EXEC_MMAP: u32 = 0x1_0000;
pub const MAY_LINK: u32 = 0x4_0000;
pub const MAY_ONEXEC: u32 = 0x2000_0000;
pub const MAY_CHANGE_PROFILE: u32 = 0x4000_0000;
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

pub const FLAG_HAT: u32 = 1;

pub struct Caps {
    pub allow: u64,
    pub audit: u64,
    pub quiet: u64,
    pub extended: u64,
}

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

    /// 文字列 s を、この決まりの種類 (s の 1 文字め) で問う (.access の profile_query_cb)。ファイルの持ち主は uid 0 と
    /// して (Linux も cond は 0): 問う人が root なら持ち主の許し (owner の決まり) を見る
    fn query(&self, s: &[u8], perms: &mut Perms) {
        let owner = crate::proc::current().cred.euid == 0;
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

/// 何か読みこんであるか (なければ、決まりを見るところはみなとばせる)
pub fn any() -> bool {
    !NS.get().profiles.is_empty()
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

/// 決まりを変えてよいか (aa_may_manage_policy: CAP_MAC_ADMIN。aios では namespace の外の root)
fn may_manage() -> Result<(), i64> {
    let c = &crate::proc::current().cred;
    if c.euid != 0 || c.ns.user.is_some() {
        audit_status("profile_manage", None, Some("not policy admin"), -EACCES);
        return Err(-EACCES);
    }
    Ok(())
}

/// /sys/kernel/security/apparmor/profiles: 1 行に「名前 (モード)」。新しく読んだものから、子は親のあと
pub fn profiles_text() -> Result<String, i64> {
    if crate::proc::current().cred.euid != 0 {
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
    let p = crate::proc::current();
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


// audit のルール (Linux の kernel/auditfilter.c と、auditsc.c のフィルタ): auditctl -a / -d / -l が送る
// struct audit_rule_data (1040 バイトのあとに文字) を読んでたしかめ、組 (list) ごとに覚える。
//
// - 組: USER (0: ユーザーの記録を捨てるか)、TASK (1: fork したスレッドに入れものを作るか)、EXIT (4: システムコールの
//   出口で記録するか)、EXCLUDE (5: その種類の記録を出さないか)、FS (6)、URING_EXIT (7)。ENTRY (2) は古いので EINVAL
// - action は NEVER (0) か ALWAYS (2)。欄は 64 まで、比べかたは = != < > <= >= & &=。bit の比べかたは引数などだけ、
//   MSGTYPE は USER と EXCLUDE だけ。値の決まり (LOGINUID_SET は 0 か 1、PERM は 15 まで、FILETYPE は S_IFMT の
//   ビットだけ、FIELD_COMPARE は 25 まで、SADDR_FAM は AF_MAX より下) も見る (Linux は見ない: aios がえらんだちがい
//   audit-field-value)。LOGINUID = 4294967295 は LOGINUID_SET = 0 に (古い形。一覧ではもとの形で返す)
// - 文字 (key、WATCH、DIR、EXE、LSM のラベル) はうしろの buf から順に。key は 256 まで。WATCH は / で始まり / で
//   終わらない、EXIT の組だけ、= だけ (親のディレクトリがなければ足すときに ENOENT)。DIR は / で始まる (なければ
//   ENOENT)。EXE は / で始まり / で終わらない (親がなければ ENOENT)
// - マスクのいちばん上の 16 ビットは組の印 (クラス: dir_write、chattr、read、write、signal) で、arm64 のシステムコールに
//   ひろげる
// - 同じルール (組、flags、action、欄、マスク) は EEXIST、ないものを消すと ENOENT。PREPEND (0x10) は前に入れて、
//   覚えるときは消す (なので PREPEND をつけたままでは消せない: Linux と同じ)。EXIT の組は先に合ったものから
// - 名前で合わせるもの (WATCH、INODE): システムコールで集めた名前 (audit.rs の PATH) ごとに (audit_filter_inodes)。
//   WATCH はそのパスにいまある inode (なければ前に見たもの: 消した・動かしたシステムコールでも合う。Linux は消える前に
//   合わせてから watch を外す)。DEVMAJOR、DEVMINOR、OBJ_UID、OBJ_GID、FILETYPE もシステムコールの名前で
// - DIR (auditctl -w DIR: Linux の audit_tree): たどって引いた名前 (NORMAL と PARENT) のどれかが、木の根かその下に
//   あれば合う (Linux は木の根とその下のマウントの根に印をつけ、名前の親をたどって印を探す)。作る・消すもの (CREATE と
//   DELETE) は、それ自身が木の根のときだけ
// - のこり: LSM のラベル (AppArmor の SUBJ_ROLE) はまだ合わない
use crate::cell::Bkl;
use crate::proc::Proc;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering::Relaxed};

const EINVAL: i64 = 22;
const ENOENT: i64 = 2;
const EEXIST: i64 = 17;
const ENOTDIR: i64 = 20;
const ENAMETOOLONG: i64 = 36;

pub const USER: u32 = 0;
pub const TASK: u32 = 1;
pub const EXIT: u32 = 4;
pub const EXCLUDE: u32 = 5;
const FS: u32 = 6;
const URING_EXIT: u32 = 7;
const PREPEND: u32 = 0x10;
pub const NEVER: u32 = 0;
const POSSIBLE: u32 = 1;
pub const ALWAYS: u32 = 2;

/// struct audit_rule_data の頭の大きさ
pub const DATA: usize = 1040;
const MAX_FIELDS: usize = 64;
const MAX_KEY_LEN: u32 = 256;
const PATH_MAX: u32 = 4096;
const UNSET: u32 = u32::MAX;

// 欄の種類 (include/uapi/linux/audit.h)
const F_PID: u32 = 0;
const F_UID: u32 = 1;
const F_EUID: u32 = 2;
const F_SUID: u32 = 3;
const F_FSUID: u32 = 4;
const F_GID: u32 = 5;
const F_EGID: u32 = 6;
const F_SGID: u32 = 7;
const F_FSGID: u32 = 8;
const F_LOGINUID: u32 = 9;
const F_PERS: u32 = 10;
const F_ARCH: u32 = 11;
const F_MSGTYPE: u32 = 12;
const F_SUBJ_USER: u32 = 13;
const F_SUBJ_CLR: u32 = 17;
const F_PPID: u32 = 18;
const F_OBJ_USER: u32 = 19;
const F_OBJ_LEV_HIGH: u32 = 23;
const F_LOGINUID_SET: u32 = 24;
const F_SESSIONID: u32 = 25;
const F_FSTYPE: u32 = 26;
const F_DEVMAJOR: u32 = 100;
const F_DEVMINOR: u32 = 101;
const F_INODE: u32 = 102;
const F_EXIT: u32 = 103;
const F_SUCCESS: u32 = 104;
const F_WATCH: u32 = 105;
const F_PERM: u32 = 106;
const F_DIR: u32 = 107;
const F_FILETYPE: u32 = 108;
const F_OBJ_UID: u32 = 109;
const F_OBJ_GID: u32 = 110;
const F_FIELD_COMPARE: u32 = 111;
const F_EXE: u32 = 112;
const F_SADDR_FAM: u32 = 113;
const F_ARG0: u32 = 200;
const F_ARG3: u32 = 203;
const F_FILTERKEY: u32 = 210;

const PERM_EXEC: u32 = 1;
const PERM_WRITE: u32 = 2;
const PERM_READ: u32 = 4;
const PERM_ATTR: u32 = 8;

/// 比べかた
#[derive(Clone, Copy, PartialEq)]
pub enum Op {
    Eq,
    Ne,
    BitMask,
    BitTest,
    Lt,
    Gt,
    Le,
    Ge,
}

impl Op {
    fn from(v: u32) -> Option<Op> {
        Some(match v {
            0x4000_0000 => Op::Eq,
            0x3000_0000 => Op::Ne,
            0x0800_0000 => Op::BitMask,
            0x4800_0000 => Op::BitTest,
            0x1000_0000 => Op::Lt,
            0x2000_0000 => Op::Gt,
            0x5000_0000 => Op::Le,
            0x6000_0000 => Op::Ge,
            _ => return None,
        })
    }

    fn code(self) -> u32 {
        match self {
            Op::Eq => 0x4000_0000,
            Op::Ne => 0x3000_0000,
            Op::BitMask => 0x0800_0000,
            Op::BitTest => 0x4800_0000,
            Op::Lt => 0x1000_0000,
            Op::Gt => 0x2000_0000,
            Op::Le => 0x5000_0000,
            Op::Ge => 0x6000_0000,
        }
    }
}

#[derive(Clone, PartialEq)]
struct Field {
    typ: u32,
    op: Op,
    val: u32,
    /// 文字の欄 (key、WATCH、DIR、EXE、LSM のラベル)
    s: Option<String>,
}

#[derive(Clone)]
pub struct Rule {
    pub listnr: u32,
    /// 送られた flags の PREPEND (覚えたあとは 0)
    flags: u32,
    /// LOGINUID = -1 で来たもの (AUDIT_LOGINUID_LEGACY)
    legacy: bool,
    pub action: u32,
    fields: Vec<Field>,
    mask: [u32; 64],
    pub key: Option<String>,
    /// EXIT の組の順 (大きいほど先に見る。ctx の prio とくらべる)
    prio: u64,
    /// 名前で合わせるもの (WATCH と、= の INODE): 出口のフィルタでは見ない (Linux は inode のハッシュの表に置く)
    by_name: bool,
    /// WATCH と DIR の道
    watch: Option<String>,
    tree: Option<String>,
    /// EXE の道と、足したときにあったもの (ファイルシステム, inode)
    exe: Option<(usize, u64)>,
    /// WATCH のファイルの、最後に見た inode (audit_watch の dev と ino)
    watch_id: Option<(usize, u64)>,
    /// DIR の木の根の本当のパス
    tree_path: Option<String>,
}

/// システムコールで集めた名前の inode (audit_names のうちルールが見るもの)
#[derive(Clone)]
pub struct NameInfo {
    pub id: (usize, u64),
    pub ino: u64,
    pub dev: (u32, u32),
    pub rdev: (u32, u32),
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    /// 本当のパス (/ から) と、たどって引いたものか (DIR のルール)
    pub path: String,
    pub walked: bool,
}

static RULES: Bkl<[Vec<Rule>; 8]> = Bkl::new([Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new()]);
/// システムコールのルールの数 (audit_n_rules: USER、EXCLUDE、FS のほか)
pub static N_RULES: AtomicU32 = AtomicU32::new(0);
/// EXIT の組の順 (prio_low は後ろに足すもの、prio_high は前に足すもの)
static PRIO: Bkl<(u64, u64)> = Bkl::new((u64::MAX / 2, u64::MAX / 2 - 1));

fn rules() -> &'static mut [Vec<Rule>; 8] {
    RULES.get()
}

// ---- 組の印 (lib/audit.c と asm-generic/audit_*.h の arm64 の番号) ----

const DIR_WRITE: &[u32] = &[33, 34, 35, 36, 37, 38, 276];
const CHATTR: &[u32] = &[52, 55, 5, 463, 6, 7, 14, 466, 15, 16, 54, 53, 452, 37, 469];
const READ: &[u32] = &[60, 443, 11, 465, 12, 13, 8, 464, 9, 10, 78, 468, 80, 79, 291];
/// write は dir_write もふくむ (asm-generic/audit_write.h が audit_dir_write.h を include する)
const WRITE: &[u32] = &[33, 34, 35, 36, 37, 38, 276, 89, 224, 60, 443, 45, 46, 200, 47];
const SIGNAL: &[u32] = &[129, 131, 130];

/// クラスの番号 (AUDIT_CLASS_*: 奇数は 32bit のもので、aios にはない)
fn class(i: usize) -> Option<&'static [u32]> {
    match i {
        0 => Some(DIR_WRITE),
        2 => Some(CHATTR),
        4 => Some(READ),
        6 => Some(WRITE),
        8 => Some(SIGNAL),
        _ => None,
    }
}

fn in_class(c: &[u32], nr: u64) -> bool {
    c.iter().any(|&x| x as u64 == nr)
}

// ---- 読む (audit_data_to_entry) ----

fn w(d: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(d[i * 4..i * 4 + 4].try_into().unwrap())
}

/// audit_field_valid: 組と、種類と比べかたと、値の決まり
fn field_valid(listnr: u32, typ: u32, op: Op, val: u32) -> Result<(), i64> {
    match typ {
        F_MSGTYPE if listnr != EXCLUDE && listnr != USER => return Err(-EINVAL),
        F_FSTYPE if listnr != FS => return Err(-EINVAL),
        F_PERM if listnr == URING_EXIT => return Err(-EINVAL),
        _ => {}
    }
    if listnr == FS && typ != F_FSTYPE && typ != F_FILTERKEY {
        return Err(-EINVAL);
    }
    let bit = matches!(op, Op::BitMask | Op::BitTest);
    let eq_only = !matches!(op, Op::Eq | Op::Ne);
    let ok_op = match typ {
        F_ARG0..=F_ARG3 | F_PERS | F_DEVMINOR => true,
        F_UID..=F_FSGID | F_LOGINUID | F_OBJ_UID | F_OBJ_GID | F_PID | F_MSGTYPE | F_PPID | F_DEVMAJOR | F_EXIT | F_SUCCESS | F_INODE | F_SESSIONID | 16 | 17 | 22 | 23 | F_SADDR_FAM => !bit,
        13..=15 | 19..=21 | F_WATCH | F_DIR | F_FILTERKEY | F_LOGINUID_SET | F_ARCH | F_FSTYPE | F_PERM | F_FILETYPE | F_FIELD_COMPARE | F_EXE => !eq_only,
        _ => false,
    };
    // 値の決まり (Linux の audit_field_valid のおしまい)。aios がえらんだちがい (audit-field-value): Linux はここで値を
    // 入れる前の f->val (0) を見るので、どれも通る。まちがった値のルールは何にも合わない (FILETYPE、PERM) か、合わせる
    // たびに WARN になる (FIELD_COMPARE) ので、足すときに EINVAL で知らせる
    const MAX_FIELD_COMPARE: u32 = 25;
    const AF_MAX: u32 = 46;
    let ok_val = match typ {
        F_LOGINUID_SET => val <= 1,
        F_PERM => val & !15 == 0,
        F_FILETYPE => val & !0o170000 == 0,
        F_FIELD_COMPARE => val <= MAX_FIELD_COMPARE,
        F_SADDR_FAM => val < AF_MAX,
        _ => true,
    };
    if ok_op && ok_val { Ok(()) } else { Err(-EINVAL) }
}

/// struct audit_rule_data (d は 1040 バイトより長い) をルールに
pub fn parse(d: &[u8]) -> Result<Rule, i64> {
    let flags = w(d, 0);
    let action = w(d, 1);
    let count = w(d, 2) as usize;
    let listnr = flags & !PREPEND;
    if !matches!(listnr, EXIT | URING_EXIT | TASK | USER | EXCLUDE | FS) || action == POSSIBLE || (action != NEVER && action != ALWAYS) || count > MAX_FIELDS {
        return Err(-EINVAL);
    }
    let mut mask = [0u32; 64];
    for (i, m) in mask.iter_mut().enumerate() {
        *m = w(d, 3 + i);
    }
    // クラスの印をひろげる
    for i in 0..16 {
        let bit = 64 * 32 - i - 1;
        if mask[bit / 32] & 1 << (bit % 32) == 0 {
            continue;
        }
        mask[bit / 32] &= !(1 << (bit % 32));
        for &nr in class(i).unwrap_or(&[]) {
            mask[nr as usize / 32] |= 1 << (nr % 32);
        }
    }
    let mut r = Rule { listnr, flags: flags & PREPEND, legacy: false, action, fields: Vec::new(), mask, key: None, prio: u64::MAX, by_name: false, watch: None, tree: None, exe: None, watch_id: None, tree_path: None };
    let buf = &d[DATA..];
    let mut at = 0usize;
    // audit_unpack_string: 0 や残りより長いものは EINVAL
    let mut unpack = |len: u32| -> Result<String, i64> {
        if len == 0 || len as usize > buf.len() - at {
            return Err(-EINVAL);
        }
        if len > PATH_MAX {
            return Err(-ENAMETOOLONG);
        }
        let b = &buf[at..at + len as usize];
        at += len as usize;
        let b = &b[..b.iter().position(|&c| c == 0).unwrap_or(b.len())];
        Ok(String::from_utf8_lossy(b).into_owned())
    };
    let mut inode_f: Option<Op> = None;
    for i in 0..count {
        let op = Op::from(w(d, 195 + i)).ok_or(-EINVAL)?;
        let (mut typ, mut val) = (w(d, 67 + i), w(d, 131 + i));
        if typ == F_LOGINUID && val == UNSET {
            (typ, val, r.legacy) = (F_LOGINUID_SET, 0, true);
        }
        field_valid(listnr, typ, op, val)?;
        let mut s = None;
        match typ {
            F_LOGINUID | F_UID..=F_FSUID | F_OBJ_UID => {
                if val == UNSET {
                    return Err(-EINVAL);
                }
                val = crate::ns::take_uid(val)?;
            }
            F_GID..=F_FSGID | F_OBJ_GID => {
                if val == UNSET {
                    return Err(-EINVAL);
                }
                val = crate::ns::take_gid(val)?;
            }
            F_SUBJ_USER..=F_SUBJ_CLR | F_OBJ_USER..=F_OBJ_LEV_HIGH => s = Some(unpack(val)?),
            F_WATCH => {
                let p = unpack(val)?;
                if !p.starts_with('/') || p.ends_with('/') || (listnr != EXIT && listnr != URING_EXIT) || op != Op::Eq || inode_f.is_some() || r.watch.is_some() || r.tree.is_some() {
                    return Err(-EINVAL);
                }
                r.watch = Some(p.clone());
                s = Some(p);
            }
            F_DIR => {
                let p = unpack(val)?;
                if !p.starts_with('/') || (listnr != EXIT && listnr != URING_EXIT) || op != Op::Eq || inode_f.is_some() || r.watch.is_some() || r.tree.is_some() {
                    return Err(-EINVAL);
                }
                r.tree = Some(p.clone());
                s = Some(p);
            }
            F_INODE => {
                if (listnr != EXIT && listnr != URING_EXIT) || inode_f.is_some() || r.watch.is_some() || r.tree.is_some() || !matches!(op, Op::Eq | Op::Ne) {
                    return Err(-EINVAL);
                }
                inode_f = Some(op);
            }
            F_FILTERKEY => {
                if r.key.is_some() || val > MAX_KEY_LEN {
                    return Err(-EINVAL);
                }
                let k = unpack(val)?;
                r.key = Some(k.clone());
                s = Some(k);
            }
            F_EXE => {
                if r.exe.is_some() || val > PATH_MAX {
                    return Err(-EINVAL);
                }
                let p = unpack(val)?;
                if !p.starts_with('/') || p.ends_with('/') {
                    return Err(-EINVAL);
                }
                // audit_alloc_mark: 親のディレクトリ (なければそのエラー) と、いまあるもの
                let child = parent_child(&p)?;
                r.exe = Some(child.unwrap_or((0, u64::MAX)));
                s = Some(p);
            }
            _ => {}
        }
        r.fields.push(Field { typ, op, val, s });
    }
    // = の INODE と WATCH は名前で合わせる (!= の INODE はふつうのルール)
    r.by_name = r.watch.is_some() || inode_f == Some(Op::Eq);
    Ok(r)
}

/// path の親のディレクトリをたしかめて (kern_path_parent: なければそのエラー)、いまある最後のもの (なければ None)
fn parent_child(path: &str) -> Result<Option<(usize, u64)>, i64> {
    let (dir, _) = path.rsplit_once('/').unwrap_or(("", path));
    let d = crate::vfs::resolve("", if dir.is_empty() { "/" } else { dir }, true)?;
    if !d.meta().is_dir() {
        return Err(-ENOTDIR);
    }
    Ok(crate::vfs::resolve("", path, false).ok().map(|i| i.id()))
}

// ---- 足す・消す・一覧 ----

/// audit_compare_rule: 同じなら true
fn same(a: &Rule, b: &Rule) -> bool {
    a.flags == b.flags && a.legacy == b.legacy && a.listnr == b.listnr && a.action == b.action && a.fields == b.fields && a.mask == b.mask
}

fn counts(listnr: u32) -> bool {
    !matches!(listnr, USER | EXCLUDE | FS)
}

/// audit_add_rule: 同じものがあれば EEXIST、WATCH の親や DIR がなければ ENOENT
pub fn add(mut r: Rule) -> Result<(), i64> {
    let list = &mut rules()[r.listnr as usize];
    if list.iter().any(|e| same(e, &r)) {
        return Err(-EEXIST);
    }
    if let Some(p) = &r.watch {
        r.watch_id = parent_child(p)?;
    }
    if let Some(p) = r.tree.clone() {
        // 木の根の本当のパス
        let (canon, _) = crate::vfs::lookup("", &p, false)?;
        r.tree_path = Some(format!("/{}", canon));
    }
    let prepend = r.flags & PREPEND != 0;
    if r.listnr == EXIT || r.listnr == URING_EXIT {
        let pr = PRIO.get();
        r.prio = if prepend {
            pr.1 += 1;
            pr.1
        } else {
            pr.0 -= 1;
            pr.0
        };
    }
    r.flags = 0;
    if counts(r.listnr) {
        N_RULES.fetch_add(1, Relaxed);
    }
    if prepend {
        list.insert(0, r);
    } else {
        list.push(r);
    }
    Ok(())
}

/// audit_del_rule: なければ ENOENT
pub fn del(r: &Rule) -> Result<(), i64> {
    let list = &mut rules()[r.listnr as usize];
    let i = list.iter().position(|e| same(e, r)).ok_or(-ENOENT)?;
    let e = list.remove(i);
    if counts(e.listnr) {
        N_RULES.fetch_sub(1, Relaxed);
    }
    Ok(())
}

/// audit_list_rules: 組の順に、ルールごとの struct audit_rule_data (と文字)
pub fn list() -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for list in rules().iter() {
        for r in list {
            let mut d = alloc::vec![0u8; DATA];
            let mut put = |i: usize, v: u32| d[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
            put(0, r.flags | r.listnr);
            put(1, r.action);
            put(2, r.fields.len() as u32);
            for (i, &m) in r.mask.iter().enumerate() {
                put(3 + i, m);
            }
            let mut buf = Vec::new();
            for (i, f) in r.fields.iter().enumerate() {
                let (typ, val) = match &f.s {
                    Some(s) => {
                        buf.extend_from_slice(s.as_bytes());
                        (f.typ, s.len() as u32)
                    }
                    None if f.typ == F_LOGINUID_SET && r.legacy && f.val == 0 => (F_LOGINUID, UNSET),
                    None => (f.typ, f.val),
                };
                put(67 + i, typ);
                put(131 + i, val);
                put(195 + i, f.op.code());
            }
            put(259, buf.len() as u32);
            d.extend_from_slice(&buf);
            out.push(d);
        }
    }
    out
}

// ---- 合わせる ----

fn cmp(left: u32, op: Op, right: u32) -> bool {
    match op {
        Op::Eq => left == right,
        Op::Ne => left != right,
        Op::Lt => left < right,
        Op::Le => left <= right,
        Op::Gt => left > right,
        Op::Ge => left >= right,
        Op::BitMask => left & right != 0,
        Op::BitTest => left & right == right,
    }
}

/// audit_uid_comparator / audit_gid_comparator (bit の比べかたは合わない)
fn idcmp(left: u32, op: Op, right: u32) -> bool {
    !matches!(op, Op::BitMask | Op::BitTest) && cmp(left, op, right)
}

/// gid の比べかた (= と != は、ほかのグループも)
fn gidcmp(p: &Proc, g: u32, op: Op, val: u32) -> bool {
    let r = idcmp(g, op, val);
    match op {
        Op::Eq if !r => p.cred.groups.contains(&val),
        Op::Ne if r => !p.cred.groups.contains(&val),
        _ => r,
    }
}

fn exe_is(p: &Proc, r: &Rule) -> bool {
    p.mm.as_ref().is_some_and(|m| Some(m.get().exe_id) == r.exe)
}

/// audit_filter (USER と EXCLUDE の組): 出してよければ true
pub fn filter(msgtype: u16, listnr: u32) -> bool {
    let Some(p) = crate::proc::try_current() else { return true };
    for e in rules()[listnr as usize].iter() {
        let mut result = false;
        for f in &e.fields {
            let c = &p.cred;
            result = match f.typ {
                F_PID => cmp(p.tgid, f.op, f.val),
                F_UID => idcmp(c.uid, f.op, f.val),
                F_GID => idcmp(c.gid, f.op, f.val),
                F_LOGINUID => idcmp(c.loginuid, f.op, f.val),
                F_LOGINUID_SET => cmp((c.loginuid != UNSET) as u32, f.op, f.val),
                F_MSGTYPE => cmp(msgtype as u32, f.op, f.val),
                F_SUBJ_USER..=F_SUBJ_CLR => false,
                F_EXE => exe_is(p, e) != (f.op == Op::Ne),
                // ほかの欄はここで見るのをやめる (Linux の goto unlock_and_return)
                _ => return true,
            };
            if !result {
                break;
            }
        }
        if result {
            return !(e.action == NEVER || listnr == EXCLUDE);
        }
    }
    true
}

/// 出口のフィルタに渡すもの
pub struct Sys<'a> {
    pub major: u64,
    pub argv: [u64; 4],
    /// (成功か, 返した値) (なければ終わるとき)
    pub ret: Option<(bool, i64)>,
    pub prio: u64,
    /// 集めた名前 (inode のあるもの)
    pub names: &'a [NameInfo],
}

/// WATCH のファイルの inode: いまあるもの (覚えなおす)、なければ前に見たもの
fn watch_id(r: &mut Rule) -> Option<(usize, u64)> {
    let p = r.watch.as_deref()?;
    if let Ok(i) = crate::vfs::resolve("", p, false) {
        r.watch_id = Some(i.id());
    }
    r.watch_id
}

/// 合ったルールの答え: 出すか (ALWAYS) と、key と prio
pub struct Hit {
    pub record: bool,
    pub key: Option<String>,
    pub prio: u64,
}

/// audit_match_perm (open と openat の flags、execve、クラス)
fn match_perm(s: &Sys, mask: u32) -> bool {
    let acc = |flags: u64| [PERM_READ, PERM_WRITE, PERM_READ | PERM_WRITE, PERM_READ | PERM_WRITE][(flags & 3) as usize];
    match s.major {
        56 => mask & acc(s.argv[2]) != 0,
        221 | 281 => mask & PERM_EXEC != 0,
        n => (mask & PERM_WRITE != 0 && in_class(WRITE, n)) || (mask & PERM_READ != 0 && in_class(READ, n)) || (mask & PERM_ATTR != 0 && in_class(CHATTR, n)),
    }
}

/// audit_field_compare (プロセスの id どうしと、プロセスとファイル (名前) の id)
fn field_compare(p: &Proc, f: &Field, s: Option<&Sys>, name: Option<&NameInfo>) -> bool {
    let c = &p.cred;
    // 1〜9: プロセスの id とファイルの持ち主
    let obj = match f.val {
        1 => Some((c.uid, true)),
        2 => Some((c.gid, false)),
        3 => Some((c.euid, true)),
        4 => Some((c.egid, false)),
        5 => Some((c.loginuid, true)),
        6 => Some((c.suid, true)),
        7 => Some((c.sgid, false)),
        8 => Some((c.fsuid, true)),
        9 => Some((c.fsgid, false)),
        _ => None,
    };
    if let Some((id, uid)) = obj {
        return any_name(s, name, |n| idcmp(id, f.op, if uid { n.uid } else { n.gid }));
    }
    let (l, r) = match f.val {
        10 => (c.uid, c.loginuid),
        11 => (c.uid, c.euid),
        12 => (c.uid, c.fsuid),
        13 => (c.uid, c.suid),
        14 => (c.loginuid, c.fsuid),
        15 => (c.loginuid, c.suid),
        16 => (c.loginuid, c.euid),
        17 => (c.euid, c.suid),
        18 => (c.euid, c.fsuid),
        19 => (c.suid, c.fsuid),
        20 => (c.gid, c.egid),
        21 => (c.gid, c.fsgid),
        22 => (c.gid, c.sgid),
        23 => (c.egid, c.fsgid),
        24 => (c.egid, c.sgid),
        25 => (c.sgid, c.fsgid),
        _ => return false,
    };
    idcmp(l, f.op, r)
}

/// 名前が木 (根のパス t) の中か: たどったものはその親のどれかが根、作る・消すものはそれ自身が根
fn in_tree(n: &NameInfo, t: &str) -> bool {
    if !n.walked {
        return n.path == t;
    }
    n.path == t || t == "/" || n.path.strip_prefix(t).is_some_and(|rest| rest.starts_with('/'))
}

/// 名前 (name があればそれ、なければシステムコールの名前のどれか) に合うか
fn any_name(s: Option<&Sys>, name: Option<&NameInfo>, f: impl Fn(&NameInfo) -> bool) -> bool {
    match name {
        Some(n) => f(n),
        None => s.is_some_and(|s| s.names.iter().any(f)),
    }
}

/// audit_filter_rules: ルール 1 つ (s がなければ fork のとき: TASK の組。name は名前のルールのとき)
fn rule_matches(p: &Proc, r: &Rule, s: Option<&Sys>, name: Option<&NameInfo>, watch: Option<(usize, u64)>) -> bool {
    if s.is_some_and(|s| r.prio <= s.prio) {
        return false;
    }
    let c = &p.cred;
    for f in &r.fields {
        let ok = match f.typ {
            F_PID => cmp(p.tgid, f.op, f.val),
            F_PPID => s.is_some() && cmp(p.ppid, f.op, f.val),
            // fork のとき (TASK の組) はいまのプロセスでないので合わない (audit_exe_compare)
            F_EXE => (s.is_some() && exe_is(p, r)) != (f.op == Op::Ne),
            F_UID => idcmp(c.uid, f.op, f.val),
            F_EUID => idcmp(c.euid, f.op, f.val),
            F_FSUID => idcmp(c.fsuid, f.op, f.val),
            F_SUID => idcmp(c.suid, f.op, f.val),
            F_GID => gidcmp(p, c.gid, f.op, f.val),
            F_EGID => gidcmp(p, c.egid, f.op, f.val),
            F_SGID => idcmp(c.sgid, f.op, f.val),
            F_FSGID => idcmp(c.fsgid, f.op, f.val),
            F_SESSIONID => cmp(c.sessionid, f.op, f.val),
            F_PERS => cmp(0, f.op, f.val),
            F_ARCH => s.is_some() && cmp(crate::seccomp::AUDIT_ARCH_AARCH64, f.op, f.val),
            F_EXIT => s.and_then(|s| s.ret).is_some_and(|(_, code)| cmp(code as u32, f.op, f.val)),
            F_SUCCESS => s.and_then(|s| s.ret).is_some_and(|(ok, _)| {
                // return_valid は 1 が成功、2 がしくじり
                let rv = if ok { 1 } else { 2 };
                cmp(rv, f.op, if f.val != 0 { 1 } else { 2 })
            }),
            F_LOGINUID => idcmp(c.loginuid, f.op, f.val),
            F_LOGINUID_SET => cmp((c.loginuid != UNSET) as u32, f.op, f.val),
            F_ARG0..=F_ARG3 => s.is_some_and(|s| cmp(s.argv[(f.typ - F_ARG0) as usize] as u32, f.op, f.val)),
            F_FILTERKEY => true,
            F_PERM => s.is_some_and(|s| match_perm(s, f.val)) != (f.op == Op::Ne),
            F_FILETYPE => s.is_some_and(|s| s.names.iter().any(|n| n.mode & 0o170000 == f.val)) != (f.op == Op::Ne),
            F_FIELD_COMPARE => field_compare(p, f, s, name),
            F_INODE => any_name(s, name, |n| cmp(n.ino as u32, f.op, f.val)),
            F_DEVMAJOR => any_name(s, name, |n| cmp(n.dev.0, f.op, f.val) || cmp(n.rdev.0, f.op, f.val)),
            F_DEVMINOR => any_name(s, name, |n| cmp(n.dev.1, f.op, f.val) || cmp(n.rdev.1, f.op, f.val)),
            F_OBJ_UID => any_name(s, name, |n| idcmp(n.uid, f.op, f.val)),
            F_OBJ_GID => any_name(s, name, |n| idcmp(n.gid, f.op, f.val)),
            // audit_watch_compare: 名前の inode が WATCH のファイル
            F_WATCH => name.is_some_and(|n| watch == Some(n.id)),
            // match_tree_refs: 名前のどれかが木の中
            F_DIR => s.is_some_and(|s| r.tree_path.as_deref().is_some_and(|t| s.names.iter().any(|n| in_tree(n, t)))),
            // 名前やソケットの番地、LSM のラベルを見るもの: まだ合わない
            _ => false,
        };
        if !ok {
            return false;
        }
    }
    true
}

/// システムコールの出口のフィルタ (audit_filter_syscall): EXIT の組で先に合ったもの
pub fn filter_syscall(p: &Proc, s: &Sys) -> Option<Hit> {
    let nr = s.major;
    for r in rules()[EXIT as usize].iter().filter(|r| !r.by_name) {
        let word = (nr / 32) as usize;
        if nr > 0xffff_ffff || word >= 64 || r.mask[word] & 1 << (nr % 32) == 0 {
            continue;
        }
        if rule_matches(p, r, Some(s), None, None) {
            return Some(Hit { record: r.action == ALWAYS, key: r.key.clone(), prio: r.prio });
        }
    }
    None
}

/// 名前ごとに名前のルール (audit_filter_inodes: WATCH と = の INODE)。名前ごとに先に合ったもの
pub fn filter_inodes(p: &Proc, s: &Sys) -> Option<Hit> {
    let nr = s.major;
    let word = (nr / 32) as usize;
    if nr > 0xffff_ffff || word >= 64 {
        return None;
    }
    let mut prio = s.prio;
    let mut last = None;
    for n in s.names {
        for r in rules()[EXIT as usize].iter_mut().filter(|r| r.by_name) {
            if r.mask[word] & 1 << (nr % 32) == 0 {
                continue;
            }
            let w = watch_id(r);
            let sys = Sys { major: s.major, argv: s.argv, ret: s.ret, prio, names: s.names };
            if rule_matches(p, r, Some(&sys), Some(n), w) {
                prio = r.prio;
                last = Some(Hit { record: r.action == ALWAYS, key: r.key.clone(), prio: r.prio });
                break;
            }
        }
        if last.is_some() {
            break;
        }
    }
    last
}

/// fork したスレッド (audit_filter_task): TASK の組で先に合ったもの
pub fn filter_task(child: &Proc) -> Option<Hit> {
    for r in rules()[TASK as usize].iter() {
        if rule_matches(child, r, None, None, None) {
            return Some(Hit { record: r.action == ALWAYS, key: if r.action == ALWAYS { r.key.clone() } else { None }, prio: r.prio });
        }
    }
    None
}

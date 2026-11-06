// ctnetlink (Linux の net/netfilter/nf_conntrack_netlink.c: NETLINK_NETFILTER の subsys 1)。conntrack (conntrack-tools) が
// 使うもの:
//   - IPCTNL_MSG_CT_GET: dump (conntrack -L。nfgenmsg の家族のものだけ、AF_UNSPEC はみな) と 1 つ (-G: 組で引く。なければ
//     ENOENT、組がなければ EINVAL)
//   - IPCTNL_MSG_CT_DELETE: 組 (CTA_TUPLE_ORIG か CTA_TUPLE_REPLY。CTA_ID があれば合うものだけ) のつながりを消す (-D。
//     なければ ENOENT)。組がなければ家族のものをみな消す (-F: ctnetlink_flush_conntrack)
//   - IPCTNL_MSG_CT_GET_STATS_CPU (-S: CPU ごとの数) と IPCTNL_MSG_CT_GET_STATS (-C: 数といちばん多いとき)
//   - IPCTNL_MSG_CT_NEW (ctnetlink_new_conntrack): なければ NLM_F_CREATE で作る (-I: 組 2 つとのこり時間がいる。
//     なければ ENOENT)、あれば NLM_F_EXCL なら EEXIST、でなければ変える (-U: mark、のこり時間、status、TCP の状態)
//   - 知らせ (ctnetlink_conntrack_event): NFNLGRP_CONNTRACK_NEW (CT_NEW、NLM_F_CREATE | NLM_F_EXCL)、UPDATE (CT_NEW)、
//     DESTROY (CT_DELETE) に。conntrack -E。道具がしたもの (作る・変える・消す) は頭の portid がそのソケット
// 答えのつながりは IPCTNL_MSG_CT_NEW (ctnetlink_fill_info: 組 2 つ、status、timeout、protoinfo (TCP の状態と窓の
// 倍率)、mark、id、use)。dump は libmnl の受ける入れ物 (4096) に入るように分ける
use crate::conntrack::{self, Conn, Tuple};
use alloc::vec::Vec;

const NLMSG_HDRLEN: usize = 16;
const NLM_F_MULTI: u16 = 2;
const NLM_F_DUMP: u16 = 0x300;
const NLMSG_DONE: u16 = 3;
const SUBSYS: u16 = 1;

const EINVAL: i64 = 22;
const ENOENT: i64 = 2;
const EOPNOTSUPP: i64 = 95;

const CT_NEW: u16 = 0;
const CT_GET: u16 = 1;
const CT_DELETE: u16 = 2;
const CT_GET_CTRZERO: u16 = 3;
const CT_GET_STATS_CPU: u16 = 4;
const CT_GET_STATS: u16 = 5;
const CT_GET_DYING: u16 = 6;
const CT_GET_UNCONFIRMED: u16 = 7;

const CTA_TUPLE_ORIG: u16 = 1;
const CTA_TUPLE_REPLY: u16 = 2;
const CTA_STATUS: u16 = 3;
const CTA_PROTOINFO: u16 = 4;
const CTA_TIMEOUT: u16 = 7;
const CTA_MARK: u16 = 8;
const CTA_COUNTERS_ORIG: u16 = 9;
const CTA_COUNTERS_REPLY: u16 = 10;
const CTA_USE: u16 = 11;
const CTA_ID: u16 = 12;

const CTA_TUPLE_IP: u16 = 1;
const CTA_TUPLE_PROTO: u16 = 2;
const CTA_IP_V4_SRC: u16 = 1;
const CTA_IP_V4_DST: u16 = 2;
const CTA_IP_V6_SRC: u16 = 3;
const CTA_IP_V6_DST: u16 = 4;
const CTA_PROTO_NUM: u16 = 1;
const CTA_PROTO_SRC_PORT: u16 = 2;
const CTA_PROTO_DST_PORT: u16 = 3;
const CTA_PROTO_ICMP_ID: u16 = 4;
const CTA_PROTO_ICMP_TYPE: u16 = 5;
const CTA_PROTO_ICMP_CODE: u16 = 6;
const CTA_PROTO_ICMPV6_ID: u16 = 7;
const CTA_PROTO_ICMPV6_TYPE: u16 = 8;
const CTA_PROTO_ICMPV6_CODE: u16 = 9;
const CTA_PROTOINFO_TCP: u16 = 1;
const CTA_PROTOINFO_TCP_STATE: u16 = 1;
const CTA_PROTOINFO_TCP_WSCALE_ORIGINAL: u16 = 2;
const CTA_PROTOINFO_TCP_WSCALE_REPLY: u16 = 3;
const CTA_PROTOINFO_TCP_FLAGS_ORIGINAL: u16 = 4;
const CTA_PROTOINFO_TCP_FLAGS_REPLY: u16 = 5;
const CTA_COUNTERS_PACKETS: u16 = 1;
const CTA_COUNTERS_BYTES: u16 = 2;
/// CTA_STATS_*: ctnetlink_ct_stat_cpu_fill_info が出すもの (found、invalid、insert、insert_failed、drop、early_drop、
/// error、search_restart、clash_resolve、chaintoolong)
const CTA_STATS_CPU: [u16; 10] = [2, 4, 8, 9, 10, 11, 12, 13, 14, 15];
const CTA_STATS_INVALID: u16 = 4;
const CTA_STATS_GLOBAL_ENTRIES: u16 = 1;
const NLM_F_EXCL: u16 = 0x200;
const NLM_F_CREATE: u16 = 0x400;
const EEXIST: i64 = 17;
const NETLINK_NETFILTER: u32 = 12;
const NFNLGRP_CONNTRACK_NEW: u32 = 1;
const NFNLGRP_CONNTRACK_UPDATE: u32 = 2;
const NFNLGRP_CONNTRACK_DESTROY: u32 = 3;
const CTA_STATS_GLOBAL_MAX_ENTRIES: u16 = 2;
const NLA_F_NESTED: u16 = 0x8000;

const AF_INET: u8 = 2;
const AF_INET6: u8 = 10;

/// 1 つのデータグラムの大きさ (libmnl の MNL_SOCKET_BUFFER_SIZE に入るように)
const DGRAM: usize = 3800;

// ---- 属性 ----

/// 属性をみな (type は NLA_F_NESTED などをはずして)
fn attrs(mut b: &[u8]) -> Vec<(u16, &[u8])> {
    let mut v = Vec::new();
    while b.len() >= 4 {
        let len = u16::from_le_bytes([b[0], b[1]]) as usize;
        if len < 4 || len > b.len() {
            break;
        }
        v.push((u16::from_le_bytes([b[2], b[3]]) & 0x3fff, &b[4..len]));
        b = &b[((len + 3) & !3).min(b.len())..];
    }
    v
}

fn get<'a>(a: &[(u16, &'a [u8])], ty: u16) -> Option<&'a [u8]> {
    a.iter().rev().find(|x| x.0 == ty).map(|x| x.1)
}

fn be16(d: Option<&[u8]>) -> Result<u16, i64> {
    let d = d.filter(|d| d.len() >= 2).ok_or(EINVAL)?;
    Ok(u16::from_be_bytes([d[0], d[1]]))
}

fn u8_of(d: Option<&[u8]>) -> Result<u8, i64> {
    d.and_then(|d| d.first().copied()).ok_or(EINVAL)
}

struct W(Vec<u8>);

impl W {
    fn attr(&mut self, ty: u16, d: &[u8]) {
        self.0.extend_from_slice(&((4 + d.len()) as u16).to_le_bytes());
        self.0.extend_from_slice(&ty.to_le_bytes());
        self.0.extend_from_slice(d);
        self.0.resize((self.0.len() + 3) & !3, 0);
    }
    fn be32(&mut self, ty: u16, v: u32) {
        self.attr(ty, &v.to_be_bytes());
    }
    fn start(&mut self, ty: u16) -> usize {
        let at = self.0.len();
        self.0.extend_from_slice(&[0, 0]);
        self.0.extend_from_slice(&(ty | NLA_F_NESTED).to_le_bytes());
        at
    }
    fn end(&mut self, at: usize) {
        let len = (self.0.len() - at) as u16;
        self.0[at..at + 2].copy_from_slice(&len.to_le_bytes());
    }
}

/// nfnl_msg_put
fn msg(typ: u16, flags: u16, seq: u32, pid: u32, family: u8, res_id: u16, attrs: &[u8]) -> Vec<u8> {
    let len = NLMSG_HDRLEN + 4 + attrs.len();
    let mut m = Vec::with_capacity(len);
    m.extend_from_slice(&(len as u32).to_le_bytes());
    m.extend_from_slice(&((SUBSYS << 8) | typ).to_le_bytes());
    m.extend_from_slice(&flags.to_le_bytes());
    m.extend_from_slice(&seq.to_le_bytes());
    m.extend_from_slice(&pid.to_le_bytes());
    m.extend_from_slice(&[family, 0]);
    m.extend_from_slice(&res_id.to_be_bytes());
    m.extend_from_slice(attrs);
    m
}

fn done(seq: u32, pid: u32) -> Vec<u8> {
    let mut m = Vec::with_capacity(NLMSG_HDRLEN + 4);
    m.extend_from_slice(&((NLMSG_HDRLEN + 4) as u32).to_le_bytes());
    m.extend_from_slice(&NLMSG_DONE.to_le_bytes());
    m.extend_from_slice(&NLM_F_MULTI.to_le_bytes());
    m.extend_from_slice(&seq.to_le_bytes());
    m.extend_from_slice(&pid.to_le_bytes());
    m.extend_from_slice(&0i32.to_le_bytes());
    m
}

// ---- 組 ----

/// ctnetlink_dump_tuples: CTA_TUPLE_IP (送り元とあて先) と CTA_TUPLE_PROTO (protocol とポートか ICMP の id・type・code)
fn put_tuple(w: &mut W, ty: u16, t: &Tuple) {
    let n = w.start(ty);
    let ip = w.start(CTA_TUPLE_IP);
    if t.l3 == 6 {
        w.attr(CTA_IP_V6_SRC, &t.src.to_be_bytes());
        w.attr(CTA_IP_V6_DST, &t.dst.to_be_bytes());
    } else {
        w.attr(CTA_IP_V4_SRC, &(t.src as u32).to_be_bytes());
        w.attr(CTA_IP_V4_DST, &(t.dst as u32).to_be_bytes());
    }
    w.end(ip);
    let p = w.start(CTA_TUPLE_PROTO);
    w.attr(CTA_PROTO_NUM, &[t.proto]);
    match t.proto {
        1 => {
            w.attr(CTA_PROTO_ICMP_ID, &t.sport.to_be_bytes());
            w.attr(CTA_PROTO_ICMP_TYPE, &[(t.dport >> 8) as u8]);
            w.attr(CTA_PROTO_ICMP_CODE, &[t.dport as u8]);
        }
        58 => {
            w.attr(CTA_PROTO_ICMPV6_ID, &t.sport.to_be_bytes());
            w.attr(CTA_PROTO_ICMPV6_TYPE, &[(t.dport >> 8) as u8]);
            w.attr(CTA_PROTO_ICMPV6_CODE, &[t.dport as u8]);
        }
        6 | 17 | 132 | 136 => {
            w.attr(CTA_PROTO_SRC_PORT, &t.sport.to_be_bytes());
            w.attr(CTA_PROTO_DST_PORT, &t.dport.to_be_bytes());
        }
        _ => {}
    }
    w.end(p);
    w.end(n);
}

/// 要求の組 (ctnetlink_parse_tuple: IP の 2 つ、protocol、ポートか ICMP の 3 つ。足りなければ EINVAL)
fn parse_tuple(d: &[u8], family: u8) -> Result<Tuple, i64> {
    let l3 = match family {
        AF_INET => 4,
        AF_INET6 => 6,
        _ => return Err(EOPNOTSUPP),
    };
    let a = attrs(d);
    let ip = attrs(get(&a, CTA_TUPLE_IP).ok_or(EINVAL)?);
    let (src, dst) = if l3 == 6 {
        let s = get(&ip, CTA_IP_V6_SRC).filter(|d| d.len() >= 16).ok_or(EINVAL)?;
        let t = get(&ip, CTA_IP_V6_DST).filter(|d| d.len() >= 16).ok_or(EINVAL)?;
        (u128::from_be_bytes(s[..16].try_into().unwrap()), u128::from_be_bytes(t[..16].try_into().unwrap()))
    } else {
        let s = get(&ip, CTA_IP_V4_SRC).filter(|d| d.len() >= 4).ok_or(EINVAL)?;
        let t = get(&ip, CTA_IP_V4_DST).filter(|d| d.len() >= 4).ok_or(EINVAL)?;
        (u32::from_be_bytes(s[..4].try_into().unwrap()) as u128, u32::from_be_bytes(t[..4].try_into().unwrap()) as u128)
    };
    let p = attrs(get(&a, CTA_TUPLE_PROTO).ok_or(EINVAL)?);
    let proto = u8_of(get(&p, CTA_PROTO_NUM))?;
    let (sport, dport) = match proto {
        1 => (be16(get(&p, CTA_PROTO_ICMP_ID))?, (u8_of(get(&p, CTA_PROTO_ICMP_TYPE))? as u16) << 8 | u8_of(get(&p, CTA_PROTO_ICMP_CODE))? as u16),
        58 => (be16(get(&p, CTA_PROTO_ICMPV6_ID))?, (u8_of(get(&p, CTA_PROTO_ICMPV6_TYPE))? as u16) << 8 | u8_of(get(&p, CTA_PROTO_ICMPV6_CODE))? as u16),
        6 | 17 | 132 | 136 => (be16(get(&p, CTA_PROTO_SRC_PORT))?, be16(get(&p, CTA_PROTO_DST_PORT))?),
        _ => (0, 0),
    };
    Ok(Tuple { l3, src, dst, proto, sport, dport })
}

/// 1 つのつながり (ctnetlink_fill_info)。use は参照の数 (dump は 1、1 つを引くときは引いた人のぶんも足して 2)
fn fill(c: &Conn, typ: u16, flags: u16, seq: u32, pid: u32, use_: u32) -> Vec<u8> {
    let mut w = W(Vec::new());
    put_tuple(&mut w, CTA_TUPLE_ORIG, &c.tuple[0]);
    put_tuple(&mut w, CTA_TUPLE_REPLY, &c.tuple[1]);
    w.be32(CTA_STATUS, c.status);
    w.be32(CTA_TIMEOUT, (conntrack::expires_ns(c) / 1_000_000_000) as u32);
    put_protoinfo(&mut w, c);
    put_acct(&mut w, c);
    w.be32(CTA_MARK, c.mark);
    w.be32(CTA_ID, c.id);
    w.be32(CTA_USE, use_);
    let family = if c.tuple[0].l3 == 6 { AF_INET6 } else { AF_INET };
    msg(typ, flags, seq, pid, family, 0, &w.0)
}

/// dump の答え: 中身を DGRAM ごとのデータグラムにまとめ、終わりに NLMSG_DONE
fn dump_out(msgs: Vec<Vec<u8>>, seq: u32, pid: u32, out: &mut Vec<Vec<u8>>) {
    let mut cur: Vec<u8> = Vec::new();
    for m in msgs {
        if !cur.is_empty() && cur.len() + m.len() > DGRAM {
            out.push(core::mem::take(&mut cur));
        }
        cur.extend_from_slice(&m);
    }
    let d = done(seq, pid);
    if !cur.is_empty() && cur.len() + d.len() > DGRAM {
        out.push(core::mem::take(&mut cur));
    }
    cur.extend_from_slice(&d);
    out.push(cur);
}

/// つながりの知らせ (ctnetlink_conntrack_event): 組 2 つ、id、status、のこり時間 (DESTROY は 0 なら出さない)、DESTROY は
/// 数と TCP の状態、ほかは IPCT_PROTOINFO なら TCP の状態、IPCT_MARK か mark が 0 でなければ mark。聞いている人が
/// いなければ作らない
pub fn event(c: &Conn, ev: u32, portid: u32) {
    use conntrack::{IPCT_DESTROY, IPCT_MARK, IPCT_NEW, IPCT_PROTOINFO};
    let (typ, flags, group) = if ev & IPCT_DESTROY != 0 {
        (CT_DELETE, 0, NFNLGRP_CONNTRACK_DESTROY)
    } else if ev & IPCT_NEW != 0 {
        (CT_NEW, NLM_F_CREATE | NLM_F_EXCL, NFNLGRP_CONNTRACK_NEW)
    } else if ev != 0 {
        (CT_NEW, 0, NFNLGRP_CONNTRACK_UPDATE)
    } else {
        return;
    };
    if !crate::netlink::has_listeners(NETLINK_NETFILTER, group) {
        return;
    }
    let mut w = W(Vec::new());
    put_tuple(&mut w, CTA_TUPLE_ORIG, &c.tuple[0]);
    put_tuple(&mut w, CTA_TUPLE_REPLY, &c.tuple[1]);
    w.be32(CTA_ID, c.id);
    w.be32(CTA_STATUS, c.status);
    let left = (conntrack::expires_ns(c) / 1_000_000_000) as u32;
    if ev & IPCT_DESTROY != 0 {
        if left != 0 {
            w.be32(CTA_TIMEOUT, left);
        }
        put_acct(&mut w, c);
        put_protoinfo(&mut w, c);
    } else {
        w.be32(CTA_TIMEOUT, left);
        if ev & IPCT_PROTOINFO != 0 {
            put_protoinfo(&mut w, c);
        }
    }
    if ev & IPCT_MARK != 0 || c.mark != 0 {
        w.be32(CTA_MARK, c.mark);
    }
    let family = if c.tuple[0].l3 == 6 { AF_INET6 } else { AF_INET };
    crate::netlink::notify_proto(NETLINK_NETFILTER, group, msg(typ, flags, 0, portid, family, 0, &w.0));
}

/// CTA_PROTOINFO (TCP だけ: 状態、窓の倍率、flags)
fn put_protoinfo(w: &mut W, c: &Conn) {
    if let Some((state, ws0, ws1, f0, f1)) = conntrack::tcp_info(c) {
        let pi = w.start(CTA_PROTOINFO);
        let t = w.start(CTA_PROTOINFO_TCP);
        w.attr(CTA_PROTOINFO_TCP_STATE, &[state]);
        w.attr(CTA_PROTOINFO_TCP_WSCALE_ORIGINAL, &[ws0]);
        w.attr(CTA_PROTOINFO_TCP_WSCALE_REPLY, &[ws1]);
        w.attr(CTA_PROTOINFO_TCP_FLAGS_ORIGINAL, &[f0, 0]);
        w.attr(CTA_PROTOINFO_TCP_FLAGS_REPLY, &[f1, 0]);
        w.end(t);
        w.end(pi);
    }
}

/// CTA_COUNTERS_ORIG / REPLY (acct のとき)
fn put_acct(w: &mut W, c: &Conn) {
    if let Some(a) = conntrack::acct(c) {
        for (ty, dir) in [(CTA_COUNTERS_ORIG, 0), (CTA_COUNTERS_REPLY, 1)] {
            let n = w.start(ty);
            w.attr(CTA_COUNTERS_PACKETS, &a[dir][0].to_be_bytes());
            w.attr(CTA_COUNTERS_BYTES, &a[dir][1].to_be_bytes());
            w.end(n);
        }
    }
}

fn be32_of(d: Option<&[u8]>) -> Option<u32> {
    d.filter(|d| d.len() >= 4).map(|d| u32::from_be_bytes(d[..4].try_into().unwrap()))
}

/// CTA_PROTOINFO の TCP の状態 (nlattr_to_tcp: 10 より大きければ EINVAL)
fn parse_tcp_state(a: &[(u16, &[u8])]) -> Result<Option<u8>, i64> {
    let Some(pi) = get(a, CTA_PROTOINFO) else { return Ok(None) };
    let pi = attrs(pi);
    let Some(t) = get(&pi, CTA_PROTOINFO_TCP) else { return Ok(None) };
    let t = attrs(t);
    match get(&t, CTA_PROTOINFO_TCP_STATE).and_then(|d| d.first().copied()) {
        Some(s) if s >= 10 => Err(EINVAL),
        s => Ok(s),
    }
}

/// CT_NEW (ctnetlink_new_conntrack)
fn new_conntrack(a: &[(u16, &[u8])], family: u8, flags: u16, me: u32) -> Result<(), i64> {
    let orig = get(a, CTA_TUPLE_ORIG).map(|d| parse_tuple(d, family)).transpose()?;
    let reply = get(a, CTA_TUPLE_REPLY).map(|d| parse_tuple(d, family)).transpose()?;
    let found = match (orig, reply) {
        (Some(t), _) => conntrack::lookup(&t).filter(|c| c.tuple[0] == t),
        (None, Some(t)) => conntrack::lookup(&t).filter(|c| c.tuple[1] == t),
        _ => None,
    };
    let tcp = parse_tcp_state(a)?;
    let mark = be32_of(get(a, CTA_MARK));
    let status = be32_of(get(a, CTA_STATUS));
    let timeout = be32_of(get(a, CTA_TIMEOUT));
    match found {
        None => {
            if flags & NLM_F_CREATE == 0 {
                return Err(ENOENT);
            }
            let (Some(o), Some(r)) = (orig, reply) else { return Err(EINVAL) };
            if o.proto != r.proto {
                return Err(EINVAL);
            }
            let t = timeout.ok_or(EINVAL)?;
            conntrack::create(o, r, t, status.unwrap_or(0), mark.unwrap_or(0), tcp, me)
        }
        Some(c) => {
            if flags & NLM_F_EXCL != 0 {
                return Err(EEXIST);
            }
            conntrack::change(c.id, timeout, status, mark, tcp, me)
        }
    }
}

/// nfnetlink の subsys 1 の要求 (batch の外)。Ok(true) は dump した (ACK はない)
pub fn request(m: &[u8], me: u32, out: &mut Vec<Vec<u8>>) -> Result<bool, i64> {
    let typ = u16::from_le_bytes([m[4], m[5]]) & 0xff;
    let flags = u16::from_le_bytes([m[6], m[7]]);
    let seq = u32::from_le_bytes(m[8..12].try_into().unwrap());
    let family = m[NLMSG_HDRLEN];
    let a = attrs(&m[NLMSG_HDRLEN + 4..]);
    let dump = flags & NLM_F_DUMP == NLM_F_DUMP;
    let fam_ok = |c: &Conn| family == 0 || (family == AF_INET6) == (c.tuple[0].l3 == 6);
    match typ {
        CT_GET | CT_GET_CTRZERO if dump => {
            let msgs = conntrack::all().iter().filter(|c| fam_ok(c)).map(|c| fill(c, CT_NEW, NLM_F_MULTI, seq, me, 1)).collect();
            dump_out(msgs, seq, me, out);
            Ok(true)
        }
        CT_GET | CT_GET_CTRZERO => {
            let (t, dir) = if let Some(d) = get(&a, CTA_TUPLE_ORIG) {
                (parse_tuple(d, family)?, 0)
            } else if let Some(d) = get(&a, CTA_TUPLE_REPLY) {
                (parse_tuple(d, family)?, 1)
            } else {
                return Err(EINVAL);
            };
            let c = conntrack::lookup(&t).filter(|c| c.tuple[dir] == t).ok_or(ENOENT)?;
            out.push(fill(&c, CT_NEW, 0, seq, me, 2));
            Ok(false)
        }
        CT_DELETE => {
            let tup = if let Some(d) = get(&a, CTA_TUPLE_ORIG) {
                Some((parse_tuple(d, family)?, 0))
            } else if let Some(d) = get(&a, CTA_TUPLE_REPLY) {
                Some((parse_tuple(d, family)?, 1))
            } else {
                None
            };
            match tup {
                Some((t, dir)) => {
                    let c = conntrack::lookup(&t).filter(|c| c.tuple[dir] == t).ok_or(ENOENT)?;
                    if let Some(id) = get(&a, CTA_ID).filter(|d| d.len() >= 4)
                        && u32::from_be_bytes(id[..4].try_into().unwrap()) != c.id
                    {
                        return Err(ENOENT);
                    }
                    conntrack::kill_by(c.id, me);
                }
                None => {
                    for c in conntrack::all().iter().filter(|c| fam_ok(c)) {
                        conntrack::kill_by(c.id, me);
                    }
                }
            }
            Ok(false)
        }
        CT_GET_STATS_CPU if dump => {
            // aios の数は CPU ごとでないので cpu 0 に (invalid のほかは、Linux のハッシュ表の作りの数で aios にない)
            let msgs = (0..crate::smp::online())
                .map(|cpu| {
                    let mut w = W(Vec::new());
                    for ty in CTA_STATS_CPU {
                        w.be32(ty, if ty == CTA_STATS_INVALID && cpu == 0 { conntrack::invalid_count() } else { 0 });
                    }
                    msg(CT_GET_STATS_CPU, NLM_F_MULTI, seq, me, 0, cpu as u16, &w.0)
                })
                .collect();
            dump_out(msgs, seq, me, out);
            Ok(true)
        }
        CT_GET_STATS => {
            let mut w = W(Vec::new());
            w.be32(CTA_STATS_GLOBAL_ENTRIES, conntrack::count() as u32);
            w.be32(CTA_STATS_GLOBAL_MAX_ENTRIES, conntrack::max() as u32);
            out.push(msg(CT_GET_STATS, 0, seq, me, 0, 0, &w.0));
            Ok(false)
        }
        CT_GET_DYING | CT_GET_UNCONFIRMED if dump => {
            dump_out(Vec::new(), seq, me, out);
            Ok(true)
        }
        CT_NEW => new_conntrack(&a, family, flags, me).map(|()| false),
        _ => Err(EOPNOTSUPP),
    }
}

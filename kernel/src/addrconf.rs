// eth0 の IPv6 の番地の自動設定 (Linux の net/ipv6/addrconf.c と ndisc.c の RA のところ)。
//   - link-local: 起こしたときに fe80::/64 + MAC からの EUI-64 (addr_gen_mode 0)。IFA_F_PERMANENT、IFAPROT_KERNEL_LL。
//     道は fe80::/64 (proto kernel、metric 256) とマルチキャストの ff00::/8 (metric 256)
//   - RS (ルーターをさがす): 起こしたときと、RA が来るまで 4 秒から倍に (rtr_solicit_interval、上は 1 時間)。
//     ff02::2 へ、hop limit 255、送り元の MAC (SLLA) つき
//   - RA (ndisc_router_discovery): link-local から hop limit 255 のものだけ。cur hop limit は hop_limit と既定の道の
//     RTAX_HOPLIMIT に、router lifetime があれば既定の道 (proto ra、metric 1024、expires)、MTU、prefix (L なら
//     その prefix の道 (proto kernel、metric 256、expires)、A で 64 ビットなら SLAAC の番地: prefix + EUI-64、
//     IFAPROT_KERNEL_RA、IFA_F_MANAGETEMPADDR、preferred と valid の lifetime)。SLLA は近所の表へ (netlink.rs)
//   - DAD と MLD はしない (QEMU のユーザーネットワークでは同じ番地の人はいない。Linux は 1 秒 tentative で待つ)
// smoltcp には、ここの番地と既定のルーターを net.rs が渡す (近所さがし (NS/NA) と echo の答えは smoltcp がする)
use crate::cell::Bkl;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

pub const IFA_F_MANAGETEMPADDR: u32 = 0x100;
pub const IFA_F_PERMANENT: u32 = 0x80;
const IFAPROT_KERNEL_LL: u8 = 3;
const IFAPROT_KERNEL_RA: u8 = 2;
/// 道の proto (RTPROT_KERNEL、RTPROT_RA)
pub const RTPROT_KERNEL: u8 = 2;
pub const RTPROT_RA: u8 = 9;

/// eth0 の IPv6 の番地
#[derive(Clone)]
pub struct Addr {
    pub addr: u128,
    pub prefix: u8,
    pub flags: u32,
    pub proto: u8,
    /// いつまで (ミリ秒、net::now)。None はいつまでも
    pub valid: Option<u64>,
    pub preferred: Option<u64>,
    /// 作った時刻と、さいごに変えた時刻 (ミリ秒)
    pub created: u64,
    pub updated: u64,
}

/// eth0 の IPv6 の道 (表 254)
#[derive(Clone)]
pub struct Route {
    pub dst: u128,
    pub len: u8,
    /// ゲートウェイ (なければ 0)
    pub gw: u128,
    pub metric: u32,
    pub proto: u8,
    /// RTN_UNICAST (1) か RTN_MULTICAST (5)
    pub typ: u8,
    pub expires: Option<u64>,
    /// RTAX_HOPLIMIT (RA の cur hop limit。0 はなし)
    pub hoplimit: u8,
    /// RTF_* (/proc/net/ipv6_route)
    pub flags: u32,
}

pub const RTF_UP: u32 = 0x1;
pub const RTF_GATEWAY: u32 = 0x2;
pub const RTF_DEFAULT: u32 = 0x10000;
pub const RTF_ADDRCONF: u32 = 0x40000;
pub const RTF_PREFIX_RT: u32 = 0x80000;
pub const RTF_EXPIRES: u32 = 0x400000;
pub const RTF_NONEXTHOP: u32 = 0x200000;
pub const RTF_LOCAL: u32 = 0x8000_0000;

struct Prefix {
    prefix: u128,
    len: u8,
    until: u64,
}

struct Router {
    addr: u128,
    until: u64,
    hoplimit: u8,
}

struct State {
    mac: [u8; 6],
    addrs: Vec<Addr>,
    prefixes: Vec<Prefix>,
    router: Option<Router>,
    hop_limit: u8,
    mtu: u32,
    /// RA を見たか、次の RS の時刻と間
    ra_seen: bool,
    next_rs: u64,
    rs_gap: u64,
}

static ST: Bkl<Option<State>> = Bkl::new(None);
/// 番地か道が変わるたびに増える (net.rs が smoltcp に渡しなおす)
static GEN: AtomicU64 = AtomicU64::new(0);

fn ms() -> u64 {
    crate::net::now().total_millis() as u64
}

fn changed() {
    GEN.fetch_add(1, Ordering::Relaxed);
}

pub fn generation() -> u64 {
    GEN.load(Ordering::Relaxed)
}

/// MAC からの EUI-64 (U/L のビットを返す)
fn eui64(mac: [u8; 6]) -> u128 {
    let b = [mac[0] ^ 2, mac[1], mac[2], 0xff, 0xfe, mac[3], mac[4], mac[5]];
    u64::from_be_bytes(b) as u128
}

/// eth0 を起こした (net::init): link-local の番地を
pub fn init(mac: [u8; 6]) {
    let now = ms();
    let ll = 0xfe80u128 << 112 | eui64(mac);
    let a = Addr { addr: ll, prefix: 64, flags: IFA_F_PERMANENT, proto: IFAPROT_KERNEL_LL, valid: None, preferred: None, created: now, updated: now };
    *ST.get() = Some(State { mac, addrs: alloc::vec![a], prefixes: Vec::new(), router: None, hop_limit: 64, mtu: 1500, ra_seen: false, next_rs: now, rs_gap: 4000 });
    changed();
}

/// eth0 の IPv6 の番地 (link-local が先)
pub fn addrs() -> Vec<Addr> {
    ST.get().as_ref().map_or(Vec::new(), |s| s.addrs.clone())
}

/// eth0 の番地か
pub fn is_mine(a: u128) -> bool {
    a != 0 && ST.get().as_ref().is_some_and(|s| s.addrs.iter().any(|x| x.addr == a))
}

pub fn hop_limit() -> u8 {
    ST.get().as_ref().map_or(64, |s| s.hop_limit)
}

pub fn mtu() -> u32 {
    ST.get().as_ref().map_or(1500, |s| s.mtu)
}

/// 既定のルーター (smoltcp の既定の道)
pub fn router() -> Option<u128> {
    ST.get().as_ref().and_then(|s| s.router.as_ref().map(|r| r.addr))
}

/// link-local (fe80::/10)
pub fn is_link_local(a: u128) -> bool {
    a >> 118 == 0x3fa
}

/// 番地の scope (ipv6_addr_scope: link 0x20、site 0x40 (fec0::/10)、host 0x10 (::1)、global 0)
pub fn scope_of(a: u128) -> u8 {
    if a == 1 {
        0x10
    } else if is_link_local(a) || (a >> 120 == 0xff && (a >> 112) & 0xf == 2) {
        0x20
    } else if a >> 118 == 0x3fb || (a >> 120 == 0xff && (a >> 112) & 0xf == 5) {
        0x40
    } else {
        0
    }
}

/// netlink の scope (RT_SCOPE_*: global 0、site 200、link 253、host 254)
pub fn rt_scope(a: u128) -> u8 {
    match scope_of(a) {
        0x10 => 254,
        0x20 => 253,
        0x40 => 200,
        _ => 0,
    }
}

fn in_prefix(a: u128, p: u128, len: u8) -> bool {
    len == 0 || (a ^ p) >> (128 - len as u32) == 0
}

/// 道の表 (rt6 の main、fib6_walk の順ではない: 呼ぶ側がならべる)
pub fn routes() -> Vec<Route> {
    let Some(s) = ST.get().as_ref() else { return Vec::new() };
    let mut v = Vec::new();
    v.push(Route { dst: 0xfe80u128 << 112, len: 64, gw: 0, metric: 256, proto: RTPROT_KERNEL, typ: 1, expires: None, hoplimit: 0, flags: RTF_UP });
    for p in &s.prefixes {
        v.push(Route {
            dst: p.prefix,
            len: p.len,
            gw: 0,
            metric: 256,
            proto: RTPROT_KERNEL,
            typ: 1,
            expires: Some(p.until),
            hoplimit: 0,
            flags: RTF_UP | RTF_EXPIRES | RTF_ADDRCONF | RTF_PREFIX_RT,
        });
    }
    if let Some(r) = &s.router {
        v.push(Route {
            dst: 0,
            len: 0,
            gw: r.addr,
            metric: 1024,
            proto: RTPROT_RA,
            typ: 1,
            expires: Some(r.until),
            hoplimit: r.hoplimit,
            flags: RTF_UP | RTF_GATEWAY | RTF_DEFAULT | RTF_ADDRCONF | RTF_EXPIRES,
        });
    }
    v.push(Route { dst: 0xffu128 << 120, len: 8, gw: 0, metric: 256, proto: RTPROT_KERNEL, typ: 5, expires: None, hoplimit: 0, flags: RTF_UP });
    v
}

/// あて先 dst へのみち (fib6_lookup と ip6_route_output): 自分の番地は None (local)、ほかは道 (長い prefix が先、同じなら
/// metric の小さいもの)
pub fn lookup(dst: u128) -> Option<Route> {
    routes().into_iter().filter(|r| in_prefix(dst, r.dst, r.len)).max_by_key(|r| (r.len, u32::MAX - r.metric))
}

/// 送り元のえらびかた (ipv6_dev_get_saddr: RFC 6724 のうち、scope のあうもの、deprecated でないもの、prefix が長く
/// あうもの): link-local やマルチキャストの link-local あてなら link-local、ほかは site か global のもの (なければ
/// link-local)
pub fn source_for(dst: u128) -> Option<u128> {
    let s = ST.get().as_ref()?;
    let now = ms();
    let link = matches!(scope_of(dst), 0x10 | 0x20);
    s.addrs
        .iter()
        .max_by_key(|a| ((scope_of(a.addr) == 0x20) == link, a.preferred.is_none_or(|t| t > now), common_bits(a.addr, dst)))
        .map(|a| a.addr)
}

fn common_bits(a: u128, b: u128) -> u32 {
    (a ^ b).leading_zeros()
}

/// 時間で変わるもの (net::poll から): lifetime の切れた番地・prefix・ルーター、送る RS (Ethernet のフレーム)
pub fn tick() -> Option<Vec<u8>> {
    let now = ms();
    let st = ST.get().as_mut()?;
    let mut ch = false;
    let before = st.addrs.len();
    st.addrs.retain(|a| a.valid.is_none_or(|t| t > now));
    ch |= st.addrs.len() != before;
    let before = st.prefixes.len();
    st.prefixes.retain(|p| p.until > now);
    ch |= st.prefixes.len() != before;
    if st.router.as_ref().is_some_and(|r| r.until <= now) {
        st.router = None;
        ch = true;
    }
    let rs = if !st.ra_seen && now >= st.next_rs {
        st.next_rs = now + st.rs_gap;
        st.rs_gap = (st.rs_gap * 2).min(3_600_000);
        Some(rs_frame(st.mac, st.addrs[0].addr))
    } else {
        None
    };
    if ch {
        changed();
    }
    rs
}

/// ICMPv6 のチェックサム (送り元とあて先の擬似ヘッダーと、中身)
pub fn icmp6_sum(src: u128, dst: u128, body: &[u8]) -> u16 {
    let mut s: u64 = 0;
    for w in src.to_be_bytes().chunks(2).chain(dst.to_be_bytes().chunks(2)) {
        s += u16::from_be_bytes([w[0], w[1]]) as u64;
    }
    s += body.len() as u64 + 58;
    for c in body.chunks(2) {
        s += u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]) as u64;
    }
    while s >> 16 != 0 {
        s = (s & 0xffff) + (s >> 16);
    }
    !(s as u16)
}

/// Ethernet + IPv6 + ICMPv6 のフレーム (hop limit 255 の近所さがしのもの)
fn nd_frame(mac: [u8; 6], dmac: [u8; 6], src: u128, dst: u128, mut icmp: Vec<u8>) -> Vec<u8> {
    let sum = icmp6_sum(src, dst, &icmp);
    icmp[2..4].copy_from_slice(&sum.to_be_bytes());
    let mut f = Vec::with_capacity(54 + icmp.len());
    f.extend_from_slice(&dmac);
    f.extend_from_slice(&mac);
    f.extend_from_slice(&[0x86, 0xdd, 0x60, 0, 0, 0]);
    f.extend_from_slice(&(icmp.len() as u16).to_be_bytes());
    f.extend_from_slice(&[58, 255]);
    f.extend_from_slice(&src.to_be_bytes());
    f.extend_from_slice(&dst.to_be_bytes());
    f.extend_from_slice(&icmp);
    f
}

/// RS (ndisc_send_rs): ff02::2 へ、SLLA つき
fn rs_frame(mac: [u8; 6], ll: u128) -> Vec<u8> {
    let mut icmp = alloc::vec![133, 0, 0, 0, 0, 0, 0, 0, 1, 1];
    icmp.extend_from_slice(&mac);
    nd_frame(mac, [0x33, 0x33, 0, 0, 0, 2], ll, 0xff02u128 << 112 | 2, icmp)
}

/// eth0 から来た IPv6 のフレーム: RA なら読む (ほかは見るだけ。smoltcp にもわたる)
pub fn rx(f: &[u8]) {
    if f.len() < 54 + 16 || f[12..14] != [0x86, 0xdd] || f[20] != 58 || f[21] != 255 {
        return;
    }
    let ip = &f[14..];
    let plen = u16::from_be_bytes([ip[4], ip[5]]) as usize;
    if ip.len() < 40 + plen || plen < 16 {
        return;
    }
    let src = u128::from_be_bytes(ip[8..24].try_into().unwrap());
    let dst = u128::from_be_bytes(ip[24..40].try_into().unwrap());
    let icmp = &ip[40..40 + plen];
    if icmp[0] != 134 || icmp[1] != 0 || !is_link_local(src) || icmp6_sum(src, dst, icmp) != 0 {
        return;
    }
    ra(src, icmp);
}

/// RA を読む (ndisc_router_discovery と addrconf_prefix_rcv)
fn ra(src: u128, m: &[u8]) {
    let now = ms();
    let Some(st) = ST.get().as_mut() else { return };
    st.ra_seen = true;
    let cur_hop = m[4];
    let lifetime = u16::from_be_bytes([m[6], m[7]]) as u64;
    if cur_hop != 0 {
        st.hop_limit = cur_hop;
    }
    if lifetime == 0 {
        if st.router.as_ref().is_some_and(|r| r.addr == src) {
            st.router = None;
        }
    } else {
        st.router = Some(Router { addr: src, until: now + lifetime * 1000, hoplimit: cur_hop });
    }
    let mut opts = &m[16..];
    while opts.len() >= 8 {
        let (ty, len) = (opts[0], opts[1] as usize * 8);
        if len == 0 || len > opts.len() {
            break;
        }
        let o = &opts[..len];
        match ty {
            // SLLA: 近所の表に (ルーター)
            1 if len >= 8 => {
                let mac: [u8; 6] = o[2..8].try_into().unwrap();
                crate::netlink::nd_router(src, mac);
            }
            3 if len >= 32 => prefix_info(st, o, now),
            5 if len >= 8 => {
                let mtu = u32::from_be_bytes(o[4..8].try_into().unwrap());
                if (1280..=1500).contains(&mtu) {
                    st.mtu = mtu;
                }
            }
            _ => {}
        }
        opts = &opts[len..];
    }
    changed();
}

/// prefix information (addrconf_prefix_rcv)
fn prefix_info(st: &mut State, o: &[u8], now: u64) {
    let len = o[2];
    let (onlink, auto) = (o[3] & 0x80 != 0, o[3] & 0x40 != 0);
    let valid = u32::from_be_bytes(o[4..8].try_into().unwrap()) as u64;
    let pref = u32::from_be_bytes(o[8..12].try_into().unwrap()) as u64;
    let prefix = u128::from_be_bytes(o[16..32].try_into().unwrap());
    if len > 128 || is_link_local(prefix) || prefix >> 120 == 0xff || pref > valid {
        return;
    }
    let prefix = if len == 0 { 0 } else { prefix >> (128 - len as u32) << (128 - len as u32) };
    let life = |v: u64| if v == 0xffff_ffff { u64::MAX } else { now + v * 1000 };
    if onlink {
        st.prefixes.retain(|p| !(p.prefix == prefix && p.len == len));
        if valid != 0 {
            st.prefixes.push(Prefix { prefix, len, until: life(valid) });
        }
    }
    if auto && len == 64 {
        let addr = prefix | eui64(st.mac);
        match st.addrs.iter_mut().find(|a| a.addr == addr) {
            Some(a) => {
                // 2 時間より短くは縮めない (RFC 4862 5.5.3 e) のは、すでに 2 時間より短いときと、新しいものが長いとき
                let left = a.valid.map_or(u64::MAX, |t| t.saturating_sub(now));
                if valid * 1000 > 7_200_000 || valid * 1000 > left {
                    a.valid = Some(life(valid));
                } else if left > 7_200_000 {
                    a.valid = Some(now + 7_200_000);
                }
                a.preferred = Some(life(pref));
                a.updated = now;
            }
            None if valid != 0 => st.addrs.push(Addr {
                addr,
                prefix: 64,
                flags: IFA_F_MANAGETEMPADDR,
                proto: IFAPROT_KERNEL_RA,
                valid: Some(life(valid)),
                preferred: Some(life(pref)),
                created: now,
                updated: now,
            }),
            None => {}
        }
    }
}

/// のこりの秒 (ifa_cacheinfo の prefered と valid: いつまでもは INFINITY_LIFE_TIME)
pub fn left_secs(t: Option<u64>) -> u32 {
    match t {
        None => u32::MAX,
        Some(u64::MAX) => u32::MAX,
        Some(t) => (t.saturating_sub(ms()) / 1000) as u32,
    }
}

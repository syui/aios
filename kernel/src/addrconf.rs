// eth0 の IPv6 の番地の自動設定 (Linux の net/ipv6/addrconf.c と ndisc.c の RA のところ)。
//   - link-local: 起こしたときに fe80::/64 + MAC からの EUI-64 (addr_gen_mode 0)。IFA_F_PERMANENT、IFAPROT_KERNEL_LL。
//     道は fe80::/64 (proto kernel、metric 256) とマルチキャストの ff00::/8 (metric 256)
//   - RS (ルーターをさがす): 起こしたときと、RA が来るまで 4 秒から倍に (rtr_solicit_interval、上は 1 時間)。
//     ff02::2 へ、hop limit 255、送り元の MAC (SLLA) つき
//   - RA (ndisc_router_discovery): link-local から hop limit 255 のものだけ。cur hop limit は hop_limit と既定の道の
//     RTAX_HOPLIMIT に、router lifetime があれば既定の道 (proto ra、metric 1024、expires)、MTU、prefix (L なら
//     その prefix の道 (proto kernel、metric 256、expires)、A で 64 ビットなら SLAAC の番地: prefix + EUI-64、
//     IFAPROT_KERNEL_RA、IFA_F_MANAGETEMPADDR、preferred と valid の lifetime)。SLLA は近所の表へ (netlink.rs)
//   - DAD (addrconf_dad_start / addrconf_dad_work / addrconf_dad_completed): 新しい番地 (link-local、SLAAC、nodad でない
//     ip -6 addr add) は IFA_F_TENTATIVE で、0〜1 秒 (rtr_solicit_delay) 待って NS を 1 つ (dad_transmits: 送り元 ::、
//     あて先は solicited-node マルチキャスト、nonce つき: enhanced_dad)、1 秒 (RetransTimer) 答えがなければ tentative を
//     はずす。そのあいだは送り元にえらばず、自分のものとして受けず (bind は EADDRNOTAVAIL)、smoltcp にも渡さない。その
//     番地の NA か、ほかの人 (nonce のちがう) の DAD の NS が来たら IFA_F_DADFAILED。RS は link-local の DAD のあとで
//   - MLD はしない (QEMU のユーザーネットワークではだれも聞いていない)
//   - netlink で足す番地と道 (ip -6 addr add / route add: netlink.rs): 番地には prefix の道 (proto kernel、metric 256。
//     同じ prefix の番地がなくなれば消す)、道はゲートウェイを通るもの (smoltcp の道の表に)
// smoltcp には、ここの番地と既定のルーターを net.rs が渡す (近所さがし (NS/NA) と echo の答えは smoltcp がする)
use crate::cell::Bkl;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

pub const IFA_F_MANAGETEMPADDR: u32 = 0x100;
pub const IFA_F_PERMANENT: u32 = 0x80;
pub const IFA_F_TENTATIVE: u32 = 0x40;
const IFA_F_DADFAILED: u32 = 0x08;
const IFA_F_NODAD: u32 = 0x02;
/// /proc/sys/net/ipv6/conf の口ごとの値 (Linux の ipv6_devconf。addr_gen_mode、disable_ipv6、forwarding は読むだけで
/// sysctl.rs に)。効くのは eth0 のもの (と DAD の all の accept_dad): hop_limit と mtu (RA でも変わる)、accept_ra (0 なら
/// RA を見ない)、autoconf (0 なら SLAAC の番地を作らない)、accept_dad (all か eth0 が 1 以上なら DAD)、dad_transmits
/// (DAD の NS の数)。use_tempaddr は覚えるだけ (一時アドレスはまだない)
#[derive(Clone, Copy)]
pub struct DevConf {
    pub accept_dad: i32,
    pub accept_ra: i32,
    pub autoconf: i32,
    pub dad_transmits: i32,
    pub hop_limit: i32,
    pub mtu: i32,
    pub use_tempaddr: i32,
}

/// CONF の並び: all、default、lo、eth0
pub const CONF_ALL: usize = 0;
pub const CONF_LO: usize = 2;
pub const CONF_ETH: usize = 3;

const fn devconf(accept_dad: i32, mtu: i32, use_tempaddr: i32) -> DevConf {
    DevConf { accept_dad, accept_ra: 1, autoconf: 1, dad_transmits: 1, hop_limit: 64, mtu, use_tempaddr }
}

/// Linux の既定: all は accept_dad 0、default と eth0 は 1、lo は -1 (と mtu 65536、use_tempaddr -1)
pub static CONF: Bkl<[DevConf; 4]> = Bkl::new([devconf(0, 1280, 0), devconf(1, 1280, 0), devconf(-1, 65536, -1), devconf(1, 1500, 0)]);

/// 口の MTU (conf の mtu の上限。all と default はなし)
pub fn dev_mtu(i: usize) -> i32 {
    match i {
        CONF_LO => 65536,
        CONF_ETH => 1500,
        _ => i32::MAX,
    }
}

/// DAD をするか (addrconf_dad_begin: all と口の accept_dad のどちらかが 1 以上)
fn dad_wanted() -> bool {
    let c = CONF.get();
    c[CONF_ALL].accept_dad >= 1 || c[CONF_ETH].accept_dad >= 1
}

/// RetransTimer (近所さがしの問い合わせの間、ミリ秒) と、はじめの NS の前に待つ上限 (rtr_solicit_delay)
const RETRANS_MS: u64 = 1000;
const DAD_DELAY_MS: u32 = 1000;
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
    /// DAD のとちゅう: 次にすること (NS を送るか、終える) の時刻と、のこりの NS の数。nonce は NS にのせる 6 バイト
    dad: Option<(u64, u8)>,
    nonce: u64,
}

impl Addr {
    /// 使える番地か (DAD がおわって、しくじっていない)
    pub fn usable(&self) -> bool {
        self.flags & (IFA_F_TENTATIVE | IFA_F_DADFAILED) == 0
    }
}

/// 新しい番地の DAD をはじめる (addrconf_dad_kick): 0〜1 秒あとにはじめの NS (dad_transmits が 0 なら NS なしでおえる)。
/// DAD をしない (accept_dad が all も口も 1 より小さい) なら None
fn dad_start(now: u64) -> (Option<(u64, u8)>, u64) {
    if !dad_wanted() {
        return (None, 0);
    }
    let delay = (crate::rand::u32() % DAD_DELAY_MS) as u64;
    let mut nonce = 0;
    while nonce == 0 {
        nonce = crate::rand::u64() & 0xffff_ffff_ffff;
    }
    (Some((now + delay, CONF.get()[CONF_ETH].dad_transmits as u8)), nonce)
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

/// netlink で足した番地の prefix の道 (addrconf_prefix_route。None はいつまでも)
struct UPrefix {
    prefix: u128,
    len: u8,
    expires: Option<u64>,
}

struct State {
    mac: [u8; 6],
    addrs: Vec<Addr>,
    prefixes: Vec<Prefix>,
    uprefixes: Vec<UPrefix>,
    /// netlink で足した道 (ip -6 route add)
    uroutes: Vec<Route>,
    router: Option<Router>,
    /// RA を見たか、次の RS の時刻と間
    ra_seen: bool,
    next_rs: u64,
    rs_gap: u64,
    /// RS を出したか (IF_RS_SENT) と、そのあとに RA を受けたか (IF_RA_RCVD)
    rs_sent: bool,
    ra_rcvd: bool,
    /// packet too big で知ったあて先ごとの path MTU (rt6_update_pmtu の例外の道): (あて先, MTU, いつまで)
    pmtu: Vec<(u128, u32, u64)>,
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
    let (dad, nonce) = dad_start(now);
    let flags = IFA_F_PERMANENT | if dad.is_some() { IFA_F_TENTATIVE } else { 0 };
    let a = Addr { addr: ll, prefix: 64, flags, proto: IFAPROT_KERNEL_LL, valid: None, preferred: None, created: now, updated: now, dad, nonce };
    // RS は link-local の DAD がおわってから (next_rs はそのときに)
    let next_rs = if dad.is_some() { u64::MAX } else { now };
    *ST.get() = Some(State { mac, addrs: alloc::vec![a], prefixes: Vec::new(), uprefixes: Vec::new(), uroutes: Vec::new(), router: None, ra_seen: false, next_rs, rs_gap: 4000, rs_sent: false, ra_rcvd: false, pmtu: Vec::new() });
    changed();
}

/// eth0 の IPv6 の番地 (link-local が先。DAD のとちゅうのものも)
pub fn addrs() -> Vec<Addr> {
    ST.get().as_ref().map_or(Vec::new(), |s| s.addrs.clone())
}

/// eth0 の使える番地 (DAD のとちゅうとしくじったものをのぞく: smoltcp に渡すもの、local の道)
pub fn usable_addrs() -> Vec<Addr> {
    ST.get().as_ref().map_or(Vec::new(), |s| s.addrs.iter().filter(|x| x.usable()).cloned().collect())
}

/// eth0 の (使える) 番地か: DAD のとちゅうの番地あては受けず、bind もできない (ipv6_chk_addr の IFA_F_TENTATIVE)
pub fn is_mine(a: u128) -> bool {
    a != 0 && ST.get().as_ref().is_some_and(|s| s.addrs.iter().any(|x| x.addr == a && x.usable()))
}

/// eth0 の inet6_dev の if_flags (rtnetlink の IFLA_INET6_FLAGS): IF_READY (0x80000000)、IF_RS_SENT (0x10)、IF_RA_RCVD (0x20)
pub fn if_flags() -> u32 {
    ST.get().as_ref().map_or(0x8000_0000, |s| 0x8000_0000 | if s.rs_sent { 0x10 } else { 0 } | if s.ra_rcvd { 0x20 } else { 0 })
}

pub fn hop_limit() -> u8 {
    CONF.get()[CONF_ETH].hop_limit as u8
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
    for p in &s.uprefixes {
        v.push(Route {
            dst: p.prefix,
            len: p.len,
            gw: 0,
            metric: 256,
            proto: RTPROT_KERNEL,
            typ: 1,
            expires: p.expires,
            hoplimit: 0,
            flags: RTF_UP | if p.expires.is_some() { RTF_EXPIRES } else { 0 },
        });
    }
    v.extend(s.uroutes.iter().cloned());
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

/// RFC 6724 の scope (__ipv6_addr_src_scope: マルチキャストはその 4 ビット、::1 は 1、link-local 2、site-local 5、ほか
/// (ULA の fc00::/7 も) global の 14)
fn rfc_scope(a: u128) -> i32 {
    if a >> 120 == 0xff {
        ((a >> 112) & 0xf) as i32
    } else if a == 1 {
        1
    } else if is_link_local(a) {
        2
    } else if a >> 118 == 0x3fb {
        5
    } else {
        14
    }
}

/// 送り元のえらびかた (ipv6_dev_get_saddr: RFC 6724 のうち、rule 1 同じ番地、rule 2 scope (あて先の scope 以上で狭い
/// ものを。なければ広いものを)、rule 3 deprecated でないもの、rule 8 prefix の長くあうもの)
pub fn source_for(dst: u128) -> Option<u128> {
    let s = ST.get().as_ref()?;
    let now = ms();
    let ds = rfc_scope(dst);
    s.addrs
        .iter()
        .filter(|a| a.usable())
        .max_by_key(|a| {
            let sc = rfc_scope(a.addr);
            let r2 = if sc >= ds { (1, -sc) } else { (0, sc) };
            (a.addr == dst, r2, a.preferred.is_none_or(|t| t > now), common_bits(a.addr, dst))
        })
        .map(|a| a.addr)
}

/// いつまで (lifetime の秒。INFINITY_LIFE_TIME は None)
fn until(now: u64, secs: u32) -> Option<u64> {
    (secs != u32::MAX).then(|| now + secs as u64 * 1000)
}

/// その番地 (eth0 のもの)
pub fn find(a: u128) -> Option<Addr> {
    ST.get().as_ref()?.addrs.iter().find(|x| x.addr == a).cloned()
}

/// netlink の番地の flags のうち、持てるもの (inet6_rtm_newaddr: NODAD、HOMEADDRESS、MANAGETEMPADDR、NOPREFIXROUTE、
/// MCAUTOJOIN、OPTIMISTIC)
pub const IFA_USER_FLAGS: u32 = 0x02 | 0x10 | 0x100 | 0x200 | 0x400 | 0x04;
const IFA_F_NOPREFIXROUTE: u32 = 0x200;

/// ip -6 addr add (inet6_addr_add): 番地 (nodad でなければ DAD をして、おわるまで tentative) と、prefix の道がなければ
/// 作る。valid がいつまでもなら IFA_F_PERMANENT。作った prefix の道があれば true
pub fn add_addr(addr: u128, plen: u8, flags: u32, preferred: u32, valid: u32, proto: u8) -> bool {
    let now = ms();
    let Some(st) = ST.get().as_mut() else { return false };
    let mut fl = flags & IFA_USER_FLAGS;
    if valid == u32::MAX {
        fl |= IFA_F_PERMANENT;
    }
    let (dad, nonce) = if fl & IFA_F_NODAD == 0 { dad_start(now) } else { (None, 0) };
    if dad.is_some() {
        fl |= IFA_F_TENTATIVE;
    }
    st.addrs.push(Addr { addr, prefix: plen, flags: fl, proto, valid: until(now, valid), preferred: until(now, preferred), created: now, updated: now, dad, nonce });
    let made = prefix_route(st, addr, plen, fl, until(now, valid));
    changed();
    made
}

/// prefix の道 (addrconf_prefix_route): なければ作り (true)、あれば時間切れをいまのものに
fn prefix_route(st: &mut State, addr: u128, plen: u8, flags: u32, expires: Option<u64>) -> bool {
    if flags & IFA_F_NOPREFIXROUTE != 0 || plen == 128 {
        return false;
    }
    let p = if plen == 0 { 0 } else { addr >> (128 - plen as u32) << (128 - plen as u32) };
    // fe80::/64 と RA の prefix の道はもうある
    if (p == 0xfe80u128 << 112 && plen == 64) || st.prefixes.iter().any(|x| x.prefix == p && x.len == plen) {
        return false;
    }
    match st.uprefixes.iter_mut().find(|x| x.prefix == p && x.len == plen) {
        Some(x) => {
            x.expires = expires;
            false
        }
        None => {
            st.uprefixes.push(UPrefix { prefix: p, len: plen, expires });
            true
        }
    }
}

/// ip -6 addr replace (inet6_addr_modify): flags と lifetime を変え、prefix の道の時間切れも
pub fn modify_addr(addr: u128, flags: u32, preferred: u32, valid: u32) {
    let now = ms();
    let Some(st) = ST.get().as_mut() else { return };
    let Some(a) = st.addrs.iter_mut().find(|x| x.addr == addr) else { return };
    let mut fl = (a.flags & !IFA_USER_FLAGS & !IFA_F_PERMANENT) | (flags & IFA_USER_FLAGS);
    // tentative と dadfailed はそのまま (IFA_USER_FLAGS の外)
    if valid == u32::MAX {
        fl |= IFA_F_PERMANENT;
    }
    a.flags = fl;
    a.valid = until(now, valid);
    a.preferred = until(now, preferred);
    a.updated = now;
    let (plen, exp) = (a.prefix, a.valid);
    prefix_route(st, addr, plen, fl, exp);
    changed();
}

/// ip -6 addr del (inet6_addr_del): 番地と、同じ prefix の番地がもうなければ prefix の道も (cleanup_prefix_route)。
/// (消した番地, 消した prefix の道があれば true)。なければ None
pub fn del_addr(addr: u128, plen: u8) -> Option<(Addr, bool)> {
    let st = ST.get().as_mut()?;
    let i = st.addrs.iter().position(|x| x.addr == addr && x.prefix == plen)?;
    let a = st.addrs.remove(i);
    let p = if plen == 0 { 0 } else { addr >> (128 - plen as u32) << (128 - plen as u32) };
    let others = st.addrs.iter().any(|x| x.prefix == plen && in_prefix(x.addr, p, plen) && x.flags & IFA_F_NOPREFIXROUTE == 0);
    let mut gone = false;
    if !others && let Some(j) = st.uprefixes.iter().position(|x| x.prefix == p && x.len == plen) {
        st.uprefixes.remove(j);
        gone = true;
    }
    changed();
    Some((a, gone))
}

/// ip -6 route add (ゲートウェイを通る unicast)。同じあて先と長さと metric の道があれば、replace ならそれと入れかえ、
/// ほかは EEXIST
pub fn add_route(r: Route, replace: bool) -> Result<Option<Route>, i64> {
    const EEXIST: i64 = 17;
    let same = |x: &Route| x.dst == r.dst && x.len == r.len && x.metric == r.metric;
    let old = routes().into_iter().find(same);
    let st = ST.get().as_mut().ok_or(-19i64)?;
    let mut replaced = None;
    if let Some(o) = old {
        if !replace {
            return Err(-EEXIST);
        }
        let Some(i) = st.uroutes.iter().position(same) else { return Err(-95) };
        replaced = Some(st.uroutes.remove(i));
        let _ = o;
    }
    st.uroutes.push(r);
    changed();
    Ok(replaced)
}

/// ip -6 route del: 来たもの (ゲートウェイ、metric、proto) にあう道のうち metric の小さいもの。netlink で足した道と、RA の
/// 既定の道 (つぎの RA でまたできる) を消せる。ほかの道は EOPNOTSUPP、なければ ESRCH
pub fn del_route(dst: u128, len: u8, gw: u128, metric: u32, proto: u8) -> Result<Route, i64> {
    let hit = |x: &Route| x.dst == dst && x.len == len && (gw == 0 || x.gw == gw) && (metric == 0 || x.metric == metric) && (proto == 0 || x.proto == proto);
    let r = routes().into_iter().filter(|x| x.typ == 1 && hit(x)).min_by_key(|x| x.metric).ok_or(-3i64)?;
    let st = ST.get().as_mut().ok_or(-3i64)?;
    if let Some(i) = st.uroutes.iter().position(|x| x.dst == r.dst && x.len == r.len && x.metric == r.metric && x.gw == r.gw) {
        st.uroutes.remove(i);
    } else if r.len == 0 && r.proto == RTPROT_RA {
        st.router = None;
    } else {
        return Err(-95);
    }
    changed();
    Ok(r)
}

/// smoltcp に渡す道 (あて先、長さ、ゲートウェイ): netlink で足した道の、prefix ごとに metric のいちばん小さいもの
pub fn user_routes() -> Vec<(u128, u8, u128)> {
    let Some(s) = ST.get().as_ref() else { return Vec::new() };
    let mut v: Vec<&Route> = s.uroutes.iter().collect();
    v.sort_by_key(|r| (r.dst, r.len, r.metric));
    v.dedup_by_key(|r| (r.dst, r.len));
    v.iter().map(|r| (r.dst, r.len, r.gw)).collect()
}

fn common_bits(a: u128, b: u128) -> u32 {
    (a ^ b).leading_zeros()
}

/// path MTU の例外の道がのこる時間 (ip6_rt_mtu_expires: 10 分)
const PMTU_EXPIRES_MS: u64 = 600_000;

/// packet too big で知ったそのあて先の path MTU と、いつまで (なければ None)
pub fn pmtu(dst: u128) -> Option<(u32, u64)> {
    let now = ms();
    ST.get().as_ref()?.pmtu.iter().find(|x| x.0 == dst && x.2 > now).map(|x| (x.1, x.2))
}

/// そのあて先への path MTU (ip6_dst_mtu): 自分あて (lo) は 65536、ほかは packet too big で知ったものか口の mtu
pub fn path_mtu(dst: u128) -> u32 {
    if dst == 1 || is_mine(dst) {
        return 65536;
    }
    pmtu(dst).map_or(CONF.get()[CONF_ETH].mtu as u32, |x| x.0)
}

/// packet too big (__ip6_rt_update_pmtu): 1280 より小さいものと、いまより小さくならないものは見ない。道のあるあて先
/// だけ、10 分の例外の道に
pub fn pmtu_update(dst: u128, mtu: u32) {
    if mtu < 1280 || mtu >= path_mtu(dst) || lookup(dst).is_none() {
        return;
    }
    let until = ms() + PMTU_EXPIRES_MS;
    let Some(st) = ST.get().as_mut() else { return };
    st.pmtu.retain(|x| x.0 != dst);
    st.pmtu.push((dst, mtu, until));
}

/// 時間で変わるもの (net::poll から): lifetime の切れた番地・prefix・ルーター、DAD (NS を送る、おえる)、送る RS。
/// 送るもの (Ethernet のフレーム) を返す
pub fn tick() -> Vec<Vec<u8>> {
    let now = ms();
    let Some(st) = ST.get().as_mut() else { return Vec::new() };
    let mut out = Vec::new();
    let mut ch = false;
    for a in st.addrs.iter_mut() {
        let Some((at, left)) = a.dad else { continue };
        if at > now {
            continue;
        }
        if left > 0 {
            out.push(dad_ns_frame(st.mac, a.addr, a.nonce));
            a.dad = Some((now + RETRANS_MS, left - 1));
        } else {
            // addrconf_dad_completed: 使えるように。link-local なら RS を
            a.dad = None;
            a.flags &= !IFA_F_TENTATIVE;
            if is_link_local(a.addr) {
                st.next_rs = now;
            }
            ch = true;
        }
    }
    let before = st.addrs.len();
    st.addrs.retain(|a| a.valid.is_none_or(|t| t > now));
    ch |= st.addrs.len() != before;
    st.pmtu.retain(|x| x.2 > now);
    let before = st.prefixes.len();
    st.prefixes.retain(|p| p.until > now);
    ch |= st.prefixes.len() != before;
    let before = st.uprefixes.len();
    st.uprefixes.retain(|p| p.expires.is_none_or(|t| t > now));
    ch |= st.uprefixes.len() != before;
    if st.router.as_ref().is_some_and(|r| r.until <= now) {
        st.router = None;
        ch = true;
    }
    if !st.ra_seen && now >= st.next_rs
        && let Some(ll) = st.addrs.iter().find(|a| is_link_local(a.addr) && a.usable())
    {
        st.next_rs = now + st.rs_gap;
        st.rs_gap = (st.rs_gap * 2).min(3_600_000);
        st.rs_sent = true;
        out.push(rs_frame(st.mac, ll.addr));
    }
    if ch {
        changed();
    }
    out
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

/// DAD の NS (ndisc_send_ns): 送り元 ::、あて先はその番地の solicited-node マルチキャスト (ff02::1:ffXX:XXXX)、
/// SLLA なし、nonce (6 バイト) つき
fn dad_ns_frame(mac: [u8; 6], target: u128, nonce: u64) -> Vec<u8> {
    let mut icmp = alloc::vec![135, 0, 0, 0, 0, 0, 0, 0];
    icmp.extend_from_slice(&target.to_be_bytes());
    icmp.extend_from_slice(&[14, 1]);
    icmp.extend_from_slice(&nonce.to_be_bytes()[2..]);
    let low = target as u32 & 0xff_ffff;
    let dst = 0xff02u128 << 112 | 1 << 32 | 0xff00_0000 | low as u128;
    let dmac = [0x33, 0x33, 0xff, (low >> 16) as u8, (low >> 8) as u8, low as u8];
    nd_frame(mac, dmac, 0, dst, icmp)
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
    if icmp[1] != 0 || icmp6_sum(src, dst, icmp) != 0 {
        return;
    }
    match icmp[0] {
        134 if is_link_local(src) => ra(src, icmp),
        135 | 136 if icmp.len() >= 24 => dad_check(icmp[0], src, &f[6..12], icmp),
        _ => {}
    }
}

/// DAD のとちゅうの番地への NA (ほかの人がもう使っている) か、ほかの人の DAD の NS (送り元 ::、nonce が自分のもので
/// ない: ndisc_recv_ns) が来たら、しくじり (addrconf_dad_failure: IFA_F_DADFAILED、tentative のまま)
fn dad_check(ty: u8, src: u128, smac: &[u8], m: &[u8]) {
    let target = u128::from_be_bytes(m[8..24].try_into().unwrap());
    let Some(st) = ST.get().as_mut() else { return };
    let Some(a) = st.addrs.iter_mut().find(|a| a.addr == target && a.dad.is_some()) else { return };
    if ty == 135 {
        if src != 0 {
            return;
        }
        // 自分の NS が返ってきたもの (nonce が同じ) は見ない
        let mut opts = &m[24..];
        while opts.len() >= 8 {
            let len = opts[1] as usize * 8;
            if len == 0 || len > opts.len() {
                break;
            }
            if opts[0] == 14 && len == 8 && opts[2..8] == a.nonce.to_be_bytes()[2..] {
                return;
            }
            opts = &opts[len..];
        }
    }
    a.dad = None;
    a.flags |= IFA_F_DADFAILED;
    let mac = alloc::format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", smac[0], smac[1], smac[2], smac[3], smac[4], smac[5]);
    crate::kmsg::push(&alloc::format!("IPv6: eth0: IPv6 duplicate address {} used by {} detected!\n", smoltcp::wire::Ipv6Address::from_bits(target), mac));
    changed();
}

/// RA を読む (ndisc_router_discovery と addrconf_prefix_rcv)
fn ra(src: u128, m: &[u8]) {
    let now = ms();
    // accept_ra が 0 (より小さい) なら RA を見ない (ipv6_accept_ra)
    if CONF.get()[CONF_ETH].accept_ra <= 0 {
        return;
    }
    let Some(st) = ST.get().as_mut() else { return };
    st.ra_seen = true;
    // ndisc_router_discovery: RS を出したあとに受けた RA
    if st.rs_sent {
        st.ra_rcvd = true;
    }
    let cur_hop = m[4];
    let lifetime = u16::from_be_bytes([m[6], m[7]]) as u64;
    if cur_hop != 0 {
        CONF.get()[CONF_ETH].hop_limit = cur_hop as i32;
    }
    if lifetime == 0 {
        if st.router.as_ref().is_some_and(|r| r.addr == src) {
            st.router = None;
        }
    } else {
        st.router = Some(Router { addr: src, until: now + lifetime * 1000, hoplimit: cur_hop });
    }
    // ユーザーのプログラムに渡す項目 (ndisc_is_useropt: prefix、RDNSS、DNSSL、6CO、captive portal、PREF64)。RA を読み
    // おえてから、1 つずつ RTM_NEWNDUSEROPT で (ndisc_ra_useropt)
    let mut useropts: Vec<Vec<u8>> = Vec::new();
    // lifetime を新しくした SLAAC の番地 (RTM_NEWADDR で知らせる)
    let mut refreshed: Vec<Addr> = Vec::new();
    let mut opts = &m[16..];
    while opts.len() >= 8 {
        let (ty, len) = (opts[0], opts[1] as usize * 8);
        if len == 0 || len > opts.len() {
            break;
        }
        let o = &opts[..len];
        if matches!(ty, 3 | 25 | 31 | 34 | 37 | 38) {
            useropts.push(o.to_vec());
        }
        match ty {
            // SLLA: 近所の表に (ルーター)
            1 if len >= 8 => {
                let mac: [u8; 6] = o[2..8].try_into().unwrap();
                crate::netlink::nd_router(src, mac);
            }
            3 if len >= 32 => {
                if let Some(a) = prefix_info(st, o, now) {
                    refreshed.push(a);
                }
            }
            5 if len >= 8 => {
                let mtu = u32::from_be_bytes(o[4..8].try_into().unwrap());
                if (1280..=1500).contains(&mtu) {
                    CONF.get()[CONF_ETH].mtu = mtu as i32;
                }
            }
            _ => {}
        }
        opts = &opts[len..];
    }
    changed();
    for a in &refreshed {
        crate::netlink::addr6_refreshed(a);
    }
    for o in useropts {
        crate::netlink::nd_useropt(src, &o);
    }
}

/// prefix information (addrconf_prefix_rcv)。もうある SLAAC の番地の lifetime を新しくして、DAD のとちゅうでなければ
/// その番地を返す (addrconf_prefix_rcv_add_addr の update_lft → ipv6_ifa_notify: RTM_NEWADDR)
fn prefix_info(st: &mut State, o: &[u8], now: u64) -> Option<Addr> {
    let len = o[2];
    let (onlink, auto) = (o[3] & 0x80 != 0, o[3] & 0x40 != 0);
    let valid = u32::from_be_bytes(o[4..8].try_into().unwrap()) as u64;
    let pref = u32::from_be_bytes(o[8..12].try_into().unwrap()) as u64;
    let prefix = u128::from_be_bytes(o[16..32].try_into().unwrap());
    if len > 128 || is_link_local(prefix) || prefix >> 120 == 0xff || pref > valid {
        return None;
    }
    let prefix = if len == 0 { 0 } else { prefix >> (128 - len as u32) << (128 - len as u32) };
    let life = |v: u64| if v == 0xffff_ffff { u64::MAX } else { now + v * 1000 };
    if onlink {
        st.prefixes.retain(|p| !(p.prefix == prefix && p.len == len));
        if valid != 0 {
            st.prefixes.push(Prefix { prefix, len, until: life(valid) });
        }
    }
    if auto && len == 64 && CONF.get()[CONF_ETH].autoconf > 0 {
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
                if a.usable() {
                    return Some(a.clone());
                }
            }
            None if valid != 0 => {
                let (dad, nonce) = dad_start(now);
                st.addrs.push(Addr {
                    addr,
                    prefix: 64,
                    flags: IFA_F_MANAGETEMPADDR | if dad.is_some() { IFA_F_TENTATIVE } else { 0 },
                    proto: IFAPROT_KERNEL_RA,
                    valid: Some(life(valid)),
                    preferred: Some(life(pref)),
                    created: now,
                    updated: now,
                    dad,
                    nonce,
                })
            }
            None => {}
        }
    }
    None
}

/// のこりの秒 (ifa_cacheinfo の prefered と valid: いつまでもは INFINITY_LIFE_TIME)
pub fn left_secs(t: Option<u64>) -> u32 {
    match t {
        None => u32::MAX,
        Some(u64::MAX) => u32::MAX,
        Some(t) => (t.saturating_sub(ms()) / 1000) as u32,
    }
}

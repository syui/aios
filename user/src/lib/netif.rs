// ネットワークのインターフェースを見る・変える (カーネルへは Linux と同じ ioctl。ip と networkd が使う)
//   アドレスを決めると DHCP はやめ、0.0.0.0 にすると DHCP にもどる (aios のカーネル)
use std::io;

pub type Addr = [u8; 4];

pub struct If {
    pub name: String,
    pub addr: Option<Addr>,
    pub prefix: u8,
    pub mac: [u8; 6],
    pub mtu: i32,
    pub up: bool,
    /// DHCP でもらったアドレスか (/proc/net/pnp)
    pub dhcp: bool,
}

const SIOCGIFCONF: libc::c_ulong = 0x8912;
const SIOCGIFFLAGS: libc::c_ulong = 0x8913;
const SIOCGIFADDR: libc::c_ulong = 0x8915;
const SIOCSIFADDR: libc::c_ulong = 0x8916;
const SIOCGIFNETMASK: libc::c_ulong = 0x891b;
const SIOCSIFNETMASK: libc::c_ulong = 0x891c;
const SIOCGIFMTU: libc::c_ulong = 0x8921;
const SIOCGIFHWADDR: libc::c_ulong = 0x8927;
const SIOCADDRT: libc::c_ulong = 0x890b;
const SIOCDELRT: libc::c_ulong = 0x890c;

/// ioctl に使うソケット
struct Sock(i32);

impl Sock {
    fn new() -> io::Result<Sock> {
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
        if fd < 0 { Err(io::Error::last_os_error()) } else { Ok(Sock(fd)) }
    }

    fn ioctl(&self, req: libc::c_ulong, buf: &mut [u8]) -> io::Result<()> {
        if unsafe { libc::ioctl(self.0, req as _, buf.as_mut_ptr()) } < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }
}

impl Drop for Sock {
    fn drop(&mut self) {
        unsafe { libc::close(self.0) };
    }
}

/// struct ifreq (名前 16 + 中身 24)
fn ifreq(name: &str) -> [u8; 40] {
    let mut r = [0u8; 40];
    let n = name.len().min(15);
    r[..n].copy_from_slice(&name.as_bytes()[..n]);
    r
}

/// sockaddr_in を ifreq の中身へ
fn put_addr(r: &mut [u8], a: Addr) {
    r[0..2].copy_from_slice(&(libc::AF_INET as u16).to_le_bytes());
    r[2..4].fill(0);
    r[4..8].copy_from_slice(&a);
}

fn get_addr(r: &[u8]) -> Addr {
    [r[4], r[5], r[6], r[7]]
}

pub fn mask(prefix: u8) -> Addr {
    let m: u32 = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix as u32) };
    m.to_be_bytes()
}

pub fn fmt(a: Addr) -> String {
    format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3])
}

pub fn parse_addr(s: &str) -> Option<Addr> {
    let p: Vec<u8> = s.split('.').map(|x| x.parse().ok()).collect::<Option<_>>()?;
    p.try_into().ok()
}

/// "192.168.1.10/24" (長さがなければ 24)
pub fn parse_cidr(s: &str) -> Option<(Addr, u8)> {
    let (a, p) = s.split_once('/').unwrap_or((s, "24"));
    let p: u8 = p.parse().ok().filter(|&p| p <= 32)?;
    Some((parse_addr(a)?, p))
}

/// インターフェースの名前の一覧
pub fn names() -> io::Result<Vec<String>> {
    let s = Sock::new()?;
    let mut buf = [0u8; 40 * 8];
    // struct ifconf { int ifc_len; char *ifc_buf; }
    let mut c = [0u8; 16];
    c[0..4].copy_from_slice(&(buf.len() as i32).to_le_bytes());
    c[8..16].copy_from_slice(&(buf.as_mut_ptr() as u64).to_le_bytes());
    s.ioctl(SIOCGIFCONF, &mut c)?;
    let len = i32::from_le_bytes(c[0..4].try_into().unwrap()) as usize;
    Ok(buf[..len.min(buf.len())]
        .chunks(40)
        .map(|r| String::from_utf8_lossy(&r[..r[..16].iter().position(|&b| b == 0).unwrap_or(16)]).into_owned())
        .collect())
}

pub fn get(name: &str) -> io::Result<If> {
    let s = Sock::new()?;
    let mut r = ifreq(name);
    s.ioctl(SIOCGIFFLAGS, &mut r)?;
    let up = u16::from_le_bytes([r[16], r[17]]) & 1 != 0;
    let mut r = ifreq(name);
    s.ioctl(SIOCGIFHWADDR, &mut r)?;
    let mac: [u8; 6] = r[18..24].try_into().unwrap();
    let mut r = ifreq(name);
    s.ioctl(SIOCGIFMTU, &mut r)?;
    let mtu = i32::from_le_bytes(r[16..20].try_into().unwrap());
    let mut r = ifreq(name);
    let addr = s.ioctl(SIOCGIFADDR, &mut r).ok().map(|_| get_addr(&r[16..]));
    let mut r = ifreq(name);
    let prefix = s.ioctl(SIOCGIFNETMASK, &mut r).ok().map_or(0, |_| u32::from_be_bytes(get_addr(&r[16..])).leading_ones() as u8);
    let dhcp = std::fs::read_to_string("/proc/net/pnp").is_ok_and(|p| p.starts_with("#PROTO: DHCP"));
    Ok(If { name: name.to_string(), addr, prefix, mac, mtu, up, dhcp })
}

/// アドレスを決める (DHCP はやめる)
pub fn set_addr(name: &str, a: Addr, prefix: u8) -> io::Result<()> {
    let s = Sock::new()?;
    let mut r = ifreq(name);
    put_addr(&mut r[16..], a);
    s.ioctl(SIOCSIFADDR, &mut r)?;
    let mut r = ifreq(name);
    put_addr(&mut r[16..], mask(prefix));
    s.ioctl(SIOCSIFNETMASK, &mut r)
}

/// DHCP にもどす
pub fn set_dhcp(name: &str) -> io::Result<()> {
    let s = Sock::new()?;
    let mut r = ifreq(name);
    put_addr(&mut r[16..], [0; 4]);
    s.ioctl(SIOCSIFADDR, &mut r)
}

/// デフォルトのゲートウェイ (None で消す)
pub fn set_gateway(gw: Option<Addr>) -> io::Result<()> {
    let s = Sock::new()?;
    // struct rtentry: rt_pad1 (8), rt_dst (16), rt_gateway (16), rt_genmask (16), rt_flags (2) ...
    let mut rt = [0u8; 128];
    put_addr(&mut rt[8..24], [0; 4]);
    put_addr(&mut rt[40..56], [0; 4]);
    match gw {
        Some(g) => {
            put_addr(&mut rt[24..40], g);
            rt[56..58].copy_from_slice(&0x3u16.to_le_bytes()); // RTF_UP | RTF_GATEWAY
            s.ioctl(SIOCADDRT, &mut rt)
        }
        None => s.ioctl(SIOCDELRT, &mut rt).or_else(|e| if e.raw_os_error() == Some(libc::ESRCH) { Ok(()) } else { Err(e) }),
    }
}

/// 経路 (/proc/net/route): (インターフェース, 行き先, ゲートウェイ, マスクの長さ)
pub fn routes() -> Vec<(String, Addr, Option<Addr>, u8)> {
    let t = std::fs::read_to_string("/proc/net/route").unwrap_or_default();
    let hex = |s: &str| u32::from_str_radix(s, 16).ok().map(|v| v.to_le_bytes());
    t.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let (dst, gw, m) = (hex(f.get(1)?)?, hex(f.get(2)?)?, hex(f.get(7)?)?);
            Some((f[0].to_string(), dst, (gw != [0; 4]).then_some(gw), u32::from_be_bytes(m).leading_ones() as u8))
        })
        .collect()
}

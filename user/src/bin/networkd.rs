// networkd: /etc/systemd/network/*.network を読んで、アドレス・ゲートウェイ・DNS を決める (自作)
// systemd-networkd と同じ書きかた (使えるのはその一部):
//   [Match]
//   Name=eth0            (* ? も。なければどれでも)
//   [Network]
//   DHCP=yes             (yes / ipv4 なら DHCP。ほかは手で)
//   Address=192.168.1.10/24
//   Gateway=192.168.1.1
//   DNS=1.1.1.1 8.8.8.8  (何行でも)
//   [Address] Address=...   [Route] Gateway=... の書きかたも読む
// ファイルは名前の順に見て、インターフェースに合った最初のものを使う。合うものがなければ何もしない
// (カーネルが DHCP でもらう)。DNS= があれば /etc/resolv.conf に書き、DHCP なら /proc/net/pnp へのリンクにもどす
//   networkd       決める (起動のときは networkd.service)
//   networkd -n    何をするかだけ出す
#[path = "../lib/netif.rs"]
#[allow(dead_code)]
mod netif;

use std::path::Path;

const DIR: &str = "/etc/systemd/network";
const RESOLV: &str = "/etc/resolv.conf";

#[derive(Default, Debug)]
struct Conf {
    file: String,
    names: Vec<String>,
    dhcp: bool,
    address: Option<(netif::Addr, u8)>,
    gateway: Option<netif::Addr>,
    dns: Vec<netif::Addr>,
}

fn main() {
    let dry = std::env::args().any(|a| a == "-n");
    let ifs = netif::names().unwrap_or_default();
    let mut files: Vec<_> = std::fs::read_dir(DIR).map(|d| d.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "network")).collect()).unwrap_or_default();
    files.sort();
    let confs: Vec<Conf> = files.iter().filter_map(|p| parse(p)).collect();
    let mut fail = false;
    for name in &ifs {
        let Some(c) = confs.iter().find(|c| c.names.is_empty() || c.names.iter().any(|n| glob(n, name))) else {
            println!("networkd: {}: no .network file (kernel DHCP)", name);
            continue;
        };
        if let Err(e) = apply(name, c, dry) {
            eprintln!("networkd: {}: {}", name, e);
            fail = true;
        }
    }
    std::process::exit(fail as i32);
}

fn apply(name: &str, c: &Conf, dry: bool) -> std::io::Result<()> {
    let mut msg = format!("networkd: {} ({})", name, c.file);
    if c.dhcp || c.address.is_none() {
        msg += " dhcp";
        if !dry {
            let now = netif::get(name)?;
            if !now.dhcp {
                netif::set_dhcp(name)?;
            }
        }
    } else if let Some((a, p)) = c.address {
        msg += &format!(" {}/{}", netif::fmt(a), p);
        if !dry {
            netif::set_addr(name, a, p)?;
            netif::set_gateway(c.gateway)?;
        }
        if let Some(g) = c.gateway {
            msg += &format!(" gw {}", netif::fmt(g));
        }
    }
    for d in &c.dns {
        msg += &format!(" dns {}", netif::fmt(*d));
    }
    if !dry {
        resolv(c)?;
    }
    println!("{}{}", msg, if dry { " (dry run)" } else { "" });
    Ok(())
}

/// DNS: 書いてあれば /etc/resolv.conf に。DHCP で DNS が書いてなければ /proc/net/pnp へのリンクにもどす
fn resolv(c: &Conf) -> std::io::Result<()> {
    let p = Path::new(RESOLV);
    if !c.dns.is_empty() {
        let body: String = c.dns.iter().map(|d| format!("nameserver {}\n", netif::fmt(*d))).collect();
        let _ = std::fs::remove_file(p);
        std::fs::write(p, format!("# networkd ({})\n{}", c.file, body))?;
    } else if c.dhcp || c.address.is_none() {
        if std::fs::read_link(p).ok().as_deref() != Some(Path::new("/proc/net/pnp")) {
            let _ = std::fs::remove_file(p);
            std::os::unix::fs::symlink("/proc/net/pnp", p)?;
        }
    }
    Ok(())
}

fn parse(path: &Path) -> Option<Conf> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut c = Conf { file: path.file_name()?.to_string_lossy().into_owned(), ..Default::default() };
    let mut sec = String::new();
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') || l.starts_with(';') {
            continue;
        }
        if let Some(s) = l.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            sec = s.to_string();
            continue;
        }
        let Some((k, v)) = l.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim());
        match (sec.as_str(), k) {
            ("Match", "Name") => c.names.extend(v.split_whitespace().map(String::from)),
            ("Network", "DHCP") => c.dhcp = matches!(v, "yes" | "true" | "ipv4" | "1" | "on"),
            ("Network", "Address") | ("Address", "Address") => {
                if c.address.is_none() {
                    c.address = netif::parse_cidr(v);
                    if c.address.is_none() {
                        eprintln!("networkd: {}: bad Address={}", c.file, v);
                    }
                }
            }
            ("Network", "Gateway") | ("Route", "Gateway") => c.gateway = netif::parse_addr(v),
            ("Network", "DNS") => c.dns.extend(v.split_whitespace().filter_map(netif::parse_addr)),
            _ => {}
        }
    }
    Some(c)
}

/// * と ? だけのワイルドカード
fn glob(pat: &str, s: &str) -> bool {
    fn m(p: &[char], s: &[char]) -> bool {
        match (p.first(), s.first()) {
            (None, None) => true,
            (Some('*'), _) => m(&p[1..], s) || (!s.is_empty() && m(p, &s[1..])),
            (Some('?'), Some(_)) => m(&p[1..], &s[1..]),
            (Some(a), Some(b)) if a == b => m(&p[1..], &s[1..]),
            _ => false,
        }
    }
    m(&pat.chars().collect::<Vec<_>>(), &s.chars().collect::<Vec<_>>())
}

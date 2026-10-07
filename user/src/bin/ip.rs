// ip: ネットワークのアドレスと経路を見る・変える (iproute2 の ip のよく使うところだけ。自作)
//   ip addr [show]                      ip a でも
//   ip addr add 192.168.1.10/24 dev eth0   アドレスを決める (DHCP はやめる)
//   ip addr flush dev eth0              アドレスを消して DHCP にもどす (del も同じ)
//   ip route [show]                     ip r でも
//   ip route add default via 192.168.1.1   (replace も同じ)
//   ip route del default
//   ip link [show]
// 起動のときに決めたいなら /etc/systemd/network/*.network (networkd)
#[path = "../lib/netif.rs"]
mod netif;

use netif::{fmt, parse_addr, parse_cidr};

fn main() {
    // 読み手のいないパイプに書いたら (| head など)、ほかのコマンドと同じように静かに終わる
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let args: Vec<String> = std::env::args().skip(1).filter(|a| a != "-4").collect();
    let a: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let r = match a.as_slice() {
        [] => usage(),
        [obj, rest @ ..] if "address".starts_with(obj) => addr(rest),
        [obj, rest @ ..] if "route".starts_with(obj) => route(rest),
        [obj, rest @ ..] if "link".starts_with(obj) => link(rest),
        _ => usage(),
    };
    if let Err(e) = r {
        eprintln!("ip: {}", e);
        std::process::exit(if e.kind() == std::io::ErrorKind::InvalidInput { 1 } else { 2 });
    }
}

fn usage() -> ! {
    eprintln!("usage: ip addr [show | add ADDR/LEN dev IF | flush dev IF]");
    eprintln!("       ip route [show | add default via GW | del default]");
    eprintln!("       ip link [show]");
    std::process::exit(1);
}

fn bad(s: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidInput, s.to_string())
}

/// "dev IF" の IF (なければ最初のインターフェース)
fn dev(rest: &[&str]) -> std::io::Result<String> {
    if let Some(i) = rest.iter().position(|&w| w == "dev") {
        return rest.get(i + 1).map(|s| s.to_string()).ok_or_else(|| bad("dev needs a name"));
    }
    netif::names()?.into_iter().next().ok_or_else(|| bad("no interface"))
}

fn header(i: &netif::If, n: usize) {
    let flags = if i.up { "BROADCAST,MULTICAST,UP,LOWER_UP" } else { "BROADCAST,MULTICAST" };
    println!("{}: {}: <{}> mtu {} state {}", n, i.name, flags, i.mtu, if i.up { "UP" } else { "DOWN" });
    let m = i.mac;
    println!("    link/ether {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} brd ff:ff:ff:ff:ff:ff", m[0], m[1], m[2], m[3], m[4], m[5]);
}

fn addr(rest: &[&str]) -> std::io::Result<()> {
    match rest {
        [] | ["show", ..] | ["list", ..] => {
            for (k, n) in netif::names()?.iter().enumerate() {
                let i = netif::get(n)?;
                header(&i, k + 1);
                if let Some(a) = i.addr {
                    let m = netif::mask(i.prefix);
                    let brd = [a[0] | !m[0], a[1] | !m[1], a[2] | !m[2], a[3] | !m[3]];
                    let how = if i.dhcp { "dynamic " } else { "" };
                    println!("    inet {}/{} brd {} scope global {}{}", fmt(a), i.prefix, fmt(brd), how, i.name);
                }
            }
            Ok(())
        }
        ["add", cidr, more @ ..] => {
            let (a, p) = parse_cidr(cidr).ok_or_else(|| bad("bad address (ADDR/LEN)"))?;
            netif::set_addr(&dev(more)?, a, p)
        }
        ["flush", more @ ..] | ["del", _, more @ ..] | ["delete", _, more @ ..] => netif::set_dhcp(&dev(more)?),
        _ => usage(),
    }
}

fn route(rest: &[&str]) -> std::io::Result<()> {
    match rest {
        [] | ["show", ..] | ["list", ..] => {
            let addr = netif::names()?.first().and_then(|n| netif::get(n).ok()).and_then(|i| i.addr);
            for (dev, dst, gw, len) in netif::routes() {
                match gw {
                    Some(g) if len == 0 => println!("default via {} dev {}", fmt(g), dev),
                    Some(g) => println!("{}/{} via {} dev {}", fmt(dst), len, fmt(g), dev),
                    None => match addr {
                        Some(a) => println!("{}/{} dev {} proto kernel scope link src {}", fmt(dst), len, dev, fmt(a)),
                        None => println!("{}/{} dev {} scope link", fmt(dst), len, dev),
                    },
                }
            }
            Ok(())
        }
        ["add" | "replace" | "change", "default", "via", gw, ..] => netif::set_gateway(Some(parse_addr(gw).ok_or_else(|| bad("bad gateway"))?)),
        ["del" | "delete", "default", ..] => netif::set_gateway(None),
        ["add" | "replace" | "del" | "delete", ..] => Err(bad("only the default route can be changed")),
        _ => usage(),
    }
}

fn link(rest: &[&str]) -> std::io::Result<()> {
    match rest {
        [] | ["show", ..] | ["list", ..] => {
            for (k, n) in netif::names()?.iter().enumerate() {
                header(&netif::get(n)?, k + 1);
            }
            Ok(())
        }
        _ => usage(),
    }
}

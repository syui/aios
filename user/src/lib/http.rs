// 小さな HTTP/1.1 クライアント (GET だけ)。Content-Length / chunked / 切断まで、リダイレクト、プロキシ (CONNECT) に対応
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

pub struct Url<'a> {
    pub scheme: &'a str,
    pub host: &'a str,
    pub port: u16,
    pub path: &'a str,
}

pub fn parse_url(url: &str) -> io::Result<Url<'_>> {
    let bad = || io::Error::new(io::ErrorKind::InvalidInput, format!("bad url: {}", url));
    let (scheme, rest) = url.split_once("://").ok_or_else(bad)?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let default = match scheme {
        "http" => 80,
        "https" => 443,
        _ => return Err(bad()),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().map_err(|_| bad())?),
        None => (hostport, default),
    };
    Ok(Url { scheme, host, port, path })
}

fn read_body(r: &mut impl BufRead, headers: &[(String, String)]) -> io::Result<Vec<u8>> {
    let h = |k: &str| headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.as_str());
    let mut body = Vec::new();
    if h("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        loop {
            let mut line = String::new();
            r.read_line(&mut line)?;
            let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or(""), 16)
                .map_err(|_| io::Error::other("bad chunk size"))?;
            if size == 0 {
                break;
            }
            let start = body.len();
            body.resize(start + size, 0);
            r.read_exact(&mut body[start..])?;
            let mut crlf = String::new();
            r.read_line(&mut crlf)?;
        }
    } else if let Some(len) = h("content-length").and_then(|v| v.parse::<usize>().ok()) {
        body.resize(len, 0);
        r.read_exact(&mut body)?;
    } else {
        r.read_to_end(&mut body)?;
    }
    Ok(body)
}

/// 1 回だけの要求。(status, headers, body)
fn request(url: &Url, stream: impl Read + Write) -> io::Result<(u16, Vec<(String, String)>, Vec<u8>)> {
    let mut r = BufReader::new(stream);
    let host = if (url.scheme, url.port) == ("http", 80) || (url.scheme, url.port) == ("https", 443) {
        url.host.to_string()
    } else {
        format!("{}:{}", url.host, url.port)
    };
    write!(
        r.get_mut(),
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: aipkg/0.1 (aios)\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        url.path, host
    )?;
    r.get_mut().flush()?;
    let mut status = String::new();
    r.read_line(&mut status)?;
    let code = status.split_whitespace().nth(1).and_then(|c| c.parse().ok()).ok_or_else(|| io::Error::other("bad response"))?;
    let mut headers = vec![];
    loop {
        let mut line = String::new();
        if r.read_line(&mut line)? == 0 || line.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let body = read_body(&mut r, &headers)?;
    Ok((code, headers, body))
}

pub type Connector = fn(&Url) -> io::Result<Box<dyn ReadWrite>>;

pub trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

/// url のホストへの TCP。https_proxy / http_proxy (all_proxy, no_proxy も) があれば
/// そのプロキシに CONNECT してトンネルを作る (curl や pacman と同じ環境変数)
pub fn tcp(url: &Url) -> io::Result<TcpStream> {
    let s = match proxy_for(url) {
        Some(p) => connect_via(&p, url)?,
        None => TcpStream::connect((url.host, url.port))?,
    };
    s.set_read_timeout(Some(Duration::from_secs(60)))?;
    Ok(s)
}

fn env_any(names: &[&str]) -> Option<String> {
    names.iter().find_map(|n| std::env::var(n).ok()).filter(|v| !v.is_empty())
}

/// 使うプロキシの URL (no_proxy に当たれば None)
fn proxy_for(url: &Url) -> Option<String> {
    let p = if url.scheme == "https" {
        env_any(&["https_proxy", "HTTPS_PROXY", "all_proxy", "ALL_PROXY"])
    } else {
        env_any(&["http_proxy", "all_proxy", "ALL_PROXY"])
    }?;
    let no = env_any(&["no_proxy", "NO_PROXY"]).unwrap_or_default();
    let host = url.host.trim_start_matches('[').trim_end_matches(']');
    let skip = no.split(',').map(str::trim).filter(|d| !d.is_empty()).any(|d| {
        let d = d.trim_start_matches('.');
        d == "*" || host == d || host.ends_with(&format!(".{}", d))
    });
    (!skip).then_some(p)
}

/// プロキシ (http://[user:pass@]host:port) に CONNECT host:port を送る
fn connect_via(proxy: &str, url: &Url) -> io::Result<TcpStream> {
    let rest = proxy.split_once("://").map_or(proxy, |(_, r)| r);
    let rest = rest.trim_end_matches('/');
    let (auth, hostport) = match rest.rsplit_once('@') {
        Some((a, h)) => (Some(a), h),
        None => (None, rest),
    };
    let (ph, pp) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().map_err(|_| io::Error::other(format!("bad proxy: {}", proxy)))?),
        None => (hostport, 80),
    };
    let mut s = TcpStream::connect((ph, pp))?;
    s.set_read_timeout(Some(Duration::from_secs(60)))?;
    let target = format!("{}:{}", url.host, url.port);
    let mut req = format!("CONNECT {t} HTTP/1.1\r\nHost: {t}\r\nUser-Agent: aipkg/0.1 (aios)\r\n", t = target);
    if let Some(a) = auth {
        req.push_str(&format!("Proxy-Authorization: Basic {}\r\n", base64(a.as_bytes())));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes())?;
    // 応答の頭だけを 1 バイトずつ読む (その後ろは TLS なので読みすぎない)
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if s.read(&mut b)? == 0 || head.len() > 16 * 1024 {
            return Err(io::Error::other("proxy closed the connection"));
        }
        head.push(b[0]);
    }
    let status = String::from_utf8_lossy(&head);
    let code = status.split_whitespace().nth(1).unwrap_or("");
    if code != "200" {
        return Err(io::Error::other(format!("proxy {}: CONNECT {}: {}", hostport, target, status.lines().next().unwrap_or(""))));
    }
    Ok(s)
}

fn base64(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// url を GET して中身を返す。https は tls で包む
pub fn get(url: &str, tls: Option<Connector>) -> io::Result<Vec<u8>> {
    let mut url = url.to_string();
    for _ in 0..5 {
        let u = parse_url(&url)?;
        let (code, headers, body) = match u.scheme {
            "http" => request(&u, tcp(&u)?)?,
            _ => match tls {
                Some(c) => request(&u, c(&u)?)?,
                None => return Err(io::Error::other("https is not supported")),
            },
        };
        match code {
            200 => return Ok(body),
            301 | 302 | 303 | 307 | 308 => {
                let loc = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("location")).map(|(_, v)| v.clone());
                let loc = loc.ok_or_else(|| io::Error::other("redirect without location"))?;
                url = if loc.starts_with("http") { loc } else { format!("{}://{}:{}{}", u.scheme, u.host, u.port, loc) };
            }
            c => return Err(io::Error::other(format!("{}: HTTP {}", url, c))),
        }
    }
    Err(io::Error::other("too many redirects"))
}

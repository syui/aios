// 小さな HTTP/1.1 クライアント (GET だけ)。Content-Length / chunked / 切断まで、リダイレクトに対応
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

pub fn tcp(url: &Url) -> io::Result<TcpStream> {
    let s = TcpStream::connect((url.host, url.port))?;
    s.set_read_timeout(Some(Duration::from_secs(60)))?;
    Ok(s)
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

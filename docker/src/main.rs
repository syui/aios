// serve: aios の docker の見本の HTTP サーバー (std だけ)
//   serve           ADDR (既定 0.0.0.0:8080) で待つ。GET / は "hello from aios"、GET /healthz は "ok"
//   serve --check   127.0.0.1 の同じポートの /healthz を聞き、200 なら 0 で終わる (compose の healthcheck。
//                   distroless の image には curl も sh もないので、自分で)
// 守り: 読み書きは 5 秒で切る (遅いクライアントが居すわらない)、頭は 8 KiB まで、同時に 64 つなぎまで (それより多ければ
// 503)、GET と HEAD のほかは 405、答えには nosniff と no-store
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const MAX_CONN: usize = 64;
const MAX_HEAD: u64 = 8 * 1024;
const TIMEOUT: Duration = Duration::from_secs(5);
static CONN: AtomicUsize = AtomicUsize::new(0);

fn main() {
    let addr = std::env::var("ADDR").unwrap_or_else(|_| "0.0.0.0:8080".into());
    if std::env::args().nth(1).as_deref() == Some("--check") {
        std::process::exit(check(&addr));
    }
    let l = TcpListener::bind(&addr).unwrap_or_else(|e| {
        eprintln!("serve: {}: {}", addr, e);
        std::process::exit(1);
    });
    eprintln!("serve: listening on {}", addr);
    for s in l.incoming().flatten() {
        if CONN.fetch_add(1, Ordering::SeqCst) >= MAX_CONN {
            CONN.fetch_sub(1, Ordering::SeqCst);
            let _ = reply(&s, "503 Service Unavailable", "busy\n", false);
            continue;
        }
        // スレッドが作れなければ (pids_limit)、そのつなぎだけ落とす
        let r = std::thread::Builder::new().spawn(move || {
            let _ = handle(&s);
            CONN.fetch_sub(1, Ordering::SeqCst);
        });
        if r.is_err() {
            CONN.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

fn handle(s: &TcpStream) -> std::io::Result<()> {
    s.set_read_timeout(Some(TIMEOUT))?;
    s.set_write_timeout(Some(TIMEOUT))?;
    let mut r = BufReader::new(s.take(MAX_HEAD));
    let mut line = String::new();
    r.read_line(&mut line)?;
    // 頭ののこり (空の行まで)
    loop {
        let mut h = String::new();
        if r.read_line(&mut h)? == 0 || h == "\r\n" || h == "\n" {
            break;
        }
    }
    let mut it = line.split_whitespace();
    let (method, path) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
    let head = method == "HEAD";
    match (method, path) {
        ("GET" | "HEAD", "/") => reply(s, "200 OK", "hello from aios\n", head),
        ("GET" | "HEAD", "/healthz") => reply(s, "200 OK", "ok\n", head),
        ("GET" | "HEAD", _) => reply(s, "404 Not Found", "not found\n", head),
        _ => reply(s, "405 Method Not Allowed", "method not allowed\n", false),
    }
}

fn reply(mut s: &TcpStream, status: &str, body: &str, head: bool) -> std::io::Result<()> {
    let h = format!(
        "HTTP/1.1 {}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nX-Content-Type-Options: nosniff\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        status,
        body.len()
    );
    s.write_all(h.as_bytes())?;
    if !head {
        s.write_all(body.as_bytes())?;
    }
    s.flush()
}

/// healthcheck: 127.0.0.1 の同じポートの /healthz
fn check(addr: &str) -> i32 {
    let port = addr.rsplit(':').next().unwrap_or("8080");
    let ok = (|| -> std::io::Result<bool> {
        let mut s = TcpStream::connect_timeout(&format!("127.0.0.1:{}", port).parse().map_err(std::io::Error::other)?, Duration::from_secs(2))?;
        s.set_read_timeout(Some(Duration::from_secs(2)))?;
        s.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
        let mut b = String::new();
        s.take(256).read_to_string(&mut b)?;
        Ok(b.starts_with("HTTP/1.1 200 "))
    })();
    if ok.unwrap_or(false) { 0 } else { 1 }
}

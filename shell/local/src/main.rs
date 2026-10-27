// aish-local: コマンドが見つからなかった行を、このマシンの AI (llama-server のアイ) に渡す (aish の基本のプラグイン)
//   aish-claude のうしろで読み、claude が答えなかったとき (claude がない、AISH_CLAUDE=0、AISH_AI=local) だけ動く。
//   その行といまのディレクトリを llama-server (OpenAI と同じ API。既定は http://127.0.0.1:8080、AISH_LOCAL_URL で
//   変える) に渡し、答えを届いたところから端末に出す。Ctrl-C で止まる (聞く子 (aish-local --ask) を自分の
//   プロセスグループにして、そのあいだ端末の前に出す。aish-claude と同じ)。
//   答えるだけで、コマンドは動かさない (小さいモデルなので。手を動かすのは Claude と aish --mcp)。
//   少し前のやりとり (15 分以内の 4 つまで) を ~/.cache/aish/local.json に残し、続けて話せるようにする。
//   llama-server が動いていなければ何もしない (aish がいつもどおり "command not found")。モデルを置いてあれば
//   (/var/lib/ai/model.gguf)、起こし方を 1 度だけ出す。AISH_LOCAL=0 でも何もしない。
//   モデルへの前置き (どこで、なぜ届いたか) は AISH_LOCAL_SYSTEM で変えられる (空なら前置きなし)
use aish_plugin::{Spec, Value, json, s};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// llama-server.service の場所 (LLAMA_ARG_HOST と LLAMA_ARG_PORT)
const URL: &str = "http://127.0.0.1:8080";
/// aimodel pull が置くモデル (あれば、このマシンで AI を使うつもり)
const MODEL: &str = "/var/lib/ai/model.gguf";
/// 続けて話すために残すやりとりの数と、残す長さ (秒)
const KEEP: usize = 4;
const KEEP_S: u64 = 15 * 60;
/// 1 つの答えの長さの上限 (トークン)
const MAX_TOKENS: u64 = 512;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 5 && args[1] == "--ask" {
        std::process::exit(talk(&args[2], &args[3], &args[4]));
    }
    let mut hinted = false;
    let spec = Spec { name: "local", hooks: &["not_found"], keys: &[], tools: &[] };
    aish_plugin::run(spec, |ev, v| match ev {
        "not_found" => ask(v, &mut hinted),
        _ => json!({}),
    });
}

fn ask(v: &Value, hinted: &mut bool) -> Value {
    // シェルのいまの環境 (aish が not_found の env に入れる。なければプラグインが起こされたときのもの)
    let env: Vec<(String, String)> = match v["env"].as_object() {
        Some(m) => m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect(),
        None => std::env::vars().collect(),
    };
    let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    if get("AISH_LOCAL") == Some("0") {
        return json!({});
    }
    let line = s(v, "line").trim().to_string();
    if line.is_empty() {
        return json!({});
    }
    let url = get("AISH_LOCAL_URL").filter(|u| !u.is_empty()).unwrap_or(URL).to_string();
    // 端末へ出す (プラグインの標準出力は aish との話に使っているので)
    let Ok(tty) = std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty") else { return json!({}) };
    // 動いていなければすぐ戻る (打ちまちがいのたびに待たせない)
    if !Server::parse(&url).is_some_and(|s| s.up()) {
        if !*hinted && std::path::Path::new(MODEL).exists() {
            *hinted = true;
            let _ = writeln_tty(&tty, "\x1b[2maish: the local AI (llama-server) is not running; to ask it: sudo systemctl start llama-server\x1b[0m");
        }
        return json!({});
    }
    let (Ok(o), Ok(e), Ok(i)) = (tty.try_clone(), tty.try_clone(), tty.try_clone()) else { return json!({}) };
    let Ok(me) = std::env::current_exe() else { return json!({}) };
    let first = v["args"][0].as_str().unwrap_or("");
    let _ = writeln_tty(&tty, &format!("\x1b[2maish: {}: not found → ai (local)\x1b[0m", first));
    let mut cmd = Command::new(me);
    cmd.env_clear().envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    cmd.arg("--ask").arg(&url).arg(s(v, "pwd")).arg(&line);
    cmd.stdin(Stdio::from(i)).stdout(Stdio::from(o)).stderr(Stdio::from(e));
    // 聞く子は自分のプロセスグループで動かし、そのあいだ端末の前に出す (Ctrl-C は子にだけ届く)。
    // プラグインは Ctrl-C を無視して起こされるので、子には戻す
    unsafe {
        cmd.pre_exec(|| {
            libc::setpgid(0, 0);
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::signal(libc::SIGTTOU, libc::SIG_DFL);
            Ok(())
        });
    }
    let fd = tty.as_raw_fd();
    let fg = unsafe { libc::tcgetpgrp(fd) };
    let Ok(mut child) = cmd.spawn() else { return json!({}) };
    let pid = child.id() as libc::pid_t;
    unsafe {
        // 端末の持ち主を変えるときの SIGTTOU で止まらないように
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
        libc::setpgid(pid, pid);
        libc::tcsetpgrp(fd, pid);
    }
    let st = child.wait();
    if fg > 0 {
        unsafe { libc::tcsetpgrp(fd, fg) };
    }
    match st {
        Ok(st) => json!({ "status": st.code().unwrap_or(130) }),
        Err(_) => json!({}),
    }
}

/// 聞く子: llama-server が読みこみ中なら待ち、行を渡して、答えを標準出力 (端末) へ。終了コード
fn talk(url: &str, pwd: &str, line: &str) -> i32 {
    let Some(srv) = Server::parse(url) else {
        eprintln!("aish: AISH_LOCAL_URL must be http://HOST[:PORT][/PATH]: {}", url);
        return 2;
    };
    let mut said = false;
    loop {
        match health(&srv) {
            Health::Ok => break,
            Health::Loading => {
                if !said {
                    eprintln!("\x1b[2maish: the local AI is still loading the model; waiting (Ctrl-C to stop)\x1b[0m");
                    said = true;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
            Health::Down(e) => {
                eprintln!("aish: the local AI ({}): {}", url, e);
                return 1;
            }
        }
    }
    let path = history_path();
    let mut turns = load(path.as_ref(), now());
    // 前置きは AISH_LOCAL_SYSTEM で変えられる (空なら前置きなし。読める長さの短いモデルのため)
    let system = std::env::var("AISH_LOCAL_SYSTEM").unwrap_or_else(|_| system(pwd));
    let mut out = io::stdout().lock();
    let mut answer = String::new();
    let mut r = Ok(());
    // 前のやりとりまで入りきらなければ (モデルの読める長さをこえた)、やりとりなしでもう一度
    for with in [true, false] {
        let mut messages = vec![];
        if !system.is_empty() {
            messages.push(json!({ "role": "system", "content": system }));
        }
        for t in turns.iter().filter(|_| with) {
            messages.push(json!({ "role": "user", "content": t.q }));
            messages.push(json!({ "role": "assistant", "content": t.a }));
        }
        messages.push(json!({ "role": "user", "content": line }));
        let body = json!({ "messages": messages, "stream": true, "max_tokens": MAX_TOKENS }).to_string();
        r = chat(&srv, &body, |p| {
            let _ = out.write_all(p.as_bytes()).and_then(|_| out.flush());
            answer.push_str(p);
        });
        if !(with && !turns.is_empty() && answer.is_empty() && r.as_ref().is_err_and(|e| e.contains("context size"))) {
            break;
        }
        turns.clear();
    }
    if !answer.is_empty() && !answer.ends_with('\n') {
        let _ = writeln!(out);
    }
    if let Err(e) = r {
        eprintln!("aish: the local AI: {}", e);
        return 1;
    }
    turns.push(Turn { t: now(), q: line.to_string(), a: answer.trim().to_string() });
    save(path.as_ref(), &turns);
    0
}

/// モデルへの前置き (いまどこで、なぜ届いたか)
fn system(pwd: &str) -> String {
    format!(
        "ここは aios のシェル (aish) で、いまのディレクトリは {pwd}。人が打った行がコマンドとして見つからなかったので、あなたに届いた。\
         打ちまちがいのコマンドなら、正しいコマンドを短く教えて。話しかけられたのなら、ふつうに短く答えて。"
    )
}

/// http://HOST[:PORT][/PATH] (llama-server は TLS なしの http)
struct Server {
    /// HOST:PORT
    addr: String,
    /// PATH (おしまいの / なし。なければ空)
    base: String,
}

impl Server {
    fn parse(url: &str) -> Option<Server> {
        let rest = url.strip_prefix("http://")?;
        let (hp, path) = rest.find('/').map_or((rest, ""), |i| rest.split_at(i));
        if hp.is_empty() {
            return None;
        }
        let addr = if hp.contains(':') { hp.to_string() } else { format!("{}:80", hp) };
        Some(Server { addr, base: path.trim_end_matches('/').to_string() })
    }

    fn connect(&self, wait: Duration) -> io::Result<TcpStream> {
        let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{}: no address", self.addr));
        for a in self.addr.to_socket_addrs()? {
            match TcpStream::connect_timeout(&a, wait) {
                Ok(s) => return Ok(s),
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    /// 聞いているものがいるか (llama-server は読みこみのあいだも聞いている)
    fn up(&self) -> bool {
        self.connect(Duration::from_millis(300)).is_ok()
    }

    /// 送って、答えの (番号, 中身を読むもの)
    fn send(&self, method: &str, path: &str, body: Option<&str>) -> io::Result<(u16, Box<dyn BufRead>)> {
        let mut s = self.connect(Duration::from_secs(3))?;
        let mut req = format!("{} {}{} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n", method, self.base, path, self.addr);
        if let Some(b) = body {
            req.push_str(&format!("Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", b.len(), b));
        } else {
            req.push_str("\r\n");
        }
        s.write_all(req.as_bytes())?;
        response(BufReader::new(s))
    }
}

enum Health {
    Ok,
    /// モデルを読みこんでいる (llama-server の /health が 503)
    Loading,
    Down(String),
}

/// /health のないもの (ほかの OpenAI と同じ API のもの) は、聞いてみる
fn health(srv: &Server) -> Health {
    match srv.send("GET", "/health", None) {
        Ok((503, _)) => Health::Loading,
        Ok(_) => Health::Ok,
        Err(e) => Health::Down(e.to_string()),
    }
}

/// /v1/chat/completions を stream で。答えの切れはしを届いたところから piece に
fn chat(srv: &Server, body: &str, mut piece: impl FnMut(&str)) -> Result<(), String> {
    let (code, r) = srv.send("POST", "/v1/chat/completions", Some(body)).map_err(|e| e.to_string())?;
    if code != 200 {
        let mut text = String::new();
        let _ = r.take(64 << 10).read_to_string(&mut text);
        let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        return Err(v["error"]["message"].as_str().map_or_else(|| format!("HTTP {}", code), |m| format!("HTTP {}: {}", code, m)));
    }
    events(r, &mut piece)
}

/// text/event-stream の data: の行から、choices[0].delta.content を順に ([DONE] まで)
fn events(r: impl BufRead, piece: &mut impl FnMut(&str)) -> Result<(), String> {
    for line in r.lines() {
        let line = line.map_err(|e| e.to_string())?;
        let Some(data) = line.strip_prefix("data:") else { continue };
        let data = data.trim();
        if data == "[DONE]" {
            break;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else { continue };
        if let Some(m) = v["error"]["message"].as_str() {
            return Err(m.to_string());
        }
        if let Some(t) = v["choices"][0]["delta"]["content"].as_str().filter(|t| !t.is_empty()) {
            piece(t);
        }
    }
    Ok(())
}

/// HTTP の答えの頭を読み、中身を読むものを返す (chunked ならほどいて)
fn response<R: BufRead + 'static>(mut r: R) -> io::Result<(u16, Box<dyn BufRead>)> {
    let mut line = String::new();
    r.read_line(&mut line)?;
    let code = line.split_whitespace().nth(1).and_then(|c| c.parse().ok()).ok_or_else(|| bad("not an HTTP answer"))?;
    let (mut chunked, mut len) = (false, None);
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 || line.trim_end().is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            if k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked") {
                chunked = true;
            } else if k == "content-length" {
                len = v.parse::<u64>().ok();
            }
        }
    }
    Ok((
        code,
        if chunked {
            Box::new(BufReader::new(Chunked { r, left: 0, done: false }))
        } else if let Some(n) = len {
            Box::new(r.take(n))
        } else {
            Box::new(r)
        },
    ))
}

/// Transfer-Encoding: chunked をほどく
struct Chunked<R> {
    r: R,
    /// いまの切れはしの残り
    left: usize,
    done: bool,
}

impl<R: BufRead> Read for Chunked<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        if self.left == 0 {
            let mut line = String::new();
            // おしまいの 0 の前に切れたら、そこまで
            if self.r.read_line(&mut line)? == 0 {
                self.done = true;
                return Ok(0);
            }
            let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or("").trim(), 16).map_err(|_| bad("bad chunk size"))?;
            if size == 0 {
                self.done = true;
                return Ok(0);
            }
            self.left = size;
        }
        let want = buf.len().min(self.left);
        let n = self.r.read(&mut buf[..want])?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the answer was cut"));
        }
        self.left -= n;
        if self.left == 0 {
            let mut crlf = String::new();
            self.r.read_line(&mut crlf)?;
        }
        Ok(n)
    }
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// 前のやりとり 1 つ (t は秒)
struct Turn {
    t: u64,
    q: String,
    a: String,
}

fn history_path() -> Option<PathBuf> {
    let cache = std::env::var("XDG_CACHE_HOME")
        .ok()
        .filter(|d| !d.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().filter(|h| !h.is_empty()).map(|h| PathBuf::from(h).join(".cache")))?;
    Some(cache.join("aish/local.json"))
}

/// KEEP_S より新しいものを、新しいほうから KEEP まで (古い順)
fn load(path: Option<&PathBuf>, now: u64) -> Vec<Turn> {
    let Some(text) = path.and_then(|p| std::fs::read_to_string(p).ok()) else { return vec![] };
    let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    let since = now.saturating_sub(KEEP_S);
    let mut turns: Vec<Turn> = v["turns"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| Some(Turn { t: t["t"].as_u64()?, q: t["q"].as_str()?.to_string(), a: t["a"].as_str()?.to_string() }))
        .filter(|t| t.t >= since)
        .collect();
    turns.drain(..turns.len().saturating_sub(KEEP));
    turns
}

/// 話したことなので、自分だけ読めるように (0600)
fn save(path: Option<&PathBuf>, turns: &[Turn]) {
    let Some(p) = path else { return };
    let keep = &turns[turns.len().saturating_sub(KEEP)..];
    let v = json!({ "turns": keep.iter().map(|t| json!({ "t": t.t, "q": t.q, "a": t.a })).collect::<Vec<_>>() });
    if let Some(d) = p.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let tmp = p.with_extension("json.new");
    let w = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp).and_then(|mut f| f.write_all(v.to_string().as_bytes()));
    if w.is_ok() {
        let _ = std::fs::rename(&tmp, p);
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn writeln_tty(mut t: &std::fs::File, s: &str) -> io::Result<()> {
    writeln!(t, "{}", s)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// body を n で 2 つに切った chunked の答え
    fn chunked(body: &str, n: usize) -> Vec<u8> {
        let mut all = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        let b = body.as_bytes();
        for part in [&b[..n], &b[n..]] {
            all.extend(format!("{:x}\r\n", part.len()).as_bytes());
            all.extend(part);
            all.extend(b"\r\n");
        }
        all.extend(b"0\r\n\r\n");
        all
    }

    fn pieces(raw: Vec<u8>) -> (u16, Result<String, String>) {
        let (code, r) = response(io::Cursor::new(raw)).unwrap();
        let mut got = String::new();
        let res = events(r, &mut |p: &str| got.push_str(p)).map(|_| got);
        (code, res)
    }

    #[test]
    fn stream_across_chunks() {
        let body = "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n\
                    data: {\"choices\":[{\"delta\":{\"content\":\"アイ\"}}]}\n\n\
                    data: {\"choices\":[{\"delta\":{\"content\":\"だよ\"}}]}\n\n\
                    data: [DONE]\n\n\
                    data: {\"choices\":[{\"delta\":{\"content\":\"(after done)\"}}]}\n\n";
        // 字のまん中 (ア の 2 バイトめ) で切っても、行はつながる
        let cut = body.find("アイ").unwrap() + 1;
        assert_eq!(pieces(chunked(body, cut)), (200, Ok("アイだよ".to_string())));
    }

    #[test]
    fn stream_with_length_and_crlf() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"ls\"}}]}\r\n\r\n";
        let raw = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}", body.len(), body).into_bytes();
        assert_eq!(pieces(raw), (200, Ok("ls".to_string())));
    }

    #[test]
    fn stream_error() {
        let body = "data: {\"error\":{\"code\":500,\"message\":\"boom\"}}\n\n";
        assert_eq!(pieces(chunked(body, 3)).1, Err("boom".to_string()));
    }

    #[test]
    fn urls() {
        let s = Server::parse("http://127.0.0.1:8080").unwrap();
        assert_eq!((s.addr.as_str(), s.base.as_str()), ("127.0.0.1:8080", ""));
        let s = Server::parse("http://example.org/llama/").unwrap();
        assert_eq!((s.addr.as_str(), s.base.as_str()), ("example.org:80", "/llama"));
        assert!(Server::parse("https://example.org").is_none());
        assert!(Server::parse("http:///x").is_none());
    }

    #[test]
    fn history_keeps_recent() {
        let dir = std::env::temp_dir().join(format!("aish-local-test-{}", std::process::id()));
        let p = dir.join("aish/local.json");
        let turns: Vec<Turn> = (0..6).map(|i| Turn { t: 1000 + i * 100, q: format!("q{}", i), a: format!("a{}", i) }).collect();
        save(Some(&p), &turns);
        // 保存は新しい 4 つ (q2..q5)。読むときは 15 分 (900 秒) より新しいものだけ: now 2100 なら t >= 1200 (q2..q5)
        let got: Vec<String> = load(Some(&p), 2100).into_iter().map(|t| t.q).collect();
        assert_eq!(got, ["q2", "q3", "q4", "q5"]);
        let got: Vec<String> = load(Some(&p), 2350).into_iter().map(|t| t.q).collect();
        assert_eq!(got, ["q5"]);
        assert_eq!(std::fs::metadata(&p).map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions()) & 0o777).ok(), Some(0o600));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

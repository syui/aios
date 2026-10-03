// aish --mcp: aish を MCP のサーバーにする (Claude が aish を使う入り口)
//   標準入力と標準出力で JSON-RPC 2.0 を 1 行ずつ話す (MCP の stdio)。
//   ツール: run (シェルの行を動かす) と、プラグインの tools (aish-edit の read / edit / write / undo など)
// シェルは 1 つで、呼ばれるたびに同じシェルで動かす: cd、変数、関数、alias は次の run にも残る。
// 設定は対話するシェルと同じ /etc/aishrc と ~/.aishrc (AISH_MCP=1 なので、そこで分けられる)。
// run の出力は memfd (メモリーの上のファイル) に受けて、そのまま答えに入れる。
// ディスクには何も残さないので、リポジトリやイメージに入ることはない
use super::{Flow, Shell, cstr, flush};
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::os::fd::FromRawFd;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;

/// MCP のサーバーのプロセス (exit でサーバーを終わらせない。子やサブシェルの exit はふつうに)
pub static PID: AtomicI32 = AtomicI32::new(0);

/// 時間切れになった run (シェルはそこで次のコマンドへ進まずに終える)
static STOP: AtomicBool = AtomicBool::new(false);

pub fn stopped() -> bool {
    STOP.load(Ordering::Relaxed)
}

/// 既定の時間切れ (ms)
const TIMEOUT_MS: u64 = 120_000;
/// 出力をそのまま返す長さ。これより長ければ頭と終わりだけ
const MAX_OUT: usize = 30_000;

const PROTOCOL: &str = "2025-06-18";

const RUN_INPUT: &str = r#"{"type":"object","properties":{
"cmd":{"type":"string","description":"動かすシェルの行 (いくつもの行、パイプ、ヒアドキュメントもよい)"},
"timeout_ms":{"type":"integer","description":"これを過ぎたら子を止める (既定 120000)"},
"stdin":{"type":"string","description":"標準入力に渡すもの (なければ /dev/null)"}},
"required":["cmd"]}"#;

const INSTRUCTIONS: &str = "aish (aios のシェル) です。run はいつも同じシェルで動くので、cd や変数は次の run に残ります。\
答えは JSON: run は {status, out, err, ms, pwd} (時間切れなら timeout: true)。\
ファイルの読み書きは read / edit / write / undo (aish-edit) を使うと確かです。";

impl Shell {
    pub fn mcp(&mut self) -> ! {
        PID.store(unsafe { libc::getpid() }, Ordering::Relaxed);
        unsafe { std::env::set_var("AISH_MCP", "1") };
        // プロトコルは自分だけが使う fd で話す。0 は /dev/null、1 は 2 (標準エラー) にして、
        // 設定や子が標準入力を食べたり、標準出力に書いてプロトコルをこわしたりしないように
        let (inp, out) = unsafe {
            let inp = libc::fcntl(0, libc::F_DUPFD_CLOEXEC, 10);
            let out = libc::fcntl(1, libc::F_DUPFD_CLOEXEC, 10);
            let null = libc::open(cstr("/dev/null").as_ptr(), libc::O_RDWR);
            libc::dup2(null, 0);
            libc::close(null);
            libc::dup2(2, 1);
            (std::fs::File::from_raw_fd(inp), std::fs::File::from_raw_fd(out))
        };
        let home = self.get_var("HOME").unwrap_or_default();
        let file = self.get_var("HISTFILE").unwrap_or_else(|| format!("{}/.aish_history", home));
        self.histfile = (!home.is_empty() || file.starts_with('/')).then_some(file);
        for rc in ["/etc/aishrc".to_string(), format!("{}/.aishrc", home)] {
            if std::path::Path::new(&rc).is_file() {
                self.builtin(&[".".into(), rc]);
            }
        }
        flush();
        let mut out = out;
        for line in std::io::BufReader::new(inp).lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            let Ok(req) = serde_json::from_str::<Value>(&line) else {
                send(&mut out, &json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "parse error" } }));
                continue;
            };
            // id のないものは知らせ (notifications/initialized など)。答えない
            let Some(id) = req.get("id").cloned() else { continue };
            let reply = match req["method"].as_str().unwrap_or("") {
                "initialize" => Ok(json!({
                    "protocolVersion": req["params"]["protocolVersion"].as_str().unwrap_or(PROTOCOL),
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "aish", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": INSTRUCTIONS,
                })),
                "ping" => Ok(json!({})),
                "tools/list" => Ok(json!({ "tools": self.mcp_tools() })),
                "tools/call" => Ok(self.mcp_call(&req["params"])),
                m => Err(json!({ "code": -32601, "message": format!("{}: no such method", m) })),
            };
            let msg = match reply {
                Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
                Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": e }),
            };
            send(&mut out, &msg);
        }
        super::exit_shell(self.status)
    }

    /// run とプラグインの tools (名前が重なれば先のもの)
    fn mcp_tools(&self) -> Vec<Value> {
        let mut tools = vec![json!({
            "name": "run",
            "description": "aish でシェルの行を動かす。答えは {status, out, err, ms, pwd}。cd や変数は次の run に残る",
            "inputSchema": serde_json::from_str::<Value>(RUN_INPUT).unwrap(),
        })];
        for (plugin, t) in self.plugins.tools() {
            if tools.iter().any(|x| x["name"] == t["name"]) {
                continue;
            }
            tools.push(json!({
                "name": t["name"],
                "description": format!("{} (aish-{})", t["description"].as_str().unwrap_or(""), plugin),
                "inputSchema": if t["input"].is_object() { t["input"].clone() } else { json!({ "type": "object" }) },
            }));
        }
        tools
    }

    fn mcp_call(&mut self, params: &Value) -> Value {
        let name = params["name"].as_str().unwrap_or("");
        let args = if params["arguments"].is_object() { params["arguments"].clone() } else { json!({}) };
        let r = if name == "run" {
            self.mcp_run(&args)
        } else {
            let pwd = std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default();
            let ev = json!({ "args": args, "pwd": pwd, "home": self.get_var("HOME").unwrap_or_default(), "histfile": self.histfile.clone().unwrap_or_default() });
            match self.plugins.tool(name, ev) {
                Some(r) => r,
                None => json!({ "error": format!("{}: no such tool (or the plugin stopped)", name) }),
            }
        };
        let is_err = r.get("error").is_some() || r.get("timeout").is_some();
        json!({ "content": [{ "type": "text", "text": r.to_string() }], "structuredContent": r, "isError": is_err })
    }

    /// run: 同じシェルで動かし、標準出力と標準エラーを分けて受ける
    fn mcp_run(&mut self, args: &Value) -> Value {
        let cmd = args["cmd"].as_str().unwrap_or("");
        let timeout = args["timeout_ms"].as_u64().unwrap_or(TIMEOUT_MS);
        flush();
        let (o, e) = (memfd("aish-out"), memfd("aish-err"));
        let input = args["stdin"].as_str().map(|s| {
            let f = memfd("aish-in");
            unsafe {
                libc::write(f, s.as_ptr() as *const _, s.len());
                libc::lseek(f, 0, libc::SEEK_SET);
            }
            f
        });
        let saved = unsafe { [libc::dup(0), libc::dup(1), libc::dup(2)] };
        unsafe {
            if let Some(f) = input {
                libc::dup2(f, 0);
            }
            libc::dup2(o, 1);
            libc::dup2(e, 2);
        }
        // 時間切れの見張り: 過ぎたら子と孫を止める (プラグインは残す)
        STOP.store(false, Ordering::Relaxed);
        let (done, wait) = std::sync::mpsc::channel::<()>();
        let fired = Arc::new(AtomicBool::new(false));
        let keep = self.plugins.pids();
        let watch = {
            let fired = fired.clone();
            std::thread::spawn(move || {
                use std::sync::mpsc::RecvTimeoutError::Timeout;
                let ms = std::time::Duration::from_millis;
                if wait.recv_timeout(ms(timeout)) != Err(Timeout) {
                    return;
                }
                fired.store(true, Ordering::Relaxed);
                STOP.store(true, Ordering::Relaxed);
                kill_children(&keep);
                // 止まらないもの (止めたあとに生まれたもの) のために、まだ終わらなければもういちど
                if wait.recv_timeout(ms(500)) == Err(Timeout) {
                    kill_children(&keep);
                }
            })
        };
        let t0 = std::time::Instant::now();
        let mut status = self.run_source(cmd, "run");
        if self.flow != Flow::None {
            self.flow = Flow::None;
        }
        flush();
        let _ = done.send(());
        let _ = watch.join();
        let ms = t0.elapsed().as_millis() as u64;
        unsafe {
            for (fd, s) in saved.iter().enumerate() {
                libc::dup2(*s, fd as i32);
                libc::close(*s);
            }
            if let Some(f) = input {
                libc::close(f);
            }
        }
        let pwd = std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default();
        let mut r = json!({ "status": status, "out": take(o), "err": take(e), "ms": ms, "pwd": pwd });
        if fired.load(Ordering::Relaxed) {
            // timeout(1) と同じ 124
            STOP.store(false, Ordering::Relaxed);
            status = 124;
            r["status"] = json!(status);
            r["timeout"] = json!(true);
        }
        r
    }
}

fn send(out: &mut std::fs::File, v: &Value) {
    let mut s = v.to_string();
    s.push('\n');
    let _ = out.write_all(s.as_bytes());
}

fn memfd(name: &str) -> i32 {
    unsafe { libc::memfd_create(cstr(name).as_ptr(), libc::MFD_CLOEXEC) }
}

/// memfd の中身を読んで閉じる (長ければ頭と終わり)
fn take(fd: i32) -> String {
    let mut f = unsafe { std::fs::File::from_raw_fd(fd) };
    let mut b = Vec::new();
    use std::io::{Read, Seek};
    let _ = f.seek(std::io::SeekFrom::Start(0));
    let _ = f.read_to_end(&mut b);
    let s = String::from_utf8_lossy(&b).into_owned();
    if s.len() <= MAX_OUT {
        return s;
    }
    let head = floor(&s, MAX_OUT / 3);
    let tail = ceil(&s, s.len() - MAX_OUT * 2 / 3);
    format!("{}\n... ({} bytes cut) ...\n{}", &s[..head], tail - head, &s[tail..])
}

fn floor(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// 自分の子と孫を止める (/proc/N/stat の親をたどる)。keep とその下は残す
fn kill_children(keep: &[i32]) {
    let me = unsafe { libc::getpid() };
    let mut parent = std::collections::HashMap::new();
    if let Ok(rd) = std::fs::read_dir("/proc") {
        for e in rd.flatten() {
            let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() else { continue };
            let Ok(st) = std::fs::read_to_string(format!("/proc/{}/stat", pid)) else { continue };
            // "pid (comm) state ppid ..." (comm に空白や ) があってもよいように、最後の ) から)
            let Some(rest) = st.rfind(')').map(|i| &st[i + 1..]) else { continue };
            if let Some(pp) = rest.split_whitespace().nth(1).and_then(|x| x.parse::<i32>().ok()) {
                parent.insert(pid, pp);
            }
        }
    }
    for (&pid, _) in parent.iter() {
        // pid から上へたどって、keep を通らずに自分に着けば、自分の下
        let mut p = pid;
        let mut mine = false;
        for _ in 0..64 {
            if keep.contains(&p) {
                break;
            }
            match parent.get(&p) {
                Some(&pp) if pp == me => {
                    mine = true;
                    break;
                }
                Some(&pp) if pp > 1 => p = pp,
                _ => break,
            }
        }
        if mine && pid != me {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

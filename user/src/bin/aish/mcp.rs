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

/// --json: 答えを structuredContent の JSON でも返す (ふだんは読む形の text だけ)
static JSON: AtomicBool = AtomicBool::new(false);

/// 時間切れになった run (シェルはそこで次のコマンドへ進まずに終える)
static STOP: AtomicBool = AtomicBool::new(false);

pub fn stopped() -> bool {
    STOP.load(Ordering::Relaxed)
}

/// 既定の時間切れ (ms)
/// (Claude Code は MCP のツールの答えを 60 秒しか待たない。それより前に止めて、bg を使うように言う)
const TIMEOUT_MS: u64 = 50_000;
/// 出力をそのまま返す長さ。これより長ければ頭と終わりだけ
const MAX_OUT: usize = 30_000;

const PROTOCOL: &str = "2025-06-18";

const RUN_INPUT: &str = r#"{"type":"object","properties":{
"cmd":{"type":"string","description":"動かすシェルの行 (いくつもの行、パイプ、ヒアドキュメントもよい)"},
"timeout_ms":{"type":"integer","description":"これを過ぎたら子を止める (既定 50000。長くかかるものは bg で)"},
"stdin":{"type":"string","description":"標準入力に渡すもの (なければ /dev/null)"},
"bg":{"type":"boolean","description":"うしろで動かしてすぐ答える ({job, pid})。出力と終わりは job で見る (重いビルドなどのあいだも、ほかのツールを使える)"}},
"required":["cmd"]}"#;

const JOB_INPUT: &str = r#"{"type":"object","properties":{
"id":{"type":"integer","description":"run bg の job (なければ一覧)"},
"wait_ms":{"type":"integer","description":"終わるまでこれだけ待つ (既定 0: すぐ答える。最大 50000: Claude Code はツールを 60 秒しか待たないので、長いものは何度か呼ぶ)"},
"kill":{"type":"boolean","description":"止める (SIGTERM をそのグループに)"}}}"#;

/// run bg で動かしているもの
struct BgJob {
    id: u64,
    pid: i32,
    /// 標準出力と標準エラー (memfd)
    out: i32,
    err: i32,
    cmd: String,
    t0: std::time::Instant,
    /// 終わったら (ステータス, かかった ms)。ステータスが -1 なら、ほかで待たれて分からない
    done: Option<(i32, u64)>,
}

static JOBS: std::sync::Mutex<Vec<BgJob>> = std::sync::Mutex::new(Vec::new());
static NEXT_JOB: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

const INSTRUCTIONS: &str = "aish (aios のシェル) です。run はいつも同じシェルで動くので、cd や変数は次の run に残ります。\
答えは JSON: run は {status, out, err, ms, pwd} (時間切れなら timeout: true)。\
重いもの (ビルドなど) は run の bg: true でうしろで動かし、job で様子と出力を見ると、そのあいだもほかのツールが使えます。\
ファイルの読み書きは read / edit / write / undo (aish-edit) を使うと確かです。";

impl Shell {
    pub fn mcp(&mut self, args: &[String]) -> ! {
        PID.store(unsafe { libc::getpid() }, Ordering::Relaxed);
        // aish --mcp [--json] [RC...]
        JSON.store(args.iter().any(|a| a == "--json"), Ordering::Relaxed);
        let rcs: Vec<String> = args.iter().filter(|a| *a != "--json").cloned().collect();
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
        // /etc/aishrc、~/.aishrc、それから aish --mcp RC... で渡したもの (ホームに設定を置けないところで)
        for rc in ["/etc/aishrc".to_string(), format!("{}/.aishrc", home)].into_iter().chain(rcs.iter().cloned()) {
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
        }), json!({
            "name": "job",
            "description": "run bg で動かしたものの様子と出力。{id, pid, done, status, out, err, ms}。id がなければ一覧。終わったものは、見たら消える",
            "inputSchema": serde_json::from_str::<Value>(JOB_INPUT).unwrap(),
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
            if args["bg"].as_bool().unwrap_or(false) { self.mcp_bg(&args) } else { self.mcp_run(&args) }
        } else if name == "job" {
            mcp_job(&args)
        } else {
            let pwd = std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default();
            let ev = json!({ "args": args, "pwd": pwd, "home": self.get_var("HOME").unwrap_or_default(), "histfile": self.histfile.clone().unwrap_or_default() });
            let r = match self.plugins.tool(name, ev) {
                Some(r) => r,
                None => json!({ "error": format!("{}: no such tool (or the plugin stopped)", name) }),
            };
            // ツールで使ったファイルも、コマンドで使ったのと同じに知らせる (aish-pick の paths に入る)
            if r.get("error").is_none()
                && let Some(p) = args["path"].as_str()
            {
                self.plugins.tell("preexec", json!({ "line": format!(": {}", super::quote(p)), "pwd": pwd }));
                self.plugins.tell("precmd", json!({ "status": 0 }));
            }
            r
        };
        let is_err = r.get("error").is_some() || r.get("timeout").is_some();
        // ふだんは読む形の text だけ (Claude Code は structuredContent があるとそちらを Claude に見せるので)。
        // aish --mcp --json なら、いままでどおり JSON (ほかのプログラムがつなぐとき)
        if JSON.load(Ordering::Relaxed) {
            json!({ "content": [{ "type": "text", "text": r.to_string() }], "structuredContent": r, "isError": is_err })
        } else {
            json!({ "content": [{ "type": "text", "text": render(&r) }], "isError": is_err })
        }
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
        // うしろのジョブも残す (止めるのは job の kill で)
        let mut keep = self.plugins.pids();
        keep.extend(JOBS.lock().map(|j| j.iter().filter(|x| x.done.is_none()).map(|x| x.pid).collect::<Vec<_>>()).unwrap_or_default());
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
        // プラグインには人が打ったときと同じように知らせる (aish-pick が使ったパスを覚える)
        let pwd = std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default();
        self.plugins.tell("preexec", json!({ "line": cmd.trim_end_matches('\n'), "pwd": pwd }));
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
            r["hint"] = json!("took too long: run it again with bg: true, and see it with job");
        }
        self.plugins.tell("precmd", json!({ "status": status }));
        r
    }

    /// run bg: シェルを fork した子で動かし (cd や変数はその子の中だけ)、すぐ答える。
    /// 出力は memfd に受けて、job で見る (ディスクには残さない)
    fn mcp_bg(&mut self, args: &Value) -> Value {
        let cmd = args["cmd"].as_str().unwrap_or("");
        flush();
        let (o, e) = (memfd("aish-bg-out"), memfd("aish-bg-err"));
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return json!({ "error": format!("fork: {}", super::last_err()) });
        }
        if pid == 0 {
            // 子: 自分のグループで (job の kill でまとめて止める)。標準入力は /dev/null のまま
            unsafe {
                libc::setpgid(0, 0);
                libc::dup2(o, 1);
                libc::dup2(e, 2);
            }
            let st = self.run_source(cmd, "run");
            super::exit_shell(st);
        }
        unsafe { libc::setpgid(pid, pid) };
        let id = NEXT_JOB.fetch_add(1, Ordering::Relaxed);
        let short: String = cmd.trim().chars().take(200).collect();
        if let Ok(mut js) = JOBS.lock() {
            js.push(BgJob { id, pid, out: o, err: e, cmd: short, t0: std::time::Instant::now(), done: None });
        }
        json!({ "job": id, "pid": pid })
    }
}

/// 終わったうしろのジョブを集める
fn reap(js: &mut [BgJob]) {
    for j in js.iter_mut().filter(|j| j.done.is_none()) {
        let mut st = 0;
        let r = unsafe { libc::waitpid(j.pid, &mut st, libc::WNOHANG) };
        let ms = j.t0.elapsed().as_millis() as u64;
        if r == j.pid {
            j.done = Some((super::exit_code(st), ms));
        } else if r < 0 {
            // ほかで待たれた (aish の wait など)。終わったが、ステータスは分からない
            j.done = Some((-1, ms));
        }
    }
}

/// job: うしろのジョブの様子と出力
fn mcp_job(a: &Value) -> Value {
    let Ok(mut js) = JOBS.lock() else { return json!({ "error": "jobs are busy" }) };
    reap(&mut js);
    let Some(id) = a["id"].as_u64() else {
        let list: Vec<Value> = js
            .iter()
            .map(|j| json!({ "id": j.id, "pid": j.pid, "cmd": j.cmd, "done": j.done.is_some(), "status": j.done.map(|d| d.0), "ms": j.done.map_or(j.t0.elapsed().as_millis() as u64, |d| d.1) }))
            .collect();
        return json!({ "jobs": list });
    };
    let Some(i) = js.iter().position(|j| j.id == id) else { return json!({ "error": format!("no job {}", id) }) };
    if a["kill"].as_bool().unwrap_or(false) && js[i].done.is_none() {
        unsafe { libc::kill(-js[i].pid, libc::SIGTERM) };
    }
    // wait_ms まで、終わるのを待つ
    let end = std::time::Instant::now() + std::time::Duration::from_millis(a["wait_ms"].as_u64().unwrap_or(0).min(50_000));
    while js[i].done.is_none() && std::time::Instant::now() < end {
        std::thread::sleep(std::time::Duration::from_millis(50));
        reap(&mut js[i..=i]);
    }
    let j = &js[i];
    let mut r = json!({ "id": j.id, "pid": j.pid, "done": j.done.is_some(), "out": peek(j.out), "err": peek(j.err) });
    match j.done {
        Some((st, ms)) => {
            r["status"] = json!(st);
            r["ms"] = json!(ms);
            // 終わったものは見たら消す
            unsafe {
                libc::close(j.out);
                libc::close(j.err);
            }
            js.remove(i);
        }
        None => r["ms"] = json!(j.t0.elapsed().as_millis() as u64),
    }
    r
}

/// memfd の中身を、閉じずに (読む場所も動かさずに) 読む。長ければ頭と終わり
fn peek(fd: i32) -> String {
    let mut b = Vec::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = unsafe { libc::pread(fd, buf.as_mut_ptr() as *mut _, buf.len(), b.len() as i64) };
        if n <= 0 {
            break;
        }
        b.extend_from_slice(&buf[..n as usize]);
    }
    cut(String::from_utf8_lossy(&b).into_owned())
}

/// 長ければ頭と終わりだけ
fn cut(s: String) -> String {
    if s.len() <= MAX_OUT {
        return s;
    }
    let head = floor(&s, MAX_OUT / 3);
    let tail = ceil(&s, s.len() - MAX_OUT * 2 / 3);
    format!("{}\n... ({} bytes cut) ...\n{}", &s[..head], tail - head, &s[tail..])
}

/// 答えを読む形にする (Claude が読む content の text)。プロトコルとしての答えは structuredContent の JSON のまま。
/// 出力や文書 (out err text) は JSON の中にエスケープせずにそのまま出し、ほかのものは終わりに 1 行の JSON で。
/// grep の matches はファイルごとにまとめる: パスの行のあとに 1 行に 1 つ (n line: text、前後の行は n のかわりに -)
fn render(r: &Value) -> String {
    let Some(o) = r.as_object() else { return r.to_string() };
    let mut s = String::new();
    let mut meta = serde_json::Map::new();
    for (k, v) in o {
        match (k.as_str(), v) {
            ("out" | "text", Value::String(t)) => s.push_str(t),
            ("err", Value::String(_)) => {}
            ("matches", Value::Array(ms)) => {
                let mut last = "";
                for m in ms {
                    let path = m["path"].as_str().unwrap_or("");
                    if path != last {
                        s.push_str(path);
                        s.push('\n');
                        last = path;
                    }
                    let n = m["n"].as_u64().map_or("-".to_string(), |n| n.to_string());
                    s.push_str(&format!("{} {}: {}\n", n, m["line"], m["text"].as_str().unwrap_or("")));
                }
            }
            _ => {
                meta.insert(k.clone(), v.clone());
            }
        }
    }
    // out と text のないもの (edit の答えなど) は、いままでどおり JSON だけ
    if s.is_empty() && !o.contains_key("out") && !o.contains_key("text") && !o.contains_key("matches") {
        return r.to_string();
    }
    if !s.is_empty() && !s.ends_with('\n') {
        s.push('\n');
    }
    if let Some(e) = o.get("err").and_then(|e| e.as_str()).filter(|e| !e.is_empty()) {
        s.push_str("[err]\n");
        s.push_str(e);
        if !e.ends_with('\n') {
            s.push('\n');
        }
    }
    // 残りがなければ {} は出さない
    if !meta.is_empty() {
        s.push_str(&Value::Object(meta).to_string());
    }
    s
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
    let s = peek(fd);
    unsafe { libc::close(fd) };
    s
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

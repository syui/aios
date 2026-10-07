// aish --mcp: aish を MCP のサーバーにする (Claude が aish を使う入り口)
//   標準入力と標準出力で JSON-RPC 2.0 を 1 行ずつ話す (MCP の stdio)。
//   ツール: run (シェルの行を動かす) と、プラグインの tools (aish-edit の read / edit / write / undo など)
// シェルは 1 つで、呼ばれるたびに同じシェルで動かす: cd、変数、関数、alias は次の run にも残る。
// 設定は対話するシェルと同じ /etc/aishrc と ~/.aishrc (AISH_MCP=1 なので、そこで分けられる)。
// run の出力は memfd (メモリーの上のファイル) に受けて、そのまま答えに入れる。
// ディスクには何も残さないので、リポジトリやイメージに入ることはない
use super::plugin::FileId;
use super::{Flow, Shell, cstr, flush};
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::os::fd::FromRawFd;
use std::ffi::CString;
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
/// 出力をそのまま返す長さ。これより長ければ頭と終わりだけ (行の切れ目で)。切ったものはメモリーに残して out で読む
const MAX_OUT: usize = 12_000;
/// 切った出力を残しておく数 (古いものから捨てる)
const KEEP_OUT: usize = 8;

/// 切った出力: (番号, 標準出力の memfd, 標準エラーの memfd)。ディスクには書かない
static SAVED: std::sync::Mutex<std::collections::VecDeque<(u64, i32, i32)>> = std::sync::Mutex::new(std::collections::VecDeque::new());
static NEXT_OUT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

const OUT_INPUT: &str = r#"{"type":"object","properties":{
"id":{"type":"integer","description":"run や job の答えの out_id (なければいちばん新しいもの)"},
"err":{"type":"boolean","description":"標準エラーのほうを (既定: 標準出力)"},
"grep":{"type":"string","description":"この文字をふくむ行だけ (行の番号つき)"},
"regex":{"type":"boolean","description":"grep を正規表現として"},
"from":{"type":"integer","description":"何行目から (1 から)"},
"to":{"type":"integer","description":"何行目まで"},
"limit":{"type":"integer","description":"返す行はいくつまで (既定 200)"}}}"#;

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
    /// もう見せた出力のバイト数 (job は新しい分だけ返す)
    seen: (usize, usize),
}

static JOBS: std::sync::Mutex<Vec<BgJob>> = std::sync::Mutex::new(Vec::new());
static NEXT_JOB: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// サーバーが起きた時刻と、そのときの aish のファイル (check がビルドしなおしたかを見る)
static STARTED: std::sync::OnceLock<(std::time::Instant, String, Option<FileId>)> = std::sync::OnceLock::new();

const INSTRUCTIONS: &str = "aish (aios のシェル) です。run はいつも同じシェルで動くので、cd や変数は次の run に残ります。\
答えは JSON: run は {status, out, err, ms, pwd} (時間切れなら timeout: true)。\
重いもの (ビルドなど) は run の bg: true でうしろで動かし、job で様子と出力を見ると、そのあいだもほかのツールが使えます。\
ファイルの読み書きは read / edit / write / undo (aish-edit) を使うと確かです。\
つながりやビルドが古くないかは check で見られます。";

impl Shell {
    pub fn mcp(&mut self, args: &[String]) -> ! {
        PID.store(unsafe { libc::getpid() }, Ordering::Relaxed);
        let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
        let _ = STARTED.set((std::time::Instant::now(), exe.clone(), FileId::of(&exe)));
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
        // 新しいビルドに入れかわったあと (reload): 読みかけの要求を先に、ツールが変わったことを知らせる
        let pending = std::env::var("AISH_MCP_PENDING").unwrap_or_default();
        let resumed = std::env::var_os("AISH_MCP_RESUMED").is_some();
        unsafe {
            std::env::remove_var("AISH_MCP_PENDING");
            std::env::remove_var("AISH_MCP_RESUMED");
        }
        if resumed {
            RELOADED.store(true, Ordering::Relaxed);
            send(&mut out, &json!({ "jsonrpc": "2.0", "method": "notifications/tools/list_changed" }));
        }
        let mut reader = std::io::BufReader::new(inp);
        let mut queue: std::collections::VecDeque<String> = pending.lines().map(String::from).collect();
        loop {
            let line = match queue.pop_front() {
                Some(l) => l,
                None => {
                    let mut l = String::new();
                    match reader.read_line(&mut l) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => l,
                    }
                }
            };
            if line.trim().is_empty() {
                continue;
            }
            let Ok(req) = serde_json::from_str::<Value>(&line) else {
                send(&mut out, &json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "parse error" } }));
                continue;
            };
            // id のないものは知らせ (notifications/initialized など)。答えない
            let Some(id) = req.get("id").cloned() else { continue };
            // ツールを使う前に: aish かプラグインがビルドしなおされていたら、同じつながりのまま新しいものに入れかわる
            // (/mcp でつなぎなおさなくていいように)。この要求と、まだ読んでいないものは新しいほうにわたす
            if matches!(req["method"].as_str(), Some("tools/call" | "tools/list")) && self.should_reload() {
                let mut rest = line.trim_end().to_string();
                rest.push('\n');
                for q in &queue {
                    rest.push_str(q.trim_end());
                    rest.push('\n');
                }
                rest.push_str(&String::from_utf8_lossy(reader.buffer()));
                reload(&rest, &mut reader, &out);
            }
            let reply = match req["method"].as_str().unwrap_or("") {
                "initialize" => Ok(json!({
                    "protocolVersion": req["params"]["protocolVersion"].as_str().unwrap_or(PROTOCOL),
                    "capabilities": { "tools": { "listChanged": true } },
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
            "description": "run bg で動かしたものの様子と出力。{id, pid, done, status, out, err, ms}。出力は前に見せたところから先だけ (shown_before はもう見せたバイト数)。id がなければ一覧。終わったものは、見たら消える",
            "inputSchema": serde_json::from_str::<Value>(JOB_INPUT).unwrap(),
        }), json!({
            "name": "out",
            "description": "run や job で長すぎて切った出力 (答えに out_id がある) を、もう一度動かさずに読む。grep で行を探すか、from / to で行の番号のところを。{id, lines, text}",
            "inputSchema": serde_json::from_str::<Value>(OUT_INPUT).unwrap(),
        }), json!({
            "name": "check",
            "description": "aish のつながりの様子: 版、起きてからの時間、プラグインが生きているか、ビルドしなおしたもの (つなぎなおすと新しくなる) や、ソースがバイナリより新しいもの (ビルドが要る)。problems が空なら ok",
            "inputSchema": { "type": "object", "properties": {} },
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
        } else if name == "out" {
            mcp_out(&args)
        } else if name == "check" {
            self.mcp_check()
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
        let mut r = r;
        if RELOADED.swap(false, Ordering::Relaxed)
            && let Some(o) = r.as_object_mut()
        {
            o.insert("reloaded".into(), json!("aish restarted with the new build (shell variables were reset; the directory is kept)"));
        }
        let is_err = r.get("error").is_some() || r.get("timeout").is_some();
        // ふだんは読む形の text だけ (Claude Code は structuredContent があるとそちらを Claude に見せるので)。
        // aish --mcp --json なら、いままでどおり JSON (ほかのプログラムがつなぐとき)
        if JSON.load(Ordering::Relaxed) {
            json!({ "content": [{ "type": "text", "text": r.to_string() }], "structuredContent": r, "isError": is_err })
        } else {
            json!({ "content": [{ "type": "text", "text": render(&r) }], "isError": is_err })
        }
    }

    /// 入れかわるか: aish かプラグインのバイナリが起きたときと変わっていて、書き終わっている (2 秒たった) こと。
    /// ビルドの途中 (cargo が動いている) と、うしろのジョブが動いているときは入れかわらない
    fn should_reload(&mut self) -> bool {
        let Some((_, exe, file)) = STARTED.get().cloned() else { return false };
        let settled = |f: &FileId| f.mtime.elapsed().is_ok_and(|d| d.as_secs() >= 2);
        let me = FileId::of(&exe).filter(|d| Some(*d) != file && settled(d)).is_some();
        let plugins = self.plugins.check().iter().any(|p| p["rebuilt"] == true && p["prog"].as_str().and_then(FileId::of).is_some_and(|f| settled(&f)));
        if !me && !plugins {
            return false;
        }
        if JOBS.lock().is_ok_and(|j| j.iter().any(|x| x.done.is_none())) {
            return false;
        }
        let src = self.get_var("AISH_SRC").filter(|s| !s.is_empty());
        src.as_deref().map(building).unwrap_or_default().is_empty()
    }

    /// check: aish とプラグインの様子と、直すこと (problems)。
    /// AISH_SRC (bin/aish-mcp.sh がリポジトリを入れる) があれば、ソースがバイナリより新しいかも見る
    fn mcp_check(&mut self) -> Value {
        let (t0, exe, file) = STARTED.get().cloned().unwrap_or((std::time::Instant::now(), String::new(), None));
        let src = self.get_var("AISH_SRC").filter(|s| !s.is_empty());
        let mut problems: Vec<String> = Vec::new();
        let mut text = format!("aish {}  pid {}  up {}\n  {}\n", env!("CARGO_PKG_VERSION"), PID.load(Ordering::Relaxed), dur(t0.elapsed().as_secs()), exe);
        let disk = FileId::of(&exe);
        let rebuilt = disk.is_some() && disk != file;
        if rebuilt {
            problems.push("aish: rebuilt after the server started; it restarts itself with the new build at the next tool call".into());
        }
        // ソースのほうが新しい: まだビルドしていない (ディスクのバイナリとくらべる)
        let stale = |dirs: &[String], bin: Option<u64>| -> Option<String> {
            let (t, p) = dirs.iter().filter_map(|d| newest(std::path::Path::new(d))).max()?;
            (bin.is_some_and(|b| t > b)).then(|| p.strip_prefix(src.as_deref().unwrap_or("")).unwrap_or(&p).trim_start_matches('/').to_string())
        };
        if let Some(src) = &src
            && let Some(p) = stale(&[format!("{}/user/src", src)], disk.map(|f| f.secs()))
        {
            problems.push(format!("aish: {} changed after the build; build it (bin/aish-mcp.sh --build) and aish restarts itself", p));
        }
        let plugins = self.plugins.check();
        let alive = plugins.iter().filter(|p| p["alive"] == true).count();
        text.push_str(&format!("plugins {}/{} alive\n", alive, plugins.len()));
        for p in &plugins {
            let name = p["name"].as_str().unwrap_or("?");
            let tools: Vec<&str> = p["tools"].as_array().map(|a| a.iter().filter_map(|t| t.as_str()).collect()).unwrap_or_default();
            let mut note = String::new();
            if p["alive"] != true {
                let why = p["why"].as_str().unwrap_or("stopped");
                note = format!("  [stopped: {}]", why);
                problems.push(format!("plugin {}: stopped ({}); rebuild it, or reconnect (/mcp), to start it again", name, why));
            } else if p["rebuilt"] == true {
                note = "  [rebuilt]".into();
                problems.push(format!("plugin {}: rebuilt after it started; aish restarts with it at the next tool call", name));
            }
            if let (Some(src), Some(prog)) = (&src, p["prog"].as_str()) {
                // aish-edit は shell/edit (と SDK の shell/plugin)
                let dir = prog.rsplit('/').next().unwrap_or("").trim_start_matches("aish-");
                let dirs = [format!("{}/shell/{}/src", src, dir), format!("{}/shell/plugin/src", src)];
                if std::path::Path::new(&dirs[0]).is_dir()
                    && let Some(f) = stale(&dirs, FileId::of(prog).map(|f| f.secs()))
                {
                    problems.push(format!("plugin {}: {} changed after the build; build it (bin/aish-mcp.sh --build) and aish restarts itself", name, f));
                }
            }
            text.push_str(&format!("  {:<8} {}{}\n", name, tools.join(" "), note));
        }
        let building = src.as_deref().map(building).unwrap_or_default();
        if let Some(src) = &src {
            text.push_str(&format!("source {}{}\n", src, if building.is_empty() { String::new() } else { format!("  (building: cargo pid {})", building.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(" ")) }));
        }
        if problems.is_empty() {
            text.push_str("ok\n");
        } else {
            text.push_str("problems\n");
            for p in &problems {
                text.push_str(&format!("  - {}\n", p));
            }
        }
        json!({ "text": text, "ok": problems.is_empty(), "problems": problems, "building": !building.is_empty() })
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
        let (out, err, id) = finish(o, e);
        let mut r = json!({ "status": status, "out": out, "err": err, "ms": ms, "pwd": pwd });
        if let Some(id) = id {
            r["out_id"] = json!(id);
        }
        if fired.load(Ordering::Relaxed) {
            // timeout(1) と同じ 124
            STOP.store(false, Ordering::Relaxed);
            status = 124;
            r["status"] = json!(status);
            r["timeout"] = json!(true);
            r["hint"] = json!("took too long: run it again with bg: true, and see it with job");
        }
        // 砂場 (aibox) の中で書けなかった: どこなら書けるかを教える (ファイルの持ち主のせいと分けられるように)
        if status != 0
            && r["err"].as_str().is_some_and(|e| e.contains("Permission denied") || e.contains("Operation not permitted") || e.contains("no new privileges"))
            && let Some(w) = sandboxed()
        {
            r["hint"] = json!(format!("aish runs in a sandbox (aibox, landlock): writable only under {}. sudo does not work inside; root actions go through `aios do`", w));
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
            js.push(BgJob { id, pid, out: o, err: e, cmd: short, t0: std::time::Instant::now(), done: None, seen: (0, 0) });
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
    let j = &mut js[i];
    let mut r = json!({ "id": j.id, "pid": j.pid, "done": j.done.is_some() });
    // 前に見せたところから先だけ (何度も見ても同じ出力をくりかえさない)
    let fresh = |all: String, seen: usize| -> String { all.get(seen..).map(String::from).unwrap_or(all) };
    if j.seen != (0, 0) {
        r["shown_before"] = json!(j.seen.0 + j.seen.1);
    }
    match j.done {
        Some((st, ms)) => {
            // 終わったものは見たら消す (長くて切ったものは out で読めるように残す)
            let seen = j.seen;
            let (out, err, oid) = if seen == (0, 0) {
                finish(j.out, j.err)
            } else {
                let (o, e) = (fresh(read_all(j.out), seen.0), fresh(read_all(j.err), seen.1));
                let (o, e, id) = (cut(o, None), cut(e, None), None);
                unsafe {
                    libc::close(j.out);
                    libc::close(j.err);
                }
                (o, e, id)
            };
            r["out"] = json!(out);
            r["err"] = json!(err);
            if let Some(oid) = oid {
                r["out_id"] = json!(oid);
            }
            r["status"] = json!(st);
            r["ms"] = json!(ms);
            js.remove(i);
        }
        None => {
            let (o, e) = (read_all(j.out), read_all(j.err));
            let (no, ne) = (o.len(), e.len());
            r["out"] = json!(cut(fresh(o, j.seen.0), None));
            r["err"] = json!(cut(fresh(e, j.seen.1), None));
            j.seen = (no, ne);
            r["ms"] = json!(j.t0.elapsed().as_millis() as u64);
        }
    }
    r
}

/// 終わったものの出力を答えにする。どちらかが長ければ頭と終わりだけにして、memfd は out のために残す (番号を返す)。
/// 短ければ閉じる
fn finish(o: i32, e: i32) -> (String, String, Option<u64>) {
    let (out, err) = (read_all(o), read_all(e));
    if out.len() <= MAX_OUT && err.len() <= MAX_OUT {
        unsafe {
            libc::close(o);
            libc::close(e);
        }
        return (out, err, None);
    }
    let id = NEXT_OUT.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut sv) = SAVED.lock() {
        sv.push_back((id, o, e));
        while sv.len() > KEEP_OUT {
            if let Some((_, a, b)) = sv.pop_front() {
                unsafe {
                    libc::close(a);
                    libc::close(b);
                }
            }
        }
    }
    (cut(out, Some(id)), cut(err, Some(id)), Some(id))
}

/// out: 切った出力を、grep か行の番号で読む
fn mcp_out(a: &Value) -> Value {
    let Ok(sv) = SAVED.lock() else { return json!({ "error": "busy" }) };
    let ent = match a["id"].as_u64() {
        Some(id) => sv.iter().find(|x| x.0 == id),
        None => sv.back(),
    };
    let Some(&(id, o, e)) = ent else {
        let have: Vec<u64> = sv.iter().map(|x| x.0).collect();
        return json!({ "error": format!("no saved output {} (have: {:?}; only the last {} long outputs are kept)", a["id"], have, KEEP_OUT) });
    };
    let text = read_all(if a["err"].as_bool().unwrap_or(false) { e } else { o });
    let lines: Vec<&str> = text.lines().collect();
    let limit = a["limit"].as_u64().unwrap_or(200) as usize;
    let from = a["from"].as_u64().unwrap_or(1).max(1) as usize;
    let to = a["to"].as_u64().map_or(lines.len(), |t| (t as usize).min(lines.len()));
    let re = match (a["grep"].as_str(), a["regex"].as_bool().unwrap_or(false)) {
        (Some(g), true) => match regex::Regex::new(g) {
            Ok(r) => Some(r),
            Err(err) => return json!({ "error": format!("regex: {}", err) }),
        },
        _ => None,
    };
    let mut out = String::new();
    let mut n = 0;
    let mut more = 0;
    for (k, l) in lines.iter().enumerate().take(to).skip(from - 1) {
        let hit = match (a["grep"].as_str(), &re) {
            (_, Some(r)) => r.is_match(l),
            (Some(g), None) => l.contains(g),
            (None, _) => true,
        };
        if !hit {
            continue;
        }
        if n == limit {
            more += 1;
            continue;
        }
        out.push_str(&format!("{}: {}\n", k + 1, l));
        n += 1;
    }
    let mut r = json!({ "id": id, "lines": lines.len(), "text": out });
    if more > 0 {
        r["more"] = json!(more);
    }
    r
}

/// memfd の中身を、閉じずに (読む場所も動かさずに) ぜんぶ読む
fn read_all(fd: i32) -> String {
    let mut b = Vec::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = unsafe { libc::pread(fd, buf.as_mut_ptr() as *mut _, buf.len(), b.len() as i64) };
        if n <= 0 {
            break;
        }
        b.extend_from_slice(&buf[..n as usize]);
    }
    String::from_utf8_lossy(&b).into_owned()
}

/// 長ければ頭 (1/3) と終わり (2/3) だけ。行の切れ目で切り、切った行の数と、out で読むための番号を書く
fn cut(s: String, id: Option<u64>) -> String {
    if s.len() <= MAX_OUT {
        return s;
    }
    let mut head = floor(&s, MAX_OUT / 3);
    if let Some(k) = s[..head].rfind('\n') {
        head = k + 1;
    }
    let mut tail = ceil(&s, s.len() - MAX_OUT * 2 / 3);
    if let Some(k) = s[tail..].find('\n') {
        tail += k + 1;
    }
    let lines = s[head..tail].matches('\n').count();
    let how = id.map_or(String::new(), |i| format!(". read it with the out tool: {{\"id\": {}, \"grep\": \"...\"}} or from/to", i));
    format!("{}... ({} lines, {} bytes cut{}) ...\n{}", &s[..head], lines, tail - head, how, &s[tail..])
}

/// 答えを読む形にする (Claude が読む content の text)。プロトコルとしての答えは structuredContent の JSON のまま。
/// 出力や文書 (out err text) は JSON の中にエスケープせずにそのまま出し、ほかのものは終わりに 1 行の JSON で。
/// grep の matches はファイルごとにまとめる: パスの行のあとに 1 行に 1 つ (n line: text、前後の行は n のかわりに -)。
/// where と outline の items も 1 行に 1 つ
fn render(r: &Value) -> String {
    let Some(o) = r.as_object() else { return r.to_string() };
    let mut s = String::new();
    let mut meta = serde_json::Map::new();
    for (k, v) in o {
        match (k.as_str(), v) {
            ("out" | "text", Value::String(t)) => s.push_str(t),
            ("err", Value::String(_)) => {}
            // where と outline (aish-map) の items: 1 行に 1 つ。where は path:line kind name: text、
            // outline は字下げして line kind name (JSON のままより短く、grep の答えと同じように読める)
            ("items", Value::Array(items)) if items.iter().all(|i| i["line"].is_u64() && i["name"].is_string()) => {
                for i in items {
                    let (line, kind, name) = (i["line"].as_u64().unwrap_or(0), i["kind"].as_str().unwrap_or(""), i["name"].as_str().unwrap_or(""));
                    match i["path"].as_str() {
                        Some(p) => s.push_str(&format!("{}:{} {} {}: {}\n", p, line, kind, name, i["text"].as_str().unwrap_or("").trim())),
                        None => s.push_str(&format!("{}{} {} {}\n", "  ".repeat(i["indent"].as_u64().unwrap_or(0) as usize), line, kind, name)),
                    }
                }
            }
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
    if s.is_empty() && !o.contains_key("out") && !o.contains_key("text") && !o.contains_key("matches") && !o.contains_key("items") {
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

/// 砂場の中なら、書けるところ (aibox の AIBOX_WRITE。なければ「決まったところ」)
fn sandboxed() -> Option<String> {
    let st = std::fs::read_to_string("/proc/self/status").ok()?;
    let n: u32 = st.lines().find_map(|l| l.strip_prefix("Landlock:"))?.trim().parse().ok()?;
    if n == 0 {
        return None;
    }
    Some(std::env::var("AIBOX_WRITE").ok().filter(|w| !w.is_empty()).map(|w| w.replace(':', ", ")).unwrap_or_else(|| "the directories it was given".into()))
}

/// 入れかわったあとの最初の答えに、そう書く
static RELOADED: AtomicBool = AtomicBool::new(false);

/// 同じ引数で自分を exec しなおす。プロトコルの fd を 0 と 1 に戻し、読みかけのもの (pending) は環境変数でわたす。
/// プラグインは閉じた標準入力で終わり、新しいほうがまた起こす。しくじったら、そのまま古いほうで続ける
fn reload(pending: &str, reader: &mut std::io::BufReader<std::fs::File>, out: &std::fs::File) {
    use std::os::fd::AsRawFd;
    let exe = STARTED.get().map(|s| s.1.clone()).unwrap_or_default();
    let args: Vec<CString> = std::env::args().map(|a| cstr(&a)).collect();
    let mut argv: Vec<*const libc::c_char> = args.iter().map(|a| a.as_ptr()).collect();
    argv.push(std::ptr::null());
    unsafe {
        std::env::set_var("AISH_MCP_PENDING", pending);
        std::env::set_var("AISH_MCP_RESUMED", "1");
        // AISH_MCP は mcp() がまたつける。ほかの子のための環境はそのまま
        std::env::remove_var("AISH_MCP");
        libc::dup2(reader.get_ref().as_raw_fd(), 0);
        libc::dup2(out.as_raw_fd(), 1);
        libc::execv(cstr(&exe).as_ptr(), argv.as_ptr());
        // しくじった: 戻す
        let null = libc::open(cstr("/dev/null").as_ptr(), libc::O_RDWR);
        libc::dup2(null, 0);
        libc::close(null);
        libc::dup2(2, 1);
        std::env::remove_var("AISH_MCP_PENDING");
        std::env::remove_var("AISH_MCP_RESUMED");
        std::env::set_var("AISH_MCP", "1");
    }
}

/// 秒を "1h 2m 3s" に
fn dur(s: u64) -> String {
    match s {
        0..60 => format!("{}s", s),
        60..3600 => format!("{}m {}s", s / 60, s % 60),
        _ => format!("{}h {}m", s / 3600, s / 60 % 60),
    }
}

/// ディレクトリの下でいちばん新しく変えたファイル (1970 からの秒, パス)
fn newest(dir: &std::path::Path) -> Option<(u64, String)> {
    let mut best: Option<(u64, String)> = None;
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let Ok(m) = e.metadata() else { continue };
        let found = if m.is_dir() {
            newest(&e.path())
        } else {
            m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| (d.as_secs(), e.path().display().to_string()))
        };
        if found.as_ref().is_some_and(|f| best.as_ref().is_none_or(|b| f.0 > b.0)) {
            best = found;
        }
    }
    best
}

/// src の下で動いている cargo (bin/aish-mcp.sh がうしろでビルドしているもの)
fn building(src: &str) -> Vec<i32> {
    let Ok(rd) = std::fs::read_dir("/proc") else { return Vec::new() };
    rd.flatten()
        .filter_map(|e| e.file_name().to_string_lossy().parse::<i32>().ok())
        .filter(|pid| std::fs::read_to_string(format!("/proc/{}/comm", pid)).is_ok_and(|c| c.trim() == "cargo"))
        .filter(|pid| std::fs::read_link(format!("/proc/{}/cwd", pid)).is_ok_and(|d| d.starts_with(src)))
        .collect()
}

fn send(out: &mut std::fs::File, v: &Value) {
    let mut s = v.to_string();
    s.push('\n');
    let _ = out.write_all(s.as_bytes());
}

fn memfd(name: &str) -> i32 {
    unsafe { libc::memfd_create(cstr(name).as_ptr(), libc::MFD_CLOEXEC) }
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

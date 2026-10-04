// aish-fix: ビルドのエラーを番号つきの一覧に (vim の quickfix。aish の基本のプラグイン)
//   build   コマンド (既定 cargo build) をうしろで動かし、終わるか wait_ms まで待って、エラーを n つきで
//   errors  動いているビルドを待って (終わっていればそのまま) エラーを n つきで
//   fix     n 番のエラーの全文 (rustc の rendered) と、そのまわりのソースを行の番号つきで
//   M-e     ビルドして、エラーを選んで「$EDITOR +行 ファイル」にする
// cargo には --message-format=json を足して、rustc の診断をそのまま読む (場所がずれない)。
// ほかのコマンド (make、cc、zig) は「path:line:col: error: ...」の行を拾う。
// Claude Code はツールの答えを 60 秒しか待たないので、長いビルドは running で答えて errors で待つ。
// 出力はこのプロセスのメモリーだけ (ディスクに何も残さない)
use aish_plugin::{Key, Spec, Tool, Tty, Value, error, json, pick, s};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const BUILD: &str = r#"{"type":"object","properties":{"cmd":{"type":"string","description":"動かすコマンド (sh -c。既定 cargo build。cargo build/check/test/clippy/run には --message-format=json を足す)"},"dir":{"type":"string","description":"動かすディレクトリ (既定 いまのディレクトリ)"},"wait_ms":{"type":"integer","description":"終わるのをこれだけ待つ (既定 45000。過ぎたら running: true で答えるので、errors で待つ)"},"warnings":{"type":"boolean","description":"警告も一覧に (既定 false: 数だけ)"}}}"#;
const ERRORS: &str = r#"{"type":"object","properties":{"wait_ms":{"type":"integer","description":"動いているビルドをこれだけ待つ (既定 45000)"},"warnings":{"type":"boolean","description":"警告も一覧に"},"kill":{"type":"boolean","description":"動いているビルドを止める"}}}"#;
const FIX: &str = r#"{"type":"object","properties":{"n":{"type":"integer","description":"build / errors の答えの n"},"context":{"type":"integer","description":"前後の行 (既定 5)"}},"required":["n"]}"#;

/// 一覧に出す数の上限 (それより多ければ more に数だけ)
const MAX_ITEMS: usize = 100;
/// エラーが拾えずにしくじったとき、答えに入れる標準エラーの終わり
const TAIL: usize = 3000;

/// 診断 1 つ
struct Diag {
    error: bool,
    /// "error[E0308]: mismatched types" のような 1 行
    head: String,
    /// 見せる場所 (ビルドしたディレクトリから見て)。場所がなければ空
    path: String,
    /// 本当の場所
    file: Option<PathBuf>,
    line: usize,
    /// 全文 (rustc の rendered。なければ head)
    rendered: String,
}

struct Build {
    cmd: String,
    dir: PathBuf,
    child: Option<Child>,
    out: Arc<Mutex<Vec<u8>>>,
    err: Arc<Mutex<Vec<u8>>>,
    readers: Vec<JoinHandle<()>>,
    start: Instant,
    ms: u128,
    status: Option<i32>,
    diags: Vec<Diag>,
}

fn main() {
    let spec = Spec {
        name: "fix",
        hooks: &["key"],
        keys: &[("M-e", "errors")],
        tools: &[
            Tool {
                name: "build",
                desc: "ビルドして (既定 cargo build。うしろで動かす)、エラーを番号 n つきで返す (n path:line: error[E..]: ...)。wait_ms を過ぎたら running: true なので errors で待つ。n は fix で全文とまわりのソースに",
                input: BUILD,
            },
            Tool { name: "errors", desc: "動いているビルドを待って、エラーを n つきで (build と同じ形)。kill: true で止める", input: ERRORS },
            Tool { name: "fix", desc: "build / errors の n 番のエラーの全文 (rustc の説明と help) と、その場所のまわりのソースを行の番号つきで", input: FIX },
        ],
    };
    let mut cur: Option<Build> = None;
    aish_plugin::run(spec, |ev, v| match ev {
        "tool" => {
            let a = &v["args"];
            match s(v, "name") {
                "build" => {
                    let cmd = a["cmd"].as_str().filter(|c| !c.trim().is_empty()).unwrap_or("cargo build");
                    let dir = resolve(s(v, "pwd"), a["dir"].as_str().unwrap_or("."));
                    if let Some(b) = cur.as_mut() {
                        b.kill();
                    }
                    match Build::start(cmd, &dir) {
                        Ok(b) => {
                            let b = cur.insert(b);
                            b.wait(wait_of(a));
                            b.answer(a["warnings"].as_bool().unwrap_or(false))
                        }
                        Err(e) => error(format!("{}: {}", cmd, e)),
                    }
                }
                "errors" => {
                    let Some(b) = cur.as_mut() else { return error("no build yet (use build first)") };
                    if a["kill"].as_bool().unwrap_or(false) {
                        b.kill();
                    }
                    b.wait(wait_of(a));
                    b.answer(a["warnings"].as_bool().unwrap_or(false))
                }
                "fix" => match cur.as_ref() {
                    Some(b) => fix(b, a),
                    None => error("no build yet (use build first)"),
                },
                n => error(format!("{}: no such tool", n)),
            }
        }
        "key" => key(s(v, "pwd"), &mut cur),
        _ => json!({}),
    });
}

fn wait_of(a: &Value) -> Duration {
    Duration::from_millis(a["wait_ms"].as_u64().unwrap_or(45_000))
}

fn resolve(pwd: &str, p: &str) -> PathBuf {
    let p = Path::new(p);
    if p.is_absolute() { p.to_path_buf() } else { Path::new(pwd).join(p) }
}

/// cargo の build check test clippy run なら --message-format=json を足す (もう書いてあればそのまま)
fn with_json(cmd: &str) -> (String, bool) {
    let words: Vec<&str> = cmd.split_whitespace().collect();
    let Some(i) = words.iter().position(|w| *w == "cargo" || w.ends_with("/cargo")) else { return (cmd.to_string(), false) };
    if cmd.contains("--message-format") || cmd.contains(['|', ';', '&', '>']) {
        return (cmd.to_string(), cmd.contains("--message-format=json"));
    }
    // cargo +toolchain -q build ... の build を見つけ、その後ろに入れる
    let Some(j) = words[i + 1..].iter().position(|w| !w.starts_with('-') && !w.starts_with('+')).map(|j| i + 1 + j) else {
        return (cmd.to_string(), false);
    };
    if !matches!(words[j], "build" | "b" | "check" | "c" | "test" | "t" | "clippy" | "run" | "r" | "rustc" | "bench") {
        return (cmd.to_string(), false);
    }
    let mut w: Vec<String> = words.iter().map(|w| w.to_string()).collect();
    w.insert(j + 1, "--message-format=json".into());
    (w.join(" "), true)
}

fn reader(mut r: impl Read + Send + 'static, buf: Arc<Mutex<Vec<u8>>>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut chunk = [0u8; 16 * 1024];
        loop {
            match r.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.lock().unwrap().extend_from_slice(&chunk[..n]),
            }
        }
    })
}

impl Build {
    fn start(cmd: &str, dir: &Path) -> std::io::Result<Build> {
        use std::os::unix::process::CommandExt;
        let (full, _) = with_json(cmd);
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&full)
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // 止めるときに孫 (rustc や cc) まで止められるように、自分のグループで
            .process_group(0)
            .spawn()?;
        let out = Arc::new(Mutex::new(Vec::new()));
        let err = Arc::new(Mutex::new(Vec::new()));
        let readers = vec![reader(child.stdout.take().unwrap(), out.clone()), reader(child.stderr.take().unwrap(), err.clone())];
        Ok(Build { cmd: full, dir: dir.to_path_buf(), child: Some(child), out, err, readers, start: Instant::now(), ms: 0, status: None, diags: vec![] })
    }

    /// 終わるか d が過ぎるまで待つ。終わったら診断を読む
    fn wait(&mut self, d: Duration) {
        let until = Instant::now() + d;
        while let Some(c) = self.child.as_mut() {
            match c.try_wait() {
                Ok(Some(st)) => {
                    use std::os::unix::process::ExitStatusExt;
                    self.status = Some(st.code().unwrap_or(128 + st.signal().unwrap_or(0)));
                    self.finish();
                }
                Ok(None) if Instant::now() < until => std::thread::sleep(Duration::from_millis(50)),
                Ok(None) => return,
                Err(_) => {
                    self.status = Some(-1);
                    self.finish();
                }
            }
        }
    }

    fn finish(&mut self) {
        self.child = None;
        self.ms = self.start.elapsed().as_millis();
        for r in self.readers.drain(..) {
            let _ = r.join();
        }
        let out = String::from_utf8_lossy(&self.out.lock().unwrap()).into_owned();
        let err = String::from_utf8_lossy(&self.err.lock().unwrap()).into_owned();
        let mut diags = Vec::new();
        for line in out.lines() {
            if line.starts_with('{') {
                cargo_line(line, &self.dir, &mut diags);
            } else {
                text_line(line, &self.dir, &mut diags);
            }
        }
        for line in err.lines() {
            text_line(line, &self.dir, &mut diags);
        }
        // 同じもの (いくつものターゲットで同じエラー) は 1 つに
        let mut seen = std::collections::HashSet::new();
        diags.retain(|d| seen.insert((d.head.clone(), d.path.clone(), d.line)));
        // エラーが先
        diags.sort_by_key(|d| !d.error);
        self.diags = diags;
    }

    fn kill(&mut self) {
        if let Some(c) = self.child.as_mut() {
            let _ = Command::new("kill").arg("-TERM").arg(format!("-{}", c.id())).stderr(Stdio::null()).status();
            let _ = c.kill();
            let _ = c.wait();
            self.status = Some(143);
            self.finish();
        }
    }

    fn shown(&self, warnings: bool) -> Vec<&Diag> {
        self.diags.iter().filter(|d| d.error || warnings).collect()
    }

    fn answer(&self, warnings: bool) -> Value {
        if self.child.is_some() {
            return json!({ "running": true, "cmd": self.cmd, "ms": self.start.elapsed().as_millis() as u64, "hint": "まだ動いている。errors で待つ" });
        }
        let errors = self.diags.iter().filter(|d| d.error).count();
        let shown = self.shown(warnings);
        let matches: Vec<Value> = shown
            .iter()
            .take(MAX_ITEMS)
            .enumerate()
            .map(|(i, d)| json!({ "n": i + 1, "path": if d.path.is_empty() { "-" } else { &d.path }, "line": d.line, "text": d.head }))
            .collect();
        let mut r = json!({
            "done": true,
            "status": self.status.unwrap_or(-1),
            "ms": self.ms as u64,
            "errors": errors,
            "warnings": self.diags.len() - errors,
            "cmd": self.cmd,
            "matches": matches,
        });
        if shown.len() > MAX_ITEMS {
            r["more"] = json!(shown.len() - MAX_ITEMS);
        }
        // しくじったのにエラーを拾えなかった (cargo そのもののエラーなど): 標準エラーの終わりを見せる
        if self.status != Some(0) && errors == 0 {
            let e = String::from_utf8_lossy(&self.err.lock().unwrap()).into_owned();
            let mut cut = e.len().saturating_sub(TAIL);
            while !e.is_char_boundary(cut) {
                cut += 1;
            }
            r["err"] = json!(&e[cut..]);
        }
        r
    }
}

/// cargo の 1 行の JSON (reason: compiler-message)
fn cargo_line(line: &str, dir: &Path, diags: &mut Vec<Diag>) {
    let Ok(v) = serde_json::from_str::<Value>(line) else { return };
    if s(&v, "reason") != "compiler-message" {
        return;
    }
    let m = &v["message"];
    let level = s(m, "level");
    let error = level.starts_with("error");
    if !error && level != "warning" {
        return;
    }
    let msg = s(m, "message");
    let head = match m["code"]["code"].as_str() {
        Some(c) => format!("{}[{}]: {}", level, c, msg),
        None => format!("{}: {}", level, msg),
    };
    // 「warning: N warnings emitted」のようなまとめは要らない
    if !error && m["spans"].as_array().is_none_or(|a| a.is_empty()) && msg.contains("warning") && msg.contains("emitted") {
        return;
    }
    let spans = m["spans"].as_array().cloned().unwrap_or_default();
    let span = spans.iter().find(|sp| sp["is_primary"].as_bool().unwrap_or(false)).or(spans.first());
    let (mut path, mut file, mut ln) = (String::new(), None, 0);
    if let Some(sp) = span {
        // マクロの中なら、呼んだ場所へ
        let mut sp = sp.clone();
        while sp["expansion"]["span"].is_object() && s(&sp, "file_name").starts_with('<') {
            sp = sp["expansion"]["span"].clone();
        }
        let f = find_file(dir, s(&sp, "file_name"), v["manifest_path"].as_str());
        path = show(dir, f.as_deref(), s(&sp, "file_name"));
        file = f;
        ln = sp["line_start"].as_u64().unwrap_or(0) as usize;
    }
    let rendered = m["rendered"].as_str().map(strip_ansi).unwrap_or_else(|| head.clone());
    diags.push(Diag { error, head, path, file, line: ln, rendered });
}

/// 「path:line:col: error: msg」(cc、zig、make、rustc の short) の行
fn text_line(line: &str, dir: &Path, diags: &mut Vec<Diag>) {
    let line = strip_ansi(line);
    for (tag, error) in [(": fatal error: ", true), (": error: ", true), (": warning: ", false)] {
        let Some(at) = line.find(tag) else { continue };
        let (loc, msg) = (&line[..at], &line[at + tag.len()..]);
        // loc = path:line[:col]
        let mut parts = loc.rsplitn(3, ':');
        let a = parts.next().unwrap_or("");
        let b = parts.next();
        let c = parts.next();
        let (p, ln) = match (a.parse::<usize>(), b.map(|b| b.parse::<usize>())) {
            (Ok(_col), Some(Ok(l))) => (c.unwrap_or(""), l),
            (Ok(l), _) => (b.map_or("", |b| b), l),
            _ => continue,
        };
        if p.is_empty() || p.contains(' ') {
            continue;
        }
        let f = find_file(dir, p, None);
        let head = format!("{}: {}", if error { "error" } else { "warning" }, msg.trim());
        diags.push(Diag { error, path: show(dir, f.as_deref(), p), file: f, line: ln, rendered: line.clone(), head });
        return;
    }
}

/// 診断のファイルの名前から本当の場所: ビルドしたディレクトリ、その上、マニフェストのディレクトリの順
fn find_file(dir: &Path, name: &str, manifest: Option<&str>) -> Option<PathBuf> {
    if name.is_empty() || name.starts_with('<') {
        return None;
    }
    let p = Path::new(name);
    if p.is_absolute() {
        return p.exists().then(|| p.to_path_buf());
    }
    let mut bases: Vec<PathBuf> = dir.ancestors().map(Path::to_path_buf).collect();
    if let Some(m) = manifest.and_then(|m| Path::new(m).parent()) {
        bases.extend(m.ancestors().map(Path::to_path_buf));
    }
    bases.into_iter().map(|b| b.join(p)).find(|f| f.is_file())
}

/// 見せる名前: ビルドしたディレクトリの下ならそこから、なければ本当の場所 (わからなければもとの名前)
fn show(dir: &Path, file: Option<&Path>, name: &str) -> String {
    match file {
        Some(f) => f.strip_prefix(dir).map_or_else(|_| f.display().to_string(), |r| r.display().to_string()),
        None => name.to_string(),
    }
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' && it.peek() == Some(&'[') {
            it.next();
            for d in it.by_ref() {
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn fix(b: &Build, a: &Value) -> Value {
    let n = a["n"].as_u64().unwrap_or(0) as usize;
    let shown: Vec<&Diag> = b.diags.iter().filter(|d| d.error).chain(b.diags.iter().filter(|d| !d.error)).collect();
    let Some(d) = n.checked_sub(1).and_then(|i| shown.get(i)) else { return error(format!("no error {} (the last build has {})", n, shown.len())) };
    let mut text = d.rendered.trim_end().to_string();
    text.push('\n');
    if let Some(f) = d.file.as_ref().filter(|_| d.line > 0) {
        let c = a["context"].as_u64().unwrap_or(5) as usize;
        if let Ok(src) = std::fs::read_to_string(f) {
            text.push_str(&format!("--- {}:{}\n", d.path, d.line));
            let from = d.line.saturating_sub(c).max(1);
            for (i, l) in src.lines().enumerate().skip(from - 1).take(c * 2 + 1) {
                let k = i + 1;
                text.push_str(&format!("{}{:>5}  {}\n", if k == d.line { '>' } else { ' ' }, k, l));
            }
        }
    }
    json!({ "n": n, "path": d.path, "line": d.line, "text": text })
}

/// M-e: ビルドして (cargo か make)、エラーを選んで $EDITOR +行 ファイル
fn key(pwd: &str, cur: &mut Option<Build>) -> Value {
    let dir = Path::new(pwd);
    let cmd = if dir.ancestors().any(|d| d.join("Cargo.toml").is_file()) {
        "cargo build"
    } else if dir.join("Makefile").is_file() || dir.join("makefile").is_file() {
        "make"
    } else {
        return json!({});
    };
    let mut tty = Tty::open().ok();
    if let Some(t) = tty.as_mut() {
        t.write(&format!("\x1b[2mfix: {} ...\x1b[0m", cmd));
    }
    if let Some(b) = cur.as_mut() {
        b.kill();
    }
    let b = match Build::start(cmd, dir) {
        Ok(b) => cur.insert(b),
        Err(_) => return json!({}),
    };
    // 終わるまで待つ。Esc / C-c / C-g で止める
    while b.child.is_some() {
        b.wait(Duration::from_millis(100));
        if let Some(t) = tty.as_mut()
            && b.child.is_some()
            && t.pending()
            && matches!(t.key(), Some(Key::Esc | Key::Byte(0x03 | 0x07)))
        {
            b.kill();
        }
    }
    if let Some(t) = tty.as_mut() {
        t.write("\r\x1b[K");
    }
    let items: Vec<String> = b.shown(true).iter().filter(|d| d.line > 0).map(|d| format!("{}:{}  {}", d.path, d.line, d.head)).collect();
    if items.is_empty() {
        return json!({});
    }
    let Some(picked) = pick("fix", &items) else { return json!({}) };
    let Some((path, line)) = picked.split_once("  ").and_then(|(loc, _)| loc.rsplit_once(':')) else { return json!({}) };
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let l = format!("{} +{} {}", editor, line, aish_plugin::escape(path));
    json!({ "line": l, "pos": l.chars().count() })
}

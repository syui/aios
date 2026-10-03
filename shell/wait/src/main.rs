// aish-wait: 何かが起きるまで待つ (aish の基本のプラグイン。端末なしの tools だけ)
//   wait  プロセスが終わる / ファイルに文字が出る / ポートが開く / ファイルができる、まで
// sleep をくりかえすループや、ログの文字を当てずっぽうに探すかわり (aish --mcp で Claude が使う)
use aish_plugin::{Spec, Tool, Value, error, json, s};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const WAIT: &str = r#"{"type":"object","properties":{
"pid":{"type":"integer","description":"このプロセスが終わるまで"},
"path":{"type":"string","description":"このファイルに text が出るまで (text がなければ、ファイルができるまで)"},
"text":{"type":"string","description":"path に出るのを待つ文字 (行の中にこの文字があればよい。正規表現ではない)"},
"new":{"type":"boolean","description":"呼んだあとに足された行だけを見る (既定 false: はじめから)"},
"port":{"type":"integer","description":"127.0.0.1 のこの TCP のポートにつながるまで"},
"timeout_ms":{"type":"integer","description":"これを過ぎたらあきらめる (既定 600000)"}}}"#;

fn main() {
    let spec = Spec {
        name: "wait",
        hooks: &[],
        keys: &[],
        tools: &[Tool {
            name: "wait",
            desc: "pid が終わる / path に text が出る (text がなければ path ができる) / port が開く、まで待つ。どれか 1 つ。{ok, ms, line?}。時間切れならしくじる",
            input: WAIT,
        }],
    };
    aish_plugin::run(spec, |ev, v| match ev {
        "tool" if s(v, "name") == "wait" => wait(v),
        "tool" => error(format!("{}: no such tool", s(v, "name"))),
        _ => json!({}),
    });
}

/// 待つもの
enum What {
    Pid(i32),
    Text { path: PathBuf, text: String, from: u64 },
    Exists(PathBuf),
    Port(u16),
}

fn wait(v: &Value) -> Value {
    let a = &v["args"];
    let timeout = Duration::from_millis(a["timeout_ms"].as_u64().unwrap_or(600_000));
    let what = if let Some(pid) = a["pid"].as_i64() {
        What::Pid(pid as i32)
    } else if let Some(port) = a["port"].as_u64() {
        What::Port(port as u16)
    } else if !s(a, "path").is_empty() {
        let p = Path::new(s(a, "path"));
        let path = if p.is_absolute() { p.to_path_buf() } else { Path::new(s(v, "pwd")).join(p) };
        match a["text"].as_str() {
            Some(t) if !t.is_empty() => {
                let from = if a["new"].as_bool().unwrap_or(false) { std::fs::metadata(&path).map_or(0, |m| m.len()) } else { 0 };
                What::Text { path, text: t.to_string(), from }
            }
            _ => What::Exists(path),
        }
    } else {
        return error("give one of pid, path, port");
    };
    let t0 = Instant::now();
    loop {
        if let Some(r) = check(&what) {
            let mut r = r;
            r["ok"] = json!(true);
            r["ms"] = json!(t0.elapsed().as_millis() as u64);
            return r;
        }
        if t0.elapsed() >= timeout {
            return json!({ "error": "timeout", "ms": t0.elapsed().as_millis() as u64 });
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// 起きていれば答え
fn check(w: &What) -> Option<Value> {
    match w {
        What::Pid(pid) => {
            // /proc/PID がない、またはゾンビ (Z) なら終わった
            match std::fs::read_to_string(format!("/proc/{}/stat", pid)) {
                Err(_) => Some(json!({})),
                Ok(st) => st.rfind(')').and_then(|i| st[i + 1..].split_whitespace().next()).filter(|s| *s == "Z").map(|_| json!({})),
            }
        }
        What::Exists(p) => p.exists().then(|| json!({})),
        What::Port(port) => std::net::TcpStream::connect_timeout(&([127, 0, 0, 1], *port).into(), Duration::from_millis(200)).ok().map(|_| json!({})),
        What::Text { path, text, from } => {
            let mut f = std::fs::File::open(path).ok()?;
            // ファイルが短くなっていたら (作りなおされた) はじめから
            let from = if f.metadata().ok()?.len() < *from { 0 } else { *from };
            f.seek(SeekFrom::Start(from)).ok()?;
            let mut b = Vec::new();
            f.read_to_end(&mut b).ok()?;
            let s = String::from_utf8_lossy(&b);
            s.lines().find(|l| l.contains(text.as_str())).map(|l| json!({ "line": l }))
        }
    }
}

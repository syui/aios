// aish-pick: キーに結びつける小さな機能 (aish の基本のプラグイン)
//   C-r      history  履歴を絞りこんで選び、行に入れる
//   C-f      file     いまのディレクトリから 3 段までのファイルを絞りこんで、カーソルのところに入れる
//   C-j      dir      最近のディレクトリ (~/.aish_dirs、chpwd で覚える) を絞りこんで cd
//   C-k      cdup     行が空なら cd ..、そうでなければカーソルから後ろを消す
//   C-p C-p  copy     打ちかけの行を端末のクリップボードへ (OSC 52)
use aish_plugin::{Spec, Tty, Value, escape, json, pick, quote, s, tilde};

fn main() {
    let spec = Spec {
        name: "pick",
        hooks: &["key", "chpwd"],
        keys: &[("C-r", "history"), ("C-f", "file"), ("C-j", "dir"), ("C-k", "cdup"), ("C-p C-p", "copy")],
    };
    let mut dirs_file = String::new();
    aish_plugin::run(spec, |ev, v| match ev {
        "hello" => {
            let home = s(v, "home");
            if !home.is_empty() {
                dirs_file = format!("{}/.aish_dirs", home);
            }
            json!({})
        }
        "chpwd" => {
            remember(&dirs_file, s(v, "pwd"));
            json!({})
        }
        "key" => key(v, &dirs_file),
        _ => json!({}),
    });
}

fn key(v: &Value, dirs_file: &str) -> Value {
    let line = s(v, "line");
    let pos = v["pos"].as_u64().unwrap_or(0) as usize;
    let home = s(v, "home");
    match s(v, "widget") {
        "history" => {
            let hist = std::fs::read_to_string(s(v, "histfile")).unwrap_or_default();
            let mut seen = std::collections::HashSet::new();
            let items: Vec<String> = hist.lines().rev().filter(|h| !h.is_empty() && seen.insert(*h)).map(String::from).collect();
            match pick("hist", &items) {
                Some(h) => json!({ "line": h, "pos": h.chars().count() }),
                None => json!({}),
            }
        }
        "file" => {
            let items = list_files(std::path::Path::new(s(v, "pwd")), 3, 5000);
            match pick("file", &items) {
                Some(f) => json!({ "insert": escape(&f) }),
                None => json!({}),
            }
        }
        "dir" => {
            // 短いものが先
            let mut items: Vec<String> = std::fs::read_to_string(dirs_file).unwrap_or_default().lines().filter(|l| !l.is_empty()).map(|d| tilde(d, home)).collect();
            items.sort_by_key(|d| d.chars().count());
            match pick("cd", &items) {
                Some(d) => {
                    let d = match d.strip_prefix('~') {
                        Some(rest) => format!("{}{}", home, rest),
                        None => d,
                    };
                    json!({ "run": format!("cd {}", quote(&d)), "silent": true })
                }
                None => json!({}),
            }
        }
        "cdup" => {
            if line.is_empty() {
                json!({ "run": "cd ..", "silent": true })
            } else {
                let kept: String = line.chars().take(pos).collect();
                json!({ "line": kept, "pos": pos })
            }
        }
        "copy" => {
            if let Ok(mut t) = Tty::open() {
                t.copy(line);
            }
            json!({})
        }
        _ => json!({}),
    }
}

/// 最近のディレクトリに足す (新しいものが先、50 まで)
fn remember(file: &str, d: &str) {
    if file.is_empty() || d.is_empty() {
        return;
    }
    let mut dirs: Vec<String> = std::fs::read_to_string(file).unwrap_or_default().lines().filter(|l| !l.is_empty() && *l != d).map(String::from).collect();
    dirs.insert(0, d.to_string());
    dirs.truncate(50);
    let _ = std::fs::write(file, dirs.join("\n") + "\n");
}

/// dir から depth 段までのファイル (.git はのぞく)。dir からの相対パス
fn list_files(dir: &std::path::Path, depth: usize, max: usize) -> Vec<String> {
    fn walk(dir: &std::path::Path, rel: &str, depth: usize, max: usize, out: &mut Vec<String>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        let mut es: Vec<_> = rd.flatten().collect();
        es.sort_by_key(|e| e.file_name());
        for e in es {
            if out.len() >= max {
                return;
            }
            let n = e.file_name().to_string_lossy().into_owned();
            if n == ".git" {
                continue;
            }
            let r = if rel.is_empty() { n.clone() } else { format!("{}/{}", rel, n) };
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if depth > 1 {
                    walk(&e.path(), &r, depth - 1, max, out);
                }
            } else {
                out.push(r);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, "", depth, max, &mut out);
    out
}

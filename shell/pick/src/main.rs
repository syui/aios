// aish-pick: キーに結びつける小さな機能 (aish の基本のプラグイン)
//   C-r      history  履歴を絞りこんで選び、行に入れる
//   C-f      file     いまのディレクトリの下のファイルを絞りこんで、カーソルのところに入れる
//                     (rg があれば rg --files: .gitignore をのぞく。なければ 3 段まで)
//   C-g      grep     打ちながら rg で探し、選んだら行を「$EDITOR +行 ファイル」にする (rg が要る)
//   C-o      recent   よく使うパス (使った回数 × 新しさの順) を絞りこんで、カーソルのところに入れる
//   C-j      dir      最近のディレクトリ (~/.aish_dirs、chpwd で覚える) を絞りこんで cd
//   C-k      cdup     行が空なら cd ..、そうでなければカーソルから後ろを消す
//   C-p C-p  copy     打ちかけの行を端末のクリップボードへ (OSC 52)
// 端末なしの顔 (tools。aish --mcp で Claude が使う):
//   history  履歴を新しい順に (query で絞りこむ)
//   dirs     最近のディレクトリ (query で絞りこむ)
//   paths    よく使うファイルとディレクトリを順位の順に (paths.rs)
mod paths;

use aish_plugin::{Spec, Tool, Tty, Value, escape, json, pick, pick_live, quote, s, tilde};

const QUERY: &str = r#"{"type":"object","properties":{"query":{"type":"string","description":"これを含むものだけ (大文字小文字は区別しない)"},"limit":{"type":"integer","description":"いくつまで (既定 50)"}}}"#;
const PATHS: &str = r#"{"type":"object","properties":{"query":{"type":"string","description":"空白で区切った語がみな、この順にパスに入っているもの (z と同じ。例: \"aios kernel\")"},"kind":{"type":"string","enum":["file","dir"],"description":"ファイルだけ / ディレクトリだけ (なければどちらも)"},"limit":{"type":"integer","description":"いくつまで (既定 20)"}}}"#;

struct State {
    home: String,
    dirs_file: String,
    db: Option<paths::Db>,
    /// preexec で受けた行と、そのときのディレクトリ (precmd で、動かしたあとにあるパスを覚える)
    pending: Option<(String, String)>,
}

fn main() {
    let spec = Spec {
        name: "pick",
        hooks: &["key", "chpwd", "preexec", "precmd"],
        keys: &[("C-r", "history"), ("C-f", "file"), ("C-g", "grep"), ("C-o", "recent"), ("C-j", "dir"), ("C-k", "cdup"), ("C-p C-p", "copy")],
        tools: &[
            Tool { name: "history", desc: "aish の履歴 (人が打ったコマンド) を新しい順に。{items: [...]}", input: QUERY },
            Tool { name: "dirs", desc: "最近 cd したディレクトリを新しい順に。{items: [...]}", input: QUERY },
            Tool {
                name: "paths",
                desc: "コマンドで使ったファイルとディレクトリを、使った回数 × 新しさの順に。{items: [{path, kind, score, time}]}",
                input: PATHS,
            },
        ],
    };
    let mut st = State { home: String::new(), dirs_file: String::new(), db: None, pending: None };
    aish_plugin::run(spec, |ev, v| match ev {
        "hello" => {
            st.home = s(v, "home").to_string();
            if !st.home.is_empty() {
                st.dirs_file = format!("{}/.aish_dirs", st.home);
            }
            st.db = Some(paths::Db::open(&st.home, s(v, "histfile")));
            json!({})
        }
        "chpwd" => {
            remember(&st.dirs_file, s(v, "pwd"));
            if let Some(db) = &mut st.db {
                db.learn(&paths_quote(s(v, "pwd")), "/", &st.home);
            }
            json!({})
        }
        "preexec" => {
            st.pending = Some((s(v, "line").to_string(), s(v, "pwd").to_string()));
            json!({})
        }
        "precmd" => {
            if let (Some((line, pwd)), Some(db)) = (st.pending.take(), &mut st.db) {
                db.learn(&line, &pwd, &st.home);
            }
            json!({})
        }
        "key" => key(v, &mut st),
        "tool" => tool(v, &mut st),
        _ => json!({}),
    });
}

/// cd したディレクトリを、行の語として渡すために (コマンドの名前と思われないよう前に : を置く)
fn paths_quote(d: &str) -> String {
    format!(": {}", d)
}

/// 端末なしの顔: 一覧を JSON で
fn tool(v: &Value, st: &mut State) -> Value {
    let dirs_file = st.dirs_file.as_str();
    let a = &v["args"];
    if s(v, "name") == "paths" {
        let terms: Vec<String> = s(a, "query").split_whitespace().map(String::from).collect();
        let limit = a["limit"].as_u64().unwrap_or(20) as usize;
        let items = st.db.as_mut().map(|db| db.rank(&terms, s(a, "kind"), limit)).unwrap_or_default();
        return json!({ "items": items });
    }
    let q = s(a, "query").to_lowercase();
    let limit = a["limit"].as_u64().unwrap_or(50) as usize;
    let all: Vec<String> = match s(v, "name") {
        "history" => {
            let hist = std::fs::read_to_string(s(v, "histfile")).unwrap_or_default();
            let mut seen = std::collections::HashSet::new();
            hist.lines().rev().filter(|h| !h.is_empty() && seen.insert(*h)).map(String::from).collect()
        }
        "dirs" => std::fs::read_to_string(dirs_file).unwrap_or_default().lines().filter(|l| !l.is_empty()).map(String::from).collect(),
        n => return aish_plugin::error(format!("{}: no such tool", n)),
    };
    let items: Vec<String> = all.into_iter().filter(|x| x.to_lowercase().contains(&q)).take(limit).collect();
    json!({ "items": items })
}

fn key(v: &Value, st: &mut State) -> Value {
    let dirs_file = st.dirs_file.as_str();
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
            let items = aish_plugin::rg_files(s(v, "pwd"), 20000).unwrap_or_else(|| list_files(std::path::Path::new(s(v, "pwd")), 3, 5000));
            match pick("file", &items) {
                Some(f) => json!({ "insert": escape(&f) }),
                None => json!({}),
            }
        }
        "grep" => {
            let pwd = s(v, "pwd");
            let picked = pick_live("rg", |q| {
                if q.chars().count() < 2 {
                    return vec![];
                }
                // 大文字があれば大文字小文字を区別する (smart case)
                let args = ["-S".to_string(), "--max-columns".into(), "200".into(), "-e".into(), q.to_string()];
                match aish_plugin::rg_json(pwd, &args, 300) {
                    None => vec!["(rg がありません: sudo ap -S ripgrep)".into()],
                    Some(r) => r
                        .items
                        .iter()
                        .map(|it| {
                            let d = &it["data"];
                            let path = aish_plugin::rg_text(&d["path"]);
                            format!("{}:{}: {}", path.strip_prefix("./").unwrap_or(&path), d["line_number"], aish_plugin::rg_text(&d["lines"]).trim())
                        })
                        .collect(),
                }
            });
            // "path:line: text" → $EDITOR +line path (Enter で開く)
            let Some((path, rest)) = picked.as_deref().and_then(|p| p.split_once(':')) else { return json!({}) };
            let Some(n) = rest.split(':').next().and_then(|n| n.parse::<u64>().ok()) else { return json!({}) };
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
            let l = format!("{} +{} {}", editor, n, escape(path));
            json!({ "line": l, "pos": l.chars().count() })
        }
        "recent" => {
            let items: Vec<String> = st.db.as_mut().map(|db| db.rank(&[], "", 500)).unwrap_or_default().iter().filter_map(|x| x["path"].as_str().map(|p| tilde(p, home))).collect();
            match pick("recent", &items) {
                Some(p) => json!({ "insert": escape(&p) }),
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

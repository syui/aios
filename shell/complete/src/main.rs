// aish-complete: Tab の補完 (aish の基本のプラグイン)
//   コマンドの位置ならコマンド (組み込み、alias、関数、PATH の動かせるもの)、$ で始まれば変数、
//   ほかはファイル (~ は HOME)。大文字小文字は区別しない。決まらないときに候補を並べて選ぶのは aish がやる
use aish_plugin::{Spec, Value, escape, json, s};

fn main() {
    let spec = Spec { name: "complete", hooks: &["complete"], keys: &[], tools: &[] };
    aish_plugin::run(spec, |ev, v| match ev {
        "complete" => complete(v),
        _ => json!({}),
    });
}

/// 語の区切り (空白、; | & < > ( )。\ で消したものはのぞく)
fn is_break(b: &[char], i: usize) -> bool {
    " \t;|&<>()".contains(b[i]) && !(i > 0 && b[i - 1] == '\\')
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
}

/// complete: { line, pos, cmds, vars, path, home, pwd } → { start, cands: [{ text, show, dir }] }
fn complete(v: &Value) -> Value {
    let buf: Vec<char> = s(v, "line").chars().collect();
    let pos = (v["pos"].as_u64().unwrap_or(buf.len() as u64) as usize).min(buf.len());
    let home = s(v, "home");
    let pwd = s(v, "pwd");
    let mut start = pos;
    while start > 0 && !is_break(&buf, start - 1) {
        start -= 1;
    }
    let word: String = buf[start..pos].iter().collect();
    // コマンドの位置か: 行の頭、; | & ( のあと、sudo などのあと
    let before: String = buf[..start].iter().collect();
    let prev = before.trim_end();
    let last_word = prev.rsplit(|c: char| " \t;|&(".contains(c)).next().unwrap_or("");
    let cmd_pos = prev.is_empty() || prev.ends_with([';', '|', '&', '(']) || ["sudo", "exec", "command", "time", "nohup", "which", "type", "doas"].contains(&last_word);
    let low = word.to_lowercase();
    let out: Vec<Value>;
    if let Some(var) = word.strip_prefix('$') {
        let lv = var.to_lowercase();
        let mut names: Vec<String> = strs(&v["vars"]).into_iter().filter(|n| n.to_lowercase().starts_with(&lv)).collect();
        names.sort();
        names.dedup();
        out = names.into_iter().map(|n| json!({ "text": format!("${}", n), "show": n, "dir": false })).collect();
    } else if cmd_pos && !word.contains('/') && !word.starts_with('~') && !word.starts_with('.') {
        let mut names: Vec<String> = strs(&v["cmds"]).into_iter().filter(|n| n.to_lowercase().starts_with(&low)).collect();
        for d in s(v, "path").split(':').filter(|d| !d.is_empty()) {
            let Ok(rd) = std::fs::read_dir(d) else { continue };
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if n.to_lowercase().starts_with(&low) && is_exec(&e.path()) {
                    names.push(n);
                }
            }
        }
        names.sort();
        names.dedup();
        out = names.into_iter().map(|n| json!({ "text": escape(&n), "show": n, "dir": false })).collect();
    } else {
        // ファイル: ~ は HOME、相対はシェルのいまのディレクトリから
        let raw = unescape(&word);
        let (dir, base) = match raw.rfind('/') {
            Some(i) => (raw[..=i].to_string(), raw[i + 1..].to_string()),
            None if raw == "~" => ("~/".to_string(), String::new()),
            None => (String::new(), raw.clone()),
        };
        let real = if let Some(r) = dir.strip_prefix('~') { format!("{}{}", home, r) } else { dir.clone() };
        let real = if real.starts_with('/') { real } else if real.is_empty() { pwd.to_string() } else { format!("{}/{}", pwd, real) };
        let lb = base.to_lowercase();
        let mut files = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&real) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if !n.to_lowercase().starts_with(&lb) || (n.starts_with('.') && !base.starts_with('.')) {
                    continue;
                }
                let is_dir = e.path().is_dir();
                // コマンドの位置なら、ディレクトリと動かせるものだけ
                if cmd_pos && !is_dir && !is_exec(&e.path()) {
                    continue;
                }
                let suffix = if is_dir { "/" } else { "" };
                files.push((format!("{}{}", n, suffix), format!("{}{}{}", escape(&dir), escape(&n), suffix), is_dir));
            }
        }
        files.sort();
        out = files.into_iter().map(|(show, text, dir)| json!({ "text": text, "show": show, "dir": dir })).collect();
    }
    json!({ "start": start, "cands": out })
}

fn is_exec(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut cs = s.chars();
    while let Some(c) = cs.next() {
        if c == '\\' {
            if let Some(d) = cs.next() {
                out.push(d);
            }
        } else {
            out.push(c);
        }
    }
    out
}

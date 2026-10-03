// aish-edit: ファイルを確かに読み書きする (aish の基本のプラグイン。端末なしの tools だけ)
//   read   行の番号つきで読む
//   edit   old をぴったり new に置きかえる (見つからない、いくつもある、ならしくじる)
//   write  まるごと書く (なければ作る)
//   grep   ファイルかディレクトリの下から、行の番号つきで探す (文字か正規表現)
//   sed    文字か正規表現で置きかえる (行の範囲をしぼれる。正規表現なら $1 で取りだしたもの)
//   lines  行の番号で置きかえる / 入れる / 消す (old でいまの中身を確かめられる)
//   undo   edit / write / sed / lines のまえに戻す
// aish --mcp で Claude が使う。取り消すための写しはこのプログラムのメモリーにだけ持つ
// (ディスクに書かないので、リポジトリやイメージに入ることはない。aish が終わると消える)
use aish_plugin::{Spec, Tool, Value, error, json, s};
use std::path::{Path, PathBuf};

const READ: &str = r#"{"type":"object","properties":{"path":{"type":"string"},"offset":{"type":"integer","description":"何行目から (1 から。既定 1)"},"limit":{"type":"integer","description":"何行 (既定 2000)"}},"required":["path"]}"#;
const EDIT: &str = r#"{"type":"object","properties":{"path":{"type":"string"},"old":{"type":"string","description":"置きかえるもの (ファイルにぴったり 1 つあること)"},"new":{"type":"string"},"all":{"type":"boolean","description":"いくつもあれば全部 (既定 false)"}},"required":["path","old","new"]}"#;
const WRITE: &str = r#"{"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}"#;
const GREP: &str = r#"{"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string","description":"ファイルかディレクトリ (既定 .。ディレクトリなら下をぜんぶ。. で始まるもの、target、node_modules、バイナリはのぞく)"},"regex":{"type":"boolean","description":"pattern を正規表現として (既定 false: そのままの文字)"},"i":{"type":"boolean","description":"大文字小文字を区別しない"},"context":{"type":"integer","description":"前後の行もいくつ (ctx: true で入る)"},"limit":{"type":"integer","description":"見つけるのはいくつまで (既定 200)"}},"required":["pattern"]}"#;
const SED: &str = r#"{"type":"object","properties":{"path":{"type":"string"},"pattern":{"type":"string"},"replace":{"type":"string","description":"正規表現なら $1 ${name} が使える"},"regex":{"type":"boolean","description":"既定 false: そのままの文字"},"i":{"type":"boolean"},"lines":{"type":"string","description":"行の範囲: \"12\" \"10-20\" \"10-\" (なければ全部)"},"count":{"type":"integer","description":"置きかえる数がこれでなければ、何もせずにしくじる (思ったところだけ変えるために)"}},"required":["path","pattern","replace"]}"#;
const LINES: &str = r#"{"type":"object","properties":{"path":{"type":"string"},"from":{"type":"integer","description":"何行目から (1 から)"},"to":{"type":"integer","description":"何行目まで (既定 from)"},"text":{"type":"string","description":"かわりに入れる行 (なければ消す)"},"insert":{"type":"boolean","description":"消さずに from の前に入れる (from が 行の数 + 1 なら終わりに足す)"},"old":{"type":"string","description":"from から to までのいまの中身 (改行でつなぐ)。違えば何もせずにしくじる"}},"required":["path","from"]}"#;
const UNDO: &str = r#"{"type":"object","properties":{"path":{"type":"string","description":"このファイルの最後の変更を戻す (なければ、いちばん新しい変更)"}}}"#;

/// 取り消すための写しの数
const KEEP: usize = 100;

/// 変える前のファイル (なかったなら None)
struct Snap {
    path: PathBuf,
    before: Option<Vec<u8>>,
    /// write が作ったディレクトリ (深いものが先。undo で空なら消す)
    dirs: Vec<PathBuf>,
}

fn main() {
    let spec = Spec {
        name: "edit",
        hooks: &[],
        keys: &[],
        tools: &[
            Tool { name: "read", desc: "ファイルを行の番号つきで読む。{path, lines (全部の行数), text}", input: READ },
            Tool { name: "edit", desc: "ファイルの old をぴったり new に置きかえる。old が見つからないか、いくつもある (all でない) ならしくじる。undo で戻せる", input: EDIT },
            Tool { name: "write", desc: "ファイルをまるごと書く (なければディレクトリごと作る)。undo で戻せる", input: WRITE },
            Tool { name: "grep", desc: "ファイルかディレクトリの下から行の番号つきで探す。{matches: [{path, line, text, ctx?}], count, truncated}", input: GREP },
            Tool { name: "sed", desc: "文字か正規表現で置きかえる (lines で行の範囲、count で数を確かめる)。{path, replaced, lines}。undo で戻せる", input: SED },
            Tool { name: "lines", desc: "行の番号で置きかえる / 入れる (insert) / 消す (text なし)。old でいまの中身を確かめる。undo で戻せる", input: LINES },
            Tool { name: "undo", desc: "edit / write / sed / lines のまえに戻す (aish が動いているあいだの 100 回まで)", input: UNDO },
        ],
    };
    let mut snaps: Vec<Snap> = Vec::new();
    aish_plugin::run(spec, |ev, v| match ev {
        "tool" => {
            let a = &v["args"];
            let path = resolve(s(v, "pwd"), s(a, "path"));
            match s(v, "name") {
                "read" => read(&path, a),
                "edit" => edit(&path, a, &mut snaps),
                "write" => write(&path, s(a, "content").as_bytes(), &mut snaps),
                "grep" => grep(s(v, "pwd"), a),
                "sed" => sed(&path, a, &mut snaps),
                "lines" => lines(&path, a, &mut snaps),
                "undo" => undo(&path, s(a, "path").is_empty(), &mut snaps),
                n => error(format!("{}: no such tool", n)),
            }
        }
        _ => json!({}),
    });
}

/// 相対パスはシェルのいまのディレクトリから
fn resolve(pwd: &str, p: &str) -> PathBuf {
    let p = Path::new(p);
    if p.is_absolute() { p.to_path_buf() } else { Path::new(pwd).join(p) }
}

/// pattern を正規表現にする (regex でなければそのままの文字として)
fn matcher(a: &Value) -> Result<regex::Regex, Value> {
    let pat = s(a, "pattern");
    if pat.is_empty() {
        return Err(error("pattern is empty"));
    }
    let pat = if a["regex"].as_bool().unwrap_or(false) { pat.to_string() } else { regex::escape(pat) };
    regex::RegexBuilder::new(&pat).case_insensitive(a["i"].as_bool().unwrap_or(false)).build().map_err(|e| error(e))
}

fn grep(pwd: &str, a: &Value) -> Value {
    let re = match matcher(a) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let root = resolve(pwd, if s(a, "path").is_empty() { "." } else { s(a, "path") });
    let ctx = a["context"].as_u64().unwrap_or(0) as usize;
    let limit = a["limit"].as_u64().unwrap_or(200) as usize;
    let mut files = Vec::new();
    walk(&root, &mut files);
    files.sort();
    let mut out = Vec::new();
    let mut count = 0;
    let mut truncated = false;
    'files: for f in &files {
        let Ok(b) = std::fs::read(f) else { continue };
        if b.len() > 8 << 20 || b[..b.len().min(8192)].contains(&0) {
            continue;
        }
        let text = String::from_utf8_lossy(&b);
        let ls: Vec<&str> = text.lines().collect();
        // 見せるのは pwd からの相対のパス (下にあれば)
        let shown = f.strip_prefix(pwd).ok().filter(|_| !pwd.is_empty()).map_or(f.display().to_string(), |r| r.display().to_string());
        let mut last = 0usize; // 出した最後の行 (前後の行を重ねて出さない)
        for (i, l) in ls.iter().enumerate() {
            if !re.is_match(l) {
                continue;
            }
            if count >= limit {
                truncated = true;
                break 'files;
            }
            count += 1;
            for j in i.saturating_sub(ctx).max(last)..i {
                out.push(json!({ "path": shown, "line": j + 1, "text": ls[j], "ctx": true }));
            }
            out.push(json!({ "path": shown, "line": i + 1, "text": l }));
            let end = (i + 1 + ctx).min(ls.len());
            for j in i + 1..end {
                if re.is_match(ls[j]) {
                    break;
                }
                out.push(json!({ "path": shown, "line": j + 1, "text": ls[j], "ctx": true }));
            }
            last = end.max(i + 1);
        }
    }
    json!({ "matches": out, "count": count, "truncated": truncated })
}

/// ディレクトリの下のファイル (. で始まるもの、target、node_modules はのぞく)
fn walk(p: &Path, out: &mut Vec<PathBuf>) {
    let Ok(m) = std::fs::symlink_metadata(p) else { return };
    if m.is_file() {
        out.push(p.to_path_buf());
        return;
    }
    if !m.is_dir() {
        return;
    }
    let Ok(rd) = std::fs::read_dir(p) else { return };
    for e in rd.flatten() {
        let n = e.file_name();
        let n = n.to_string_lossy();
        if n.starts_with('.') || n == "target" || n == "node_modules" {
            continue;
        }
        walk(&e.path(), out);
    }
}

/// "12" "10-20" "10-" → (はじめ, おわり) (1 から。おわりは含む)
fn range(r: &str) -> Option<(usize, usize)> {
    if r.is_empty() {
        return Some((1, usize::MAX));
    }
    let (a, b) = r.split_once('-').unwrap_or((r, r));
    let a: usize = a.trim().parse().ok()?;
    let b: usize = if b.trim().is_empty() { usize::MAX } else { b.trim().parse().ok()? };
    (a >= 1 && b >= a).then_some((a, b))
}

fn sed(path: &Path, a: &Value, snaps: &mut Vec<Snap>) -> Value {
    let re = match matcher(a) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let Some((from, to)) = range(s(a, "lines")) else { return error(format!("{}: bad lines (give 12, 10-20 or 10-)", s(a, "lines"))) };
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => return error(format!("{}: {}", path.display(), e)),
    };
    let rep = s(a, "replace");
    let lit = !a["regex"].as_bool().unwrap_or(false);
    let mut out = String::new();
    let mut n = 0;
    let mut changed = Vec::new();
    for (i, l) in text.split_inclusive('\n').enumerate() {
        let k = re.find_iter(l).count();
        if (from..=to).contains(&(i + 1)) && k > 0 {
            n += k;
            changed.push(i + 1);
            if lit {
                out.push_str(&re.replace_all(l, regex::NoExpand(rep)));
            } else {
                out.push_str(&re.replace_all(l, rep));
            }
        } else {
            out.push_str(l);
        }
    }
    if n == 0 {
        return error(format!("{}: pattern not found", path.display()));
    }
    if let Some(c) = a["count"].as_u64()
        && c as usize != n
    {
        return error(format!("{}: would replace {} (count says {}). lines: {:?}", path.display(), n, c, changed));
    }
    match write(path, out.as_bytes(), snaps) {
        r if r.get("error").is_some() => r,
        _ => json!({ "path": path.display().to_string(), "replaced": n, "lines": changed }),
    }
}

fn lines(path: &Path, a: &Value, snaps: &mut Vec<Snap>) -> Value {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => return error(format!("{}: {}", path.display(), e)),
    };
    let mut ls: Vec<String> = text.split_inclusive('\n').map(String::from).collect();
    let from = a["from"].as_u64().unwrap_or(0) as usize;
    if from < 1 {
        return error("from starts at 1");
    }
    let insert = a["insert"].as_bool().unwrap_or(false);
    let to = if insert { from - 1 } else { a["to"].as_u64().map_or(from, |t| t as usize) };
    let max = if insert { ls.len() + 1 } else { ls.len() };
    if from < 1 || from > max || (!insert && to < from) || to > ls.len() {
        return error(format!("{}: lines {}-{} out of 1-{}", path.display(), from, to, ls.len()));
    }
    if let Some(old) = a["old"].as_str() {
        let now: String = ls[from - 1..to].concat();
        if now.trim_end_matches('\n') != old.trim_end_matches('\n') {
            return error(format!("{}: lines {}-{} are now:\n{}", path.display(), from, to, now));
        }
    }
    // 終わりの行に改行がなければ、足す行の前に入れる
    if insert && from == ls.len() + 1 && ls.last().is_some_and(|l| !l.ends_with('\n')) {
        ls.last_mut().unwrap().push('\n');
    }
    let mut new: Vec<String> = Vec::new();
    if let Some(t) = a["text"].as_str().filter(|t| !t.is_empty()) {
        new = t.split_inclusive('\n').map(String::from).collect();
        if let Some(l) = new.last_mut()
            && !l.ends_with('\n')
        {
            l.push('\n');
        }
    }
    let (removed, inserted) = (to + 1 - from, new.len());
    ls.splice(from - 1..to, new);
    let out = ls.concat();
    match write(path, out.as_bytes(), snaps) {
        r if r.get("error").is_some() => r,
        _ => json!({ "path": path.display().to_string(), "removed": removed, "inserted": inserted, "lines": ls.len() }),
    }
}

fn read(path: &Path, a: &Value) -> Value {
    let text = match std::fs::read(path) {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(e) => return error(format!("{}: {}", path.display(), e)),
    };
    let from = a["offset"].as_u64().unwrap_or(1).max(1) as usize;
    let limit = a["limit"].as_u64().unwrap_or(2000) as usize;
    let lines: Vec<&str> = text.lines().collect();
    let mut out = String::new();
    for (i, l) in lines.iter().enumerate().skip(from - 1).take(limit) {
        out.push_str(&format!("{:6}\t{}\n", i + 1, l));
    }
    json!({ "path": path.display().to_string(), "lines": lines.len(), "text": out })
}

fn edit(path: &Path, a: &Value, snaps: &mut Vec<Snap>) -> Value {
    let (old, new) = (s(a, "old"), s(a, "new"));
    if old.is_empty() {
        return error("old is empty");
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => return error(format!("{}: {}", path.display(), e)),
    };
    let n = text.matches(old).count();
    let all = a["all"].as_bool().unwrap_or(false);
    if n == 0 {
        return error(format!("{}: old not found", path.display()));
    }
    if n > 1 && !all {
        return error(format!("{}: old found {} times (make it longer, or all: true)", path.display(), n));
    }
    let out = if all { text.replace(old, new) } else { text.replacen(old, new, 1) };
    match write(path, out.as_bytes(), snaps) {
        r if r.get("error").is_some() => r,
        _ => json!({ "path": path.display().to_string(), "replaced": if all { n } else { 1 } }),
    }
}

fn write(path: &Path, content: &[u8], snaps: &mut Vec<Snap>) -> Value {
    let before = std::fs::read(path).ok();
    // なかったディレクトリ (深いものが先)
    let dirs: Vec<PathBuf> = path.ancestors().skip(1).take_while(|d| !d.as_os_str().is_empty() && !d.exists()).map(Path::to_path_buf).collect();
    if let Some(d) = path.parent()
        && let Err(e) = std::fs::create_dir_all(d)
    {
        return error(format!("{}: {}", d.display(), e));
    }
    if let Err(e) = std::fs::write(path, content) {
        return error(format!("{}: {}", path.display(), e));
    }
    let created = before.is_none();
    snaps.push(Snap { path: path.to_path_buf(), before, dirs });
    if snaps.len() > KEEP {
        snaps.remove(0);
    }
    json!({ "path": path.display().to_string(), "bytes": content.len(), "created": created })
}

fn undo(path: &Path, latest: bool, snaps: &mut Vec<Snap>) -> Value {
    let Some(i) = snaps.iter().rposition(|x| latest || x.path == path) else {
        return error("nothing to undo");
    };
    let snap = snaps.remove(i);
    let r = match &snap.before {
        Some(b) => std::fs::write(&snap.path, b),
        None => std::fs::remove_file(&snap.path),
    };
    if r.is_ok() {
        for d in &snap.dirs {
            let _ = std::fs::remove_dir(d);
        }
    }
    match r {
        Ok(()) => json!({ "path": snap.path.display().to_string(), "removed": snap.before.is_none() }),
        Err(e) => error(format!("{}: {}", snap.path.display(), e)),
    }
}

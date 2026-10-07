// aish-edit: ファイルを確かに読み書きする (aish の基本のプラグイン。端末なしの tools だけ)
//   read   行の番号つきで読む
//   edit   old をぴったり new に置きかえる (見つからない、いくつもある、ならしくじる)
//   write  まるごと書く (なければ作る)
//   grep   ファイルかディレクトリの下から、行の番号つきで探す (文字か正規表現)
//   sed    文字か正規表現で置きかえる (行の範囲をしぼれる。正規表現なら $1 で取りだしたもの)
//   lines  行の番号で置きかえる / 入れる / 消す (old でいまの中身を確かめられる)
//   hit    grep で見つけた n 番のまわりを読む (パスも行もいらない)
//   each   grep で見つけた行 (only で番号をしぼる) だけを置きかえる。grep のあと変わった行は飛ばす
//   undo   edit / write / sed / lines / each のまえに戻す (each は何ファイルでも 1 回で)
// grep の答えは、よく使うファイル (aish-pick の paths) のものが先で、見つけた行に番号 n がつく
// aish --mcp で Claude が使う。取り消すための写しはこのプログラムのメモリーにだけ持つ
// (ディスクに書かないので、リポジトリやイメージに入ることはない。aish が終わると消える)
use aish_plugin::{Spec, Tool, Value, error, json, s};
use std::path::{Path, PathBuf};

const READ: &str = r#"{"type":"object","properties":{"path":{"type":"string"},"offset":{"type":"integer","description":"何行目から (1 から。既定 1)"},"limit":{"type":"integer","description":"何行 (既定 2000)"}},"required":["path"]}"#;
const EDIT: &str = r#"{"type":"object","properties":{"path":{"type":"string"},"old":{"type":"string","description":"置きかえるもの (ファイルにぴったり 1 つあること)"},"new":{"type":"string"},"all":{"type":"boolean","description":"いくつもあれば全部 (既定 false)"},"edits":{"type":"array","items":{"type":"object","properties":{"old":{"type":"string"},"new":{"type":"string"},"all":{"type":"boolean"}},"required":["old","new"]},"description":"いくつも置きかえるとき (old new のかわりに)。順にあて、どれかがしくじれば何も書かない"}},"required":["path"]}"#;
const WRITE: &str = r#"{"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}"#;
const GREP: &str = r#"{"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string","description":"ファイルかディレクトリ (既定 .。ディレクトリなら下をぜんぶ。rg があれば .gitignore と隠しファイルとバイナリをのぞく。なければ . で始まるもの、target、node_modules、バイナリをのぞく)"},"glob":{"type":"array","items":{"type":"string"},"description":"rg の -g (例: [\"*.rs\", \"!target\"])。rg が要る"},"hidden":{"type":"boolean","description":"隠しファイルも (rg の --hidden)"},"regex":{"type":"boolean","description":"pattern を正規表現として (既定 false: そのままの文字)"},"i":{"type":"boolean","description":"大文字小文字を区別しない"},"context":{"type":"integer","description":"前後の行もいくつ (ctx: true で入る)"},"limit":{"type":"integer","description":"見つけるのはいくつまで (既定 200)"}},"required":["pattern"]}"#;
const SED: &str = r#"{"type":"object","properties":{"path":{"type":"string"},"pattern":{"type":"string"},"replace":{"type":"string","description":"正規表現なら $1 ${name} が使える"},"regex":{"type":"boolean","description":"既定 false: そのままの文字"},"i":{"type":"boolean"},"lines":{"type":"string","description":"行の範囲: \"12\" \"10-20\" \"10-\" (なければ全部)"},"count":{"type":"integer","description":"置きかえる数がこれでなければ、何もせずにしくじる (思ったところだけ変えるために)"},"subs":{"type":"array","items":{"type":"object","properties":{"pattern":{"type":"string"},"replace":{"type":"string"},"regex":{"type":"boolean"},"i":{"type":"boolean"}},"required":["pattern","replace"]},"description":"いくつも置きかえるとき (pattern replace のかわりに)。行ごとに順にあてる"}},"required":["path"]}"#;
const LINES: &str = r#"{"type":"object","properties":{"path":{"type":"string"},"from":{"type":"integer","description":"何行目から (1 から)"},"to":{"type":"integer","description":"何行目まで (既定 from)"},"text":{"type":"string","description":"かわりに入れる行 (なければ消す)"},"insert":{"type":"boolean","description":"消さずに from の前に入れる (from が 行の数 + 1 なら終わりに足す)"},"old":{"type":"string","description":"from から to までのいまの中身 (改行でつなぐ)。違えば何もせずにしくじる"}},"required":["path","from"]}"#;
const UNDO: &str = r#"{"type":"object","properties":{"path":{"type":"string","description":"このファイルの最後の変更を戻す (なければ、いちばん新しい変更)"}}}"#;

const HIT: &str = r#"{"type":"object","properties":{"n":{"type":"integer","description":"grep の答えの n"},"context":{"type":"integer","description":"前後の行 (既定 5)"}},"required":["n"]}"#;
const EACH: &str = r#"{"type":"object","properties":{"pattern":{"type":"string"},"replace":{"type":"string","description":"正規表現なら $1 が使える"},"regex":{"type":"boolean"},"i":{"type":"boolean"},"only":{"type":"array","items":{"type":"integer"},"description":"この番号 (grep の n) だけ (なければ全部)"},"subs":{"type":"array","items":{"type":"object","properties":{"pattern":{"type":"string"},"replace":{"type":"string"},"regex":{"type":"boolean"},"i":{"type":"boolean"}},"required":["pattern","replace"]},"description":"いくつも置きかえるとき (pattern replace のかわりに)。行ごとに順にあてる"}}}"#;

/// 取り消すための写しの数
const KEEP: usize = 100;

/// いまの呼び出しの組 (tool が来るたびに 1 つ進める)
static GROUP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// grep で見つけた行 (hit と each が使う)
struct Hit {
    path: PathBuf,
    /// grep の答えに出したパス (each の答えも同じ形で)
    shown: String,
    line: usize,
    /// 見つけたときの行 (each はこれと同じときだけ変える)
    text: String,
}

/// 変える前のファイル (なかったなら None)
struct Snap {
    /// 1 回の呼び出しで変えたもの (each は何ファイルでも同じ組。undo は組ごと戻す)
    group: u64,
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
            Tool { name: "grep", desc: "ファイルかディレクトリの下から行の番号つきで探す (ripgrep があればそれで)。{matches: [{path, line, text, ctx?}], count, truncated, engine}", input: GREP },
            Tool { name: "sed", desc: "文字か正規表現で置きかえる (lines で行の範囲、count で数を確かめる)。{path, replaced, lines}。undo で戻せる", input: SED },
            Tool { name: "lines", desc: "行の番号で置きかえる / 入れる (insert) / 消す (text なし)。old でいまの中身を確かめる。undo で戻せる", input: LINES },
            Tool { name: "hit", desc: "grep の n 番のまわりを行の番号つきで読む。{n, path, line, text}", input: HIT },
            Tool { name: "each", desc: "grep で見つけた行だけを置きかえる (only で番号をしぼる)。grep のあとで変わった行は飛ばす。undo 1 回で全部戻る。{changed: [{n, path, line, text}], skipped: [{n, why}]}", input: EACH },
            Tool { name: "undo", desc: "edit / write / sed / lines / each のまえに戻す (each は 1 回で全部。aish が動いているあいだの 100 回まで)", input: UNDO },
        ],
    };
    let mut snaps: Vec<Snap> = Vec::new();
    let mut hits: Vec<Hit> = Vec::new();
    let mut home = String::new();
    aish_plugin::run(spec, |ev, v| match ev {
        "hello" => {
            home = s(v, "home").to_string();
            json!({})
        }
        "tool" => {
            GROUP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let a = &v["args"];
            let path = resolve(s(v, "pwd"), s(a, "path"));
            match s(v, "name") {
                "read" => read(&path, a),
                "edit" => edit(&path, a, &mut snaps),
                "write" => write(&path, s(a, "content").as_bytes(), &mut snaps),
                "grep" => grep(s(v, "pwd"), a, &home, &mut hits),
                "hit" => hit(a, &hits),
                "each" => each(a, &mut hits, &mut snaps),
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
    regex::RegexBuilder::new(&pat).case_insensitive(a["i"].as_bool().unwrap_or(false)).build().map_err(error)
}

/// 置きかえ (sed と each): pattern → replace か、subs: [{pattern, replace, regex?, i?}, ...] を順に。
/// regex と i は、subs の中になければ外のものを使う
struct Subs(Vec<(regex::Regex, String, bool)>);

impl Subs {
    /// 順にあてた行と、置きかえた数
    fn apply(&self, l: &str) -> (String, usize) {
        let mut cur = l.to_string();
        let mut n = 0;
        for (re, rep, lit) in &self.0 {
            let k = re.find_iter(&cur).count();
            if k > 0 {
                n += k;
                cur = if *lit { re.replace_all(&cur, regex::NoExpand(rep)).into_owned() } else { re.replace_all(&cur, rep.as_str()).into_owned() };
            }
        }
        (cur, n)
    }
}

fn subs(a: &Value) -> Result<Subs, Value> {
    let list: Vec<Value> = match a["subs"].as_array() {
        Some(v) if !v.is_empty() => v.clone(),
        Some(_) => return Err(error("subs is empty")),
        None => vec![a.clone()],
    };
    let mut out = Vec::new();
    for (k, x) in list.iter().enumerate() {
        let mut x = x.clone();
        for f in ["regex", "i"] {
            if x.get(f).is_none() {
                x[f] = a[f].clone();
            }
        }
        let re = matcher(&x).map_err(|e| if list.len() > 1 { error(format!("{} (subs[{}])", s(&e, "error"), k)) } else { e })?;
        out.push((re, s(&x, "replace").to_string(), !x["regex"].as_bool().unwrap_or(false)));
    }
    Ok(Subs(out))
}

/// 長い行は切る (minify した JS など。答えが読めなくならないように)
const LINE_MAX: usize = 300;

fn clip_line(l: &str) -> String {
    let l = l.trim_end_matches(['\n', '\r']);
    if l.chars().count() <= LINE_MAX {
        return l.to_string();
    }
    let head: String = l.chars().take(LINE_MAX).collect();
    format!("{}…(+{} chars)", head, l.chars().count() - LINE_MAX)
}

fn grep(pwd: &str, a: &Value, home: &str, hits: &mut Vec<Hit>) -> Value {
    let mut r = grep_raw(pwd, a);
    if r.get("error").is_none() {
        let ms = r["matches"].as_array().cloned().unwrap_or_default();
        r["matches"] = json!(number(ms, pwd, home, hits));
    }
    r
}

/// 見つけたものを、よく使うファイルが先になるように並べ (ファイルの中の順はそのまま)、
/// 見つけた行に番号 n をつけて hits に覚える。長い行はここで切る
fn number(ms: Vec<Value>, pwd: &str, home: &str, hits: &mut Vec<Hit>) -> Vec<Value> {
    let scores = aish_plugin::path_scores(home);
    // ファイルごとのかたまり (出てきた順)
    let mut blocks: Vec<(String, Vec<Value>)> = Vec::new();
    for m in ms {
        let p = s(&m, "path").to_string();
        match blocks.last_mut() {
            Some((q, v)) if *q == p => v.push(m),
            _ => blocks.push((p, vec![m])),
        }
    }
    let score = |p: &str| scores.get(&resolve(pwd, p).display().to_string()).copied().unwrap_or(0.0);
    blocks.sort_by(|a, b| score(&b.0).total_cmp(&score(&a.0)));
    hits.clear();
    let mut out = Vec::new();
    for (_, v) in blocks {
        for mut m in v {
            let full = s(&m, "text").trim_end_matches(['\n', '\r']).to_string();
            m["text"] = json!(clip_line(&full));
            if m.get("ctx").is_none() {
                hits.push(Hit { path: resolve(pwd, s(&m, "path")), shown: s(&m, "path").to_string(), line: m["line"].as_u64().unwrap_or(0) as usize, text: full });
                m["n"] = json!(hits.len());
            }
            out.push(m);
        }
    }
    out
}

fn grep_raw(pwd: &str, a: &Value) -> Value {
    let re = match matcher(a) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let limit = a["limit"].as_u64().unwrap_or(200) as usize;
    if let Some(r) = grep_rg(pwd, a, limit) {
        return r;
    }
    if a.get("glob").is_some() {
        return error("glob needs rg (ripgrep: aipkg -S ripgrep)");
    }
    let root = resolve(pwd, if s(a, "path").is_empty() { "." } else { s(a, "path") });
    let ctx = a["context"].as_u64().unwrap_or(0) as usize;
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
            let from = i.saturating_sub(ctx).max(last);
            for (j, t) in ls.iter().enumerate().take(i).skip(from) {
                out.push(json!({ "path": shown, "line": j + 1, "text": t, "ctx": true }));
            }
            out.push(json!({ "path": shown, "line": i + 1, "text": l }));
            let end = (i + 1 + ctx).min(ls.len());
            for (j, t) in ls.iter().enumerate().take(end).skip(i + 1) {
                if re.is_match(t) {
                    break;
                }
                out.push(json!({ "path": shown, "line": j + 1, "text": t, "ctx": true }));
            }
            last = end.max(i + 1);
        }
    }
    json!({ "matches": out, "count": count, "truncated": truncated, "engine": "aish" })
}

/// rg (ripgrep) で探す。rg がなければ None (aish の中のもので探す)
fn grep_rg(pwd: &str, a: &Value, limit: usize) -> Option<Value> {
    // 番号が呼ぶたびに変わらないように、パスの順で (rg は 1 つのスレッドで探す)
    let mut args: Vec<String> = vec!["--sort".into(), "path".into()];
    if !a["regex"].as_bool().unwrap_or(false) {
        args.push("-F".into());
    }
    if a["i"].as_bool().unwrap_or(false) {
        args.push("-i".into());
    }
    if a["hidden"].as_bool().unwrap_or(false) {
        args.push("--hidden".into());
    }
    if let Some(c) = a["context"].as_u64().filter(|c| *c > 0) {
        args.push(format!("-C{}", c));
    }
    let globs: Vec<String> = match &a["glob"] {
        Value::String(g) => vec![g.clone()],
        Value::Array(v) => v.iter().filter_map(|g| g.as_str().map(String::from)).collect(),
        _ => vec![],
    };
    for g in globs {
        args.push("-g".into());
        args.push(g);
    }
    args.extend(["-e".into(), s(a, "pattern").to_string(), "--".into(), if s(a, "path").is_empty() { ".".into() } else { s(a, "path").to_string() }]);
    let r = aish_plugin::rg_json(pwd, &args, limit)?;
    if r.status == 2 && r.items.is_empty() {
        return Some(error(format!("rg: {}", r.err.trim())));
    }
    let mut out = Vec::new();
    let mut count = 0;
    for it in &r.items {
        let d = &it["data"];
        let path = aish_plugin::rg_text(&d["path"]);
        let path = path.strip_prefix("./").unwrap_or(&path).to_string();
        let mut m = json!({ "path": path, "line": d["line_number"], "text": aish_plugin::rg_text(&d["lines"]) });
        if it["type"] == "context" {
            m["ctx"] = json!(true);
        } else {
            count += 1;
        }
        out.push(m);
    }
    Some(json!({ "matches": out, "count": count, "truncated": r.truncated, "engine": "rg" }))
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
    let re = match subs(a) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let Some((from, to)) = range(s(a, "lines")) else { return error(format!("{}: bad lines (give 12, 10-20 or 10-)", s(a, "lines"))) };
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => return error(format!("{}: {}", path.display(), e)),
    };
    let mut out = String::new();
    let mut n = 0;
    let mut changed = Vec::new();
    for (i, l) in text.split_inclusive('\n').enumerate() {
        let (new, k) = if (from..=to).contains(&(i + 1)) { re.apply(l) } else { (l.to_string(), 0) };
        if k > 0 {
            n += k;
            changed.push(i + 1);
        }
        out.push_str(&new);
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
        _ => {
            // 変えた行とまわり (edit と同じ形)
            let starts: Vec<usize> = std::iter::once(0).chain(out.match_indices('\n').map(|(i, _)| i + 1)).collect();
            let spans: Vec<(usize, usize)> = changed.iter().filter_map(|&l| starts.get(l - 1).map(|&a| (a, out[a..].find('\n').unwrap_or(out.len() - a)))).collect();
            let (_, shown) = around(&out, &spans);
            json!({ "path": path.display().to_string(), "replaced": n, "lines": changed, "text": shown })
        }
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

/// edit: old → new。edits: [{old, new, all?}, ...] なら順に (前のものを変えたあとの中身に) あてて、
/// どれかがしくじれば何も書かない (ぜんぶか、なにもしないか)
fn edit(path: &Path, a: &Value, snaps: &mut Vec<Snap>) -> Value {
    let list: Vec<Value> = match a["edits"].as_array() {
        Some(v) => v.clone(),
        None => vec![a.clone()],
    };
    if list.is_empty() {
        return error("edits is empty");
    }
    let mut text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => return error(format!("{}: {}", path.display(), e)),
    };
    let mut replaced = 0;
    let mut spans: Vec<(usize, usize)> = vec![];
    for (i, e) in list.iter().enumerate() {
        // いくつもあるときは、どれでしくじったか (edits の何番目か) を言う
        let at = if list.len() > 1 { format!(" (edits[{}])", i) } else { String::new() };
        let (old, new) = (s(e, "old"), s(e, "new"));
        if old.is_empty() {
            return error(format!("old is empty{}", at));
        }
        let n = text.matches(old).count();
        let all = e["all"].as_bool().unwrap_or(false);
        if n == 0 {
            return error(format!("{}: old not found{}", path.display(), at));
        }
        if n > 1 && !all {
            return error(format!("{}: old found {} times (make it longer, or all: true){}", path.display(), n, at));
        }
        // うしろから置きかえ、変えたところ (バイトの場所と長さ) を覚えておく。前のものの場所はずらす
        let idxs: Vec<usize> = if all { text.match_indices(old).map(|(i, _)| i).collect() } else { text.find(old).into_iter().collect() };
        for &p in idxs.iter().rev() {
            text.replace_range(p..p + old.len(), new);
            let d = new.len() as isize - old.len() as isize;
            for sp in spans.iter_mut() {
                if sp.0 > p {
                    sp.0 = (sp.0 as isize + d) as usize;
                }
            }
            spans.push((p, new.len()));
        }
        replaced += idxs.len();
    }
    match write(path, text.as_bytes(), snaps) {
        r if r.get("error").is_some() => r,
        _ => {
            let (lines, shown) = around(&text, &spans);
            json!({ "path": path.display().to_string(), "replaced": replaced, "lines": lines, "text": shown })
        }
    }
}

/// 変えたところ (spans) の行と、前後 2 行を read と同じ形で (40 行まで)。答えの lines は変えた行の番号
fn around(text: &str, spans: &[(usize, usize)]) -> (Vec<usize>, String) {
    let all: Vec<&str> = text.lines().collect();
    let mut changed: Vec<(usize, usize)> = spans
        .iter()
        .map(|&(p, len)| {
            let a = text[..p.min(text.len())].matches('\n').count() + 1;
            let b = a + text[p.min(text.len())..(p + len).min(text.len())].matches('\n').count();
            (a, b)
        })
        .collect();
    changed.sort();
    let mut show: Vec<(usize, usize)> = vec![];
    for &(a, b) in &changed {
        let (x, y) = (a.saturating_sub(2).max(1), (b + 2).min(all.len().max(1)));
        match show.last_mut() {
            Some(l) if x <= l.1 + 1 => l.1 = l.1.max(y),
            _ => show.push((x, y)),
        }
    }
    let mut out = String::new();
    let mut n = 0;
    for (k, &(x, y)) in show.iter().enumerate() {
        if k > 0 {
            out.push_str("   ...\n");
        }
        for i in x..=y {
            if n == 40 {
                out.push_str("   ... (more changes)\n");
                return (changed.iter().map(|c| c.0).collect(), out);
            }
            if let Some(l) = all.get(i - 1) {
                out.push_str(&format!("{:6}\t{}\n", i, l));
                n += 1;
            }
        }
    }
    (changed.iter().map(|c| c.0).collect(), out)
}

fn hit(a: &Value, hits: &[Hit]) -> Value {
    let n = a["n"].as_u64().unwrap_or(0) as usize;
    let Some(h) = n.checked_sub(1).and_then(|i| hits.get(i)) else { return error(format!("no hit {} (grep found {})", n, hits.len())) };
    let c = a["context"].as_u64().unwrap_or(5) as usize;
    let from = h.line.saturating_sub(c).max(1);
    let mut r = read(&h.path, &json!({ "offset": from, "limit": c * 2 + 1 }));
    r["n"] = json!(n);
    r["line"] = json!(h.line);
    r
}

fn each(a: &Value, hits: &mut [Hit], snaps: &mut Vec<Snap>) -> Value {
    let re = match subs(a) {
        Ok(r) => r,
        Err(e) => return e,
    };
    if hits.is_empty() {
        return error("grep first");
    }
    let only: Vec<usize> = a["only"].as_array().map(|v| v.iter().filter_map(|x| x.as_u64().map(|x| x as usize)).collect()).unwrap_or_default();
    let mut changed = Vec::new();
    let mut skipped = Vec::new();
    // ファイルごとに読んで、まとめて書く
    let mut files: Vec<PathBuf> = Vec::new();
    for (i, h) in hits.iter().enumerate() {
        if (only.is_empty() || only.contains(&(i + 1))) && !files.contains(&h.path) {
            files.push(h.path.clone());
        }
    }
    for f in files {
        let text = match std::fs::read_to_string(&f) {
            Ok(t) => t,
            Err(e) => {
                skipped.push(json!({ "path": f.display().to_string(), "why": e.to_string() }));
                continue;
            }
        };
        let mut ls: Vec<String> = text.split_inclusive('\n').map(String::from).collect();
        let mut any = false;
        for (i, h) in hits.iter_mut().enumerate() {
            if h.path != f || !(only.is_empty() || only.contains(&(i + 1))) {
                continue;
            }
            let Some(l) = h.line.checked_sub(1).and_then(|k| ls.get_mut(k)) else {
                skipped.push(json!({ "n": i + 1, "why": "line is gone" }));
                continue;
            };
            let end = &l[l.trim_end_matches(['\n', '\r']).len()..];
            let body = l.trim_end_matches(['\n', '\r']);
            if body != h.text {
                skipped.push(json!({ "n": i + 1, "why": "changed since grep", "now": clip_line(body) }));
                continue;
            }
            let (new, k) = re.apply(body);
            if k == 0 {
                skipped.push(json!({ "n": i + 1, "why": "pattern not in this line" }));
                continue;
            }
            *l = format!("{}{}", new, end);
            changed.push(json!({ "n": i + 1, "path": h.shown, "line": h.line, "text": clip_line(&new) }));
            h.text = new;
            any = true;
        }
        if any && let r = write(&f, ls.concat().as_bytes(), snaps)
            && r.get("error").is_some()
        {
            return r;
        }
    }
    json!({ "changed": changed, "skipped": skipped })
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
    snaps.push(Snap { group: GROUP.load(std::sync::atomic::Ordering::Relaxed), path: path.to_path_buf(), before, dirs });
    if snaps.len() > KEEP {
        snaps.remove(0);
    }
    json!({ "path": path.display().to_string(), "bytes": content.len(), "created": created })
}

fn undo(path: &Path, latest: bool, snaps: &mut Vec<Snap>) -> Value {
    // path があればそのファイルの最後の変更、なければいちばん新しい組 (each なら何ファイルでも) をみな
    let take: Vec<Snap> = if latest {
        let Some(g) = snaps.last().map(|x| x.group) else { return error("nothing to undo") };
        let k = snaps.iter().position(|x| x.group == g).unwrap_or(snaps.len());
        snaps.split_off(k)
    } else {
        let Some(i) = snaps.iter().rposition(|x| x.path == path) else { return error("nothing to undo") };
        vec![snaps.remove(i)]
    };
    let mut done = Vec::new();
    for snap in take.iter().rev() {
        let r = match &snap.before {
            Some(b) => std::fs::write(&snap.path, b),
            None => std::fs::remove_file(&snap.path),
        };
        if let Err(e) = r {
            return error(format!("{}: {}", snap.path.display(), e));
        }
        for d in &snap.dirs {
            let _ = std::fs::remove_dir(d);
        }
        done.push(json!({ "path": snap.path.display().to_string(), "removed": snap.before.is_none() }));
    }
    if done.len() == 1 { done.remove(0) } else { json!({ "undone": done }) }
}

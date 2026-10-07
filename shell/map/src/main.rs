// aish-map: 探さなくていいように (aish の基本のプラグイン)
//   where    名前から定義の場所 (fn struct enum trait、C の関数と #define、シェルの関数、def class ...)
//   where body: true  いちばん上の定義の中身もいっしょに (場所を見て read する 2 回が 1 回に)
//   outline  ファイルの中の定義を行の番号つきで (ファイルをぜんぶ読まなくてよい)
//   M-.      where を絞りこんで選び、行を「$EDITOR +行 ファイル」にする (エディタの「定義へ」と同じキー)
// where は rg で定義の形を探す (そのたびにいまのファイルを見るので、索引もキャッシュも持たない)。
// 順位: 名前がぴったり → 前が同じ → 含む。同じなら、よく使うファイル (aish-pick の paths) が先
use aish_plugin::{Spec, Tool, Value, error, json, pick_live, s};
use std::path::{Path, PathBuf};

const WHERE: &str = r#"{"type":"object","properties":{"name":{"type":"string","description":"探す名前 (大文字小文字は区別しない。ぴったり、前が同じ、含む、の順)"},"kind":{"type":"string","description":"fn struct enum trait type mod const static macro impl define class def ... のどれかだけ"},"path":{"type":"string","description":"探すところ (既定: いまのディレクトリの git のいちばん上、なければいまのディレクトリ)"},"limit":{"type":"integer","description":"いくつまで (既定 30)"},"body":{"type":"boolean","description":"いちばん上のものに、定義の中身 (body: 行の番号つき、200 行まで) もつける。read しなくてよい"}},"required":["name"]}"#;
const OUTLINE: &str = r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}"#;

/// 言語: ファイルの名前 (拡張子か、名前そのもの) と、定義の形 (種類, 正規表現)。
/// 正規表現の NAME が名前 (where では問い、outline では何でも)、(?P<kind>...) があればそれが種類
struct Lang {
    exts: &'static [&'static str],
    names: &'static [&'static str],
    defs: &'static [(&'static str, &'static str)],
}

const VIS: &str = r"(?:pub(?:\([^)]*\))?\s+)?";

const LANGS: &[Lang] = &[
    Lang {
        exts: &["rs"],
        names: &[],
        defs: &[
            ("fn", r"^\s*VIS(?:(?:const|async|unsafe|extern(?:\s+\x22[^\x22]*\x22)?)\s+)*fn\s+NAME"),
            ("type", r"^\s*VIS(?:unsafe\s+)?(?P<kind>struct|enum|trait|type|mod|union)\s+NAME"),
            ("const", r"^\s*VIS(?P<kind>const|static)\s+(?:mut\s+)?NAME\s*:"),
            ("macro", r"^\s*(?:#\[macro_export\]\s*)?macro_rules!\s*NAME"),
            ("impl", r"^\s*(?:unsafe\s+)?impl(?:<[^>]*>)?\s+(?:[\w:]+(?:<[^>]*>)?\s+for\s+)?NAME"),
        ],
    },
    Lang {
        exts: &["c", "h", "cc", "cpp", "cxx", "hpp", "hh"],
        names: &[],
        defs: &[
            ("fn", r"^(?:[A-Za-z_][\w\s\*]*?[\s\*])?NAME\s*\([^;]*$"),
            ("define", r"^\s*#\s*define\s+NAME"),
            ("type", r"^\s*(?:typedef\s+)?(?P<kind>struct|union|enum|class)\s+NAME\s*(?:\{|:|$)"),
        ],
    },
    Lang { exts: &["sh", "bash", "zsh"], names: &["PKGBUILD", "aishrc", ".aishrc", ".profile", "profile"], defs: &[("fn", r"^\s*(?:function\s+)?NAME\s*\(\)"), ("fn", r"^\s*function\s+NAME")] },
    Lang { exts: &["py"], names: &[], defs: &[("def", r"^\s*(?:async\s+)?(?P<kind>def|class)\s+NAME")] },
    Lang {
        exts: &["js", "mjs", "cjs", "jsx", "ts", "tsx", "mts"],
        names: &[],
        defs: &[
            ("fn", r"^\s*(?:export\s+)?(?:default\s+)?(?:async\s+)?(?P<kind>function\*?|class|interface|type|enum)\s+NAME"),
            ("const", r"^\s*(?:export\s+)?(?P<kind>const|let|var)\s+NAME\s*[=:]"),
        ],
    },
    Lang { exts: &["zig"], names: &[], defs: &[("fn", r"^\s*(?:pub\s+)?(?:(?:export|extern|inline)\s+)*(?P<kind>fn|const|var)\s+NAME")] },
    Lang { exts: &["go"], names: &[], defs: &[("func", r"^func\s+(?:\([^)]*\)\s*)?NAME"), ("type", r"^type\s+NAME")] },
];

/// rg の -t で探すもの (PKGBUILD は --type-add で足す)
const RG_TYPES: &[&str] = &["rust", "c", "cpp", "sh", "pkgbuild", "py", "js", "ts", "zig", "go"];

fn lang_of(p: &Path) -> Option<usize> {
    let file = p.file_name()?.to_str()?;
    let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
    LANGS.iter().position(|l| l.names.contains(&file) || (!ext.is_empty() && l.exts.contains(&ext)))
}

/// 言語ごとの、名前を name にした定義の形 (1 回だけ作る)
fn compile(name: &str, flags: &str) -> Vec<Vec<(&'static str, regex::Regex)>> {
    LANGS.iter().map(|l| l.defs.iter().filter_map(|(k, p)| Some((*k, regex::Regex::new(&format!("{}{}", flags, def_re(p, name))).ok()?))).collect()).collect()
}

/// 定義の形を、name の部分を差しかえた正規表現に
fn def_re(pat: &str, name: &str) -> String {
    pat.replace("VIS", VIS).replace("NAME", &format!("(?P<name>{})", name))
}

fn main() {
    let spec = Spec {
        name: "map",
        hooks: &[],
        keys: &[("M-.", "where")],
        tools: &[
            Tool { name: "where", desc: "名前から定義の場所を返す (探さなくていい)。ぴったり → 前が同じ → 含む、同じならよく使うファイルが先。{items: [{name, kind, path, line, text, body?}]}。body: true でいちばん上の定義の中身も", input: WHERE },
            Tool { name: "outline", desc: "ファイルの中の定義の一覧 (行の番号、種類、名前、字下げ)。ファイルをぜんぶ読まずに形がわかる。{path, items: [{line, kind, name, indent}]}", input: OUTLINE },
        ],
    };
    let mut home = String::new();
    aish_plugin::run(spec, |ev, v| match ev {
        "hello" => {
            home = s(v, "home").to_string();
            json!({})
        }
        "tool" => {
            let a = &v["args"];
            let pwd = s(v, "pwd");
            match s(v, "name") {
                "where" => {
                    let root = if s(a, "path").is_empty() { git_root(pwd) } else { resolve(pwd, s(a, "path")) };
                    match find(&root, pwd, s(a, "name"), s(a, "kind"), a["limit"].as_u64().unwrap_or(30) as usize, &home) {
                        Ok(mut items) => {
                            if a["body"].as_bool() == Some(true)
                                && let Some(first) = items.first_mut()
                            {
                                let path = resolve(pwd, first["path"].as_str().unwrap_or(""));
                                if let Some(b) = body(&path, first["line"].as_u64().unwrap_or(1) as usize) {
                                    first["body"] = b.into();
                                }
                            }
                            json!({ "items": items })
                        }
                        Err(e) => e,
                    }
                }
                "outline" => outline(&resolve(pwd, s(a, "path"))),
                n => error(format!("{}: no such tool", n)),
            }
        }
        "key" => {
            let pwd = s(v, "pwd");
            let root = git_root(pwd);
            let picked = pick_live("where", |q| {
                if q.trim().is_empty() {
                    return vec![];
                }
                match find(&root, pwd, q.trim(), "", 200, &home) {
                    Ok(items) => items.iter().map(|x| format!("{} {}  {}:{}", x["kind"].as_str().unwrap_or(""), x["name"].as_str().unwrap_or(""), x["path"].as_str().unwrap_or(""), x["line"])).collect(),
                    Err(e) => vec![format!("({})", e["error"].as_str().unwrap_or(""))],
                }
            });
            // "kind name  path:line" → $EDITOR +line path
            let Some((path, line)) = picked.as_deref().and_then(|p| p.rsplit_once("  ")).and_then(|(_, loc)| loc.rsplit_once(':')) else { return json!({}) };
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
            let l = format!("{} +{} {}", editor, line, aish_plugin::escape(path));
            json!({ "line": l, "pos": l.chars().count() })
        }
        _ => json!({}),
    });
}

fn resolve(pwd: &str, p: &str) -> PathBuf {
    let p = Path::new(p);
    if p.is_absolute() { p.to_path_buf() } else { Path::new(pwd).join(p) }
}

/// pwd から上へたどって .git のあるところ (なければ pwd)
fn git_root(pwd: &str) -> PathBuf {
    let mut d = PathBuf::from(pwd);
    loop {
        if d.join(".git").exists() {
            return d;
        }
        if !d.pop() {
            return PathBuf::from(pwd);
        }
    }
}

/// 見せるパス: pwd の下なら相対、ほかは絶対
fn show(abs: &Path, pwd: &str) -> String {
    match abs.strip_prefix(pwd) {
        Ok(r) if !pwd.is_empty() => r.display().to_string(),
        _ => abs.display().to_string(),
    }
}

fn find(root: &Path, pwd: &str, name: &str, kind: &str, limit: usize, home: &str) -> Result<Vec<Value>, Value> {
    if name.is_empty() {
        return Err(error("name is empty"));
    }
    let q = regex::escape(name);
    let word = format!(r"\w*{}\w*", q);
    // rg には名前のない形で (同じ名前のグループがいくつもあると rg の正規表現にならない)
    let mut args: Vec<String> = vec!["-i".into(), "--type-add".into(), "pkgbuild:PKGBUILD".into(), "--max-columns".into(), "300".into()];
    for t in RG_TYPES {
        args.push("-t".into());
        args.push(t.to_string());
    }
    for l in LANGS {
        for (_, p) in l.defs {
            args.push("-e".into());
            args.push(def_re(p, &word).replace("(?P<name>", "(").replace("(?P<kind>", "("));
        }
    }
    args.push(".".into());
    let r = aish_plugin::rg_json(&root.display().to_string(), &args, 5000).ok_or_else(|| error("where needs rg (ripgrep: sudo ap -S ripgrep)"))?;
    if r.status == 2 && r.items.is_empty() {
        return Err(error(format!("rg: {}", r.err.trim())));
    }
    let scores = aish_plugin::path_scores(home);
    let res = compile(&word, "(?i)");
    let low = name.to_lowercase();
    let mut found: Vec<(u8, f64, Value)> = Vec::new();
    for it in r.items.iter().filter(|it| it["type"] == "match") {
        let d = &it["data"];
        let rel = aish_plugin::rg_text(&d["path"]);
        let abs = root.join(rel.strip_prefix("./").unwrap_or(&rel));
        let text = aish_plugin::rg_text(&d["lines"]);
        let text = text.trim_end();
        let Some(lang) = lang_of(&abs) else { continue };
        // その言語の形で、名前と種類を取りだす (ほかの言語の形にだけ合ったものはのぞく)
        let Some((k, n)) = res[lang].iter().find_map(|(k, re)| {
            let c = re.captures(text)?;
            Some((c.name("kind").map_or(*k, |m| m.as_str()).to_string(), c.name("name")?.as_str().to_string()))
        }) else {
            continue;
        };
        if !kind.is_empty() && k != kind {
            continue;
        }
        let nl = n.to_lowercase();
        let tier = if n == name {
            0
        } else if nl == low {
            1
        } else if nl.starts_with(&low) {
            2
        } else {
            3
        };
        let score = scores.get(&abs.display().to_string()).copied().unwrap_or(0.0);
        let text: String = text.trim().chars().take(200).collect();
        found.push((tier, score, json!({ "name": n, "kind": k, "path": show(&abs, pwd), "line": d["line_number"], "text": text })));
    }
    found.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.total_cmp(&a.1)).then(a.2["path"].as_str().map(str::len).cmp(&b.2["path"].as_str().map(str::len))));
    Ok(found.into_iter().take(limit).map(|x| x.2).collect())
}

/// 定義の中身: line (1 から) から、かっこが閉じるまで (Python は字下げが戻るまで)。行の番号つき、200 行まで
fn body(path: &Path, line: usize) -> Option<String> {
    const MAX: usize = 200;
    let b = std::fs::read(path).ok()?;
    let text = String::from_utf8_lossy(&b);
    let lines: Vec<&str> = text.lines().collect();
    let start = line.checked_sub(1).filter(|&i| i < lines.len())?;
    let end = if path.extension().is_some_and(|e| e == "py") { py_end(&lines, start) } else { brace_end(&lines, start) };
    let last = end.min(start + MAX - 1);
    let mut out: String = (start..=last).map(|i| format!("{:>6}\t{}\n", i + 1, lines[i])).collect();
    if last < end {
        out.push_str(&format!("... ({} more lines)\n", end - last));
    }
    Some(out)
}

/// { } が閉じる行 (文字と // のコメントの中はかぞえない)。開く前に ; で終わればそこ
fn brace_end(lines: &[&str], start: usize) -> usize {
    let mut depth = 0i32;
    let mut opened = false;
    for (i, l) in lines.iter().enumerate().skip(start) {
        let mut q: Option<char> = None;
        let mut esc = false;
        let mut prev = ' ';
        let cs: Vec<char> = l.chars().collect();
        let mut j = 0;
        while j < cs.len() {
            let c = cs[j];
            j += 1;
            // '{' や '\'' (文字ひとつ) はとばす。'a のような寿命はそのまま
            if q.is_none() && c == '\'' {
                let n = if cs.get(j) == Some(&'\\') { 3 } else { 2 };
                if cs.get(j + n - 1) == Some(&'\'') {
                    j += n;
                    continue;
                }
            }
            if let Some(qc) = q {
                if esc {
                    esc = false;
                } else if c == '\\' {
                    esc = true;
                } else if c == qc {
                    q = None;
                }
            } else if c == '/' && prev == '/' {
                break;
            } else if c == '"' {
                q = Some(c);
            } else if c == '{' {
                depth += 1;
                opened = true;
            } else if c == '}' {
                depth -= 1;
            }
            prev = c;
        }
        if opened && depth <= 0 {
            return i;
        }
        if !opened && l.trim_end().ends_with(';') {
            return i;
        }
    }
    lines.len() - 1
}

/// Python: 字下げが def の行と同じか浅い行の前まで
fn py_end(lines: &[&str], start: usize) -> usize {
    let ind = |l: &str| l.len() - l.trim_start().len();
    let base = ind(lines[start]);
    let mut end = start;
    for (i, l) in lines.iter().enumerate().skip(start + 1) {
        if l.trim().is_empty() {
            continue;
        }
        if ind(l) <= base {
            break;
        }
        end = i;
    }
    end
}

fn outline(path: &Path) -> Value {
    let text = match std::fs::read(path) {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(e) => return error(format!("{}: {}", path.display(), e)),
    };
    let md = path.extension().is_some_and(|e| e == "md");
    let Some(lang) = lang_of(path) else {
        if !md {
            return error(format!("{}: no outline for this kind of file", path.display()));
        }
        // Markdown: 見出し
        let items: Vec<Value> = text
            .lines()
            .enumerate()
            .filter(|(_, l)| l.starts_with('#'))
            .map(|(i, l)| {
                let depth = l.chars().take_while(|c| *c == '#').count();
                json!({ "line": i + 1, "kind": "h", "name": l[depth..].trim(), "indent": depth - 1 })
            })
            .collect();
        return json!({ "path": path.display().to_string(), "items": items });
    };
    let res = compile(r"[A-Za-z_]\w*", "").swap_remove(lang);
    let mut items = Vec::new();
    for (i, l) in text.lines().enumerate() {
        for (k, re) in &res {
            if let Some(c) = re.captures(l) {
                let indent = l.chars().take_while(|c| c.is_whitespace()).map(|c| if c == '\t' { 4 } else { 1 }).sum::<usize>() / 4;
                let kind = c.name("kind").map_or(*k, |m| m.as_str());
                items.push(json!({ "line": i + 1, "kind": kind, "name": c.name("name").map_or("", |m| m.as_str()), "indent": indent }));
                break;
            }
        }
    }
    json!({ "path": path.display().to_string(), "items": items })
}

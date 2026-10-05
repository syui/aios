// パッケージの配布元の最新の版を見て (check)、PKGBUILD と .aios.json を書きかえる (edit)
//   どこを見るか (pkg/upstream.json) は check が作る。人は書かない。材料は 2 つ:
//     1. PKGBUILD の source が git (git+URL#tag=...) か GitHub / GitLab のリリースなら、そのリポジトリとタグの形
//     2. ほかは Arch の .nvchecker.toml (gitlab.archlinux.org の packaging/packages/NAME。Arch と名前が
//        ちがうものは PKGBUILD に _arch=NAME)
//   最新はタグ (git ls-remote) か配布元のページ (nvchecker の regex) から。いまの pkgver とくらべる
// 自作のもの (0.0.1) と、コミットで決めているもの (#commit=) は見ない。取ってくるのは git と fetch (なければ curl)
use serde_json::{Map, Value, json};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// pkg の下に作るリスト
pub const FILE: &str = "upstream.json";
const ARCH: &str = "https://gitlab.archlinux.org/archlinux/packaging/packages";
/// いちどに見に行く数
const JOBS: usize = 8;

/// pkg/*/NAME/PKGBUILD (名前の順)
pub fn pkgbuilds(pkg: &Path) -> Vec<(String, PathBuf)> {
    let mut v = Vec::new();
    for repo in fs::read_dir(pkg).into_iter().flatten().flatten() {
        for d in fs::read_dir(repo.path()).into_iter().flatten().flatten() {
            let f = d.path().join("PKGBUILD");
            if f.is_file() {
                v.push((d.file_name().to_string_lossy().into_owned(), f));
            }
        }
    }
    v.sort();
    v
}

/// PKGBUILD の 1 つの変数 (NAME=値)。引用符はとる。展開はしない ($pkgver はタグの形として残したい)
fn scalar(text: &str, key: &str) -> Option<String> {
    let l = text.lines().find_map(|l| l.strip_prefix(&format!("{}=", key)))?;
    Some(l.trim().trim_matches(|c| c == '\'' || c == '"').to_string())
}

/// source=(...) の 1 つめ (name:: はとる)
fn first_source(text: &str) -> Option<String> {
    let start = text.find("\nsource=(")? + "\nsource=(".len();
    let body = &text[start..];
    let body = &body[..body.find(')')?];
    let s = body.split_whitespace().next()?.trim_matches(|c| c == '\'' || c == '"');
    Some(s.rsplit_once("::").map_or(s, |(_, u)| u).to_string())
}

/// source からリポジトリとタグの形。#commit= なら Err("commit")
fn from_source(src: &str) -> Result<Option<(String, String)>, String> {
    if let Some(rest) = src.strip_prefix("git+") {
        let (url, frag) = rest.split_once('#').unwrap_or((rest, ""));
        return match frag.split_once('=') {
            Some(("tag", t)) => Ok(Some((url.to_string(), t.to_string()))),
            Some(("commit", _)) => Err("pinned to a commit".into()),
            _ => Ok(None),
        };
    }
    // https://github.com/O/R/releases/download/TAG/FILE
    if let Some(rest) = src.strip_prefix("https://github.com/") {
        let p: Vec<&str> = rest.split('/').collect();
        if p.len() > 5 && p[2] == "releases" && p[3] == "download" {
            return Ok(Some((format!("https://github.com/{}/{}.git", p[0], p[1]), p[4].to_string())));
        }
    }
    // https://gitlab.HOST/PROJECT/-/releases/TAG/downloads/FILE
    if src.starts_with("https://gitlab.")
        && let Some((proj, rest)) = src.split_once("/-/releases/")
    {
        return Ok(rest.split('/').next().map(|t| (format!("{}.git", proj), t.to_string())));
    }
    Ok(None)
}

/// タグの形 (PKGBUILD の書き方: $pkgver ${pkgver} ${pkgver//./_}) を正規表現に。
/// 版の字は数と英字と . (//./_ なら _)。pkgver を使っていなければ None
fn template_regex(t: &str) -> Option<(String, bool)> {
    let forms = [("${pkgver//./_}", true), ("${pkgver}", false), ("$pkgver", false)];
    let (pat, under) = forms.iter().find(|(p, _)| t.contains(p))?;
    let (a, b) = t.split_once(pat)?;
    let ver = if *under { "([0-9][0-9A-Za-z_]*)" } else { "([0-9][0-9A-Za-z.]*)" };
    if b.contains('$') || a.contains('$') {
        return None;
    }
    Some((format!("^{}{}{}$", regex::escape(a), ver, regex::escape(b)), *under))
}

/// .nvchecker.toml の最初の表を読む (使うのは文字と真偽だけ)
fn parse_toml(text: &str) -> Option<Map<String, Value>> {
    let mut m = Map::new();
    let mut seen = false;
    for l in text.lines() {
        let l = l.trim();
        if l.starts_with('[') {
            if seen {
                break;
            }
            seen = true;
            continue;
        }
        if !seen || l.is_empty() || l.starts_with('#') {
            continue;
        }
        let Some((k, v)) = l.split_once('=') else { continue };
        let v = v.trim();
        let val = if let Some(s) = v.strip_prefix('\'') {
            json!(s[..s.find('\'').unwrap_or(s.len())])
        } else if let Some(s) = v.strip_prefix('"') {
            // "..." は \\ と \" をもどす
            let mut out = String::new();
            let mut it = s.chars();
            while let Some(c) = it.next() {
                match c {
                    '"' => break,
                    '\\' => out.extend(it.next()),
                    c => out.push(c),
                }
            }
            json!(out)
        } else if v == "true" || v == "false" {
            json!(v == "true")
        } else {
            continue;
        };
        m.insert(k.trim().to_string(), val);
    }
    seen.then_some(m)
}

/// url の中身 (fetch か curl)
fn get(url: &str) -> Result<String, String> {
    let out = if Command::new("fetch").arg("--help").output().is_ok() {
        Command::new("fetch").args([url, "-o", "-"]).output()
    } else {
        Command::new("curl").args(["-fsSL", "-m", "30", url]).output()
    }
    .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("{}: {}", url, String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// リポジトリのタグ
fn tags(git: &str) -> Result<Vec<String>, String> {
    let out = Command::new("git")
        .args(["ls-remote", "--tags", "--refs", git])
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| format!("git: {}", e))?;
    if !out.status.success() {
        return Err(format!("git ls-remote {}: {}", git, String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.split_once("refs/tags/").map(|(_, t)| t.to_string())).collect())
}

/// 1 つのパッケージをどこで見るか ({name, pkgver, ...})。見ないものは skip に理由
fn rule(name: &str, path: &Path) -> Value {
    let text = fs::read_to_string(path).unwrap_or_default();
    let pkgver = scalar(&text, "pkgver").unwrap_or_default();
    let mut r = json!({ "name": name, "pkgver": pkgver });
    if pkgver.starts_with("0.0.1") {
        r["skip"] = json!("aios");
        return r;
    }
    match first_source(&text).map(|s| from_source(&s)) {
        Some(Err(e)) => {
            r["skip"] = json!(e);
            return r;
        }
        Some(Ok(Some((git, tag)))) => {
            let Some((re, under)) = template_regex(&tag) else {
                // ほかの変数の版 (libunwind の $_llvm) で取ってくるもの。その変数のほうを見る
                r["skip"] = json!(format!("the source tag {} does not use pkgver", tag));
                return r;
            };
            r["from"] = json!("pkgbuild");
            r["git"] = json!(git);
            r["tag"] = json!(re);
            if under {
                r["under"] = json!(true);
            }
            return r;
        }
        _ => {}
    }
    let arch = scalar(&text, "_arch").unwrap_or_else(|| name.to_string());
    match get(&format!("{}/{}/-/raw/main/.nvchecker.toml", ARCH, arch)).ok().and_then(|t| parse_toml(&t)) {
        Some(m) if m.get("source").and_then(|v| v.as_str()) == Some("manual") => r["skip"] = json!(format!("arch {} checks it by hand", arch)),
        Some(m) => {
            r["from"] = json!(format!("arch:{}", arch));
            r["nvchecker"] = Value::Object(m);
        }
        None => r["skip"] = json!(format!("no git source in the PKGBUILD, and no .nvchecker.toml for arch {}", arch)),
    }
    r
}

/// 名前ごとに f を JOBS 本ずつ並べて動かす (順番はそのまま)
fn par<T: Send, R: Send>(items: Vec<T>, f: impl Fn(T) -> R + Sync) -> Vec<R> {
    let items: std::sync::Mutex<Vec<(usize, T)>> = std::sync::Mutex::new(items.into_iter().enumerate().collect());
    let out = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..JOBS {
            s.spawn(|| {
                while let Some((i, t)) = {
                    let x = items.lock().unwrap().pop();
                    x
                } {
                    let r = f(t);
                    out.lock().unwrap().push((i, r));
                }
            });
        }
    });
    let mut out = out.into_inner().unwrap();
    out.sort_by_key(|x| x.0);
    out.into_iter().map(|x| x.1).collect()
}

/// pkg/upstream.json を作る (名前を渡せばそれだけ作りなおす)
pub fn upstream(pkg: &Path, only: &[String]) -> Result<Vec<Value>, String> {
    let file = pkg.join(FILE);
    let mut old: Vec<Value> = fs::read_to_string(&file).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let todo: Vec<(String, PathBuf)> = pkgbuilds(pkg).into_iter().filter(|(n, _)| only.is_empty() || only.contains(n)).collect();
    if todo.is_empty() {
        return Err("no such package".into());
    }
    let new = par(todo, |(n, p)| rule(&n, &p));
    old.retain(|o| !new.iter().any(|n| n["name"] == o["name"]) && pkgbuilds(pkg).iter().any(|(n, _)| o["name"] == n.as_str()));
    old.extend(new.iter().cloned());
    old.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    let mut text = String::from("[\n");
    for (i, r) in old.iter().enumerate() {
        text.push_str(&format!("  {}{}\n", r, if i + 1 < old.len() { "," } else { "" }));
    }
    text.push_str("]\n");
    fs::write(&file, text).map_err(|e| format!("{}: {}", file.display(), e))?;
    Ok(new)
}

/// upstream.json を読む。まだないもの (と refresh なら only か全部) は作ってから
fn rules(pkg: &Path, only: &[String], refresh: bool) -> Result<Vec<Value>, String> {
    let read = || -> Vec<Value> { fs::read_to_string(pkg.join(FILE)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default() };
    let have = read();
    let missing: Vec<String> = pkgbuilds(pkg).into_iter().map(|(n, _)| n).filter(|n| !have.iter().any(|r| r["name"] == n.as_str())).collect();
    if refresh {
        upstream(pkg, only)?;
    } else if !missing.is_empty() {
        upstream(pkg, &missing)?;
    }
    Ok(read())
}

/// 版をくらべる (pacman の vercmp と同じ考え: 数と英字のかたまりごと、数は数として)
pub fn vercmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    let seg = |s: &str| -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        let mut cur = String::new();
        for c in s.chars() {
            let same = cur.chars().last().is_some_and(|l| l.is_ascii_digit() == c.is_ascii_digit());
            if !c.is_ascii_alphanumeric() {
                if !cur.is_empty() {
                    v.push(std::mem::take(&mut cur));
                }
            } else if cur.is_empty() || same {
                cur.push(c);
            } else {
                v.push(std::mem::replace(&mut cur, c.to_string()));
            }
        }
        if !cur.is_empty() {
            v.push(cur);
        }
        v
    };
    let (x, y) = (seg(a), seg(b));
    for (p, q) in x.iter().zip(y.iter()) {
        let o = match (p.parse::<u64>(), q.parse::<u64>()) {
            (Ok(m), Ok(n)) => m.cmp(&n),
            (Ok(_), Err(_)) => Greater,
            (Err(_), Ok(_)) => Less,
            _ => p.cmp(q),
        };
        if o != Equal {
            return o;
        }
    }
    // 長いほうが新しい: 1.2 < 1.2.1、3.7 < 3.7c、10.5 < 10.5p1 (rc や beta は先にとばしている)
    x.len().cmp(&y.len())
}

/// 出たばかりのもの (rc beta ...) はとばす
fn prerelease(v: &str) -> bool {
    let l = v.to_ascii_lowercase();
    ["rc", "alpha", "beta", "pre", "dev", "snapshot"].iter().any(|w| l.contains(w))
}

/// Python の置きかえ (\1) を Rust の regex の形 (${1}) に
fn py_repl(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\\' && it.peek().is_some_and(|d| d.is_ascii_digit()) {
            out.push_str(&format!("${{{}}}", it.next().unwrap_or('0')));
        } else if c == '$' {
            out.push_str("$$");
        } else {
            out.push(c);
        }
    }
    out
}

fn full(re: &str) -> Result<regex::Regex, String> {
    regex::Regex::new(&format!("^(?:{})$", re)).map_err(|e| format!("regex {:?}: {}", re, e))
}

/// nvchecker の書き方で、いちばん新しい版
fn nvchecker(m: &Map<String, Value>) -> Result<String, String> {
    let s = |k: &str| m.get(k).and_then(|v| v.as_str());
    let raw: Vec<String> = match s("source").unwrap_or("") {
        "git" => tags(s("git").ok_or("git: no url")?)?,
        "github" => tags(&format!("https://github.com/{}.git", s("github").ok_or("github: no repo")?))?,
        "gitlab" => tags(&format!("https://{}/{}.git", s("host").unwrap_or("gitlab.com"), s("gitlab").ok_or("gitlab: no repo")?))?,
        "regex" => {
            let page = get(s("url").ok_or("regex: no url")?)?;
            let re = regex::Regex::new(s("regex").ok_or("regex: no regex")?).map_err(|e| e.to_string())?;
            re.captures_iter(&page).filter_map(|c| c.get(1).or(c.get(0)).map(|x| x.as_str().to_string())).collect()
        }
        other => return Err(format!("nvchecker source {:?} is not supported", other)),
    };
    let inc = s("include_regex").map(full).transpose()?;
    let exc = s("exclude_regex").map(full).transpose()?;
    let ignored: Vec<&str> = s("ignored").unwrap_or("").split_whitespace().collect();
    let from = s("from_pattern").map(regex::Regex::new).transpose().map_err(|e| e.to_string())?;
    let mut vs = Vec::new();
    for t in raw {
        if inc.as_ref().is_some_and(|r| !r.is_match(&t)) || exc.as_ref().is_some_and(|r| r.is_match(&t)) || ignored.contains(&t.as_str()) {
            continue;
        }
        let mut v = t.clone();
        // nvchecker と同じく、prefix はついていればとる (なければそのまま)
        if let Some(x) = s("prefix").and_then(|p| v.strip_prefix(p)) {
            v = x.to_string();
        }
        // nvchecker と同じく、合わなければそのまま
        if let Some(f) = &from {
            v = f.replace_all(&v, py_repl(s("to_pattern").unwrap_or(""))).into_owned();
        }
        // 版らしいものだけ (数で始まり、- や空白がない。node-v26.10.0-linux-x64-musl の -linux... は落とす)
        if v.starts_with(|c: char| c.is_ascii_digit()) && v.chars().all(|c| c.is_ascii_alphanumeric() || "._+".contains(c)) && !prerelease(&v) {
            vs.push(v);
        }
    }
    vs.into_iter().max_by(|a, b| vercmp(a, b)).ok_or_else(|| "no version found".into())
}

/// upstream.json の 1 つの行から、いちばん新しい版
pub fn latest(r: &Value) -> Result<String, String> {
    if let Some(m) = r["nvchecker"].as_object() {
        return nvchecker(m);
    }
    let git = r["git"].as_str().ok_or("no rule")?;
    let re = regex::Regex::new(r["tag"].as_str().unwrap_or("")).map_err(|e| e.to_string())?;
    let under = r["under"] == true;
    tags(git)?
        .iter()
        .filter_map(|t| re.captures(t).and_then(|c| c.get(1)).map(|v| if under { v.as_str().replace('_', ".") } else { v.as_str().to_string() }))
        .filter(|v| !prerelease(v))
        .max_by(|a, b| vercmp(a, b))
        .ok_or_else(|| "no matching tag".into())
}

/// check: upstream.json と、いまの PKGBUILD の pkgver をくらべる
pub fn check(pkg: &Path, only: &[String], refresh: bool) -> Result<Vec<Value>, String> {
    let rules = rules(pkg, if refresh { only } else { &[] }, refresh)?;
    let now: std::collections::HashMap<String, String> =
        pkgbuilds(pkg).into_iter().map(|(n, p)| (n, scalar(&fs::read_to_string(p).unwrap_or_default(), "pkgver").unwrap_or_default())).collect();
    let skips: Vec<(String, String)> = rules.iter().filter_map(|r| Some((r["name"].as_str()?.to_string(), r["skip"].as_str()?.to_string()))).collect();
    let todo: Vec<Value> = rules.into_iter().filter(|r| r.get("skip").is_none() && (only.is_empty() || only.iter().any(|o| r["name"] == o.as_str()))).collect();
    let rs = par(todo, |r| {
        let name = r["name"].as_str().unwrap_or("").to_string();
        let cur = now.get(&name).cloned().unwrap_or_default();
        match latest(&r) {
            Ok(v) => json!({ "name": name, "pkgver": cur, "latest": v, "new": vercmp(&v, &cur).is_gt() }),
            Err(e) => json!({ "name": name, "pkgver": cur, "error": e }),
        }
    });
    // しくじったもの (配布元が切れたなど) は、一覧の前の値を残す
    let mut seen: std::collections::HashMap<String, String> = rs.iter().filter_map(|r| Some((r["name"].as_str()?.to_string(), r["latest"].as_str()?.to_string()))).collect();
    for (n, why) in skips {
        let short = if why == "aios" { "aios" } else if why.contains("commit") { "commit" } else if why.contains("pkgver") { "follows another version" } else if why.contains("by hand") { "manual" } else { "no upstream" };
        seen.insert(n, format!("({})", short));
    }
    overview(pkg, &seen);
    Ok(rs)
}

/// 一覧に作るファイル: 1 つのパッケージに 1 行で name type src now latest
pub const OVERVIEW: &str = "pkg.json";

/// pkg/pkg.json を作りなおす。latest は seen (いま見たもの) か、前のファイルのもの
pub fn overview(pkg: &Path, seen: &std::collections::HashMap<String, String>) {
    let file = pkg.join(OVERVIEW);
    let old: Vec<Value> = fs::read_to_string(&file).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let items = pkgbuilds(pkg);
    let rows: Vec<Vec<(&str, String)>> = par(items, |(name, path)| {
        let text = fs::read_to_string(&path).unwrap_or_default();
        let dir = path.parent().unwrap_or(Path::new("."));
        let ty = dir.parent().and_then(|d| d.file_name()).map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
        let src = if first_source(&text).is_some() { expand_source(dir, &text).unwrap_or_default() } else { String::new() };
        let latest = seen.get(&name).cloned().or_else(|| old.iter().find(|o| o["name"] == name.as_str()).and_then(|o| o["latest"].as_str().map(String::from))).unwrap_or_default();
        let now = scalar(&text, "pkgver").unwrap_or_default();
        let now = if now.contains('$') { expand_var(dir, &text, "pkgver") } else { now };
        vec![("name", name), ("type", ty), ("src", src), ("now", now), ("latest", latest)]
    });
    // 列をそろえる: "key": "value", のかたまりを、列ごとにいちばん長いものの幅に
    let cells: Vec<Vec<String>> = rows.iter().map(|r| r.iter().map(|(k, v)| format!("{}: {}", json!(k), json!(v))).collect()).collect();
    let n = cells.first().map_or(0, |c| c.len());
    let w: Vec<usize> = (0..n).map(|i| cells.iter().map(|c| c[i].chars().count()).max().unwrap_or(0)).collect();
    let mut text = String::from("[\n");
    for (j, c) in cells.iter().enumerate() {
        let mut line = String::from("  {");
        for (i, cell) in c.iter().enumerate() {
            if i + 1 < n {
                line.push_str(&format!("{},{}", cell, " ".repeat(w[i] - cell.chars().count() + 1)));
            } else {
                line.push_str(cell);
            }
        }
        line.push('}');
        if j + 1 < cells.len() {
            line.push(',');
        }
        text.push_str(&line);
        text.push('\n');
    }
    text.push_str("]\n");
    let _ = fs::write(&file, text);
}

/// PKGBUILD の変数を bash で展開する (pkgver=0.0.1.r${_commit:0:7} など)
fn expand_var(dir: &Path, text: &str, key: &str) -> String {
    Command::new("bash")
        .args(["-c", &format!("eval \"$1\"; printf %s \"${{{}}}\"", key), "-", text])
        .current_dir(dir)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

// ---- edit ----

/// edit: NAME の pkgver を ver (なければ配布元の最新) にする。pkgrel は 1 に。source が tarball なら
/// 取ってきて sha256sums の 1 つめを書きかえる。.aios.json の pkg の版と updated も
pub fn edit(root: &Path, name: &str, ver: Option<&str>) -> Result<Value, String> {
    let pkg = root.join("pkg");
    let (_, path) = pkgbuilds(&pkg).into_iter().find(|(n, _)| n == name).ok_or_else(|| format!("{}: no PKGBUILD", name))?;
    let old_text = fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let old = scalar(&old_text, "pkgver").unwrap_or_default();
    let ver = match ver {
        Some(v) => v.to_string(),
        None => {
            let r = rules(&pkg, &[], false)?.into_iter().find(|r| r["name"] == name).ok_or("no rule")?;
            if let Some(s) = r["skip"].as_str() {
                return Err(format!("{}: not checked ({})", name, s));
            }
            latest(&r)?
        }
    };
    if !ver.chars().all(|c| c.is_ascii_alphanumeric() || "._+".contains(c)) {
        return Err(format!("{}: bad version", ver));
    }
    let mut r = json!({ "name": name, "from": old, "to": ver, "files": [] });
    if ver == old {
        r["same"] = json!(true);
        return Ok(r);
    }
    let set = |text: &str, key: &str, val: &str| -> String {
        text.lines().map(|l| if l.starts_with(&format!("{}=", key)) { format!("{}={}", key, val) } else { l.to_string() }).collect::<Vec<_>>().join("\n") + "\n"
    };
    let mut text = set(&set(&old_text, "pkgver", &ver), "pkgrel", "1");
    // sha256sums の 1 つめが SKIP でなければ、新しい source を取ってきて計りなおす
    if let Some(sum) = first_sum(&text).filter(|s| s != "SKIP") {
        let dir = path.parent().unwrap_or(Path::new("."));
        let url = expand_source(dir, &text)?;
        let old_url = expand_source(dir, &old_text)?;
        if url == old_url {
            return Err(format!("{}: the source does not use $pkgver ({})", name, url));
        }
        let new = sha256_url(&url)?;
        text = text.replacen(&sum, &new, 1);
        r["sha256"] = json!(new);
    }
    fs::write(&path, &text).map_err(|e| e.to_string())?;
    let mut files = vec![json!(path.strip_prefix(root).unwrap_or(&path).display().to_string())];
    if aios_json(&root.join(".aios.json"), name, &ver)? {
        files.push(json!(".aios.json"));
    }
    files.push(json!("pkg/pkg.json"));
    overview(&pkg, &std::collections::HashMap::new());
    r["files"] = json!(files);
    Ok(r)
}

/// sha256sums=( の 1 つめ
fn first_sum(text: &str) -> Option<String> {
    let start = text.find("sha256sums=(")? + "sha256sums=(".len();
    let s = text[start..].split_whitespace().next()?.trim_end_matches(')');
    Some(s.trim_matches(|c| c == '\'' || c == '"').to_string())
}

/// source の 1 つめを bash で展開する (${pkgver%.*} なども)。name:: はとる
fn expand_source(dir: &Path, text: &str) -> Result<String, String> {
    let out = Command::new("bash")
        .args(["-c", "eval \"$1\"; printf %s \"${source[0]}\"", "-", text])
        .current_dir(dir)
        .output()
        .map_err(|e| format!("bash: {}", e))?;
    let s = String::from_utf8_lossy(&out.stdout).into_owned();
    Ok(s.rsplit_once("::").map_or(s.clone(), |(_, u)| u.to_string()))
}

/// url を取ってきて sha256 (取ってきたものは消す)
fn sha256_url(url: &str) -> Result<String, String> {
    use sha2::Digest;
    use std::io::Read;
    let tmp = std::env::temp_dir().join(format!("aish-pkg-{}", std::process::id()));
    let ok = if Command::new("fetch").arg("--help").output().is_ok() {
        Command::new("fetch").arg(url).arg("-o").arg(&tmp).status()
    } else {
        Command::new("curl").args(["-fsSL", "-o"]).arg(&tmp).arg(url).status()
    }
    .is_ok_and(|s| s.success());
    if !ok {
        let _ = fs::remove_file(&tmp);
        return Err(format!("{}: could not download", url));
    }
    let mut f = fs::File::open(&tmp).map_err(|e| e.to_string())?;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    let _ = fs::remove_file(&tmp);
    Ok(h.finalize().iter().map(|b| format!("{:02x}", b)).collect())
}

/// .aios.json の pkg の下の "NAME": "版" と "updated" を書きかえる (形はそのまま)。NAME がなければ false
fn aios_json(path: &Path, name: &str, ver: &str) -> Result<bool, String> {
    let Ok(text) = fs::read_to_string(path) else { return Ok(false) };
    let start = text.find("\"pkg\"").unwrap_or(0);
    let re = regex::Regex::new(&format!(r#""{}":(\s*)"[^"]*""#, regex::escape(name))).map_err(|e| e.to_string())?;
    let Some(m) = re.captures_at(&text, start) else { return Ok(false) };
    let whole = m.get(0).unwrap();
    let mut out = format!("{}\"{}\":{}\"{}\"{}", &text[..whole.start()], name, &m[1], ver, &text[whole.end()..]);
    let up = regex::Regex::new(r#""updated":(\s*)"[^"]*""#).map_err(|e| e.to_string())?;
    out = up.replace(&out, format!("\"updated\":${{1}}\"{}\"", today())).into_owned();
    fs::write(path, out).map_err(|e| e.to_string())?;
    Ok(true)
}

/// きょうの日付 (UTC、YYYY-MM-DD)
fn today() -> String {
    let days = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() / 86400) as i64;
    // 1970-01-01 からの日数を暦に (Howard Hinnant の civil_from_days)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{:02}-{:02}", yoe + era * 400 + if m <= 2 { 1 } else { 0 }, m, d)
}

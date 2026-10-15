// 脆弱性 (CVE) の見はり (pkg_check の vulns): aios のパッケージの版 (pkgver) を、「この版で直した」の一覧とくらべる
//   Alpine Linux の secdb (https://secdb.alpinelinux.org/edge/ の main.json と community.json。パッケージごとに
//   「この版 (VER-rN) で直した CVE」)。pkg/c のもの (配布元のソースから作るもの) と、Alpine の同じ名前のものと同じものと
//   わかっているもの (ALSO)
//     newer: 直した版のほうが新しい (配布元の新しい版で直っている。pkg_edit で上げる)
//     patch: Alpine が同じ版にパッチをあてて直している (VER-r1 から)。aios に同じパッチがなければ直っていない
//   Mozilla の advisories (MOZILLA の firefox。Alpine は firefox の secfixes を書かず、Arch の security tracker は
//   2023 年からほとんど動いていない (firefox は 2019 年の 67.0.4 まで) ので、配布元のもの):
//   github.com/mozilla/foundation-security-advisories の announce/YEAR/mfsaYEAR-N.yml の fixed_in の「Firefox VER」と
//   その advisories の CVE。git で浅く、ことしと去年のところだけ取る (2026 年だけで 110 ほど、CVE は 2600 ほど)
//   Rust の crate (cargo audit と同じ RustSec の advisory-db: github.com/rustsec/advisory-db の crates/NAME/RUSTSEC-*.md):
//     crate: aios の Cargo.lock (カーネル、user、shell など) と、pkg/rust のものの配布元の Cargo.lock (source の git の
//     タグかコミットのもの。PKGBUILD の cargo_update CRATE@VER (pkg/rust/cross.sh) で上げたものは、上げた版で)。
//     informational (unmaintained や unsound) と withdrawn のもの、Linux や aarch64 にあてはまらないものは出さない
//   npm (.aios.json の pkg.linux.npm に人が書いた版): npm audit と同じ registry.npmjs.org の advisories
//     range: その advisory があてはまる版の範囲
// PKGBUILD かそのとなりのファイル (パッチなど) に番号 (CVE-、RUSTSEC-、GHSA-) を書いておけば、直した (かあてはまらない) と
// みて出さない (aios の Cargo.lock は、となりの Cargo.toml などに)。
// pkg/rust の coreutils (uutils) や tar (aios のもの) は名前が同じでも別のものなので Alpine とはくらべない。名前のちがうもの
// (glib2 は glib ...) は ALIAS で。どこも書いていないもの (mesa など) は見られない
// どれも 1 日に 1 回取ってきて build/aish-pkg/secdb/ に置く。取れなければ前のもので (取りなおすのは 1 時間に 1 回まで)
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

const SECDB: [&str; 2] = ["https://secdb.alpinelinux.org/edge/main.json", "https://secdb.alpinelinux.org/edge/community.json"];
/// aios (Arch の名前) と Alpine で名前のちがうもの
const ALIAS: [(&str, &str); 3] = [("glib2", "glib"), ("gdk-pixbuf2", "gdk-pixbuf"), ("freetype2", "freetype")];
/// pkg/c のほかで、Alpine の同じ名前のものと同じもの
const ALSO: [&str; 1] = ["sudo-rs"];
/// Mozilla の advisories で見るもの: (aios の名前, fixed_in の名前)。Firefox ESR や Firefox for iOS は別のもの
const MOZILLA: [(&str, &str); 1] = [("firefox", "Firefox")];
const MFSA: &str = "https://github.com/mozilla/foundation-security-advisories.git";
const RUSTSEC: &str = "https://github.com/rustsec/advisory-db.git";
/// npm audit がたずねるところ: {名前: [版]} を POST すると {名前: [advisory]}
const NPM_AUDIT: &str = "https://registry.npmjs.org/-/npm/v1/security/advisories/bulk";

const HOUR: Duration = Duration::from_secs(3600);
const DAY: Duration = Duration::from_secs(24 * 3600);

/// Alpine の名前 → (版 VER-rN → CVE)
type Db = HashMap<String, BTreeMap<String, Vec<String>>>;

/// 直っていないもののまとまり
#[derive(Debug, PartialEq)]
pub struct Fix {
    /// newer (新しい版で直っている)、patch (Alpine が同じ版にパッチをあてて直している)、crate (Cargo.lock の crate)、
    /// range (npm)
    pub how: &'static str,
    /// 直した版 (newer)、パッチをあてた Alpine の版 VER-rN (patch)、直った版の範囲 (crate。直ったものがなければ "")、
    /// あてはまる版の範囲 (range)
    pub version: String,
    /// どこで知ったか: alpine、Mozilla の advisory (mfsa2026-106)、rustsec、npm
    pub source: String,
    /// CVE (RUSTSEC と npm は、その番号: RUSTSEC-2026-0346、GHSA-xxxx-xxxx-xxxx)
    pub cves: Vec<String>,
    /// crate: その crate と Cargo.lock の版 (rustls 0.23.45)
    pub what: String,
}

/// 1 つのパッケージ (か Cargo.lock) の直っていないもの
#[derive(Debug, PartialEq)]
pub struct Vuln {
    pub name: String,
    /// aios の版 (pkgver。配布元の HEAD から作るものは HEAD、aios の Cargo.lock は "")
    pub now: String,
    pub fixes: Vec<Fix>,
}

/// テキストの中の番号: CVE-YYYY-N、RUSTSEC-YYYY-N、GHSA-xxxx-xxxx-xxxx
fn ids_in(text: &str) -> BTreeSet<String> {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"CVE-\d{4}-\d{4,}|RUSTSEC-\d{4}-\d{4,}|GHSA(?:-[0-9a-z]{4}){3}").expect("regex"));
    re.find_iter(text).map(|m| m.as_str().to_string()).collect()
}

/// dir のファイル (PKGBUILD、パッチ、Cargo.toml など) に書いてある番号: 直したか、あてはまらないもの
fn done_in(dir: &Path) -> BTreeSet<String> {
    let mut done = BTreeSet::new();
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        if let Ok(t) = fs::read_to_string(e.path()) {
            done.extend(ids_in(&t));
        }
    }
    done
}

/// ファイルが書かれてからどれだけたったか
fn age(p: &Path) -> Option<Duration> {
    fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok())
}

/// 取れなかったものを使っているときの 1 行: what を N 日前のもので (取れなかったわけ)
fn stale_note(what: &str, old: Duration, why: &str) -> String {
    format!("using the {} from {} days ago (could not fetch it{}{})", what, old.as_secs() / 86400, if why.is_empty() { "" } else { ": " }, why.trim())
}

/// secdb を読む (1 日たったものは取ってくる)。取れなかったら前のものと、そう書いた 1 行
fn secdb(root: &Path) -> Result<(Db, String), String> {
    let dir = root.join("build/aish-pkg/secdb");
    let _ = fs::create_dir_all(&dir);
    // 取れなかったときに、見るたびに待たないように (ネットのない aios など)。中は取れなかったわけ (取れたら空)
    let tried = dir.join("tried");
    let may = age(&tried).is_none_or(|a| a > HOUR);
    let (mut db, mut note, mut touched) = (Db::new(), String::new(), false);
    for url in SECDB {
        let f = dir.join(url.rsplit('/').next().unwrap_or("secdb.json"));
        if may && age(&f).is_none_or(|a| a > DAY) {
            touched = true;
            match crate::up::get(url).and_then(|t| serde_json::from_str::<Value>(&t).map(|_| t).map_err(|e| format!("{}: {}", url, e))) {
                Ok(t) => {
                    let part = f.with_extension("json.part");
                    if fs::write(&part, t).is_ok() {
                        let _ = fs::rename(&part, &f);
                    }
                }
                Err(e) => note = e,
            }
        }
        let text = fs::read_to_string(&f).map_err(|e| format!("{}: {}{}", f.display(), e, if note.is_empty() { String::new() } else { format!(" ({})", note) }))?;
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {}", f.display(), e))?;
        for p in v["packages"].as_array().into_iter().flatten() {
            let Some(name) = p["pkg"]["name"].as_str() else { continue };
            let fixes = db.entry(name.to_string()).or_default();
            for (ver, cves) in p["pkg"]["secfixes"].as_object().into_iter().flatten() {
                // "CVE-2021-27219 GHSL-2021-045" のように、ほかの番号とならんでいるものもある
                fixes.entry(ver.clone()).or_default().extend(cves.as_array().into_iter().flatten().filter_map(|c| c.as_str()).flat_map(ids_in));
            }
        }
    }
    if touched {
        let _ = fs::write(&tried, &note);
    }
    // 1 日より古いものを使っていれば (いま取れなかったか、1 時間のうちに取れなかったので取りに行かなかった)
    let old = SECDB.iter().filter_map(|u| age(&dir.join(u.rsplit('/').next().unwrap_or("")))).max().unwrap_or_default();
    if old > DAY {
        let why = if note.is_empty() { fs::read_to_string(&tried).unwrap_or_default() } else { note };
        note = stale_note("Alpine secdb", old, &why);
    } else {
        note.clear();
    }
    Ok((db, note))
}

/// 1 つのパッケージ: secfixes (Alpine の版 VER-rN → CVE) のうち、aios の版 now (pkgver) で直っていないもの。done は直したと
/// 書いてある CVE
fn open(now: &str, secfixes: &BTreeMap<String, Vec<String>>, done: &BTreeSet<String>) -> Vec<Fix> {
    use std::cmp::Ordering::*;
    let mut out = Vec::new();
    for (fix, cves) in secfixes {
        // "0" はその CVE が Alpine のものにはあてはまらないという印
        let (ver, rel) = fix.rsplit_once("-r").map_or((fix.as_str(), 0), |(v, r)| (v, r.parse::<u32>().unwrap_or(0)));
        if ver == "0" {
            continue;
        }
        // 1.2.3_rc1 などは出る前のもの: 1.2.3 にはみな入っている
        let pre = ["_rc", "_alpha", "_beta", "_pre"].iter().find_map(|s| ver.find(s));
        let (how, version) = match crate::up::vercmp(now, pre.map_or(ver, |i| &ver[..i])) {
            Less => ("newer", ver.to_string()),
            Equal if rel > 0 && pre.is_none() => ("patch", fix.clone()),
            _ => continue,
        };
        let left: Vec<String> = cves.iter().filter(|c| !done.contains(*c)).cloned().collect();
        if !left.is_empty() {
            out.push(Fix { how, version, source: "alpine".into(), cves: left, what: String::new() });
        }
    }
    out
}

/// いまの年 (1970 年 1 月 1 日からの日の数から)
fn year_now() -> i64 {
    let days = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() / 86400) as i64;
    // H. Hinnant の civil_from_days
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    yoe + era * 400 + i64::from(mp >= 10)
}

/// git を動かして、その出力 (しくじったら、サブコマンドと git の言ったことの終わりの行)
fn git_out(args: &[&str]) -> Result<String, String> {
    let o = Command::new("git").args(args).env("GIT_TERMINAL_PROMPT", "0").output().map_err(|e| format!("git: {}", e))?;
    if o.status.success() {
        return Ok(String::from_utf8_lossy(&o.stdout).into_owned());
    }
    // -C DIR のあとのサブコマンド
    let (mut it, mut sub) = (args.iter(), "");
    while let Some(a) = it.next() {
        if *a == "-C" {
            it.next();
        } else if !a.starts_with('-') {
            sub = a;
            break;
        }
    }
    let err = String::from_utf8_lossy(&o.stderr);
    Err(format!("git {}: {}", sub, err.trim().lines().last().unwrap_or("failed")))
}

fn git(args: &[&str]) -> Result<(), String> {
    git_out(args).map(|_| ())
}

/// git のリポジトリ url を base/name に浅く置く (はじめはファイルの中身なしで clone して、dirs のところだけ取る)。1 日たって
/// いれば取りなおす (しくじったら 1 時間は取りに行かない)。答えは、1 日より古いもの (what) を使っているか、まだ 1 度も
/// 取れていなければ、そう書いた 1 行 (取れていれば "")
fn git_copy(base: &Path, name: &str, what: &str, url: &str, dirs: &[&str]) -> String {
    let (repo, ok, tried) = (base.join(name), base.join(format!("{}.fetched", name)), base.join(format!("{}.tried", name)));
    let mut note = String::new();
    if age(&ok).is_none_or(|a| a > DAY) && age(&tried).is_none_or(|a| a > HOUR) {
        let r = repo.to_str().unwrap_or("");
        let got = if repo.join(".git").is_dir() {
            git(&["-C", r, "fetch", "-q", "--depth", "1", "origin", "HEAD"]).and_then(|_| git(&["-C", r, "reset", "-q", "--hard", "FETCH_HEAD"]))
        } else {
            let _ = fs::remove_dir_all(&repo);
            git(&["clone", "-q", "--depth", "1", "--filter=blob:none", "--sparse", url, r])
        };
        let mut set = vec!["-C", r, "sparse-checkout", "set"];
        set.extend(dirs);
        match got.and_then(|_| git(&set)) {
            Ok(()) => {
                let _ = fs::write(&ok, "");
                let _ = fs::remove_file(&tried);
            }
            Err(e) => {
                let _ = fs::write(&tried, &e);
                note = e;
            }
        }
    }
    let why = if note.is_empty() { fs::read_to_string(&tried).unwrap_or_default() } else { note };
    match age(&ok) {
        Some(a) if a > DAY => stale_note(what, a, &why),
        Some(_) => String::new(),
        None => why,
    }
}

/// Mozilla の advisories (mfsa の id, YAML) を読む。1 日たっていれば git で取りなおす (ことしと去年のところだけ)。
/// 取れなかったら前のものと、そう書いた 1 行
fn mfsa(root: &Path) -> Result<(Vec<(String, String)>, String), String> {
    let base = root.join("build/aish-pkg/secdb");
    let _ = fs::create_dir_all(&base);
    let y = year_now();
    let years = [format!("announce/{}", y - 1), format!("announce/{}", y)];
    let note = git_copy(&base, "mfsa", "Mozilla advisories", MFSA, &[&years[0], &years[1]]);
    let repo = base.join("mfsa");
    let mut out = Vec::new();
    for y in &years {
        for e in fs::read_dir(repo.join(y)).into_iter().flatten().flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "yml")
                && let Ok(t) = fs::read_to_string(&p)
            {
                out.push((p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(), t));
            }
        }
    }
    if out.is_empty() {
        return Err(format!("no Mozilla advisories in {}{}", repo.display(), if note.is_empty() { String::new() } else { format!(" ({})", note) }));
    }
    Ok((out, note))
}

/// 1 つの advisory (mfsaYEAR-N.yml): fixed_in の product の版 ("- Firefox 158.0.1"。"- Firefox ESR 140.17" は別のもの) と、
/// advisories のキーの CVE ("  CVE-2026-109704:")
fn mfsa_file(text: &str, product: &str) -> (Vec<String>, Vec<String>) {
    let (mut fixed, mut cves, mut list) = (Vec::new(), Vec::new(), false);
    let item = format!("- {} ", product);
    for l in text.lines() {
        if l.starts_with("fixed_in:") {
            list = true;
            continue;
        }
        if list {
            if l.starts_with("- ") {
                if let Some(v) = l.strip_prefix(&item).map(str::trim).filter(|v| v.starts_with(|c: char| c.is_ascii_digit())) {
                    fixed.push(v.to_string());
                }
                continue;
            }
            list = false;
        }
        if let Some(k) = l.strip_prefix("  ").and_then(|k| k.trim_end().strip_suffix(':')).filter(|k| k.starts_with("CVE-")) {
            cves.push(k.to_string());
        }
    }
    (fixed, cves)
}

/// Mozilla の advisories のうち、product の now (pkgver) で直っていないもの (直した版がいまより新しいもの)
fn open_mfsa(now: &str, product: &str, advs: &[(String, String)], done: &BTreeSet<String>) -> Vec<Fix> {
    let mut out: Vec<Fix> = Vec::new();
    for (id, text) in advs {
        let (fixed, cves) = mfsa_file(text, product);
        // いまより新しい版のうち、いちばん古いもの (いくつもの版の線に入れたもの)
        let Some(v) = fixed.iter().filter(|v| crate::up::vercmp(now, v).is_lt()).min_by(|a, b| crate::up::vercmp(a, b)) else { continue };
        let left: Vec<String> = cves.into_iter().filter(|c| !done.contains(c)).collect();
        if !left.is_empty() {
            out.push(Fix { how: "newer", version: v.clone(), source: id.clone(), cves: left, what: String::new() });
        }
    }
    out.sort_by(|a, b| crate::up::vercmp(&a.version, &b.version).then_with(|| a.source.cmp(&b.source)));
    out
}

// ---- Rust の crate (RustSec) ----

/// RustSec の 1 つの advisory
#[derive(Debug, Default, PartialEq)]
struct Adv {
    id: String,
    /// CVE や GHSA の番号
    aliases: Vec<String>,
    /// 直った版と、はじめからあてはまらない版 (semver の範囲: ">= 0.23.46" ">= 0.9.20, < 0.10.0" "^0.8.6")
    patched: Vec<String>,
    unaffected: Vec<String>,
}

/// advisory (RUSTSEC-*.md) の頭の ```toml ... ``` から (crate, advisory)。使うのは文字と文字の配列だけ。
/// informational (unmaintained、unsound、notice) と withdrawn のもの、aios (Linux, aarch64) にあてはまらないもの
/// ([affected] の os と arch) は None
fn rustsec_file(text: &str) -> Option<(String, Adv)> {
    let body = text.strip_prefix("```toml")?;
    let body = &body[..body.find("\n```")?];
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r#""((?:[^"\\]|\\.)*)"|'([^']*)'"#).expect("regex"));
    let (mut kv, mut sect, mut stmt): (HashMap<String, Vec<String>>, String, String) = (HashMap::new(), String::new(), String::new());
    for l in body.lines() {
        if l.trim_start().starts_with('#') {
            continue;
        }
        stmt.push_str(l);
        stmt.push('\n');
        // 配列は何行にもなる: 文字の外の [ と ] の数がそろうまでためる
        let bare = re.replace_all(&stmt, "");
        if bare.matches('[').count() > bare.matches(']').count() {
            continue;
        }
        let s = std::mem::take(&mut stmt);
        let t = s.trim();
        if t.starts_with('[') {
            sect = t.trim_matches(|c| c == '[' || c == ']').trim().to_string();
        } else if let Some((k, v)) = t.split_once('=') {
            let vals = re.captures_iter(v).map(|c| c.get(1).or(c.get(2)).map_or("", |m| m.as_str()).to_string()).collect();
            kv.insert(format!("{}.{}", sect, k.trim()), vals);
        }
    }
    let one = |k: &str| kv.get(k).and_then(|v| v.first()).cloned();
    let list = |k: &str| kv.get(k).cloned().unwrap_or_default();
    if one("advisory.informational").is_some() || one("advisory.withdrawn").is_some() {
        return None;
    }
    let (os, arch) = (list("affected.os"), list("affected.arch"));
    if (!os.is_empty() && !os.iter().any(|o| o == "linux")) || (!arch.is_empty() && !arch.iter().any(|a| a == "aarch64")) {
        return None;
    }
    Some((one("advisory.package")?, Adv { id: one("advisory.id")?, aliases: list("advisory.aliases"), patched: list("versions.patched"), unaffected: list("versions.unaffected") }))
}

/// RustSec の advisory-db (crates/ のところだけ。1 日に 1 回 git で取りなおす): crate の名前 → advisories
fn rustsec(root: &Path) -> Result<(HashMap<String, Vec<Adv>>, String), String> {
    let base = root.join("build/aish-pkg/secdb");
    let _ = fs::create_dir_all(&base);
    let note = git_copy(&base, "rustsec", "RustSec advisory-db", RUSTSEC, &["crates"]);
    let crates = base.join("rustsec/crates");
    let mut db: HashMap<String, Vec<Adv>> = HashMap::new();
    for d in fs::read_dir(&crates).into_iter().flatten().flatten() {
        for e in fs::read_dir(d.path()).into_iter().flatten().flatten() {
            if let Ok(t) = fs::read_to_string(e.path())
                && let Some((krate, a)) = rustsec_file(&t)
            {
                db.entry(krate).or_default().push(a);
            }
        }
    }
    if db.is_empty() {
        return Err(format!("no RustSec advisories in {}{}", crates.display(), if note.is_empty() { String::new() } else { format!(" ({})", note) }));
    }
    Ok((db, note))
}

/// Cargo.lock の crates.io の crate: (名前, 版)。workspace のもの (source のないもの) と git のものは見ない
fn lock_crates(text: &str) -> Vec<(String, String)> {
    text.split("[[package]]")
        .skip(1)
        .filter_map(|b| {
            let get = |k: &str| b.lines().find_map(|l| l.strip_prefix(k)).map(|v| v.trim().trim_matches('"').to_string());
            if !get("source = ")?.contains("crates.io") {
                return None;
            }
            Some((get("name = ")?, get("version = ")?))
        })
        .collect()
}

/// cargo の semver で、同じものとして上げられる版か (1.0.102 と 1.0.104、0.9.18 と 0.9.21。0.9 と 0.10 はちがう)
fn same_series(a: &semver::Version, b: &semver::Version) -> bool {
    a.major == b.major && (a.major > 0 || (a.minor == b.minor && (a.minor > 0 || a.patch == b.patch)))
}

/// PKGBUILD の cargo_update CRATE@VER... (pkg/rust/cross.sh) を crates にあてる: 同じ semver の中で VER より古ければ VER に
fn raise(crates: &mut [(String, String)], pkgbuild: &str) {
    for l in pkgbuild.lines() {
        let Some(args) = l.trim_start().strip_prefix("cargo_update ") else { continue };
        for (c, want) in args.split_whitespace().filter_map(|a| a.split_once('@')) {
            let Ok(w) = semver::Version::parse(want) else { continue };
            for (n, v) in crates.iter_mut() {
                if n == c
                    && let Ok(have) = semver::Version::parse(v)
                    && same_series(&have, &w)
                    && have < w
                {
                    *v = want.to_string();
                }
            }
        }
    }
}

/// crates のうち、RustSec の advisory があてはまるもの (crate と版ごとに、直った版の範囲ごとに 1 つ)。done は直したか
/// あてはまらないと書いてある番号 (RUSTSEC か、その aliases の CVE)
fn open_rust(crates: &[(String, String)], db: &HashMap<String, Vec<Adv>>, done: &BTreeSet<String>) -> Vec<Fix> {
    let mut out: Vec<Fix> = Vec::new();
    for (name, ver) in crates {
        let (Some(advs), Ok(v)) = (db.get(name), semver::Version::parse(ver)) else { continue };
        // 1.0.0-rc.1 などは、その版の範囲 (>= 0.9.20 など) に入らないので、1.0.0 として見る
        let v = if v.pre.is_empty() { v } else { semver::Version::new(v.major, v.minor, v.patch) };
        let hit = |rs: &[String]| rs.iter().any(|r| semver::VersionReq::parse(r).is_ok_and(|q| q.matches(&v)));
        for a in advs {
            if hit(&a.patched) || hit(&a.unaffected) || done.contains(&a.id) || a.aliases.iter().any(|x| done.contains(x)) {
                continue;
            }
            let (what, fixed) = (format!("{} {}", name, ver), a.patched.join(" or "));
            match out.iter_mut().find(|f| f.what == what && f.version == fixed) {
                Some(f) => f.cves.push(a.id.clone()),
                None => out.push(Fix { how: "crate", version: fixed, source: "rustsec".into(), cves: vec![a.id.clone()], what }),
            }
        }
    }
    for f in &mut out {
        f.cves.sort();
    }
    out
}

/// aios のリポジトリの Cargo.lock (いちばん上 (カーネル) と、そのすぐ下のディレクトリ (user、shell、desktop など) のもの)
fn own_locks(root: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(root).into_iter().flatten().flatten().map(|e| e.path().join("Cargo.lock")).chain([root.join("Cargo.lock")]).filter(|p| p.is_file()).collect();
    v.sort();
    v
}

/// pkg/rust の name の配布元の Cargo.lock (src は PKGBUILD の source の 1 つめ: git+URL#tag=T か #commit=C か
/// #branch=B、なければ HEAD)。dir/NAME-REF.lock に置き、タグとコミットのものは取りなおさない (ほかは 1 日に 1 回)。
/// git で浅く、ファイルの中身は Cargo.lock のものだけ取る (しくじったら 1 時間は取りに行かず、前のものがあればそれで)。
/// 答えは (Cargo.lock, 取ったところ: タグ、コミット、HEAD)
fn upstream_lock(dir: &Path, name: &str, src: &str) -> Result<(String, String), String> {
    let rest = src.strip_prefix("git+").ok_or_else(|| format!("not a git source: {}", src))?;
    let (url, frag) = rest.split_once('#').unwrap_or((rest, ""));
    let (want, fixed) = match frag.split_once('=') {
        Some(("tag", t)) => (format!("refs/tags/{}", t), true),
        Some(("commit", c)) => (c.to_string(), true),
        Some(("branch", b)) => (format!("refs/heads/{}", b), false),
        _ => ("HEAD".to_string(), false),
    };
    let short = want.rsplit('/').next().unwrap_or("HEAD").to_string();
    let file = dir.join(format!("{}-{}.lock", name, short));
    let old = fs::read_to_string(&file);
    if let Ok(t) = &old
        && age(&file).is_some_and(|a| fixed || a < DAY)
    {
        return Ok((t.clone(), short));
    }
    let tried = dir.join(format!("{}.tried", name));
    if age(&tried).is_some_and(|a| a < HOUR) {
        return old.map(|t| (t, short)).map_err(|_| fs::read_to_string(&tried).unwrap_or_default());
    }
    let repo = dir.join(format!("{}.git", name));
    let g = repo.to_str().unwrap_or("");
    let _ = fs::remove_dir_all(&repo);
    let got = git(&["init", "-q", "--bare", g])
        .and_then(|_| git(&["-C", g, "fetch", "-q", "--depth", "1", "--filter=blob:none", url, &want]))
        .and_then(|_| git_out(&["-C", g, "cat-file", "-p", "FETCH_HEAD:Cargo.lock"]));
    let _ = fs::remove_dir_all(&repo);
    match got {
        Ok(t) => {
            let _ = fs::write(&file, &t);
            let _ = fs::remove_file(&tried);
            Ok((t, short))
        }
        Err(e) => {
            let e = format!("{} {}: {}", url, short, e);
            let _ = fs::write(&tried, &e);
            old.map(|t| (t, short)).map_err(|_| e)
        }
    }
}

/// 見る 1 つの Cargo.lock
struct Lock {
    /// パッケージの名前か、aios の Cargo.lock のパス (user/Cargo.lock)
    name: String,
    /// その版 (Vuln の now)
    now: String,
    /// crates.io の crate (cargo_update で上げたあと)
    crates: Vec<(String, String)>,
    /// 直したか、あてはまらないと書いてある番号
    done: BTreeSet<String>,
}

/// 見る Cargo.lock: aios のものと pkg/rust の配布元のもの。取れなかったものは notes に
fn rust_locks(root: &Path, names: &[String], notes: &mut Vec<String>) -> Vec<Lock> {
    let mut out = Vec::new();
    for p in own_locks(root) {
        let name = p.strip_prefix(root).unwrap_or(&p).to_string_lossy().into_owned();
        if (names.is_empty() || names.contains(&name))
            && let Ok(t) = fs::read_to_string(&p)
        {
            out.push(Lock { name, now: String::new(), crates: lock_crates(&t), done: done_in(p.parent().unwrap_or(root)) });
        }
    }
    let dir = root.join("build/aish-pkg/secdb/locks");
    let _ = fs::create_dir_all(&dir);
    // aios のツリーから作るもの (base や aikernel。source がない) は上の Cargo.lock で見ている
    let todo: Vec<(String, PathBuf, String)> = crate::up::pkgbuilds(&root.join("pkg"))
        .into_iter()
        .filter(|(name, pb)| (names.is_empty() || names.contains(name)) && pb.parent().and_then(|d| d.parent()).and_then(|d| d.file_name()).is_some_and(|k| k == "rust"))
        .filter_map(|(name, pb)| {
            let text = fs::read_to_string(&pb).ok()?;
            let src = crate::up::expand_source(pb.parent()?, &text).ok().filter(|s| s.starts_with("git+"))?;
            Some((name, pb, src))
        })
        .collect();
    let got = crate::up::par(todo, |(name, pb, src)| {
        let r = upstream_lock(&dir, &name, &src);
        (name, pb, r)
    });
    for (name, pb, r) in got {
        match r {
            Ok((text, at)) => {
                let pkgbuild = fs::read_to_string(&pb).unwrap_or_default();
                let mut crates = lock_crates(&text);
                raise(&mut crates, &pkgbuild);
                let ver = crate::up::version(&pb);
                let ver = ver.rsplit(':').next().unwrap_or(&ver);
                let now = ver.rsplit_once('-').map_or(ver, |(v, _)| v);
                out.push(Lock { name, now: if now.is_empty() { at } else { now.to_string() }, crates, done: done_in(pb.parent().unwrap_or(root)) });
            }
            Err(e) => notes.push(format!("{}: no upstream Cargo.lock ({})", name, e.trim())),
        }
    }
    out
}

// ---- npm ----

/// .aios.json の pkg.linux.npm (人が書く: "pnpm": "12.9.1"、"@anthropic-ai/claude-code": "2.1.289 (ap -S claude)")
fn npm_list(root: &Path) -> Vec<(String, String)> {
    let v: Value = fs::read_to_string(root.join(".aios.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    v["pkg"]["linux"]["npm"].as_object().into_iter().flatten().filter_map(|(n, ver)| Some((n.clone(), ver.as_str()?.split_whitespace().next()?.to_string()))).collect()
}

/// url に JSON を POST した答え (curl)
fn post(url: &str, body: &str) -> Result<String, String> {
    let o = Command::new("curl").args(["-fsS", "-m", "30", "-X", "POST", "-H", "content-type: application/json", "--data-binary", body, url]).output().map_err(|e| format!("curl: {}", e))?;
    if !o.status.success() {
        return Err(format!("{}: {}", url, String::from_utf8_lossy(&o.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

/// npm の advisories (npm audit と同じもの) を {名前: [版]} でたずねる。1 日に 1 回 (たずねるものが変われば、すぐ)。
/// 取れなければ前のもの (同じものをたずねたときの答え) と、そう書いた 1 行
fn npm_audit(root: &Path, list: &[(String, String)]) -> Result<(Value, String), String> {
    let base = root.join("build/aish-pkg/secdb");
    let _ = fs::create_dir_all(&base);
    let (file, tried) = (base.join("npm.json"), base.join("npm.tried"));
    let ask = Value::Object(list.iter().map(|(n, v)| (n.clone(), json!([v]))).collect());
    let old = fs::read_to_string(&file).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).filter(|o| o["ask"] == ask);
    let mut note = String::new();
    if !(old.is_some() && age(&file).is_some_and(|a| a < DAY)) && age(&tried).is_none_or(|a| a > HOUR) {
        match post(NPM_AUDIT, &ask.to_string()).and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| format!("{}: {}", NPM_AUDIT, e))) {
            Ok(ans) => {
                let _ = fs::write(&file, json!({ "ask": ask, "answer": ans }).to_string());
                let _ = fs::remove_file(&tried);
                return Ok((ans, String::new()));
            }
            Err(e) => {
                let _ = fs::write(&tried, &e);
                note = e;
            }
        }
    }
    let why = if note.is_empty() { fs::read_to_string(&tried).unwrap_or_default() } else { note };
    match old {
        Some(o) => {
            let a = age(&file).unwrap_or_default();
            Ok((o["answer"].clone(), if a > DAY { stale_note("npm advisories", a, &why) } else { String::new() }))
        }
        None => Err(format!("npm advisories: {}", if why.is_empty() { "not fetched yet" } else { why.trim() })),
    }
}

/// npm の 1 つのパッケージの advisories (答えの NAME の配列): あてはまる版の範囲ごとに、GHSA の番号 (url の終わり)
fn open_npm(advs: &Value) -> Vec<Fix> {
    let mut out: Vec<Fix> = Vec::new();
    for a in advs.as_array().into_iter().flatten() {
        let id = a["url"].as_str().and_then(|u| u.rsplit('/').next()).filter(|s| s.starts_with("GHSA-")).map_or_else(|| format!("npm-{}", a["id"]), String::from);
        let range = a["vulnerable_versions"].as_str().unwrap_or("").to_string();
        match out.iter_mut().find(|f| f.version == range) {
            Some(f) => f.cves.push(id),
            None => out.push(Fix { how: "range", version: range, source: "npm".into(), cves: vec![id], what: String::new() }),
        }
    }
    out
}

/// pkg/c のもの (と ALSO)、Rust の crate (aios と pkg/rust の Cargo.lock)、npm のものの直っていないもの (names があれば
/// それだけ。aios の Cargo.lock は user/Cargo.lock などの名前で)。答えと、説明の 1 行 (取れなかったときなど)
pub fn check(root: &Path, names: &[String]) -> Result<(Vec<Vuln>, String), String> {
    let (db, note) = secdb(root)?;
    let mut notes = vec![note];
    // Mozilla の advisories は、それを見るパッケージがあるときだけ取る
    let mut moz: Option<Vec<(String, String)>> = None;
    let mut out = Vec::new();
    for (name, pb) in crate::up::pkgbuilds(&root.join("pkg")) {
        let dir: PathBuf = pb.parent().unwrap_or(Path::new(".")).to_path_buf();
        let kind = dir.parent().and_then(|d| d.file_name()).map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
        if !(names.is_empty() || names.contains(&name)) || !(kind == "c" || ALSO.contains(&name.as_str())) {
            continue;
        }
        let alpine = db.get(ALIAS.iter().find(|(a, _)| *a == name).map_or(name.as_str(), |(_, b)| b));
        let product = MOZILLA.iter().find(|(n, _)| *n == name).map(|(_, p)| *p);
        if alpine.is_none() && product.is_none() {
            continue;
        }
        // [epoch:]pkgver-pkgrel の pkgver (pkgver() で決めるものは "" なので見ない)
        let ver = crate::up::version(&pb);
        let ver = ver.rsplit(':').next().unwrap_or(&ver);
        let now = ver.rsplit_once('-').map_or(ver, |(v, _)| v);
        if now.is_empty() {
            continue;
        }
        // PKGBUILD とパッチなどに書いてある CVE は直したとみる
        let done = done_in(&dir);
        let mut fixes = alpine.map(|s| open(now, s, &done)).unwrap_or_default();
        if let Some(p) = product {
            if moz.is_none() {
                moz = Some(match mfsa(root) {
                    Ok((a, n)) => {
                        notes.push(n);
                        a
                    }
                    Err(e) => {
                        notes.push(e);
                        Vec::new()
                    }
                });
            }
            fixes.extend(open_mfsa(now, p, moz.as_deref().unwrap_or_default(), &done));
        }
        if !fixes.is_empty() {
            out.push(Vuln { name, now: now.to_string(), fixes });
        }
    }
    let locks = rust_locks(root, names, &mut notes);
    if !locks.is_empty() {
        match rustsec(root) {
            Ok((db, n)) => {
                notes.push(n);
                for l in locks {
                    let fixes = open_rust(&l.crates, &db, &l.done);
                    if !fixes.is_empty() {
                        out.push(Vuln { name: l.name, now: l.now, fixes });
                    }
                }
            }
            Err(e) => notes.push(e),
        }
    }
    let npm: Vec<(String, String)> = npm_list(root).into_iter().filter(|(n, _)| names.is_empty() || names.contains(n)).collect();
    if !npm.is_empty() {
        match npm_audit(root, &npm) {
            Ok((ans, n)) => {
                notes.push(n);
                for (name, ver) in npm {
                    let fixes = open_npm(&ans[&name]);
                    if !fixes.is_empty() {
                        out.push(Vuln { name, now: ver, fixes });
                    }
                }
            }
            Err(e) => notes.push(e),
        }
    }
    notes.retain(|n| !n.is_empty());
    Ok((out, notes.join("; ")))
}

/// 1 行で: 2.8.5: CVE-a CVE-b fixed in 2.9.0; CVE-c patched by Alpine in 1.2.6-r1。crate なら rustls 0.23.45 RUSTSEC-a
/// fixed in >= 0.23.46、npm なら GHSA-a affects <1.2.3。番号が多ければ 4 つと残りの数 (Mozilla は 1 つのバグに 1 つの
/// CVE なので、1 つの版に何十も)
pub fn describe(v: &Vuln) -> String {
    let list = |c: &[String]| if c.len() > 4 { format!("{} and {} more", c[..4].join(" "), c.len() - 4) } else { c.join(" ") };
    let parts: Vec<String> = v
        .fixes
        .iter()
        .map(|f| match f.how {
            "newer" if f.source == "alpine" => format!("{} fixed in {}", list(&f.cves), f.version),
            "newer" => format!("{} fixed in {} ({})", list(&f.cves), f.version, f.source),
            "crate" if f.version.is_empty() => format!("{} {} (no fixed version)", f.what, list(&f.cves)),
            "crate" => format!("{} {} fixed in {}", f.what, list(&f.cves), f.version),
            "range" => format!("{} affects {}", list(&f.cves), f.version),
            _ => format!("{} patched by Alpine in {}", list(&f.cves), f.version),
        })
        .collect();
    if v.now.is_empty() { parts.join("; ") } else { format!("{}: {}", v.now, parts.join("; ")) }
}

/// vulns があるときの、どうするかの行 (出たものの種類ごと)
pub fn hints(vs: &[Vuln]) -> Vec<&'static str> {
    let has = |h: &[&str]| vs.iter().any(|v| v.fixes.iter().any(|f| h.contains(&f.how)));
    let mut out = Vec::new();
    if has(&["newer", "patch"]) {
        out.push("vulns: fixed in VER → pkg_edit to VER or newer; patched by Alpine → add the patch from aports (main/NAME or community/NAME) and name the CVE in the PKGBUILD (name it there too when it does not apply to aios)");
    }
    if has(&["crate"]) {
        out.push("vulns: a crate fixed in VER → pkg/rust: cargo_update CRATE@VER in build() (pkg/rust/cross.sh), then pkg_edit rebuild; aios's own Cargo.lock: cargo update -p CRATE; no fixed version, or not in the program (a dev-dependency) → name the RUSTSEC id in the PKGBUILD (or beside the Cargo.lock) and why");
    }
    if has(&["range"]) {
        out.push("vulns: npm → install a version outside the range (npm install -g NAME@latest; claude: ap -Syu) and write it in .aios.json (pkg.linux.npm)");
    }
    out
}

pub fn to_json(vs: &[Vuln]) -> Value {
    json!(
        vs.iter()
            .map(|v| json!({ "name": v.name, "now": v.now, "fixes": v.fixes.iter().map(|f| {
                let mut j = json!({ "how": f.how, "version": f.version, "source": f.source, "cves": f.cves });
                if !f.what.is_empty() {
                    j["crate"] = json!(f.what);
                }
                j
            }).collect::<Vec<_>>() }))
            .collect::<Vec<_>>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fix(how: &'static str, version: &str, source: &str, cves: &[&str]) -> Fix {
        Fix { how, version: version.into(), source: source.into(), cves: cves.iter().map(|c| c.to_string()).collect(), what: String::new() }
    }

    #[test]
    fn open_fixes() {
        let mut s: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut add = |k: &str, v: &[&str]| s.insert(k.into(), v.iter().map(|c| c.to_string()).collect());
        add("0", &["CVE-2020-0001"]);
        add("1.2.5-r0", &["CVE-2024-0002"]);
        add("1.2.6-r0", &["CVE-2024-0005"]);
        add("1.2.6-r1", &["CVE-2026-6042"]);
        add("1.2.6-r2", &["CVE-2026-40200", "CVE-2026-0003"]);
        add("1.2.7_rc1-r0", &["CVE-2026-0006"]);
        add("1.2.8-r0", &["CVE-2026-0004"]);
        // 1.2.6: 1.2.5 と 1.2.6-r0 のものは直っている。1.2.6 のパッチのもの (書いてあるものはのぞく) と、その先のものが出る
        let done: BTreeSet<String> = ["CVE-2026-0003".to_string()].into();
        let o = open("1.2.6", &s, &done);
        let a = |h, v, c| fix(h, v, "alpine", &[c]);
        assert_eq!(o, vec![a("patch", "1.2.6-r1", "CVE-2026-6042"), a("patch", "1.2.6-r2", "CVE-2026-40200"), a("newer", "1.2.7_rc1", "CVE-2026-0006"), a("newer", "1.2.8", "CVE-2026-0004")]);
        // 1.2.7 には 1.2.7_rc1 で直したものが入っている
        assert_eq!(open("1.2.7", &s, &BTreeSet::new()), vec![a("newer", "1.2.8", "CVE-2026-0004")]);
        assert!(open("1.2.8", &s, &BTreeSet::new()).is_empty());
    }

    #[test]
    fn mozilla_advisories() {
        let a = "## mfsa2026-106.yml\nannounced: October 13, 2026\nfixed_in:\n- Firefox 158\n- Firefox ESR 153.5\n- Firefox for iOS 158\ntitle: x\nadvisories:\n  CVE-2026-1001:\n    title: a\n    bugs:\n      - url: 1\n  CVE-2026-1002:\n    title: b\n";
        assert_eq!(mfsa_file(a, "Firefox"), (vec!["158".to_string()], vec!["CVE-2026-1001".to_string(), "CVE-2026-1002".to_string()]));
        let b = "fixed_in:\n- Firefox 157.0.2\n- Firefox 158.0.1\nadvisories:\n  CVE-2026-2001:\n    title: c\n";
        let advs = vec![("mfsa2026-106".to_string(), a.to_string()), ("mfsa2026-112".to_string(), b.to_string())];
        // 158.0 は 158 (の advisory) で直っていて、158.0.1 のものだけ
        assert_eq!(open_mfsa("158.0", "Firefox", &advs, &BTreeSet::new()), vec![fix("newer", "158.0.1", "mfsa2026-112", &["CVE-2026-2001"])]);
        // 157.0.1 にはどちらも。いまより新しいうちのいちばん古い版 (157.0.2)
        assert_eq!(open_mfsa("157.0.1", "Firefox", &advs, &BTreeSet::new()), vec![fix("newer", "157.0.2", "mfsa2026-112", &["CVE-2026-2001"]), fix("newer", "158", "mfsa2026-106", &["CVE-2026-1001", "CVE-2026-1002"])]);
        assert!(open_mfsa("158.0.1", "Firefox", &advs, &BTreeSet::new()).is_empty());
        // ESR だけの advisory は Firefox のものではない
        assert!(mfsa_file("fixed_in:\n- Firefox ESR 140.17\nadvisories:\n  CVE-2026-3001:\n", "Firefox").0.is_empty());
    }

    #[test]
    fn this_year() {
        let y = year_now();
        assert!((2024..2200).contains(&y), "{}", y);
    }

    #[test]
    fn ids_in_text() {
        let c = ids_in("# CVE-2025-26519 (iconv) のパッチは 1.2.6 に入った\nCVE-2026-40200.patch CVE-12 CVE-2021-27219 GHSL-2021-045\n# RUSTSEC-2026-0204 GHSA-35jh-r3h4-6jhm RUSTSEC-12");
        assert_eq!(c.into_iter().collect::<Vec<_>>(), ["CVE-2021-27219", "CVE-2025-26519", "CVE-2026-40200", "GHSA-35jh-r3h4-6jhm", "RUSTSEC-2026-0204"]);
    }

    #[test]
    fn describe_fixes() {
        let v = Vuln { name: "musl".into(), now: "1.2.6".into(), fixes: vec![fix("patch", "1.2.6-r1", "alpine", &["CVE-2026-6042"]), fix("newer", "1.2.7", "alpine", &["CVE-a", "CVE-b"])] };
        assert_eq!(describe(&v), "1.2.6: CVE-2026-6042 patched by Alpine in 1.2.6-r1; CVE-a CVE-b fixed in 1.2.7");
        let f = Vuln { name: "firefox".into(), now: "158.0".into(), fixes: vec![fix("newer", "158.0.1", "mfsa2026-112", &["CVE-1", "CVE-2", "CVE-3", "CVE-4", "CVE-5", "CVE-6"])] };
        assert_eq!(describe(&f), "158.0: CVE-1 CVE-2 CVE-3 CVE-4 and 2 more fixed in 158.0.1 (mfsa2026-112)");
        let mut r = fix("crate", ">= 0.23.46", "rustsec", &["RUSTSEC-1", "RUSTSEC-2"]);
        r.what = "rustls 0.23.45".into();
        let mut n = fix("crate", "", "rustsec", &["RUSTSEC-3"]);
        n.what = "rsa 0.9.10".into();
        assert_eq!(describe(&Vuln { name: "user/Cargo.lock".into(), now: String::new(), fixes: vec![r, n] }), "rustls 0.23.45 RUSTSEC-1 RUSTSEC-2 fixed in >= 0.23.46; rsa 0.9.10 RUSTSEC-3 (no fixed version)");
        assert_eq!(describe(&Vuln { name: "pnpm".into(), now: "12.9.1".into(), fixes: vec![fix("range", "<12.9.2", "npm", &["GHSA-a"])] }), "12.9.1: GHSA-a affects <12.9.2");
    }

    const ADV: &str = "```toml\n[advisory]\nid = \"RUSTSEC-2026-0204\"\npackage = \"crossbeam-epoch\"\ndate = \"2026-09-01\"\ntitle = \"Use after free in `Guard::defer` [x\"\naliases = [\"CVE-2026-1234\", \"GHSA-aaaa-bbbb-cccc\"]\n# a comment with [\n\n[affected.functions]\n\"crossbeam_epoch::Guard::defer\" = [\"< 0.9.20\"]\n\n[versions]\npatched = [\n    \">= 0.9.20, < 0.10.0\",\n    \">= 0.10.2\",\n]\nunaffected = [\"< 0.9.0\"]\n```\n\n# Use after free\n\nThe body [is markdown.\n";

    #[test]
    fn rustsec_advisories() {
        let (krate, a) = rustsec_file(ADV).expect("advisory");
        assert_eq!(krate, "crossbeam-epoch");
        assert_eq!(
            a,
            Adv { id: "RUSTSEC-2026-0204".into(), aliases: vec!["CVE-2026-1234".into(), "GHSA-aaaa-bbbb-cccc".into()], patched: vec![">= 0.9.20, < 0.10.0".into(), ">= 0.10.2".into()], unaffected: vec!["< 0.9.0".into()] }
        );
        // informational と withdrawn、Linux や aarch64 にあてはまらないものは見ない
        let with = |extra: &str| ADV.replacen("date = ", &format!("{}\ndate = ", extra), 1);
        assert!(rustsec_file(&with("informational = \"unsound\"")).is_none());
        assert!(rustsec_file(&with("withdrawn = \"2026-09-02\"")).is_none());
        assert!(rustsec_file(&ADV.replace("[versions]", "[affected]\nos = [\"windows\"]\n\n[versions]")).is_none());
        assert!(rustsec_file(&ADV.replace("[versions]", "[affected]\narch = [\"x86_64\"]\n\n[versions]")).is_none());
        assert!(rustsec_file(&ADV.replace("[versions]", "[affected]\nos = [\"linux\", \"windows\"]\n\n[versions]")).is_some());
    }

    #[test]
    fn rust_locks_open() {
        let lock = "version = 4\n\n[[package]]\nname = \"crossbeam-epoch\"\nversion = \"0.9.18\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"x\"\n\n[[package]]\nname = \"crossbeam-epoch\"\nversion = \"0.8.2\"\nsource = \"sparse+https://index.crates.io/\"\n\n[[package]]\nname = \"crossbeam-epoch\"\nversion = \"0.10.0\"\nsource = \"git+https://github.com/x/y#abc\"\n\n[[package]]\nname = \"fd-find\"\nversion = \"10.5.0\"\ndependencies = [\n \"crossbeam-epoch 0.9.18\",\n]\n";
        let mut crates = lock_crates(lock);
        assert_eq!(crates, [("crossbeam-epoch".to_string(), "0.9.18".to_string()), ("crossbeam-epoch".to_string(), "0.8.2".to_string())]);
        let (k, a) = rustsec_file(ADV).expect("advisory");
        let db: HashMap<String, Vec<Adv>> = [(k, vec![a])].into();
        // 0.8.2 は unaffected (< 0.9.0)
        let mut w = fix("crate", ">= 0.9.20, < 0.10.0 or >= 0.10.2", "rustsec", &["RUSTSEC-2026-0204"]);
        w.what = "crossbeam-epoch 0.9.18".into();
        assert_eq!(open_rust(&crates, &db, &BTreeSet::new()), vec![w]);
        // 番号を書いたもの (aliases の CVE でも) は出ない
        assert!(open_rust(&crates, &db, &["CVE-2026-1234".to_string()].into()).is_empty());
        // cargo_update で上げたもの (同じ semver の中だけ: 0.8.2 は 0.9 にならない)
        raise(&mut crates, "build() {\n  # crossbeam-epoch (RUSTSEC-2026-0204)\n  cargo_update crossbeam-epoch@0.9.21 other@1.0.0\n  musl_build\n}\n");
        assert_eq!(crates[0].1, "0.9.21");
        assert_eq!(crates[1].1, "0.8.2");
        assert!(open_rust(&crates, &db, &BTreeSet::new()).is_empty());
        // 出る前の版は、その版として
        assert!(open_rust(&[("crossbeam-epoch".into(), "0.10.2-rc.1".into())], &db, &BTreeSet::new()).is_empty());
        assert!(!open_rust(&[("crossbeam-epoch".into(), "0.10.0".into())], &db, &BTreeSet::new()).is_empty());
    }

    #[test]
    fn npm_advisories() {
        let ans: Value = serde_json::from_str(r#"{"lodash":[{"id":1106913,"url":"https://github.com/advisories/GHSA-35jh-r3h4-6jhm","vulnerable_versions":"<4.17.21"},{"id":1108258,"url":"https://github.com/advisories/GHSA-29mw-wpgm-hmr9","vulnerable_versions":"<4.17.21"},{"id":7,"url":"","vulnerable_versions":"<=4.17.23"}]}"#).expect("json");
        assert_eq!(open_npm(&ans["lodash"]), vec![fix("range", "<4.17.21", "npm", &["GHSA-35jh-r3h4-6jhm", "GHSA-29mw-wpgm-hmr9"]), fix("range", "<=4.17.23", "npm", &["npm-7"])]);
        assert!(open_npm(&ans["pnpm"]).is_empty());
    }
}

// 中にまるごと入っているライブラリ (静的リンク) の見はり
//   PKGBUILD の build() は c_deps NAME... (meson_cross / cmake_cross も) で repo/aarch64 のパッケージを sysroot に
//   広げてリンクする。静的なライブラリ (.a) はプログラムの中に入るので、ライブラリを新しくしても (セキュリティの直しも)、
//   それを入れたパッケージを作りなおすまで届かない (ncurses なら less procps-ng tmux vim vlc zsh)
//   入っているとみるのは、ライブラリのパッケージに .a があり、作るほうが静的にリンクする (zig_env の -static のまま。
//   shared_env、meson_cross、cmake_cross、-dynamic なら動的) か、ライブラリに .so がないとき
//   作ったときに入れた版は .PKGINFO の makedepend = NAME=VER-REL (zig.sh の c_deps が書きとめ、mkpkg.sh が入れる)。
//   それのない前のパッケージは、作った日 (builddate) がライブラリのものより前なら古いとみる
// 作った Zig も makedepend = zig=ZIG_VERSION-ZIG_REL に残る (zig.sh の zig_env)。pkg/c/zig.sh のいまのものとちがえば
// rebuild に (古い libc.so の見本でリンクしたもののように、作りなおすまで直らない)。書きとめる前に作ったものは
// unrecorded に、作った日の zig.sh の ZIG_VERSION (git の履歴から) ごとに
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// パッケージの中身を覚えておくところ (ファイルの名前・大きさ・時刻ごと)
const CACHE: &str = "build/aish-pkg/embed.json";

/// PKGBUILD の c_deps / meson_cross / cmake_cross に書いたパッケージ。musl は数えない (静的なものには zig が
/// 自分の musl を入れ、動的なものは musl の libc.so を使う)
pub fn deps(text: &str) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    for l in text.lines() {
        let mut w = l.split('#').next().unwrap_or("").split_whitespace();
        if matches!(w.next(), Some("c_deps" | "meson_cross" | "cmake_cross")) {
            for n in w {
                if n != "musl" && !n.starts_with('$') && !v.iter().any(|x| x == n) {
                    v.push(n.to_string());
                }
            }
        }
    }
    v
}

/// Zig で作るパッケージか (zig_env を呼ぶもの。rust の musl_zig も中で呼ぶ)
fn uses_zig(text: &str) -> bool {
    text.lines().map(|l| l.split('#').next().unwrap_or("")).any(|l| l.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).any(|w| w == "zig_env" || w == "musl_zig"))
}

/// いまの Zig: pkg/c/zig.sh の ZIG_VERSION-ZIG_REL (作ったものの makedepend = zig=... とくらべる)
pub fn zig_now(root: &Path) -> Option<String> {
    let t = fs::read_to_string(root.join("pkg/c/zig.sh")).ok()?;
    let get = |k: &str| t.lines().find_map(|l| l.strip_prefix(k)?.strip_prefix('=')).map(|v| v.trim().to_string());
    Some(format!("{}-{}", get("ZIG_VERSION")?, get("ZIG_REL")?))
}

/// pkg/c/zig.sh の ZIG_VERSION が変わった時刻と版 (git の履歴から。古い順)
fn zig_history(root: &Path) -> Vec<(u64, String)> {
    let Ok(o) = Command::new("git").arg("-C").arg(root).args(["log", "--reverse", "--format=@%ct", "-p", "--", "pkg/c/zig.sh"]).stderr(Stdio::null()).output() else { return vec![] };
    let (mut t, mut v) = (0, Vec::new());
    for l in String::from_utf8_lossy(&o.stdout).lines() {
        if let Some(ts) = l.strip_prefix('@').and_then(|s| s.parse::<u64>().ok()) {
            t = ts;
        } else if let Some(ver) = l.strip_prefix("+ZIG_VERSION=") {
            v.push((t, ver.trim().to_string()));
        }
    }
    v
}

/// 静的にリンクするか (zig_env の LDFLAGS の -static のままか)
fn links_static(text: &str) -> bool {
    !text.lines().map(|l| l.split('#').next().unwrap_or("")).any(|l| {
        matches!(l.split_whitespace().next(), Some("shared_env" | "meson_cross" | "cmake_cross")) || l.contains("-dynamic")
    })
}

/// .PKGINFO (mkpkg.sh はいちばん前に置くので、そこまでしか展開しない。ちがえば全部から)
fn pkginfo(file: &Path) -> String {
    if let Ok(mut c) = Command::new("zstd").arg("-dcq").arg(file).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() {
        let mut text = None;
        if let Some(mut o) = c.stdout.take() {
            let mut h = [0u8; 512];
            if o.read_exact(&mut h).is_ok() && h.starts_with(b".PKGINFO\0") {
                let size = std::str::from_utf8(&h[124..136]).ok().and_then(|s| usize::from_str_radix(s.trim_matches(|c: char| c == '\0' || c == ' '), 8).ok()).unwrap_or(0);
                let mut b = vec![0u8; size];
                if o.read_exact(&mut b).is_ok() {
                    text = Some(String::from_utf8_lossy(&b).into_owned());
                }
            }
        }
        let _ = c.kill();
        let _ = c.wait();
        if let Some(t) = text {
            return t;
        }
    }
    Command::new("tar").arg("-I").arg("zstd").arg("-xOf").arg(file).arg(".PKGINFO").output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default()
}

/// ファイルの名前・大きさ・時刻 (作りなおせば変わる)
fn key(file: &Path) -> String {
    let m = fs::metadata(file).ok();
    let t = m.as_ref().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
    format!("{}|{}|{}", file.file_name().unwrap_or_default().to_string_lossy(), m.map_or(0, |m| m.len()), t)
}

/// パッケージについて覚えておくもの
struct Cache {
    root: PathBuf,
    file: PathBuf,
    old: Map<String, Value>,
    new: Map<String, Value>,
}

impl Cache {
    fn load(root: &Path) -> Cache {
        let file = root.join(CACHE);
        let old = fs::read_to_string(&file).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v.as_object().cloned()).unwrap_or_default();
        Cache { root: root.to_path_buf(), file, old, new: Map::new() }
    }

    /// 今回使ったものと、前のもののうちまだ repo/aarch64 にそのまま (同じ大きさと時刻で) あるものを書く
    /// (作りなおしたものや外したものは消える)。前は今回使ったものだけを書いたので、1 つだけ見る embedders のあとは
    /// 1 つしか残らず、次はみな読みなおした
    fn save(&self) {
        let mut out = self.new.clone();
        for (k, v) in &self.old {
            if !out.contains_key(k) && self.current(k) {
                out.insert(k.clone(), v.clone());
            }
        }
        if out != self.old {
            // 書いてから入れかえる (pkg_build がいくつも同時に書いても、こわれたものを読まないように)
            let _ = fs::create_dir_all(self.file.parent().unwrap_or(Path::new(".")));
            let tmp = self.file.with_extension(format!("json.{}", std::process::id()));
            if fs::write(&tmp, Value::Object(out).to_string()).is_ok() {
                let _ = fs::rename(&tmp, &self.file);
            }
        }
    }

    /// k (名前|大きさ|時刻) のファイルが、いまも repo/aarch64 にそのままあるか
    fn current(&self, k: &str) -> bool {
        let name = k.split('|').next().unwrap_or("");
        !name.is_empty() && crate::test::KINDS.iter().map(|kind| self.root.join("repo/aarch64").join(kind).join(name)).any(|p| p.is_file() && key(&p) == k)
    }

    fn get(&mut self, file: &Path) -> &mut Map<String, Value> {
        let k = key(file);
        if !self.new.contains_key(&k) {
            let v = self.old.get(&k).cloned().unwrap_or_else(|| json!({}));
            self.new.insert(k.clone(), v);
        }
        self.new.get_mut(&k).and_then(Value::as_object_mut).expect("an object")
    }

    /// (版 VER-REL, 作った日, 作ったときに入れたもの NAME → VER-REL)
    fn info(&mut self, file: &Path) -> (String, u64, BTreeMap<String, String>) {
        let e = self.get(file);
        if !e.contains_key("ver") {
            let text = pkginfo(file);
            let field = |k: &str| text.lines().filter_map(|l| l.strip_prefix(&format!("{} = ", k))).map(String::from).collect::<Vec<_>>();
            e.insert("ver".into(), json!(field("pkgver").first().cloned().unwrap_or_default()));
            e.insert("date".into(), json!(field("builddate").first().and_then(|d| d.parse::<u64>().ok()).unwrap_or(0)));
            let made: Map<String, Value> = field("makedepend").iter().filter_map(|d| d.split_once('=')).map(|(n, v)| (n.to_string(), json!(v))).collect();
            e.insert("made".into(), Value::Object(made));
        }
        let made = e["made"].as_object().map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string())).collect()).unwrap_or_default();
        (e["ver"].as_str().unwrap_or("").to_string(), e["date"].as_u64().unwrap_or(0), made)
    }

    /// (.a があるか, .so があるか)。中の名前を全部見るので、覚えておく
    fn kinds(&mut self, file: &Path) -> (bool, bool) {
        let e = self.get(file);
        if !e.contains_key("a") {
            let list = Command::new("tar").arg("-I").arg("zstd").arg("-tf").arg(file).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
            let so = regex::Regex::new(r"(^|/)lib[^/]*\.so(\.[0-9]+)*$").expect("regex");
            e.insert("a".into(), json!(list.lines().any(|l| l.ends_with(".a"))));
            e.insert("so".into(), json!(list.lines().any(|l| so.is_match(l))));
        }
        (e["a"] == true, e["so"] == true)
    }

    /// パッケージのライブラリと使うライブラリ (elf::Dynamic)。中を広げて ELF を読むので、覚えておく
    fn dynamic(&mut self, file: &Path) -> crate::elf::Dynamic {
        let root = self.root.clone();
        let e = self.get(file);
        if !e.contains_key("needs")
            && let Some(d) = unpack_dynamic(&root, file)
        {
            e.insert("sonames".into(), json!(d.sonames));
            e.insert("provides".into(), json!(d.provides));
            e.insert("needs".into(), json!(d.needs));
        }
        let list = |k: &str| e.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
        crate::elf::Dynamic { sonames: list("sonames"), provides: list("provides"), needs: list("needs") }
    }
}

/// パッケージを build/aish-pkg/elf.PID/ に広げて elf::dynamic (広げられなければ None。覚えない)
fn unpack_dynamic(root: &Path, file: &Path) -> Option<crate::elf::Dynamic> {
    let dir = root.join(format!("build/aish-pkg/elf.{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    let d = (fs::create_dir_all(&dir).is_ok() && crate::test::untar(file, &dir).is_ok()).then(|| crate::elf::dynamic(&dir, &crate::test::files(file)));
    let _ = fs::remove_dir_all(&dir);
    d
}

/// パッケージ file のライブラリの名前 (elf::Dynamic の provides)。pkg_build が作る前に見ておく (前のものは mkpkg.sh が外す)
pub fn provides(root: &Path, file: &Path) -> Vec<String> {
    let mut cache = Cache::load(root);
    let d = cache.dynamic(file);
    cache.save();
    d.provides
}

/// pkg_build / pkg_fetch のあと: 前のもの (その provides が before) にあって新しいもの new にないライブラリを、NEEDED に
/// 書いたほかのパッケージ (作りなおすまで動かない)。{gone: {LIB: [NAME...]}, rebuild: [NAME...], sonames: [いまの SONAME]}。
/// なくなったものを使うものがなければ None
pub fn gone(root: &Path, name: &str, before: &[String], new: &Path) -> Option<Value> {
    let mut cache = Cache::load(root);
    let now = cache.dynamic(new);
    let lost: Vec<&String> = before.iter().filter(|s| !now.provides.contains(s)).collect();
    let mut users: BTreeMap<String, Vec<String>> = BTreeMap::new();
    if !lost.is_empty() {
        for (n, f) in crate::test::latest(root) {
            if n == name {
                continue;
            }
            let d = cache.dynamic(&f);
            for l in &lost {
                if d.needs.contains(l) {
                    users.entry((*l).clone()).or_default().push(n.clone());
                }
            }
        }
    }
    cache.save();
    if users.is_empty() {
        return None;
    }
    let rebuild: std::collections::BTreeSet<&String> = users.values().flatten().collect();
    Some(json!({ "gone": users, "rebuild": rebuild, "sonames": now.sonames }))
}

/// repo/aarch64 のパッケージみなの elf::Dynamic (aish-pkg scan の needs)
pub fn dynamics(root: &Path) -> Vec<(String, crate::elf::Dynamic)> {
    let mut cache = Cache::load(root);
    let v = crate::test::latest(root).into_iter().map(|(n, f)| (n, cache.dynamic(&f))).collect();
    cache.save();
    v
}

/// 古いライブラリが入ったままのパッケージ: NAME → [(ライブラリ, 作ったときの版 (わからなければ ""), いまの版)]
pub fn stale(root: &Path) -> BTreeMap<String, Vec<(String, String, String)>> {
    let mut cache = Cache::load(root);
    let mut out: BTreeMap<String, Vec<(String, String, String)>> = BTreeMap::new();
    let zig = zig_now(root);
    for (name, path) in crate::up::pkgbuilds(&root.join("pkg")) {
        let text = fs::read_to_string(&path).unwrap_or_default();
        let ds = deps(&text);
        let z = uses_zig(&text);
        if ds.is_empty() && !z {
            continue;
        }
        let Some(pf) = crate::test::find(root, &name) else { continue };
        let (_, date, made) = cache.info(&pf);
        // 作った Zig (書きとめのないものは unrecorded)
        if z
            && let (Some(now), Some(was)) = (&zig, made.get("zig"))
            && was != now
        {
            out.entry(name.clone()).or_default().push(("zig".into(), was.clone(), now.clone()));
        }
        let stat = links_static(&text);
        for d in ds {
            let Some(df) = crate::test::find(root, &d) else { continue };
            let (a, so) = cache.kinds(&df);
            if !a || (so && !stat) {
                continue;
            }
            let (now, ddate, _) = cache.info(&df);
            let was = match made.get(&d) {
                Some(v) if *v != now => Some(v.clone()),
                None if ddate > date => Some(String::new()),
                _ => None,
            };
            if let Some(w) = was {
                out.entry(name.clone()).or_default().push((d, w, now));
            }
        }
    }
    cache.save();
    out
}

/// Zig で作るのに makedepend = zig=... のないパッケージ (書きとめる前に作ったもの): 作った日の zig.sh の ZIG_VERSION
/// (git の履歴から。わからなければ "") → 名前。作りなおせば書きとめがつく
pub fn unrecorded(root: &Path) -> BTreeMap<String, Vec<String>> {
    let hist = zig_history(root);
    let mut cache = Cache::load(root);
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, path) in crate::up::pkgbuilds(&root.join("pkg")) {
        if !uses_zig(&fs::read_to_string(&path).unwrap_or_default()) {
            continue;
        }
        let Some(pf) = crate::test::find(root, &name) else { continue };
        let (_, date, made) = cache.info(&pf);
        if !made.contains_key("zig") {
            let ver = hist.iter().rev().find(|(t, _)| *t <= date).map(|(_, v)| v.clone()).unwrap_or_default();
            out.entry(ver).or_default().push(name);
        }
    }
    cache.save();
    out
}

/// LIB を中に入れるパッケージ (LIB がまだ作っていなければ、c_deps に書いたもの全部)
pub fn embedders(root: &Path, lib: &str) -> Vec<String> {
    let mut cache = Cache::load(root);
    let (a, so) = crate::test::find(root, lib).map_or((true, false), |f| cache.kinds(&f));
    cache.save();
    if !a {
        return vec![];
    }
    crate::up::pkgbuilds(&root.join("pkg"))
        .into_iter()
        .filter_map(|(n, p)| {
            let text = fs::read_to_string(&p).ok()?;
            (deps(&text).iter().any(|d| d == lib) && (!so || links_static(&text))).then_some(n)
        })
        .collect()
}

/// 答えの JSON: [{name, libs: [{name, was, now}]}] (was が "" なら、作った日のほうが前)
pub fn to_json(st: &BTreeMap<String, Vec<(String, String, String)>>) -> Value {
    json!(st.iter().map(|(n, libs)| json!({ "name": n, "libs": libs.iter().map(|(l, w, v)| json!({ "name": l, "was": w, "now": v })).collect::<Vec<_>>() })).collect::<Vec<_>>())
}

/// 1 行の説明: ncurses 6.6-1 → 6.6-2 (作ったときの版がわからなければ: ncurses 6.6-2 is newer)
pub fn describe(libs: &[(String, String, String)]) -> String {
    libs.iter().map(|(l, w, v)| if w.is_empty() { format!("{} {} is newer", l, v) } else { format!("{} {} → {}", l, w, v) }).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deps_from_pkgbuild() {
        let t = "build() {\n  # c_deps old (comment)\n  c_deps zlib ffmpeg  ncurses # static\n  meson_cross zlib libpng\n  cmake_cross\n}\n";
        assert_eq!(deps(t), ["zlib", "ffmpeg", "ncurses", "libpng"]);
        assert_eq!(deps("  c_deps musl $extra pcre2\n"), ["pcre2"]);
        // zig.sh の関数の定義 (c_deps() {) は数えない
        assert!(deps("c_deps() {\n").is_empty());
    }

    #[test]
    fn static_or_dynamic() {
        assert!(links_static("  zig_env\n  c_deps ncurses\n  ./configure\n"));
        assert!(!links_static("  c_deps zlib\n  shared_env\n"));
        assert!(!links_static("  meson_cross zlib\n"));
        assert!(!links_static("  LDFLAGS=\"-dynamic -Wl,-s\" ./configure\n"));
        // コメントの中のものは数えない
        assert!(links_static("  # shared_env はいらない (-dynamic にしない)\n  c_deps ncurses\n"));
    }

    #[test]
    fn built_with_zig() {
        assert!(uses_zig("build() {\n  . \"$startdir/../zig.sh\"; zig_env\n}\n"));
        assert!(uses_zig("  musl_zig || return 1\n"));
        assert!(!uses_zig("  # zig_env はいらない\n  cargo build\n"));
        assert!(!uses_zig("  zig_fix_libc \"$pkgdir/opt/zig/lib\"\n"));
    }

    #[test]
    fn describe_lines() {
        let l = vec![("ncurses".to_string(), "6.6-1".to_string(), "6.6-2".to_string()), ("zlib".to_string(), String::new(), "1.3.1-2".to_string())];
        assert_eq!(describe(&l), "ncurses 6.6-1 → 6.6-2, zlib 1.3.1-2 is newer");
    }
}

// 中にまるごと入っているライブラリ (静的リンク) の見はり
//   PKGBUILD の build() は c_deps NAME... (meson_cross / cmake_cross も) で repo/aarch64 のパッケージを sysroot に
//   広げてリンクする。静的なライブラリ (.a) はプログラムの中に入るので、ライブラリを新しくしても (セキュリティの直しも)、
//   それを入れたパッケージを作りなおすまで届かない (ncurses なら less procps-ng tmux vim vlc zsh)
//   入っているとみるのは、ライブラリのパッケージに .a があり、作るほうが静的にリンクする (zig_env の -static のまま。
//   shared_env、meson_cross、cmake_cross、-dynamic なら動的) か、ライブラリに .so がないとき
//   作ったときに入れた版は .PKGINFO の makedepend = NAME=VER-REL (zig.sh の c_deps が書きとめ、mkpkg.sh が入れる)。
//   それのない前のパッケージは、作った日 (builddate) がライブラリのものより前なら古いとみる
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
    file: PathBuf,
    old: Map<String, Value>,
    new: Map<String, Value>,
}

impl Cache {
    fn load(root: &Path) -> Cache {
        let file = root.join(CACHE);
        let old = fs::read_to_string(&file).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v.as_object().cloned()).unwrap_or_default();
        Cache { file, old, new: Map::new() }
    }

    /// 今回使ったものだけ書く (古いパッケージのものは消える)
    fn save(&self) {
        if self.new != self.old {
            let _ = fs::create_dir_all(self.file.parent().unwrap_or(Path::new(".")));
            let _ = fs::write(&self.file, Value::Object(self.new.clone()).to_string());
        }
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
}

/// 古いライブラリが入ったままのパッケージ: NAME → [(ライブラリ, 作ったときの版 (わからなければ ""), いまの版)]
pub fn stale(root: &Path) -> BTreeMap<String, Vec<(String, String, String)>> {
    let mut cache = Cache::load(root);
    let mut out: BTreeMap<String, Vec<(String, String, String)>> = BTreeMap::new();
    for (name, path) in crate::up::pkgbuilds(&root.join("pkg")) {
        let text = fs::read_to_string(&path).unwrap_or_default();
        let ds = deps(&text);
        if ds.is_empty() {
            continue;
        }
        let Some(pf) = crate::test::find(root, &name) else { continue };
        let (_, date, made) = cache.info(&pf);
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
    fn describe_lines() {
        let l = vec![("ncurses".to_string(), "6.6-1".to_string(), "6.6-2".to_string()), ("zlib".to_string(), String::new(), "1.3.1-2".to_string())];
        assert_eq!(describe(&l), "ncurses 6.6-1 → 6.6-2, zlib 1.3.1-2 is newer");
    }
}

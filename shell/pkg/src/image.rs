// pkg_image: リポジトリの中の ghcr.io の image (docker/ の Dockerfile と compose.yml、image/、.github/workflows/) の
// タグと digest を、ghcr.io のいちばん新しいもの (latest と同じ digest のタグ) とくらべ、update で書きかえる。
// 前は image を出しなおすたびに、ghcr.io の token と manifest を curl で見て、ファイルを 1 つずつ書きかえていた。
// docker/Dockerfile だけ古い builder (r940) のままになっていた (2026-11-13)
//   ok        いちばん新しいタグと digest
//   old       タグが古い
//   moved     タグは同じで、digest がちがう (タグを出しなおした)
//   unpinned  digest がない (update でも書きかえない。pin で digest をつける)
use aish_plugin::{Value, json, s};
use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};

const ACCEPT: &str = "application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json";
const HOST: &str = "ghcr.io/";
/// latest と同じ digest のタグを探すとき、新しいものから見るのはここまで
const MAX_HEADS: usize = 8;

/// ファイルの中の image の書き方 (ghcr.io/REPO[:TAG][@sha256:DIGEST])
#[derive(Debug, Clone, PartialEq)]
pub struct Ref {
    pub text: String,
    pub repo: String,
    pub tag: String,
    pub digest: String,
}

pub fn refs(t: &str) -> Vec<Ref> {
    let mut out = Vec::new();
    let mut rest = t;
    while let Some(i) = rest.find(HOST) {
        let after = &rest[i + HOST.len()..];
        let repo_len = after.find(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit() || "._/-".contains(c))).unwrap_or(after.len());
        let repo = after[..repo_len].trim_end_matches(['/', '.', '-']).to_string();
        let mut n = repo.len();
        let mut tag = String::new();
        if after[n..].starts_with(':') {
            let t = &after[n + 1..];
            let l = t.find(|c: char| !(c.is_ascii_alphanumeric() || "._-".contains(c))).unwrap_or(t.len());
            tag = t[..l].to_string();
            n += 1 + l;
        }
        let mut digest = String::new();
        if let Some(d) = after[n..].strip_prefix("@sha256:") {
            let l = d.find(|c: char| !c.is_ascii_hexdigit()).unwrap_or(d.len());
            if l == 64 {
                digest = format!("sha256:{}", &d[..64]);
                n += 8 + 64;
            }
        }
        if !repo.is_empty() && (!tag.is_empty() || !digest.is_empty()) {
            out.push(Ref { text: format!("{}{}", HOST, &after[..n]), repo, tag, digest });
        }
        rest = &after[n.max(1).min(after.len())..];
    }
    out
}

/// タグの新しい順 (数字のところは数でくらべる。数字で始まるものを先に)
fn version_key(t: &str) -> (bool, Vec<(u64, String)>) {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut digit = None;
    for c in t.chars() {
        let d = c.is_ascii_digit();
        if digit.is_some_and(|x| x != d) {
            parts.push(std::mem::take(&mut cur));
        }
        digit = Some(d);
        cur.push(c);
    }
    parts.push(cur);
    let key = parts.iter().filter(|p| !p.chars().all(|c| ".-_".contains(c))).map(|p| p.parse::<u64>().map_or((0, p.clone()), |n| (n, String::new()))).collect();
    (t.starts_with(|c: char| c.is_ascii_digit()), key)
}

fn newest_first(tags: &mut [String]) {
    tags.sort_by_key(|t| std::cmp::Reverse(version_key(t)));
}

fn curl(args: &[&str]) -> Result<String, String> {
    let o = Command::new("curl").args(["-fsS", "-m", "20"]).args(args).stdin(Stdio::null()).output().map_err(|e| format!("curl: {}", e))?;
    if !o.status.success() {
        return Err(String::from_utf8_lossy(&o.stderr).trim().to_string());
    }
    Ok(String::from_utf8_lossy(&o.stdout).to_string())
}

/// ghcr.io のいちばん新しいもの: (タグ, digest, latest と同じか, タグの数)
type Newest = Result<(String, String, bool, usize), String>;

fn newest(repo: &str) -> Newest {
    let tok = curl(&[&format!("https://ghcr.io/token?scope=repository:{}:pull", repo)])?;
    let tok = aish_plugin::parse(&tok).map(|v| s(&v, "token").to_string()).unwrap_or_default();
    if tok.is_empty() {
        return Err("no token (not public?)".into());
    }
    let auth = format!("Authorization: Bearer {}", tok);
    let accept = format!("Accept: {}", ACCEPT);
    let head = |tag: &str| -> Option<String> {
        let h = curl(&["-I", "-H", &auth, "-H", &accept, &format!("https://ghcr.io/v2/{}/manifests/{}", repo, tag)]).ok()?;
        h.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("docker-content-digest").then(|| v.trim().to_string())
        })
    };
    let list = curl(&["-H", &auth, &format!("https://ghcr.io/v2/{}/tags/list?n=1000", repo)])?;
    let mut tags: Vec<String> = aish_plugin::parse(&list).and_then(|v| v["tags"].as_array().map(|t| t.iter().filter_map(|x| x.as_str().map(String::from)).collect())).unwrap_or_default();
    let has_latest = tags.iter().any(|t| t == "latest");
    tags.retain(|t| t != "latest");
    newest_first(&mut tags);
    let n = tags.len();
    if has_latest {
        let want = head("latest").ok_or("cannot read latest")?;
        for t in tags.iter().take(MAX_HEADS) {
            if head(t).as_deref() == Some(want.as_str()) {
                return Ok((t.clone(), want, true, n));
            }
        }
        if tags.is_empty() {
            return Ok(("latest".into(), want, true, n));
        }
    }
    let t = tags.first().ok_or("no tags")?.clone();
    let d = head(&t).ok_or(format!("cannot read {}", t))?;
    Ok((t, d, false, n))
}

fn short(d: &str) -> &str {
    let d = d.strip_prefix("sha256:").unwrap_or(d);
    &d[..d.len().min(12)]
}

pub fn image(root: &Path, a: &Value) -> Value {
    let dirs = ["docker", "image", ".github/workflows"];
    let files: Vec<String> = match a["files"].as_array() {
        Some(f) => f.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
        None => {
            let o = Command::new("git").arg("-C").arg(root).arg("ls-files").args(dirs).output();
            o.map(|o| String::from_utf8_lossy(&o.stdout).lines().map(String::from).collect()).unwrap_or_default()
        }
    };
    let names: Vec<&str> = a["names"].as_array().map(|x| x.iter().filter_map(|n| n.as_str()).collect()).unwrap_or_default();
    let want = |repo: &str| names.is_empty() || names.iter().any(|n| repo == *n || repo.rsplit('/').next() == Some(n));
    // repo ごとに (ファイル, Ref)
    let mut by: BTreeMap<String, Vec<(String, Ref)>> = BTreeMap::new();
    for f in &files {
        let Ok(t) = std::fs::read_to_string(root.join(f)) else { continue };
        for r in refs(&t) {
            if want(&r.repo) {
                by.entry(r.repo.clone()).or_default().push((f.clone(), r));
            }
        }
    }
    if by.is_empty() {
        return json!({ "text": format!("no ghcr.io images with a tag or digest in {}\n", if a["files"].is_array() { "files".to_string() } else { dirs.join(" ") }), "ok": true });
    }
    // ghcr.io は repo ごとにいっしょに見る
    let found: Vec<(String, Newest)> = std::thread::scope(|sc| {
        let hs: Vec<_> = by.keys().map(|r| (r.clone(), sc.spawn(move || newest(r)))).collect();
        hs.into_iter().map(|(r, h)| (r, h.join().unwrap_or_else(|_| Err("panicked".into())))).collect()
    });
    let update = a["update"] == true;
    let pin = a["pin"] == true;
    let mut text = String::new();
    let (mut stale, mut errors) = (0, Vec::new());
    let mut edits: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    for (repo, res) in &found {
        let (tag, dig, latest, n) = match res {
            Ok(x) => x,
            Err(e) => {
                errors.push(format!("{}: {}", repo, e));
                text.push_str(&format!("{}: {}\n", repo, e));
                continue;
            }
        };
        text.push_str(&format!("{}: newest {}@{}{} ({} tags)\n", repo, tag, short(dig), if *latest { " = latest" } else { "" }, n));
        let mut seen = std::collections::HashSet::new();
        for (f, r) in &by[repo] {
            if !seen.insert((f.clone(), r.text.clone())) {
                continue;
            }
            let state = if r.digest.is_empty() {
                "unpinned"
            } else if r.tag == *tag && r.digest == *dig {
                "ok"
            } else if r.tag == *tag || r.tag.is_empty() {
                "moved"
            } else {
                "old"
            };
            let now = format!("{}{}", if r.tag.is_empty() { "-".into() } else { r.tag.clone() }, if r.digest.is_empty() { String::new() } else { format!("@{}", short(&r.digest)) });
            text.push_str(&format!("  {}: {} {}\n", f, now, state));
            if state != "ok" {
                stale += 1;
                if update && (state != "unpinned" || pin) {
                    edits.entry(f.clone()).or_default().push((r.text.clone(), format!("{}{}:{}@{}", HOST, repo, tag, dig)));
                }
            }
        }
    }
    let mut changed = Vec::new();
    for (f, reps) in &edits {
        let p = root.join(f);
        let Ok(mut t) = std::fs::read_to_string(&p) else { continue };
        let before = t.clone();
        for (old, new) in reps {
            // 同じ ref の前に :TAG だけのもの (digest のないもの) がつながっていないところだけ
            t = replace_ref(&t, old, new);
        }
        if t != before && std::fs::write(&p, &t).is_ok() {
            changed.push(f.clone());
        }
    }
    if !changed.is_empty() {
        text.push_str(&format!("updated: {}\n", changed.join(" ")));
    } else if stale > 0 && !update {
        text.push_str("update: true rewrites old and moved pins to the newest tag and digest (pin: true also pins unpinned ones)\n");
    }
    let mut r = json!({ "text": text, "ok": stale == 0 && errors.is_empty(), "stale": stale, "updated": changed });
    if !errors.is_empty() {
        r["errors"] = json!(errors);
    }
    r
}

/// old をそのまま new に。ただし old のすぐあとに ref の続き (:TAG や @sha256:) があるところは別の ref なので残す
fn replace_ref(t: &str, old: &str, new: &str) -> String {
    let mut out = String::with_capacity(t.len());
    let mut rest = t;
    while let Some(i) = rest.find(old) {
        let after = &rest[i + old.len()..];
        out.push_str(&rest[..i]);
        if after.starts_with(|c: char| c == '@' || c == ':' || c.is_ascii_alphanumeric() || "._-".contains(c)) {
            out.push_str(old);
        } else {
            out.push_str(new);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: &str = "6039d9e67630100421bcb1b1b0e769df59fe251ce34237b07896e3da9e878231";

    #[test]
    fn parse() {
        let t = format!("FROM ghcr.io/syui/aios-static:20261105@sha256:{} AS run\n  image: ghcr.io/syui/aios-redis:8.10.3\n  ghcr.io/syui/aios:${{{{ x }}}}\nghcr.io/syui/aios@sha256:{}", D, D);
        let r = refs(&t);
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].repo, "syui/aios-static");
        assert_eq!(r[0].tag, "20261105");
        assert_eq!(r[0].digest, format!("sha256:{}", D));
        assert_eq!(r[0].text, format!("ghcr.io/syui/aios-static:20261105@sha256:{}", D));
        assert_eq!((r[1].tag.as_str(), r[1].digest.as_str()), ("8.10.3", ""));
        assert_eq!((r[2].repo.as_str(), r[2].tag.as_str()), ("syui/aios", ""));
    }

    #[test]
    fn order() {
        let mut t: Vec<String> = ["0.0.1.r940.g4d08978", "ci", "0.0.1.r955.gfb57887", "0.0.1.r99.gaaaaaaa", "26.13.1", "26.9.0"].iter().map(|s| s.to_string()).collect();
        newest_first(&mut t);
        assert_eq!(t[0], "26.13.1");
        let aios: Vec<&String> = t.iter().filter(|x| x.starts_with("0.0.1")).collect();
        assert_eq!(aios, ["0.0.1.r955.gfb57887", "0.0.1.r940.g4d08978", "0.0.1.r99.gaaaaaaa"]);
        assert_eq!(t.last().unwrap(), "ci");
    }

    #[test]
    fn replace() {
        let t = "a ghcr.io/x/y:1 b ghcr.io/x/y:1@sha256:00 c ghcr.io/x/y:12\n";
        assert_eq!(replace_ref(t, "ghcr.io/x/y:1", "N"), "a N b ghcr.io/x/y:1@sha256:00 c ghcr.io/x/y:12\n");
    }
}

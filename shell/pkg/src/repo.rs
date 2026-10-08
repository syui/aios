// パッケージを作って (build)、ai/repo に送る (push)
//   repo/aarch64/{rust,c,shell,desktop}/ が ai/repo の aarch64/ の写し + ここで作ったもの。
//   build: なければ ai/repo からそろえ、bin/mkpkg.sh で作って、同じパッケージの古い版を外し、aios.db を作りなおす
//   push:  ai/repo の最新とくらべて、変わるもの (足す・上げる) を見せてから bin/gitea.sh repo (署名つきの 1 コミット)。
//          ai/repo のほうが新しいもの (ほかで送ったもの) を消したり下げたりするときは止まる (force で送る)
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const AIREPO: &str = "https://git.syui.ai/ai/repo.git";
const KINDS: [&str; 4] = ["rust", "c", "shell", "desktop"];

fn sh(cmd: &mut Command) -> Result<(), String> {
    let st = cmd.status().map_err(|e| e.to_string())?;
    if st.success() { Ok(()) } else { Err(format!("{:?}: {}", cmd, st)) }
}

/// build/airepo を ai/repo の main の最新に (浅い clone)
fn sync_cache(root: &Path) -> Result<PathBuf, String> {
    let dir = root.join("build/airepo");
    if dir.join(".git").is_dir() {
        sh(Command::new("git").arg("-C").arg(&dir).args(["fetch", "-q", "--depth", "1", "origin", "main"]))?;
        sh(Command::new("git").arg("-C").arg(&dir).args(["reset", "-q", "--hard", "FETCH_HEAD"]))?;
    } else {
        let _ = fs::create_dir_all(root.join("build"));
        sh(Command::new("git").args(["clone", "-q", "--depth", "1", "-b", "main", AIREPO]).arg(&dir))?;
    }
    Ok(dir)
}

/// NAME-VER-REL-ARCH.pkg.tar.zst → (NAME, VER-REL)
pub fn parse(file: &str) -> Option<(String, String)> {
    let base = file.strip_suffix(".pkg.tar.zst")?;
    let mut p: Vec<&str> = base.rsplitn(4, '-').collect();
    if p.len() != 4 {
        return None;
    }
    p.reverse();
    Some((p[0].to_string(), format!("{}-{}", p[1], p[2])))
}

/// DIR の下の種類ごとのパッケージ: (種類, 名前) → (版, ファイル名)
fn packages(dir: &Path) -> BTreeMap<(String, String), (String, String)> {
    let mut m = BTreeMap::new();
    for k in KINDS {
        for e in fs::read_dir(dir.join(k)).into_iter().flatten().flatten() {
            let f = e.file_name().to_string_lossy().into_owned();
            if let Some((n, v)) = parse(&f) {
                m.insert((k.to_string(), n), (v, f));
            }
        }
    }
    m
}

/// build NAME: repo/aarch64/KIND/ に作り、古い版を外して aios.db を作りなおす
pub fn build(root: &Path, name: &str) -> Result<Value, String> {
    let (_, path) = crate::up::pkgbuilds(&root.join("pkg")).into_iter().find(|(n, _)| n == name).ok_or_else(|| format!("{}: no PKGBUILD", name))?;
    let dir = path.parent().unwrap_or(Path::new("."));
    let kind = dir.parent().and_then(|d| d.file_name()).map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
    // pkgver() は git rev-list --count を使う。浅いクローン (クラウドのセッション) だと小さな数になって、
    // ai/repo のものより古い版に見えてしまうので、先に履歴を全部取ってくる
    let shallow = Command::new("git").arg("-C").arg(root).args(["rev-parse", "--is-shallow-repository"]).output().map(|o| String::from_utf8_lossy(&o.stdout).trim() == "true").unwrap_or(false);
    if shallow {
        sh(Command::new("git").arg("-C").arg(root).args(["fetch", "-q", "--unshallow", "origin"]))?;
    }
    let local = root.join("repo/aarch64");
    if !local.is_dir() {
        // はじめて: ai/repo の aarch64/ をそろえてから (送るときに、ほかのパッケージが消えないように)
        let cache = sync_cache(root)?;
        let _ = fs::create_dir_all(root.join("repo"));
        sh(Command::new("cp").arg("-a").arg(cache.join("aarch64")).arg(&local))?;
    }
    sh(Command::new("bin/mkpkg.sh").arg(dir.strip_prefix(root).unwrap_or(dir)).current_dir(root))?;
    // できたもの: いちばん新しい NAME-*.pkg.tar.zst
    let kdir = local.join(&kind);
    let mut mine: Vec<(std::time::SystemTime, String)> = fs::read_dir(&kdir)
        .map_err(|e| format!("{}: {}", kdir.display(), e))?
        .flatten()
        .filter_map(|e| {
            let f = e.file_name().to_string_lossy().into_owned();
            if parse(&f)?.0 != name {
                return None;
            }
            Some((e.metadata().ok()?.modified().ok()?, f))
        })
        .collect();
    mine.sort();
    let (_, file) = mine.pop().ok_or("no package was made")?;
    let removed: Vec<String> = mine.into_iter().map(|(_, f)| f).collect();
    for f in &removed {
        let _ = fs::remove_file(kdir.join(f));
    }
    sh(Command::new("bin/mkrepo.sh").arg(kdir.strip_prefix(root).unwrap_or(&kdir)).current_dir(root))?;
    // .aios.json に新しいパッケージを足す (版がちがえば直す)
    let aios_json = crate::up::aios_sync(root).unwrap_or_default();
    Ok(json!({ "name": name, "kind": kind, "file": file, "removed": removed, "aios_json": aios_json }))
}

/// push: ai/repo とくらべて、bin/gitea.sh repo で送る
pub fn push(root: &Path, force: bool) -> Result<Value, String> {
    let local = root.join("repo/aarch64");
    if !local.is_dir() {
        return Err("repo/aarch64: nothing built yet (pkg_build)".into());
    }
    let cache = sync_cache(root)?;
    let (theirs, ours) = (packages(&cache.join("aarch64")), packages(&local));
    let mut changes = Vec::new();
    let mut conflicts = Vec::new();
    for (key, (v, _)) in &ours {
        match theirs.get(key) {
            None => changes.push(format!("{}/{} {} (new)", key.0, key.1, v)),
            Some((tv, _)) if tv == v => {}
            Some((tv, _)) if crate::up::vercmp(v, tv).is_gt() => changes.push(format!("{}/{} {} → {}", key.0, key.1, tv, v)),
            Some((tv, _)) => conflicts.push(format!("{}/{} ai/repo has {}, here {} (older)", key.0, key.1, tv, v)),
        }
    }
    for (key, (tv, _)) in &theirs {
        if !ours.contains_key(key) {
            conflicts.push(format!("{}/{} {} is in ai/repo but not here (would be removed)", key.0, key.1, tv));
        }
    }
    // 変わるもののうち、pkg_test で確かめていないもの
    let untested: Vec<String> = ours
        .iter()
        .filter(|(key, (v, _))| theirs.get(*key).is_none_or(|(tv, _)| tv != v))
        .filter(|(_, (_, f))| !crate::test::stamp(root, f).exists())
        .map(|(key, (v, _))| format!("{}/{} {}", key.0, key.1, v))
        .collect();
    let mut r = json!({ "changes": changes, "conflicts": conflicts, "untested": untested });
    if changes.is_empty() && conflicts.is_empty() {
        r["text"] = json!("ai/repo is up to date\n");
        return Ok(r);
    }
    if !untested.is_empty() && !force {
        return Err(format!("not tested: {} (pkg_test first, or force)", untested.join("; ")));
    }
    if !conflicts.is_empty() && !force {
        return Err(format!("ai/repo is newer: {} (bring them into repo/aarch64, or force)", conflicts.join("; ")));
    }
    sh(Command::new("bin/gitea.sh").arg("repo").current_dir(root))?;
    let head = String::from_utf8_lossy(&Command::new("git").args(["ls-remote", AIREPO, "refs/heads/main"]).output().map_err(|e| e.to_string())?.stdout).split_whitespace().next().unwrap_or("").to_string();
    r["head"] = json!(head);
    r["text"] = json!(format!("{}\nai/repo: {}\n", changes.iter().chain(conflicts.iter()).cloned().collect::<Vec<_>>().join("\n"), &head[..head.len().min(7)]));
    Ok(r)
}

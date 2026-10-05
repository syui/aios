// mkpkg: PKGBUILD から pacman と同じ形のパッケージ (NAME-VER-REL-ARCH.pkg.tar.zst) を作る (aios build pkg)。
// bin/mkpkg.sh と同じことを aios の中でする。シェルの関数 (pkgver prepare build package) は bash (brush) で動かし、
// ソースを取ってくる、sha256 を確かめる、.PKGINFO を書く、tar と zstd にまとめるのはここ (Rust) でする
// (aios には GNU tar や zstd のコマンドがない)。
//   source: git+URL (#tag= #commit= #branch=)、http(s) (fetch で取る)、PKGBUILD の横のファイル。NAME::URL で名前
//   sha256sums: SKIP 以外は確かめる。install=: パッケージの .INSTALL になる
//   作業の場所は ~/.cache/aios/build/NAME (src/ と pkg/)
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

const CARCH: &str = "aarch64";

/// PKGBUILD の変数 (配列も文字列の並び)
type Vars = BTreeMap<String, Vec<String>>;

const SCALARS: &[&str] = &["pkgname", "pkgver", "pkgrel", "epoch", "pkgdesc", "url", "install"];
const ARRAYS: &[&str] = &["arch", "license", "depends", "optdepends", "makedepends", "provides", "conflicts", "replaces", "backup", "source", "sha256sums"];

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// PKGBUILD を読んで変数を NUL で区切って出す (名前 NUL 値 NUL ...)
fn read_vars(startdir: &Path) -> Result<Vars, String> {
    let mut script = String::from("set -e\nsource ./PKGBUILD\n");
    for k in SCALARS {
        script.push_str(&format!("printf '%s\\0%s\\0' {k} \"${{{k}:-}}\"\n"));
    }
    for k in ARRAYS {
        script.push_str(&format!("for v in \"${{{k}[@]}}\"; do printf '%s\\0%s\\0' {k} \"$v\"; done\n"));
    }
    let out = Command::new("bash").arg("-c").arg(&script).current_dir(startdir).output().map_err(|e| format!("bash: {}", e))?;
    if !out.status.success() {
        return Err(format!("PKGBUILD: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let mut vars = Vars::new();
    let parts: Vec<&[u8]> = out.stdout.split(|&b| b == 0).collect();
    for kv in parts.chunks(2) {
        if let [k, v] = kv {
            let v = String::from_utf8_lossy(v).into_owned();
            let e = vars.entry(String::from_utf8_lossy(k).into_owned()).or_default();
            if !v.is_empty() || ARRAYS.contains(&std::str::from_utf8(k).unwrap_or("")) {
                e.push(v);
            }
        }
    }
    Ok(vars)
}

fn one<'a>(v: &'a Vars, k: &str) -> &'a str {
    v.get(k).and_then(|a| a.first()).map(String::as_str).unwrap_or("")
}

fn sha256(path: &Path) -> Result<String, String> {
    let mut f = std::fs::File::open(path).map_err(|e| format!("{}: {}", path.display(), e))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{:02x}", b)).collect())
}

fn run(cmd: &mut Command) -> Result<(), String> {
    let st = cmd.status().map_err(|e| format!("{:?}: {}", cmd.get_program(), e))?;
    if st.success() { Ok(()) } else { Err(format!("{:?} failed ({})", cmd.get_program(), st)) }
}

/// URL を FILE に (aios は fetch、ほかは curl)
fn download(url: &str, file: &Path) -> Result<(), String> {
    let part = file.with_extension("part");
    let have_fetch = Command::new("fetch").arg("--help").output().is_ok();
    if have_fetch {
        run(Command::new("fetch").arg(url).arg("-o").arg(&part))?;
    } else {
        run(Command::new("curl").args(["-fL", "-o"]).arg(&part).arg(url))?;
    }
    std::fs::rename(&part, file).map_err(|e| e.to_string())
}

fn fetch_git(dir: &Path, url: &str, frag: &str) -> Result<(), String> {
    let r = match frag.split_once('=') {
        Some(("tag", t)) => format!("refs/tags/{}", t),
        Some(("commit", c)) => c.to_string(),
        Some(("branch", b)) => format!("refs/heads/{}", b),
        None if frag.is_empty() => "HEAD".into(),
        _ => return Err(format!("unknown fragment #{}", frag)),
    };
    if !dir.join(".git").exists() {
        run(Command::new("git").args(["init", "-q"]).arg(dir))?;
    }
    run(Command::new("git").arg("-C").arg(dir).args(["fetch", "-q", "--depth", "1", url, &r]))?;
    run(Command::new("git").arg("-C").arg(dir).args(["checkout", "-q", "--force", "FETCH_HEAD"]))
}

fn sources(v: &Vars, startdir: &Path, srcdir: &Path) -> Result<(), String> {
    let empty = Vec::new();
    let sums = v.get("sha256sums").unwrap_or(&empty);
    for (i, s) in v.get("source").unwrap_or(&empty).iter().enumerate() {
        let want = sums.get(i).map(String::as_str).unwrap_or("SKIP");
        let (name, src) = match s.split_once("::") {
            Some((n, u)) => (Some(n.to_string()), u),
            None => (None, s.as_str()),
        };
        if let Some(u) = src.strip_prefix("git+") {
            let (u, frag) = u.split_once('#').unwrap_or((u, ""));
            let name = name.unwrap_or_else(|| u.rsplit('/').next().unwrap_or(u).trim_end_matches(".git").to_string());
            println!("==> {} {}", u, frag);
            fetch_git(&srcdir.join(&name), u, frag)?;
        } else if src.contains("://") {
            let name = name.unwrap_or_else(|| src.rsplit('/').next().unwrap_or(src).to_string());
            let file = srcdir.join(&name);
            if !file.exists() {
                println!("==> {}", src);
                download(src, &file)?;
            }
            if want != "SKIP" && sha256(&file)? != want {
                let _ = std::fs::remove_file(&file);
                return Err(format!("{}: sha256 mismatch", name));
            }
        } else {
            let name = name.unwrap_or_else(|| src.to_string());
            let link = srcdir.join(&name);
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(startdir.join(src), &link).map_err(|e| format!("{}: {}", link.display(), e))?;
        }
    }
    Ok(())
}

fn packager() -> String {
    if let Ok(p) = std::env::var("PACKAGER") {
        return p;
    }
    let git = |k: &str| Command::new("git").args(["config", k]).output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    match (git("user.name"), git("user.email")) {
        (n, _) if n.is_empty() => "Unknown Packager".into(),
        (n, m) if m.is_empty() => n,
        (n, m) => format!("{} <{}>", n, m),
    }
}

/// pkgdir の中のもの (ディレクトリ、ファイル、リンク) を名前の順に
fn walk(dir: &Path, base: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let mut names: Vec<_> = std::fs::read_dir(dir)?.flatten().map(|e| e.path()).collect();
    names.sort();
    for p in names {
        let rel = p.strip_prefix(base).unwrap().to_path_buf();
        if rel.as_os_str() == ".PKGINFO" {
            continue;
        }
        out.push(rel);
        if std::fs::symlink_metadata(&p)?.is_dir() {
            walk(&p, base, out)?;
        }
    }
    Ok(())
}

/// pkgdir を tar (持ち主は root、.PKGINFO が先頭) にして zstd で縮める
fn archive(pkgdir: &Path, out: &Path) -> Result<(), String> {
    let e = |x: std::io::Error| x.to_string();
    let mut files = vec![PathBuf::from(".PKGINFO")];
    walk(pkgdir, pkgdir, &mut files).map_err(e)?;
    let mut b = tar::Builder::new(Vec::new());
    b.follow_symlinks(false);
    for rel in &files {
        let p = pkgdir.join(rel);
        let m = std::fs::symlink_metadata(&p).map_err(e)?;
        let mut h = tar::Header::new_gnu();
        h.set_metadata_in_mode(&m, tar::HeaderMode::Complete);
        h.set_uid(0);
        h.set_gid(0);
        h.set_mode(m.permissions().mode() & 0o7777);
        h.set_mtime(m.mtime().max(0) as u64);
        if m.file_type().is_symlink() {
            let target = std::fs::read_link(&p).map_err(e)?;
            h.set_size(0);
            b.append_link(&mut h, rel, &target).map_err(e)?;
        } else if m.is_dir() {
            h.set_size(0);
            b.append_data(&mut h, rel, std::io::empty()).map_err(e)?;
        } else {
            b.append_data(&mut h, rel, std::fs::File::open(&p).map_err(e)?).map_err(e)?;
        }
    }
    let tar = b.into_inner().map_err(e)?;
    // zstd のコマンドがあればそれで (-19、bin/mkpkg.sh と同じ)。なければ ruzstd (速いが、あまり縮まない)
    if let Ok(mut c) = Command::new("zstd").args(["-q", "-19", "-T0", "-f", "-o"]).arg(out).stdin(std::process::Stdio::piped()).spawn() {
        use std::io::Write;
        let wrote = c.stdin.take().map(|mut i| i.write_all(&tar));
        if c.wait().is_ok_and(|s| s.success()) && wrote.is_some_and(|w| w.is_ok()) {
            return Ok(());
        }
    }
    let z = ruzstd::encoding::compress_to_vec(&tar[..], ruzstd::encoding::CompressionLevel::Fastest);
    std::fs::write(out, z).map_err(|x| format!("{}: {}", out.display(), x))
}

fn du(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| match std::fs::symlink_metadata(e.path()) {
                    Ok(m) if m.is_dir() => du(&e.path()),
                    Ok(m) => m.len(),
                    Err(_) => 0,
                })
                .sum()
        })
        .unwrap_or(0)
}

/// startdir (PKGBUILD のあるディレクトリ) のパッケージを作り、dest に置く。できたファイルを返す
pub fn build(startdir: &Path, dest: &Path) -> Result<PathBuf, String> {
    let startdir = std::fs::canonicalize(startdir).map_err(|e| format!("{}: {}", startdir.display(), e))?;
    if !startdir.join("PKGBUILD").exists() {
        return Err(format!("{}: no PKGBUILD", startdir.display()));
    }
    let mut v = read_vars(&startdir)?;
    let name = one(&v, "pkgname").to_string();
    if name.is_empty() {
        return Err("PKGBUILD: no pkgname".into());
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let work = PathBuf::from(home).join(".cache/aios/build").join(&name);
    let (srcdir, pkgdir) = (work.join("src"), work.join("pkg"));
    std::fs::create_dir_all(&srcdir).map_err(|e| format!("{}: {}", srcdir.display(), e))?;
    sources(&v, &startdir, &srcdir)?;

    // pkgver() prepare() build() は srcdir で、package() は umask 022 で。pkgver は書きもどして読みなおす
    let _ = std::fs::remove_dir_all(&pkgdir);
    std::fs::create_dir_all(&pkgdir).map_err(|e| e.to_string())?;
    let verfile = work.join("pkgver");
    let script = format!(
        "set -e\nsource ./PKGBUILD\n\
         if declare -F pkgver >/dev/null; then pkgver=$(cd \"$srcdir\" && pkgver); fi\n\
         printf '%s' \"$pkgver\" > {ver}\n\
         for f in prepare build; do if declare -F $f >/dev/null; then echo \"==> $pkgname: $f()\"; (cd \"$srcdir\" && $f); fi; done\n\
         echo \"==> $pkgname: package()\"\n\
         (cd \"$srcdir\" && umask 022 && package)\n",
        ver = sh_quote(&verfile.to_string_lossy())
    );
    run(Command::new("bash")
        .arg("-c")
        .arg(&script)
        .current_dir(&startdir)
        .env("startdir", &startdir)
        .env("srcdir", &srcdir)
        .env("pkgdir", &pkgdir)
        .env("CARCH", CARCH)
        .env("PACKAGER", packager()))?;
    if let Ok(ver) = std::fs::read_to_string(&verfile) {
        v.insert("pkgver".into(), vec![ver.trim().to_string()]);
    }
    let install = one(&v, "install");
    if !install.is_empty() {
        let to = pkgdir.join(".INSTALL");
        std::fs::copy(startdir.join(install), &to).map_err(|e| format!("{}: {}", install, e))?;
        let _ = std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o644));
    }

    let mut fullver = format!("{}-{}", one(&v, "pkgver"), one(&v, "pkgrel"));
    let epoch = one(&v, "epoch");
    if !epoch.is_empty() && epoch != "0" {
        fullver = format!("{}:{}", epoch, fullver);
    }
    let arch = if v.get("arch").is_some_and(|a| a.len() == 1 && a[0] == "any") { "any" } else { CARCH };
    let mut info = String::from("# Generated by aios build pkg\n");
    let mut add = |k: &str, vals: &[String]| {
        for x in vals.iter().filter(|x| !x.is_empty()) {
            info.push_str(&format!("{} = {}\n", k, x));
        }
    };
    let s = |x: &str| vec![x.to_string()];
    let a = |k: &str| v.get(k).cloned().unwrap_or_default();
    add("pkgname", &s(&name));
    add("pkgbase", &s(&name));
    add("pkgver", &s(&fullver));
    add("pkgdesc", &s(one(&v, "pkgdesc")));
    add("url", &s(one(&v, "url")));
    add("builddate", &s(&std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()).to_string()));
    add("packager", &s(&packager()));
    add("size", &s(&du(&pkgdir).to_string()));
    add("arch", &s(arch));
    add("license", &a("license"));
    add("replaces", &a("replaces"));
    add("conflict", &a("conflicts"));
    add("provides", &a("provides"));
    add("backup", &a("backup"));
    add("depend", &a("depends"));
    add("optdepend", &a("optdepends"));
    std::fs::write(pkgdir.join(".PKGINFO"), info).map_err(|e| e.to_string())?;

    std::fs::create_dir_all(dest).map_err(|e| format!("{}: {}", dest.display(), e))?;
    // 同じパッケージの古い版は消す
    if let Ok(rd) = std::fs::read_dir(dest) {
        let pre = format!("{}-", name);
        let suf = format!("-{}.pkg.tar.zst", arch);
        for f in rd.flatten() {
            let n = f.file_name().to_string_lossy().into_owned();
            if let Some(mid) = n.strip_prefix(&pre).and_then(|r| r.strip_suffix(&suf))
                && mid.matches('-').count() == 1
                && mid.starts_with(|c: char| c.is_ascii_digit())
            {
                let _ = std::fs::remove_file(f.path());
            }
        }
    }
    let out = dest.join(format!("{}-{}-{}.pkg.tar.zst", name, fullver, arch));
    println!("==> {}: tar + zstd", name);
    archive(&pkgdir, &out)?;
    Ok(out)
}

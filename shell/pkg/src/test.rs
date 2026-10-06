// 作ったパッケージを確かめる (pkg_test)
//   1. パッケージと、それが使うもの (.PKGINFO の depend をたどる) と musl を build/aish-pkg/test/NAME/ に広げる
//   2. 中の ELF がみな aarch64 か
//   3. .PKGINFO の版が PKGBUILD (pkgver-pkgrel) と同じか。repo/aarch64 のほかのパッケージとファイルがぶつかっていないか
//   4. bin/ のプログラムを --version (なければ -version、-V、version、--help) で動かす (aarch64 の上ならそのまま、ほかでは qemu-aarch64 -L 広げたところ)。
//      1 つでも動けばよい。出力に pkgver があるかも見る
// 通ったら build/aish-pkg/test/FILE.ok を置く。pkg_push は、変わるもののうち .ok のないものがあると止まる
use serde_json::{Value, json};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const KINDS: [&str; 4] = ["rust", "c", "shell", "desktop"];

/// 確かめた印
pub fn stamp(root: &Path, file: &str) -> PathBuf {
    root.join("build/aish-pkg/test").join(format!("{}.ok", file))
}

/// repo/aarch64 の中の NAME のパッケージ (いちばん新しいもの)
fn find(root: &Path, name: &str) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for k in KINDS {
        for e in fs::read_dir(root.join("repo/aarch64").join(k)).into_iter().flatten().flatten() {
            let f = e.file_name().to_string_lossy().into_owned();
            if crate::repo::parse(&f).is_some_and(|(n, _)| n == name)
                && let Ok(t) = e.metadata().and_then(|m| m.modified())
                && best.as_ref().is_none_or(|b| t > b.0)
            {
                best = Some((t, e.path()));
            }
        }
    }
    best.map(|b| b.1)
}

fn untar(file: &Path, dir: &Path) -> Result<(), String> {
    let st = Command::new("tar").arg("-I").arg("zstd").arg("-xf").arg(file).arg("-C").arg(dir).arg("--exclude=.PKGINFO").arg("--exclude=.INSTALL").stdout(Stdio::null()).status().map_err(|e| e.to_string())?;
    if st.success() { Ok(()) } else { Err(format!("tar {}: {}", file.display(), st)) }
}

fn pkginfo(file: &Path) -> String {
    Command::new("tar").arg("-I").arg("zstd").arg("-xOf").arg(file).arg(".PKGINFO").output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default()
}

fn field<'a>(info: &'a str, key: &str) -> Vec<&'a str> {
    info.lines().filter_map(|l| l.strip_prefix(&format!("{} = ", key))).collect()
}

/// ELF なら (aarch64 か, 動かせるものか)
fn elf(path: &Path) -> Option<(bool, bool)> {
    let mut b = [0u8; 20];
    fs::File::open(path).ok()?.read_exact(&mut b).ok()?;
    if &b[..4] != b"\x7fELF" {
        return None;
    }
    let ty = u16::from_le_bytes([b[16], b[17]]);
    let machine = u16::from_le_bytes([b[18], b[19]]);
    // ET_EXEC か、ET_DYN でも bin/ にあるもの (PIE) は動かす
    Some((machine == 183, ty == 2 || ty == 3))
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => walk(&p, out),
            // bin/ のリンク (opt/c/bin/zig → ../../zig/zig) も、パッケージの中の ELF を指していれば
            Ok(t) if t.is_file() || (t.is_symlink() && p.is_file()) => out.push(p),
            _ => {}
        }
    }
}

/// cmd を timeout まで動かす: (終わりの番号, 出力の最初の行)
fn run(mut cmd: Command, timeout: Duration) -> (Option<i32>, String) {
    let Ok(mut c) = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn() else { return (None, "cannot start".into()) };
    let t0 = Instant::now();
    let st = loop {
        match c.try_wait() {
            Ok(Some(s)) => break s.code(),
            Ok(None) if t0.elapsed() < timeout => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = c.kill();
                let _ = c.wait();
                return (None, "timeout".into());
            }
        }
    };
    let mut out = String::new();
    if let Some(mut o) = c.stdout.take() {
        let _ = o.read_to_string(&mut out);
    }
    if out.trim().is_empty()
        && let Some(mut e) = c.stderr.take()
    {
        let _ = e.read_to_string(&mut out);
    }
    (st, out.lines().find(|l| !l.trim().is_empty()).unwrap_or("").chars().take(120).collect())
}

pub fn test(root: &Path, name: &str) -> Result<Value, String> {
    let file = find(root, name).ok_or_else(|| format!("{}: not built (pkg_build)", name))?;
    let fname = file.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let info = pkginfo(&file);
    let mut checks: Vec<Value> = Vec::new();
    let mut ok = true;
    // 版: PKGBUILD と .PKGINFO
    let want = crate::up::pkgbuilds(&root.join("pkg")).into_iter().find(|(n, _)| n == name).map(|(_, p)| crate::up::version(&p)).unwrap_or_default();
    let have = field(&info, "pkgver").first().copied().unwrap_or("").to_string();
    let same = have == want || have.ends_with(&format!(":{}", want)) || want.is_empty();
    ok &= same;
    checks.push(json!({ "check": "version", "ok": same, "pkginfo": have, "pkgbuild": want }));
    // 広げる: パッケージ、depend をたどったもの、musl
    let dir = root.join("build/aish-pkg/test").join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    untar(&file, &dir)?;
    let mut seen: Vec<String> = vec![name.to_string()];
    let mut todo: Vec<String> = field(&info, "depend").iter().map(|d| d.split(['<', '>', '=']).next().unwrap_or("").to_string()).collect();
    todo.push("musl".into());
    let mut deps = Vec::new();
    while let Some(d) = todo.pop() {
        if d.is_empty() || seen.contains(&d) {
            continue;
        }
        seen.push(d.clone());
        if let Some(f) = find(root, &d) {
            untar(&f, &dir)?;
            todo.extend(field(&pkginfo(&f), "depend").iter().map(|x| x.split(['<', '>', '=']).next().unwrap_or("").to_string()));
            deps.push(d);
        }
    }
    // /lib は /usr/lib (base と同じ)
    if !dir.join("lib").exists() {
        let _ = std::os::unix::fs::symlink("usr/lib", dir.join("lib"));
    }
    // ELF: このパッケージのものがみな aarch64 か
    let mut mine = Vec::new();
    let pkgdir = root.join("build/aish-pkg/test").join(format!("{}.files", name));
    let _ = fs::remove_dir_all(&pkgdir);
    let _ = fs::create_dir_all(&pkgdir);
    untar(&file, &pkgdir)?;
    walk(&pkgdir, &mut mine);
    let elves: Vec<(PathBuf, bool, bool)> = mine.iter().filter_map(|p| elf(p).map(|(a, x)| (p.clone(), a, x))).collect();
    let wrong: Vec<String> = elves.iter().filter(|e| !e.1).map(|e| e.0.strip_prefix(&pkgdir).unwrap_or(&e.0).display().to_string()).collect();
    ok &= wrong.is_empty();
    checks.push(json!({ "check": "aarch64", "ok": wrong.is_empty(), "elf": elves.len(), "wrong": wrong }));
    // ほかのパッケージとファイルがぶつかっていないか (pango 1.58 が glib を抱えこんで glib2 とぶつかった)
    let mine_rel: std::collections::HashSet<String> = mine.iter().filter_map(|p| p.strip_prefix(&pkgdir).ok()).map(|p| p.display().to_string()).collect();
    let mut clashes: Vec<String> = Vec::new();
    for k in KINDS {
        for e in fs::read_dir(root.join("repo/aarch64").join(k)).into_iter().flatten().flatten() {
            let f = e.file_name().to_string_lossy().into_owned();
            let Some((other, _)) = crate::repo::parse(&f) else { continue };
            if other == name {
                continue;
            }
            let list = Command::new("tar").arg("-I").arg("zstd").arg("-tf").arg(e.path()).output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
            for l in list.lines().filter(|l| !l.ends_with('/') && *l != ".PKGINFO" && *l != ".INSTALL") {
                if mine_rel.contains(l) {
                    clashes.push(format!("{} ({})", l, other));
                }
            }
        }
    }
    ok &= clashes.is_empty();
    checks.push(json!({ "check": "files", "ok": clashes.is_empty(), "clashes": clashes.len(), "with": clashes.iter().take(5).cloned().collect::<Vec<_>>() }));
    // 動かす: bin/ の ELF を --version で
    let native = std::env::consts::ARCH == "aarch64";
    let qemu = ["qemu-aarch64-static", "qemu-aarch64"].into_iter().find(|q| Command::new(q).arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok());
    let bins: Vec<PathBuf> = elves.iter().filter(|e| e.1 && e.2 && e.0.parent().is_some_and(|d| d.ends_with("bin"))).map(|e| e.0.strip_prefix(&pkgdir).unwrap_or(&e.0).to_path_buf()).collect();
    let short = want.split(':').next_back().unwrap_or("").split('-').next().unwrap_or("").to_string();
    if bins.is_empty() {
        checks.push(json!({ "check": "run", "ok": true, "note": "no programs in bin/ (a library)" }));
    } else if !native && qemu.is_none() {
        checks.push(json!({ "check": "run", "ok": true, "note": "skipped: not aarch64 and no qemu-aarch64" }));
    } else {
        let mut runs = Vec::new();
        let mut any = false;
        let mut seen_ver = false;
        for b in &bins {
            let path = dir.join(b);
            // --version がなければ -version (ffmpeg)、-V (tmux)、version (zig)、それもなければ --help
            let (mut st, mut first) = (None, String::new());
            for flag in ["--version", "-version", "-V", "version", "--help"] {
                let mut cmd = if native {
                    let mut c = Command::new(&path);
                    c.env("LD_LIBRARY_PATH", format!("{}/usr/lib:{}/opt/c/lib", dir.display(), dir.display()));
                    c
                } else {
                    let mut c = Command::new(qemu.unwrap_or_default());
                    c.arg("-L").arg(&dir).arg(&path);
                    c
                };
                cmd.arg(flag);
                (st, first) = run(cmd, Duration::from_secs(15));
                if st == Some(0) {
                    break;
                }
            }
            any |= st == Some(0);
            seen_ver |= !short.is_empty() && first.contains(&short);
            runs.push(json!({ "bin": b.display().to_string(), "status": st, "out": first }));
        }
        ok &= any;
        checks.push(json!({ "check": "run", "ok": any, "version_seen": seen_ver, "bins": runs }));
    }
    let _ = fs::remove_dir_all(&pkgdir);
    let _ = fs::remove_dir_all(&dir);
    let st = stamp(root, &fname);
    if ok {
        let _ = fs::write(&st, "");
    } else {
        let _ = fs::remove_file(&st);
    }
    let text = checks
        .iter()
        .map(|c| format!("{} {}{}", if c["ok"] == true { "ok  " } else { "FAIL" }, c["check"].as_str().unwrap_or(""), match c["check"].as_str() {
            Some("version") => format!(" {} (PKGBUILD {})", c["pkginfo"].as_str().unwrap_or(""), c["pkgbuild"].as_str().unwrap_or("")),
            Some("files") => c["with"].as_array().filter(|w| !w.is_empty()).map_or(" no clash with other packages".to_string(), |w| format!(" {} files also in other packages: {}", c["clashes"], w.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", "))),
            Some("aarch64") => format!(" {} ELF{}", c["elf"], c["wrong"].as_array().filter(|w| !w.is_empty()).map_or(String::new(), |w| format!(", not aarch64: {}", w.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" ")))),
            Some("run") => c["note"].as_str().map(|n| format!(" ({})", n)).unwrap_or_else(|| {
                c["bins"].as_array().map_or(String::new(), |bs| bs.iter().map(|b| format!("\n       {} → {} {}", b["bin"].as_str().unwrap_or(""), b["status"], b["out"].as_str().unwrap_or(""))).collect())
            }),
            _ => String::new(),
        }))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(json!({ "name": name, "file": fname, "ok": ok, "deps": deps, "checks": checks, "text": format!("{}\n{}\n", fname, text) }))
}

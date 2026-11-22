// try: C (か C++) のコードを aarch64 の静的なプログラムにして (zig cc。なければ aarch64-linux-gnu-gcc)、動いている VM の
// 中で動かし、出力と終了コードを返す
use super::{ready, secs, sh, vmpy};
use aish_plugin::{Value, error, json, s};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// zig (PATH か $ZIG か build/zig/*/zig)
fn zig(root: &Path) -> Option<String> {
    if let Ok(z) = std::env::var("ZIG") {
        return Some(z);
    }
    if sh(root, "zig", &["version"], &[], Duration::from_secs(10)).0 == 0 {
        return Some("zig".into());
    }
    let d = root.join("build/zig");
    std::fs::read_dir(&d).ok()?.filter_map(|e| e.ok()).map(|e| e.path().join("zig")).find(|p| p.is_file()).map(|p| p.display().to_string())
}

/// C を aarch64 の静的なプログラムにするもの: zig cc (musl) か、なければ aarch64-linux-gnu-gcc (glibc。クラウドの
/// セッションはこちらだけを入れる)。(プログラム, 頭につける引数)
pub(crate) fn compiler(root: &Path, cxx: bool) -> Option<(String, Vec<String>)> {
    if let Some(z) = zig(root) {
        return Some((z, vec![if cxx { "c++" } else { "cc" }.into(), "-target".into(), "aarch64-linux-musl".into()]));
    }
    let gcc = if cxx { "aarch64-linux-gnu-g++" } else { "aarch64-linux-gnu-gcc" };
    (sh(root, gcc, &["--version"], &[], Duration::from_secs(10)).0 == 0).then(|| (gcc.to_string(), vec!["-pthread".into()]))
}

pub(crate) fn try_(root: &Path, a: &Value) -> Value {
    let cxx = s(a, "lang") == "c++";
    let Some((cc, head)) = compiler(root, cxx) else { return error("no compiler: zig (PATH, $ZIG or build/zig/*/zig) or aarch64-linux-gnu-gcc") };
    let dir = root.join("build/aish-vm/try");
    let _ = std::fs::create_dir_all(&dir);
    let src = match a["file"].as_str().filter(|f| !f.is_empty()) {
        Some(f) => PathBuf::from(f),
        None => {
            let p = dir.join(if cxx { "try.cc" } else { "try.c" });
            if std::fs::write(&p, s(a, "code")).is_err() {
                return error("cannot write the source");
            }
            p
        }
    };
    let bin = dir.join("try");
    let (srcs, bins) = (src.display().to_string(), bin.display().to_string());
    let mut args: Vec<String> = head;
    args.extend(["-O2".into(), "-static".into(), "-s".into(), "-o".into(), bins.clone(), srcs]);
    if let Some(f) = a["cflags"].as_array() {
        args.extend(f.iter().filter_map(|x| x.as_str().map(String::from)));
    }
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let (st, _, cerr, cms, _) = sh(root, &cc, &argv, &[], Duration::from_secs(120));
    if st != 0 {
        return json!({ "error": "compile failed", "compile": cerr.trim(), "compile_ms": cms });
    }
    if !ready(root) {
        return error("the VM is not running (vm start first)");
    }
    let (pst, _, perr, _, _) = vmpy(root, &["put", &bins, "/tmp/.try"], &[], Duration::from_secs(55));
    if pst != 0 {
        return json!({ "error": "put failed", "err": perr.trim() });
    }
    let t = secs(a, "timeout", 30.0, 50.0);
    let cmd = format!("chmod +x /tmp/.try && /tmp/.try {}", s(a, "args"));
    let (st, out, err, ms, to) = vmpy(root, &["run", &cmd, "-t", &format!("{}", t)], &[], Duration::from_secs_f64(t + 5.0));
    let mut r = json!({ "status": st, "out": out, "ms": ms, "compile_ms": cms });
    if !err.trim().is_empty() {
        r["err"] = json!(err.trim());
    }
    if to {
        r["timeout"] = json!(true);
    }
    // 警告
    if !cerr.trim().is_empty() {
        r["compile"] = json!(cerr.trim());
    }
    r
}

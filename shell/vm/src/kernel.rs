// カーネルを選ぶ・作る・起こす: ファイルのパス / "." (作業中のソース) / git のコミット (別の作業場所 build/aish-vm/wt)。
// 作ったものは build/aish-vm/kernels/ にとっておく。start (うしろで起こす) も
use super::{note, sh, strings, vmpy};
use crate::sandbox::switchable;
use aish_plugin::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// dir (ワークスペース) のカーネルを作る。できたもののパス
fn build_kernel(root: &Path, dir: &Path) -> Result<PathBuf, String> {
    // 作業中のソースと、コミットの作業場所 (wt) は置き場を分ける。同じ置き場だと、wt で作ったあとに作業中の
    // ソースを作ると、cargo はソースが変わっていないので作りなおさず、置き場の aios (wt のもの) がそのまま残る
    // (ab で a と b が同じカーネルになっていた)
    let target = root.join(if dir == root { "build/aish-vm/target" } else { "build/aish-vm/target-wt" });
    let env = [("AIOS_INITRD", "none".to_string()), ("CARGO_TARGET_DIR", target.display().to_string())];
    let (st, _, err, _, _) = sh(dir, "cargo", &["build", "--release", "-p", "aios"], &env, Duration::from_secs(1800));
    if st != 0 {
        let tail: Vec<&str> = err.lines().filter(|l| l.starts_with("error")).take(5).collect();
        return Err(format!("kernel build failed: {}", tail.join(" / ")));
    }
    Ok(target.join("aarch64-unknown-none-softfloat/release/aios"))
}

pub(crate) fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let (st, out, err, _, _) = sh(dir, "git", args, &[], Duration::from_secs(300));
    if st == 0 { Ok(out.trim().to_string()) } else { Err(format!("git {}: {}", args.join(" "), err.trim())) }
}

/// 別の作業場所 (build/aish-vm/wt。なければ作る)
pub(crate) fn worktree(root: &Path) -> Result<PathBuf, String> {
    let wt = root.join("build/aish-vm/wt");
    if !wt.join(".git").exists() {
        let _ = std::fs::create_dir_all(root.join("build/aish-vm"));
        git(root, &["worktree", "add", "--detach", &wt.display().to_string(), "HEAD"])?;
    }
    Ok(wt)
}

/// カーネルの選び方 (パス / . / git のコミット) から、とっておいたカーネルのパス
pub(crate) fn kernel_for(root: &Path, spec: &str) -> Result<PathBuf, String> {
    let kdir = root.join("build/aish-vm/kernels");
    let _ = std::fs::create_dir_all(&kdir);
    if spec.is_empty() {
        return Err("no kernel".into());
    }
    let p = if Path::new(spec).is_absolute() { PathBuf::from(spec) } else { root.join(spec) };
    if spec != "." && p.is_file() {
        return Ok(p);
    }
    if spec == "." {
        let k = build_kernel(root, root)?;
        let dst = kdir.join("work");
        std::fs::copy(&k, &dst).map_err(|e| e.to_string())?;
        return Ok(dst);
    }
    let sha = git(root, &["rev-parse", "--verify", &format!("{}^{{commit}}", spec)])?;
    let dst = kdir.join(&sha);
    if dst.is_file() {
        return Ok(dst);
    }
    let wt = worktree(root)?;
    git(&wt, &["checkout", "--detach", "-f", &sha])?;
    let k = build_kernel(root, &wt)?;
    std::fs::copy(&k, &dst).map_err(|e| e.to_string())?;
    Ok(dst)
}

/// いま動いている VM のカーネルの ELF (boot で書く)
pub(crate) fn booted(root: &Path) -> PathBuf {
    root.join("build/aish-vm/booted-kernel")
}

/// VM を止めて、kernel で起こす (snapshot ならディスクに書き残さない、off は切って)。起動に失敗したら Err
pub(crate) fn boot(root: &Path, kernel: Option<&Path>, snapshot: bool, www: &str, off: &[String]) -> Result<Value, String> {
    boot_env(root, kernel, snapshot, www, off, &[])
}

/// boot に bin/run.sh への環境を足して (AIOS_SMP など)
pub(crate) fn boot_env(root: &Path, kernel: Option<&Path>, snapshot: bool, www: &str, off: &[String], extra: &[(&str, String)]) -> Result<Value, String> {
    vmpy(root, &["stop"], &[], Duration::from_secs(90));
    let mut env: Vec<(&str, String)> = extra.to_vec();
    if let Some(k) = kernel {
        env.push(("AIOS_KERNEL", k.display().to_string()));
    }
    // カーネルのコマンドライン (bin/run.sh の AIOS_CMDLINE は、カーネルを直に起こすときの -append)
    if !off.is_empty() {
        env.push(("AIOS_CMDLINE", format!("aios.off={}", off.join(","))));
    }
    // panic のバックトレースを関数の名前にするため (symbolize)。aios.img のカーネルなら無い
    let _ = match kernel {
        Some(k) => std::fs::write(booted(root), k.display().to_string()),
        None => std::fs::remove_file(booted(root)),
    };
    if snapshot {
        env.push(("AIOS_QEMU_ARGS", "-snapshot".into()));
    }
    let mut args = vec!["start"];
    if !www.is_empty() {
        args.extend(["--www", www]);
    }
    let (st, out, err, ms, _) = vmpy(root, &args, &env, Duration::from_secs(1000));
    let r = aish_plugin::parse(out.trim()).unwrap_or(json!({}));
    if st != 0 || r["ready"] != true {
        return Err(format!("the VM did not come up ({} ms): {}", ms, err.lines().rev().take(3).collect::<Vec<_>>().join(" / ")));
    }
    Ok(r)
}

pub(crate) fn job_start(root: &Path, a: &Value) -> Result<Value, String> {
    let k = match a["kernel"].as_str().filter(|k| !k.is_empty()) {
        Some(spec) => {
            note(json!({ "step": "kernel", "spec": spec }));
            Some(kernel_for(root, spec)?)
        }
        None => None,
    };
    let www = a["www"].as_str().unwrap_or("");
    let off = strings(&a["off"]);
    if !off.is_empty() {
        let names = switchable(root)?;
        if let Some(n) = off.iter().find(|n| !names.contains(n)) {
            return Err(format!("off: {} cannot be turned off (kernel.json: {})", n, names.join(" ")));
        }
    }
    let r = boot(root, k.as_deref(), a["snapshot"] == true, www, &off)?;
    let mut v = json!({ "ready": true, "boot_s": r["boot_s"], "kernel": k.map(|p| p.display().to_string()) });
    if !off.is_empty() {
        v["off"] = json!(off);
    }
    Ok(v)
}

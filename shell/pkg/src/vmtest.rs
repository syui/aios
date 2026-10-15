// pkg_test の vm: 作ったパッケージを aios の VM に ap -S で入れて、bin/ のプログラムを aios のカーネルの上で動かす。
//   pkg_test のほかの確かめはこのマシンの上 (qemu-user はこのマシンの Linux のカーネルを使う) なので、aios のカーネルに
//   ないもの (システムコールなど) を使うプログラムは、ここで見つかる。このマシンで動いたのに aios で動かないものがあれば
//   しくじり
//   VM は test/vm.py (-snapshot で、repo/ を http://10.0.2.2:8000/ に出したもの)。動いていなければ起こし、終わっても
//   止めない (次の pkg_test がすぐ使えるように。止めるのは vm の stop)。動いていても -snapshot でないか、www が repo/ で
//   ないものは使わない (disk.img に書き残したり、パッケージを取れなかったりするので)
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// test/vm.py を動かす: (終わりの番号, 標準出力, 標準エラー)
fn vmpy(root: &Path, args: &[&str], env: &[(&str, &str)]) -> (i32, String, String) {
    let mut c = Command::new("python3");
    c.arg("test/vm.py").args(args).current_dir(root);
    for (k, v) in env {
        c.env(k, v);
    }
    match c.output() {
        Ok(o) => (o.status.code().unwrap_or(-1), String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned()),
        Err(e) => (-1, String::new(), e.to_string()),
    }
}

/// 出力のさいごの JSON の行
fn last_json(s: &str) -> Value {
    s.lines().rev().find_map(aish_plugin::parse).unwrap_or_else(|| json!({}))
}

/// 使える VM を用意する (動いていなければ起こす)。どうしたかの 1 行
fn ready(root: &Path) -> Result<String, String> {
    let repo = root.join("repo").canonicalize().map_err(|e| format!("repo/: {}", e))?;
    let st = last_json(&vmpy(root, &["status"], &[]).1);
    if st["running"] == true {
        let www = st["www"].as_str().map(PathBuf::from).and_then(|p| p.canonicalize().ok());
        if st["ready"] == true && st["snapshot"] == true && www.as_ref() == Some(&repo) {
            return Ok("the running VM".into());
        }
        return Err("a VM is running without -snapshot or without www=repo/ (installing would write to disk.img, or could not reach the packages): stop it (vm stop) and pkg_test starts its own".into());
    }
    let (code, out, err) = vmpy(root, &["start", "--www", "repo"], &[("AIOS_QEMU_ARGS", "-snapshot")]);
    let r = last_json(&out);
    if code != 0 || r["ready"] != true {
        return Err(format!("the VM did not start: {}", err.trim().lines().last().unwrap_or("")));
    }
    Ok(format!("a VM started for this (-snapshot, booted in {}s; vm stop stops it)", r["boot_s"]))
}

/// 中でうしろで動かして (vm.py bg / job)、終わるまで待つ: (終わりの番号、止められたら None, 出力)
fn bg(root: &Path, cmd: &str, limit: Duration) -> Result<(Option<i64>, String), String> {
    let (_, out, err) = vmpy(root, &["bg", cmd], &[]);
    let id = last_json(&out)["job"].as_u64().ok_or_else(|| format!("vm bg: {}", err.trim()))?.to_string();
    let t0 = Instant::now();
    let mut text = String::new();
    loop {
        let (_, out, err) = vmpy(root, &["job", &id, "-w", "30"], &[]);
        let r = last_json(&out);
        if r["done"].is_null() {
            return Err(format!("vm job {}: {}", id, r["error"].as_str().unwrap_or(err.trim())));
        }
        if let Some(o) = r["out"].as_str().filter(|o| !o.is_empty()) {
            text.push_str(o);
            text.push('\n');
        }
        if r["done"] == true {
            return Ok((r["status"].as_i64(), text));
        }
        if t0.elapsed() > limit {
            let _ = vmpy(root, &["job", &id, "--kill"], &[]);
            return Err(format!("it did not end in {}s (stopped)", limit.as_secs()));
        }
    }
}

/// bin/ のものを、このマシンの run と同じ順の引数で (--version、-version、-V、version、--help のどれかで 0 なら動いた)。
/// 1 つに 1 行で "@@ パス<TAB>終わりの番号<TAB>出力の最初の行"。aios の中の ai (root でない人) で、aiwm がアプリにつける
/// XDG_DATA_DIRS で (/opt/c/share の GSettings の schema などを見る。シリアルの端末にはない)
fn script(bins: &[PathBuf]) -> String {
    let list: Vec<String> = bins.iter().map(|b| format!("'/{}'", b.display().to_string().replace('\'', ""))).collect();
    format!(
        "export XDG_DATA_DIRS=/opt/c/share:/usr/local/share:/usr/share\nfor b in {}; do\n  st=127; out=\n  for f in --version -version -V version --help; do\n    out=$(timeout 15 \"$b\" \"$f\" 2>&1 < /dev/null); st=$?\n    [ \"$st\" = 0 ] && break\n  done\n  first=$(printf '%s\\n' \"$out\" | sed '/^[[:space:]]*$/d' | head -n 1 | cut -c1-120)\n  printf '@@ %s\\t%s\\t%s\\n' \"$b\" \"$st\" \"$first\"\ndone\n",
        list.join(" ")
    )
}

/// 確かめる。host は、このマシンの run で動かしたもの ({bin, status, out})
pub fn check(root: &Path, name: &str, bins: &[PathBuf], host: &[Value]) -> Value {
    let fail = |e: String| json!({ "check": "vm", "ok": false, "error": e });
    let how = match ready(root) {
        Ok(h) => h,
        Err(e) => return fail(e),
    };
    // aipkg.conf を repo/ (www) に向け、前に取ってきた同じ名前のファイルは消す (同じ版で作りなおしたものも入るように)
    let prep = format!("sudo sed -i 's#https://git.syui.ai/ai/repo/raw/branch/main/#http://10.0.2.2:8000/#' /etc/aipkg.conf; sudo rm -f /var/cache/aipkg/{}-[0-9]*.pkg.tar.zst", name);
    let (code, out, err) = vmpy(root, &["run", &prep, "-t", "60"], &[]);
    if code != 0 {
        return fail(format!("could not point aipkg at repo/: {}{}", out.trim(), err.trim()));
    }
    match bg(root, &format!("sudo ap -Sy {}", name), Duration::from_secs(900)) {
        Ok((Some(0), _)) => {}
        Ok((st, text)) => return fail(format!("ap -Sy {} in aios: exit {}: {}", name, st.map_or("?".into(), |s| s.to_string()), text.trim().lines().last().unwrap_or(""))),
        Err(e) => return fail(format!("ap -Sy {} in aios: {}", name, e)),
    }
    if bins.is_empty() {
        return json!({ "check": "vm", "ok": true, "how": how, "note": "installed with ap -S; no programs in bin/" });
    }
    let text = match bg(root, &script(bins), Duration::from_secs(600)) {
        Ok((_, t)) => t,
        Err(e) => return fail(format!("running the programs in aios: {}", e)),
    };
    let mut runs = Vec::new();
    for l in text.lines().filter_map(|l| l.strip_prefix("@@ ")) {
        let mut f = l.splitn(3, '\t');
        let (b, st, first) = (f.next().unwrap_or(""), f.next().unwrap_or(""), f.next().unwrap_or(""));
        runs.push(json!({ "bin": b.trim_start_matches('/'), "status": st.parse::<i64>().ok(), "out": first }));
    }
    // このマシンで動いたのに aios で動かないもの
    let ran = |rs: &[Value], b: &str| rs.iter().any(|r| r["bin"] == b && r["status"] == 0);
    let differ: Vec<Value> = runs.iter().filter(|r| r["status"] != 0 && ran(host, r["bin"].as_str().unwrap_or(""))).cloned().collect();
    // このマシンで動かせなかったとき (qemu がない) は、aios で 1 つでも動けばよい
    let any = runs.iter().any(|r| r["status"] == 0);
    let ok = runs.len() == bins.len() && differ.is_empty() && (any || !host.iter().any(|r| r["status"] == 0));
    json!({ "check": "vm", "ok": ok, "how": how, "bins": runs, "differ": differ })
}

/// pkg_test の答えの vm の行
pub fn describe(c: &Value) -> String {
    if let Some(e) = c["error"].as_str() {
        return format!(" {}", e);
    }
    let how = c["how"].as_str().unwrap_or("");
    let bins = c["bins"].as_array().map_or(0, Vec::len);
    let ran = c["bins"].as_array().map_or(0, |b| b.iter().filter(|r| r["status"] == 0).count());
    match c["differ"].as_array().filter(|d| !d.is_empty()) {
        Some(d) => format!(
            " ran here but not in aios ({}): {}",
            how,
            d.iter().map(|r| format!("{} → {} {}", r["bin"].as_str().unwrap_or(""), r["status"], r["out"].as_str().unwrap_or(""))).collect::<Vec<_>>().join("; ")
        ),
        None if bins == 0 => format!(" installed in aios with ap -S ({})", how),
        None if ran == bins => format!(" installed in aios with ap -S; {} program{} ran on the aios kernel ({})", bins, if bins == 1 { "" } else { "s" }, how),
        None => format!(" installed in aios with ap -S; {} of {} programs ran on the aios kernel, the others fail here too ({})", ran, bins, how),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_lists_the_programs() {
        let s = script(&[PathBuf::from("opt/c/bin/pcre2grep"), PathBuf::from("opt/c/bin/it's")]);
        assert!(s.contains("\nfor b in '/opt/c/bin/pcre2grep' '/opt/c/bin/its'; do\n"));
        assert!(s.starts_with("export XDG_DATA_DIRS=/opt/c/share:"));
        assert!(s.contains("--version -version -V version --help"));
        assert!(s.contains("printf '@@ %s\\t%s\\t%s\\n'"));
    }
}

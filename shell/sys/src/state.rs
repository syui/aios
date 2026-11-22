// 状態の木と変えるもの: get (aios get --json)、do (aiosd に頼んで変える: aios do)、tune (sysctl の値を順に変えて
// コマンドの時間をくらべる: aios tune)
use aish_plugin::{Value, error, json, s};
use std::io::Write;
use std::process::{Command, Stdio};

/// tune: aios tune NAME=V1,V2 [-n N] [--apply] --json -- CMD。root でなければ sudo -n で
pub(crate) fn tune(a: &Value) -> Value {
    let vals: Vec<&str> = a["values"].as_array().map(|v| v.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
    let (name, cmd) = (s(a, "name"), s(a, "cmd"));
    if name.is_empty() || vals.is_empty() || cmd.is_empty() {
        return error("give name, values and cmd");
    }
    let mut args = vec!["aios".to_string(), "tune".into(), format!("{}={}", name, vals.join(",")), "-n".into(), a["n"].as_u64().unwrap_or(3).to_string(), "--json".into()];
    if a["apply"] == true {
        args.push("--apply".into());
    }
    args.push("--".into());
    args.push(cmd.to_string());
    let root = unsafe { libc::geteuid() } == 0;
    let mut c = if root { Command::new(&args[0]) } else { Command::new("sudo") };
    if root {
        c.args(&args[1..]);
    } else {
        c.arg("-n").args(&args);
    }
    match c.stdin(Stdio::null()).output() {
        Ok(o) => {
            let out = String::from_utf8_lossy(&o.stdout);
            let mut r = out.lines().find_map(|l| serde_json::from_str::<Value>(l).ok().filter(|v| v.is_object())).unwrap_or_else(|| json!({}));
            if !o.status.success() {
                r["error"] = json!(String::from_utf8_lossy(&o.stderr).trim().to_string());
            }
            // --apply のときは apply の結果の行も
            let rest: Vec<&str> = out.lines().filter(|l| !l.starts_with('{')).collect();
            if !rest.is_empty() {
                r["text"] = json!(rest.join("\n"));
            }
            r
        }
        Err(e) => error(format!("aios: {}", e)),
    }
}

pub(crate) fn get(a: &Value) -> Value {
    let path = a["path"].as_str().unwrap_or("");
    let mut c = Command::new("aios");
    c.arg("get");
    if !path.is_empty() {
        c.arg(path);
    }
    match c.arg("--json").output() {
        Ok(o) if o.status.success() => match serde_json::from_slice::<Value>(&o.stdout) {
            Ok(v) if v.is_object() => v,
            Ok(v) => json!({ "value": v }),
            Err(e) => error(format!("aios get: {}", e)),
        },
        Ok(o) => error(String::from_utf8_lossy(&o.stderr).trim().to_string()),
        Err(e) => error(format!("aios: {} (aios の base パッケージのコマンド)", e)),
    }
}

/// aiosd に 1 つ頼む (/run/aiosd.sock に 1 行の JSON)
pub(crate) fn do_(a: &Value) -> Value {
    use std::io::BufRead;
    let mut c = match std::os::unix::net::UnixStream::connect("/run/aiosd.sock") {
        Ok(c) => c,
        Err(e) => return error(format!("/run/aiosd.sock: {} (aiosd is not running: sudo systemctl enable --now aiosd)", e)),
    };
    let mut line = String::new();
    if writeln!(c, "{}", a).is_err() || std::io::BufReader::new(&c).read_line(&mut line).is_err() {
        return error("aiosd did not answer");
    }
    serde_json::from_str(&line).unwrap_or_else(|_| error("aiosd: bad answer"))
}

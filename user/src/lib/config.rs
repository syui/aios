#![allow(dead_code)] // aios (読むだけ) と aiosd (そろえる) で使うものが違う
// /etc/aios.json (望む状態) と状態の木をくらべて、そろえるための手順を作る (doc/aios.md の段階 3)。
// aios diff / aios config (読むだけ) と aiosd の apply / rollback (root で動かす) が使う。
// 書いていないものは触らない。pkg は「入っていてほしいもの」で、書いていないものを外しはしない
use serde_json::{Map, Value, json};
use std::fs;

pub const PATH: &str = "/etc/aios.json";
/// apply のたびの記録 (1.json 2.json ...)
pub const HISTORY: &str = "/var/lib/aios/history";
/// aios が書く modules-load.d のファイル (kernel.modules)
const MODULES: &str = "/etc/modules-load.d/aios.conf";

/// 一番上に書けるもの
const KEYS: [&str; 5] = ["host", "pkg", "service", "kernel", "user"];

/// そろえるための 1 つの手順
#[derive(Clone)]
pub struct Step {
    /// 何を (pkg git、service sshd、host.name ...)
    pub what: String,
    pub from: String,
    pub to: String,
    pub act: Act,
}

#[derive(Clone)]
pub enum Act {
    Run(Vec<String>),
    Write(String, String),
    Hostname(String),
}

impl Step {
    pub fn json(&self) -> Value {
        let how = match &self.act {
            Act::Run(c) => c.join(" "),
            Act::Write(p, _) => format!("write {}", p),
            Act::Hostname(n) => format!("hostname {}", n),
        };
        json!({ "what": self.what, "from": self.from, "to": self.to, "do": how })
    }
}

/// 名前に使ってよい字 (コマンドの引数にするので、空白や - で始まるものは受けない)
fn ok_name(n: &str) -> bool {
    !n.is_empty() && !n.starts_with('-') && n.chars().all(|c| c.is_ascii_alphanumeric() || "-_.+@".contains(c))
}

pub fn load(path: &str) -> Result<Value, String> {
    let t = fs::read_to_string(path).map_err(|e| format!("{}: {}", path, e))?;
    let v: Value = serde_json::from_str(&t).map_err(|e| format!("{}: {}", path, e))?;
    let Some(m) = v.as_object() else { return Err(format!("{}: not a JSON object", path)) };
    for k in m.keys() {
        if !KEYS.contains(&k.as_str()) {
            return Err(format!("{}: unknown key {:?} (one of: {})", path, k, KEYS.join(" ")));
        }
    }
    Ok(v)
}

fn run(v: &[&str]) -> Act {
    Act::Run(v.iter().map(|s| s.to_string()).collect())
}

/// いまの状態から cfg にそろえる手順
pub fn plan(cfg: &Value) -> Result<Vec<Step>, String> {
    let mut steps = Vec::new();
    let step = |what: String, from: &str, to: &str, act: Act| Step { what, from: from.into(), to: to.into(), act };

    // host.name
    if let Some(name) = cfg["host"]["name"].as_str() {
        if !ok_name(name) {
            return Err(format!("host.name: bad name {:?}", name));
        }
        let now = fs::read_to_string("/etc/hostname").unwrap_or_default().trim().to_string();
        if now != name {
            steps.push(step("host.name".into(), &now, name, Act::Hostname(name.into())));
        }
    }

    // pkg: 入っていないものだけ (1 回の aipkg -S で)
    if let Some(list) = cfg.get("pkg") {
        let Some(a) = list.as_array() else { return Err("pkg: must be a list of names".into()) };
        let have = crate::state::installed();
        let mut want = Vec::new();
        for p in a {
            let Some(n) = p.as_str().filter(|n| ok_name(n)) else { return Err(format!("pkg: bad name {}", p)) };
            if !have.contains_key(n) {
                want.push(n.to_string());
            }
        }
        if !want.is_empty() {
            let mut cmd = vec!["aipkg".to_string(), "-S".to_string()];
            cmd.extend(want.iter().cloned());
            steps.push(step(format!("pkg {}", want.join(" ")), "absent", "installed", Act::Run(cmd)));
        }
    }

    // service: enabled (enable して動かす) / disabled (止めて disable)
    if let Some(svcs) = cfg.get("service") {
        let Some(m) = svcs.as_object() else { return Err("service: must be {\"NAME\": \"enabled\" | \"disabled\"}".into()) };
        let now = crate::state::collect("service").unwrap_or_default();
        for (name, want) in m {
            if !ok_name(name) {
                return Err(format!("service: bad name {:?}", name));
            }
            let cur = now.as_array().and_then(|a| a.iter().find(|s| s["name"] == name.as_str()));
            let Some(cur) = cur else { return Err(format!("service: no unit {:?}", name)) };
            let (enabled, active) = (cur["enabled"] == true, cur["active"] == "active");
            match want.as_str() {
                Some("enabled") => {
                    if !enabled {
                        steps.push(step(format!("service {} enable", name), "disabled", "enabled", run(&["systemctl", "enable", name])));
                    }
                    if !active {
                        steps.push(step(format!("service {} start", name), cur["active"].as_str().unwrap_or(""), "active", run(&["systemctl", "start", name])));
                    }
                }
                Some("disabled") => {
                    if active {
                        steps.push(step(format!("service {} stop", name), "active", "inactive", run(&["systemctl", "stop", name])));
                    }
                    if enabled {
                        steps.push(step(format!("service {} disable", name), "enabled", "disabled", run(&["systemctl", "disable", name])));
                    }
                }
                _ => return Err(format!("service.{}: \"enabled\" or \"disabled\"", name)),
            }
        }
    }

    // kernel.modules: /etc/modules-load.d/aios.conf に書き、入っていないものは今 modprobe
    if let Some(mods) = cfg["kernel"].get("modules") {
        let Some(a) = mods.as_array() else { return Err("kernel.modules: must be a list".into()) };
        let names: Vec<&str> = a.iter().filter_map(|m| m.as_str()).collect();
        if names.len() != a.len() || names.iter().any(|n| !ok_name(n)) {
            return Err("kernel.modules: bad name".into());
        }
        let content = format!("# aios apply (/etc/aios.json の kernel.modules) が書く\n{}\n", names.join("\n"));
        let now = fs::read_to_string(MODULES).unwrap_or_default();
        // 空のリストで、ファイルもなければ、そろっている
        if now != content && !(names.is_empty() && now.is_empty()) {
            steps.push(step("kernel.modules".into(), &now.lines().filter(|l| !l.starts_with('#')).collect::<Vec<_>>().join(" "), &names.join(" "), Act::Write(MODULES.into(), content)));
        }
        let loaded = fs::read_to_string("/proc/modules").unwrap_or_default();
        for n in names {
            if !loaded.lines().any(|l| l.split_whitespace().next() == Some(n)) {
                steps.push(step(format!("module {}", n), "unloaded", "loaded", run(&["modprobe", n])));
            }
        }
    }

    // user: いなければ作る。shell と groups (足すだけ) をそろえる
    if let Some(users) = cfg.get("user") {
        let Some(m) = users.as_object() else { return Err("user: must be {\"NAME\": {\"shell\": ..., \"groups\": [...]}}".into()) };
        let now = crate::state::collect("user").unwrap_or_default();
        let all = fs::read_to_string("/etc/passwd").unwrap_or_default();
        for (name, u) in m {
            if !ok_name(name) {
                return Err(format!("user: bad name {:?}", name));
            }
            let shell = u["shell"].as_str();
            if shell.is_some_and(|s| !s.starts_with('/') || s.contains(char::is_whitespace)) {
                return Err(format!("user.{}.shell: must be an absolute path", name));
            }
            let groups: Vec<&str> = u["groups"].as_array().map(|a| a.iter().filter_map(|g| g.as_str()).collect()).unwrap_or_default();
            if groups.iter().any(|g| !ok_name(g)) {
                return Err(format!("user.{}.groups: bad name", name));
            }
            let exists = all.lines().any(|l| l.split(':').next() == Some(name.as_str()));
            if !exists {
                let mut cmd = vec!["useradd".to_string(), "-m".to_string()];
                if let Some(s) = shell {
                    cmd.extend(["-s".to_string(), s.to_string()]);
                }
                if !groups.is_empty() {
                    cmd.extend(["-G".to_string(), groups.join(",")]);
                }
                cmd.push(name.clone());
                steps.push(step(format!("user {}", name), "absent", "present", Act::Run(cmd)));
                continue;
            }
            let cur = now.as_array().and_then(|a| a.iter().find(|x| x["name"] == name.as_str())).cloned().unwrap_or_default();
            if let Some(s) = shell
                && cur["shell"].as_str() != Some(s)
            {
                steps.push(step(format!("user {} shell", name), cur["shell"].as_str().unwrap_or(""), s, run(&["usermod", "-s", s, name])));
            }
            let have: Vec<&str> = cur["groups"].as_array().map(|a| a.iter().filter_map(|g| g.as_str()).collect()).unwrap_or_default();
            let missing: Vec<&str> = groups.iter().copied().filter(|g| !have.contains(g)).collect();
            if !missing.is_empty() {
                steps.push(step(format!("user {} groups", name), &have.join(","), &groups.join(","), run(&["usermod", "-a", "-G", &missing.join(","), name])));
            }
        }
    }
    Ok(steps)
}

/// 手順を 1 つ動かす (root で)。(うまくいったか, 出力)
pub fn exec(s: &Step, path_env: &str) -> (bool, String) {
    match &s.act {
        Act::Run(cmd) => match std::process::Command::new(&cmd[0]).args(&cmd[1..]).env("PATH", path_env).stdin(std::process::Stdio::null()).output() {
            Ok(o) => (o.status.success(), format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))),
            Err(e) => (false, format!("{}: {}", cmd[0], e)),
        },
        Act::Write(p, c) => {
            if let Some(d) = std::path::Path::new(p).parent() {
                let _ = fs::create_dir_all(d);
            }
            match fs::write(p, c) {
                Ok(()) => (true, String::new()),
                Err(e) => (false, format!("{}: {}", p, e)),
            }
        }
        Act::Hostname(n) => {
            let r = fs::write("/etc/hostname", format!("{}\n", n));
            let c = std::ffi::CString::new(n.as_str()).unwrap_or_default();
            let ok = unsafe { libc::sethostname(c.as_ptr(), n.len()) } == 0;
            (r.is_ok() && ok, r.err().map(|e| e.to_string()).unwrap_or_default())
        }
    }
}

/// いまの状態を aios.json の形で (aios config。はじめて /etc/aios.json を作るとき)
pub fn export() -> Value {
    let mut m = Map::new();
    let host = crate::state::collect("host").unwrap_or_default();
    m.insert("host".into(), json!({ "name": host["name"] }));
    let pkgs: Vec<Value> = crate::state::installed().keys().map(|k| json!(k)).collect();
    m.insert("pkg".into(), Value::Array(pkgs));
    let mut svc = Map::new();
    for s in crate::state::collect("service").unwrap_or_default().as_array().cloned().unwrap_or_default() {
        if let Some(n) = s["name"].as_str() {
            svc.insert(n.into(), json!(if s["enabled"] == true { "enabled" } else { "disabled" }));
        }
    }
    m.insert("service".into(), Value::Object(svc));
    let mut mods: Vec<Value> = Vec::new();
    if let Ok(d) = fs::read_dir("/etc/modules-load.d") {
        let mut files: Vec<_> = d.flatten().map(|e| e.path()).collect();
        files.sort();
        for f in files {
            for l in fs::read_to_string(f).unwrap_or_default().lines() {
                let l = l.trim();
                if !l.is_empty() && !l.starts_with('#') && !mods.contains(&json!(l)) {
                    mods.push(json!(l));
                }
            }
        }
    }
    m.insert("kernel".into(), json!({ "modules": mods }));
    let mut users = Map::new();
    for u in crate::state::collect("user").unwrap_or_default().as_array().cloned().unwrap_or_default() {
        if u["uid"].as_u64().unwrap_or(0) >= 1000 && u["uid"].as_u64() != Some(65534) {
            users.insert(u["name"].as_str().unwrap_or("").into(), json!({ "shell": u["shell"], "groups": u["groups"] }));
        }
    }
    m.insert("user".into(), Value::Object(users));
    Value::Object(m)
}

// ---- 記録 ----

/// 記録の番号 (小さい順)
pub fn history() -> Vec<u64> {
    let mut v: Vec<u64> = fs::read_dir(HISTORY)
        .map(|d| d.flatten().filter_map(|e| e.file_name().to_string_lossy().strip_suffix(".json")?.parse().ok()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

pub fn read_history(n: u64) -> Option<Value> {
    serde_json::from_str(&fs::read_to_string(format!("{}/{}.json", HISTORY, n)).ok()?).ok()
}

pub fn write_history(rec: &Value) -> u64 {
    let n = history().last().copied().unwrap_or(0) + 1;
    let _ = fs::create_dir_all(HISTORY);
    let mut rec = rec.clone();
    rec["n"] = json!(n);
    let _ = fs::write(format!("{}/{}.json", HISTORY, n), serde_json::to_string_pretty(&rec).unwrap_or_default());
    n
}

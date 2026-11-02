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
/// networkd が読むところ (net)。aios が書くのは 00-aios-IFACE.network (名前の順で先に読まれる)
const NETWORK: &str = "/etc/systemd/network";

/// 一番上に書けるもの
const KEYS: [&str; 8] = ["host", "pkg", "service", "kernel", "user", "net", "sysctl", "firewall"];

/// ファイアウォール (firewall): nft の決まりのファイル (Linux の nftables.service が起動で読むところ)、aios が入れた
/// ときの記録 (生成番号とファイルの中身のしるし)、入れる前の決まり (confirm がなければもどす: aiosd)
pub const NFT_CONF: &str = "/etc/nftables.conf";
pub const FW_STATE: &str = "/var/lib/aios/firewall.json";
pub const FW_PREV: &str = "/var/lib/aios/firewall.prev.nft";
pub const FW_PREV_CONF: &str = "/var/lib/aios/nftables.conf.prev";
/// confirm を待っているもの (aiosd): {"n", "deadline", "confirm", "prev"}。起動したときに残っていれば、もどす
pub const FW_PENDING: &str = "/var/lib/aios/firewall-pending.json";
/// aios が書いた /etc/nftables.conf のしるし (2 行目)
const FW_MARK: &str = "# aios apply (/etc/aios.json の firewall) が書く";

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
    Remove(String),
    Hostname(String),
    /// /etc/nftables.conf の中身と、confirm を待つ秒 (0 は待たない)
    Firewall(String, u64),
}

impl Step {
    pub fn json(&self) -> Value {
        let how = match &self.act {
            Act::Run(c) => c.join(" "),
            Act::Write(p, _) => format!("write {}", p),
            Act::Remove(p) => format!("rm {}", p),
            Act::Hostname(n) => format!("hostname {}", n),
            Act::Firewall(_, 0) => format!("nft -c -f, then nft -f {}", NFT_CONF),
            Act::Firewall(_, c) => format!("nft -c -f, then nft -f {} (reverted unless confirmed in {} s)", NFT_CONF, c),
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
    // sysctl: /etc/sysctl.d/aios.conf に書き (起動のときに init が入れる)、いまの値がちがうものは /proc/sys に
    if let Some(sc) = cfg.get("sysctl") {
        let Some(m) = sc.as_object() else { return Err("sysctl: must be {\"NAME\": VALUE} (vm.min_free_kbytes ...)".into()) };
        let mut content = String::from("# aios apply (/etc/aios.json の sysctl) が書く\n");
        let mut now = Vec::new();
        for (k, v) in m {
            if !crate::sysctl::ok_key(k) {
                return Err(format!("sysctl: bad name {:?}", k));
            }
            let val = match v {
                Value::String(s) if !s.contains('\n') => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => (if *b { "1" } else { "0" }).to_string(),
                _ => return Err(format!("sysctl.{}: must be a number or a string", k)),
            };
            content.push_str(&format!("{} = {}\n", k, val));
            let cur = crate::sysctl::get(k).map_err(|_| format!("sysctl.{}: not in /proc/sys", k))?;
            // 値がいくつもあるもの (タブで分ける) も、空白の数はくらべない
            if cur.split_whitespace().ne(val.split_whitespace()) {
                now.push(step(format!("sysctl {}", k), &cur, &val, Act::Write(crate::sysctl::path(k), format!("{}\n", val))));
            }
        }
        let file = fs::read_to_string(crate::sysctl::AIOS_CONF).unwrap_or_default();
        let keys = |t: &str| crate::sysctl::parse(t).into_iter().map(|(k, v, _)| format!("{}={}", k, v)).collect::<Vec<_>>().join(" ");
        if m.is_empty() {
            if !file.is_empty() {
                steps.push(step("sysctl.conf".into(), &keys(&file), "", Act::Remove(crate::sysctl::AIOS_CONF.into())));
            }
        } else if file != content {
            steps.push(step("sysctl.conf".into(), &keys(&file), &keys(&content), Act::Write(crate::sysctl::AIOS_CONF.into(), content)));
        }
        steps.extend(now);
    }

    // net: インターフェースごとに 00-aios-IFACE.network を書いて networkd で決める。
    // aios が書いたもので、もう書いていないインターフェースのものは消す
    if let Some(net) = cfg.get("net") {
        let Some(m) = net.as_object() else {
            return Err("net: must be {\"IFACE\": {\"dhcp\": true} | {\"address\": \"A.B.C.D/N\", \"gateway\": ..., \"dns\": [...]}}".into());
        };
        let live = crate::state::collect("net").unwrap_or_default();
        let mut redo = false;
        for (name, n) in m {
            if !ok_name(name) {
                return Err(format!("net: bad interface name {:?}", name));
            }
            let (content, addr) = network_file(name, n)?;
            let path = format!("{}/00-aios-{}.network", NETWORK, name);
            let now = fs::read_to_string(&path).unwrap_or_default();
            if now != content {
                steps.push(step(format!("net.{}", name), &network_summary(&now), &network_summary(&content), Act::Write(path, content)));
                redo = true;
                continue;
            }
            // ファイルはそろっていても、いまのアドレスがちがえば networkd をもういちど
            let cur = live["interfaces"].as_array().and_then(|a| a.iter().find(|i| i["name"] == name.as_str())).cloned().unwrap_or_default();
            redo |= match &addr {
                Some(a) => cur["addr"].as_str() != Some(a),
                None => cur["dhcp"] != true,
            };
        }
        let mut old: Vec<String> = fs::read_dir(NETWORK)
            .map(|d| d.flatten().filter_map(|e| e.file_name().to_str()?.strip_prefix("00-aios-")?.strip_suffix(".network").map(String::from)).collect())
            .unwrap_or_default();
        old.sort();
        for name in old.iter().filter(|n| !m.contains_key(*n)) {
            let path = format!("{}/00-aios-{}.network", NETWORK, name);
            steps.push(step(format!("net.{}", name), &network_summary(&fs::read_to_string(&path).unwrap_or_default()), "", Act::Remove(path)));
            redo = true;
        }
        if redo {
            steps.push(step("net".into(), "", "applied", run(&["networkd"])));
        }
    }
    // firewall: /etc/nftables.conf に書いて nft -f で入れ (先に nft -c でたしかめる)、起動でも入るように nftables.service を
    // enable。ファイルがそろっていても、aios が入れたあとにだれかが nft で変えていれば (生成番号がちがう) 入れなおす
    if let Some(fw) = cfg.get("firewall") {
        let (content, confirm) = firewall_content(fw)?;
        if nft().is_none() {
            steps.push(step("pkg nftables (firewall)".into(), "absent", "installed", run(&["aipkg", "-S", "nftables"])));
        }
        let now = fs::read_to_string(NFT_CONF).unwrap_or_default();
        let rec: Value = fs::read_to_string(FW_STATE).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        let live_gen = fw_generation();
        let same_file = now == content && rec["sha"].as_str() == Some(&sha(&content));
        // 起動しなおしていれば (生成番号は 1 から)、起動の nftables.service が aios の書いたファイルを入れている
        let rebooted = rec["boot_id"].as_str() != Some(&boot_id()) && std::path::Path::new("/etc/systemd/system/multi-user.target.wants/nftables.service").exists();
        let same_live = live_gen.is_none() || rec["generation"].as_u64() == live_gen || (rebooted && same_file);
        if !same_file || !same_live {
            let from = if !same_file { fw_summary(&now) } else { format!("changed outside aios (generation {} != {})", live_gen.unwrap_or(0), rec["generation"]) };
            steps.push(step("firewall".into(), &from, &fw_summary(&content), Act::Firewall(content, confirm)));
        }
        if !std::path::Path::new("/etc/systemd/system/multi-user.target.wants/nftables.service").exists() {
            steps.push(step("service nftables enable".into(), "disabled", "enabled", run(&["systemctl", "enable", "nftables"])));
        }
    }
    Ok(steps)
}

/// nft のプログラム (パッケージ nftables は /opt/c/bin に)
pub fn nft() -> Option<&'static str> {
    ["/opt/c/bin/nft", "/usr/sbin/nft", "/usr/bin/nft"].into_iter().find(|p| std::path::Path::new(p).exists())
}

/// いまの決まりの生成番号 (/proc/ai/fw のはじめの行。aios のカーネルで、読める人だけ)
pub fn fw_generation() -> Option<u64> {
    let t = fs::read_to_string("/proc/ai/fw").ok()?;
    let v: Value = serde_json::from_str(t.lines().next()?).ok()?;
    v["generation"].as_u64()
}

/// 中身のしるし (FNV-1a 64)
pub fn sha(s: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{:016x}", h)
}

/// aios.json の firewall から /etc/nftables.conf の中身と confirm の秒。書きかた: 決まりの文字 ("table inet filter {...}")、
/// 行の並び、{"rules": 文字か行の並び, "confirm": 秒 (10 から 3600。なければ待たない)}
pub fn firewall_content(fw: &Value) -> Result<(String, u64), String> {
    let lines = |v: &Value| -> Result<String, String> {
        match v {
            Value::String(s) => Ok(s.clone()),
            Value::Array(a) => a.iter().map(|l| l.as_str().map(String::from).ok_or("firewall.rules: a list of lines (strings)".to_string())).collect::<Result<Vec<_>, _>>().map(|v| v.join("\n")),
            _ => Err("firewall.rules: nft rules as a string or a list of lines".into()),
        }
    };
    let (rules, confirm) = match fw {
        Value::Object(o) => {
            for k in o.keys() {
                if !["rules", "confirm"].contains(&k.as_str()) {
                    return Err(format!("firewall: unknown key {:?} (one of: rules confirm)", k));
                }
            }
            let c = match o.get("confirm") {
                None => 0,
                Some(c) => c.as_u64().filter(|c| (10..=3600).contains(c)).ok_or("firewall.confirm: seconds from 10 to 3600")?,
            };
            (lines(o.get("rules").unwrap_or(&Value::String(String::new())))?, c)
        }
        v => (lines(v)?, 0),
    };
    if rules.contains('\0') {
        return Err("firewall.rules: no NUL".into());
    }
    let mut c = format!("#!/opt/c/bin/nft -f\n{}。手で変えても、次の aios apply でもどる\nflush ruleset\n", FW_MARK);
    if !rules.trim().is_empty() {
        c.push('\n');
        c.push_str(rules.trim_end());
        c.push('\n');
    }
    Ok((c, confirm))
}

/// 決まりのファイルを 1 行に (diff の from / to): 表の数と行の数
fn fw_summary(t: &str) -> String {
    if t.is_empty() {
        return "none".into();
    }
    let body: Vec<&str> = t.lines().filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#') && l.trim() != "flush ruleset").collect();
    let tables = body.iter().filter(|l| l.trim_start().starts_with("table ")).count();
    format!("{} tables, {} lines, {}", tables, body.len(), sha(t))
}

/// 決まりを入れる (root で): nft -c -f でたしかめ、いまの決まりを nft list ruleset でとっておき (confirm がないときに
/// もどすため)、/etc/nftables.conf に書いて nft -f。生成番号を記録に
fn fw_load(content: &str, path_env: &str) -> (bool, String) {
    let nft = nft().unwrap_or("nft");
    let _ = fs::create_dir_all("/var/lib/aios");
    let cand = "/var/lib/aios/nftables.new";
    if let Err(e) = fs::write(cand, content) {
        return (false, format!("{}: {}", cand, e));
    }
    let sh = |args: &[&str]| std::process::Command::new(nft).args(args).env("PATH", path_env).stdin(std::process::Stdio::null()).output();
    match sh(&["-c", "-f", cand]) {
        Ok(o) if o.status.success() => {}
        Ok(o) => return (false, format!("nft -c: {}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))),
        Err(e) => return (false, format!("{}: {}", nft, e)),
    }
    let live = sh(&["list", "ruleset"]).map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    let _ = fs::write(FW_PREV, format!("flush ruleset\n{}", live));
    match fs::read_to_string(NFT_CONF) {
        Ok(t) => {
            let _ = fs::write(FW_PREV_CONF, t);
        }
        Err(_) => {
            let _ = fs::remove_file(FW_PREV_CONF);
        }
    }
    if let Err(e) = fs::rename(cand, NFT_CONF) {
        return (false, format!("{}: {}", NFT_CONF, e));
    }
    match sh(&["-f", NFT_CONF]) {
        Ok(o) if o.status.success() => {}
        Ok(o) => return (false, format!("nft -f: {}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))),
        Err(e) => return (false, format!("{}: {}", nft, e)),
    }
    fw_record(content);
    (true, String::new())
}

/// aios が入れたときの生成番号、中身のしるし、起動の番号 (生成番号は起動ごとに 1 から)
pub fn fw_record(content: &str) {
    let rec = json!({ "generation": fw_generation(), "sha": sha(content), "boot_id": boot_id() });
    let _ = fs::write(FW_STATE, rec.to_string() + "\n");
}

fn boot_id() -> String {
    fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap_or_default().trim().to_string()
}

/// 起動したとき (aiosd): nftables.service が入れたのが aios の書いた /etc/nftables.conf なら、この起動の生成番号を記録に
/// (aios diff が「aios の外で変わった」と言わないように)
pub fn fw_boot_sync() {
    let Some(rec) = fs::read_to_string(FW_STATE).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) else { return };
    if rec["boot_id"].as_str() == Some(&boot_id()) {
        return;
    }
    // まだ入っていなければ (生成番号は 1 のまま: 起動の順で aiosd が先だったとき) 記録しない
    if let Ok(t) = fs::read_to_string(NFT_CONF)
        && rec["sha"].as_str() == Some(&sha(&t))
        && fw_generation().is_some_and(|g| g > 1)
    {
        fw_record(&t);
    }
}

/// net.IFACE から networkd のファイルを作る。(中身, 手で決めたアドレス)
fn network_file(name: &str, n: &Value) -> Result<(String, Option<String>), String> {
    let Some(o) = n.as_object() else { return Err(format!("net.{}: must be {{\"dhcp\": true}} or {{\"address\": ...}}", name)) };
    for k in o.keys() {
        if !["dhcp", "address", "gateway", "dns"].contains(&k.as_str()) {
            return Err(format!("net.{}: unknown key {:?} (one of: dhcp address gateway dns)", name, k));
        }
    }
    let ip = |s: &str| s.parse::<std::net::Ipv4Addr>().is_ok();
    let dhcp = n["dhcp"].as_bool().unwrap_or(false);
    let addr = n["address"].as_str();
    if dhcp == addr.is_some() {
        return Err(format!("net.{}: give either \"dhcp\": true or \"address\"", name));
    }
    let mut s = format!("# aios apply (/etc/aios.json の net.{}) が書く\n[Match]\nName={}\n\n[Network]\n", name, name);
    if dhcp {
        s.push_str("DHCP=yes\n");
    }
    if let Some(a) = addr {
        let ok = a.split_once('/').is_some_and(|(h, p)| ip(h) && p.parse::<u8>().is_ok_and(|p| p <= 32));
        if !ok {
            return Err(format!("net.{}.address: must be A.B.C.D/N", name));
        }
        s.push_str(&format!("Address={}\n", a));
    }
    if let Some(g) = n.get("gateway") {
        let Some(g) = g.as_str().filter(|g| ip(g)) else { return Err(format!("net.{}.gateway: must be A.B.C.D", name)) };
        if dhcp {
            return Err(format!("net.{}.gateway: DHCP gives the gateway", name));
        }
        s.push_str(&format!("Gateway={}\n", g));
    }
    if let Some(d) = n.get("dns") {
        let list: Vec<&str> = d.as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
        if !d.is_array() || list.len() != d.as_array().map_or(0, |a| a.len()) || list.iter().any(|x| !ip(x)) {
            return Err(format!("net.{}.dns: must be a list of A.B.C.D", name));
        }
        if !list.is_empty() {
            s.push_str(&format!("DNS={}\n", list.join(" ")));
        }
    }
    Ok((s, addr.map(String::from)))
}

/// networkd のファイルを 1 行に (diff の from / to)
fn network_summary(t: &str) -> String {
    t.lines().filter(|l| l.contains('=') && !l.starts_with("Name=") && !l.starts_with('#')).collect::<Vec<_>>().join(" ")
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
        Act::Remove(p) => match fs::remove_file(p) {
            Ok(()) => (true, String::new()),
            Err(e) => (false, format!("{}: {}", p, e)),
        },
        Act::Firewall(c, _) => fw_load(c, path_env),
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
    // net: いまのアドレス (lo は書かない)。DHCP のものは dhcp だけ
    let net = crate::state::collect("net").unwrap_or_default();
    let mut ifs = Map::new();
    let gw = |dev: &str| net["routes"].as_array().and_then(|a| a.iter().find(|r| r["dev"] == dev && r["dst"] == "0.0.0.0/0")).and_then(|r| r["gw"].as_str().map(String::from));
    for i in net["interfaces"].as_array().cloned().unwrap_or_default() {
        let Some(name) = i["name"].as_str().filter(|n| *n != "lo") else { continue };
        if i["dhcp"] == true {
            ifs.insert(name.into(), json!({ "dhcp": true }));
        } else if let Some(a) = i["addr"].as_str() {
            let mut o = json!({ "address": a });
            if let Some(g) = gw(name) {
                o["gateway"] = json!(g);
            }
            if net["dns"].as_array().is_some_and(|d| !d.is_empty()) {
                o["dns"] = net["dns"].clone();
            }
            ifs.insert(name.into(), o);
        }
    }
    m.insert("net".into(), Value::Object(ifs));
    // sysctl: aios apply で書いたもの (ほかの /proc/sys は既定のまま)
    let mut sc = Map::new();
    for (k, v, _) in crate::sysctl::parse(&fs::read_to_string(crate::sysctl::AIOS_CONF).unwrap_or_default()) {
        sc.insert(k, v.parse::<i64>().map_or(json!(v), |n| json!(n)));
    }
    m.insert("sysctl".into(), Value::Object(sc));
    // firewall: aios が書いた /etc/nftables.conf の決まり (flush ruleset とはじめの説明をのぞいた行)
    if let Ok(t) = fs::read_to_string(NFT_CONF)
        && t.lines().nth(1).is_some_and(|l| l.starts_with(FW_MARK))
    {
        let lines: Vec<Value> = t.lines().skip(3).skip_while(|l| l.is_empty()).map(|l| json!(l)).collect();
        m.insert("firewall".into(), json!({ "rules": lines }));
    }
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

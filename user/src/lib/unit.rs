#![allow(dead_code)] // init / systemctl / journalctl で使うものがそれぞれ違う
// systemd のユニットファイル (.service) を読む
use std::collections::BTreeMap;
use std::fs;

pub const UNIT_DIRS: [&str; 2] = ["/usr/lib/systemd/system", "/etc/systemd/system"];
pub const WANTS_DIR: &str = "/etc/systemd/system/multi-user.target.wants";
pub const CTL: &str = "/run/aiinit.ctl";

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Restart {
    No,
    Always,
    OnFailure,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Type {
    Simple,
    Oneshot,
}

#[derive(Clone, Debug)]
pub struct Unit {
    pub name: String,
    pub path: String,
    pub description: String,
    pub after: Vec<String>,
    pub wants: Vec<String>,
    pub typ: Type,
    pub exec_start: Vec<String>,
    /// ExecStart の先頭の '-': 失敗しても failed にしない
    pub ignore_failure: bool,
    pub restart: Restart,
    pub restart_sec: f64,
    pub env: Vec<(String, String)>,
    pub workdir: Option<String>,
    pub tty: bool,
    pub wanted_by: Vec<String>,
    /// User= / Group=: このユーザー (とグループ) で動かす
    pub user: Option<String>,
    pub group: Option<String>,
    /// ConditionPathExists=: なければ起動しないで飛ばす ("!" で始まれば、あれば飛ばす)
    pub cond_paths: Vec<String>,
}

/// systemd と同じように空白で区切る (クォートは外す)
pub fn split_words(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut quote = None;
    let mut has = false;
    for c in s.chars() {
        match (quote, c) {
            (None, '"' | '\'') => {
                quote = Some(c);
                has = true;
            }
            (Some(q), _) if q == c => quote = None,
            (None, ' ' | '\t') => {
                if has {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            _ => {
                cur.push(c);
                has = true;
            }
        }
    }
    if has {
        out.push(cur);
    }
    out
}

fn parse_secs(v: &str) -> f64 {
    let v = v.trim();
    if let Some(ms) = v.strip_suffix("ms") {
        return ms.parse::<f64>().unwrap_or(100.0) / 1000.0;
    }
    v.trim_end_matches('s').parse().unwrap_or(0.1)
}

pub fn parse(name: &str, path: &str, text: &str) -> Unit {
    let mut kv: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut section = String::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            section = line.trim_matches(['[', ']']).to_string();
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            kv.entry(format!("{}.{}", section, k.trim())).or_default().push(v.trim().to_string());
        }
    }
    let one = |k: &str| kv.get(k).and_then(|v| v.last()).cloned().unwrap_or_default();
    let many = |k: &str| kv.get(k).map(|v| v.iter().flat_map(|s| s.split_whitespace().map(String::from)).collect()).unwrap_or_default();
    let mut exec = one("Service.ExecStart");
    let ignore_failure = exec.starts_with('-');
    if ignore_failure {
        exec.remove(0);
    }
    let env = kv
        .get("Service.Environment")
        .map(|v| {
            v.iter()
                .flat_map(|s| split_words(s))
                .filter_map(|w| w.split_once('=').map(|(a, b)| (a.to_string(), b.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let mut wants: Vec<String> = many("Unit.Wants");
    wants.extend(many("Unit.Requires"));
    Unit {
        name: name.to_string(),
        path: path.to_string(),
        description: one("Unit.Description"),
        after: many("Unit.After"),
        wants,
        typ: if one("Service.Type") == "oneshot" { Type::Oneshot } else { Type::Simple },
        exec_start: split_words(&exec),
        ignore_failure,
        restart: match one("Service.Restart").as_str() {
            "always" => Restart::Always,
            "on-failure" => Restart::OnFailure,
            _ => Restart::No,
        },
        restart_sec: kv.get("Service.RestartSec").map_or(0.1, |v| parse_secs(v.last().unwrap())),
        env,
        workdir: kv.get("Service.WorkingDirectory").and_then(|v| v.last().cloned()),
        tty: matches!(one("Service.StandardInput").as_str(), "tty" | "tty-force"),
        wanted_by: many("Install.WantedBy"),
        user: Some(one("Service.User")).filter(|v| !v.is_empty()),
        group: Some(one("Service.Group")).filter(|v| !v.is_empty()),
        cond_paths: kv.get("Unit.ConditionPathExists").cloned().unwrap_or_default().into_iter().filter(|v| !v.is_empty()).collect(),
    }
}

/// 名前 (.service は省略可) から
pub fn full_name(name: &str) -> String {
    if name.contains('.') { name.to_string() } else { format!("{}.service", name) }
}

/// ユニットをすべて読む (/etc が /usr/lib より優先)。
/// NAME.service.d/*.conf (drop-in) があれば、うしろに足して読む (同じ項目はあとのものが勝つ。
/// ExecStart= のように、空にしてから書きなおすのも systemd と同じ)
pub fn load_all() -> BTreeMap<String, Unit> {
    let mut texts: BTreeMap<String, (String, String)> = BTreeMap::new();
    for dir in UNIT_DIRS {
        let Ok(rd) = fs::read_dir(dir) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.ends_with(".service") {
                continue;
            }
            let path = format!("{}/{}", dir, name);
            if let Ok(text) = fs::read_to_string(&path) {
                texts.insert(name, (path, text));
            }
        }
    }
    let mut out = BTreeMap::new();
    for (name, (path, mut text)) in texts {
        let mut drops: Vec<(String, String)> = vec![];
        for dir in UNIT_DIRS {
            for e in fs::read_dir(format!("{}/{}.d", dir, name)).into_iter().flatten().flatten() {
                let f = e.file_name().to_string_lossy().to_string();
                if f.ends_with(".conf") {
                    // 同じ名前なら /etc のもの
                    drops.retain(|(n, _)| *n != f);
                    drops.push((f, e.path().to_string_lossy().to_string()));
                }
            }
        }
        drops.sort();
        for (_, p) in drops {
            if let Ok(t) = fs::read_to_string(&p) {
                text.push('\n');
                text.push_str(&t);
            }
        }
        out.insert(name.clone(), parse(&name, &path, &text));
    }
    out
}

/// multi-user.target.wants にあるもの
pub fn enabled() -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(WANTS_DIR)
        .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect())
        .unwrap_or_default();
    v.sort();
    v
}

/// UNIX 秒を "2026-09-30 10:00:00" に (UTC)
pub fn fmt_time(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    // civil_from_days (Howard Hinnant)
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC", y, m, d, rem / 3600, rem % 3600 / 60, rem % 60)
}

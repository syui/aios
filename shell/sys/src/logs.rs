// kmsg (カーネルのメッセージ、/proc/kmsg) と log (サービスのログ、/var/log/UNIT.log)
use super::{kmsg_text, read, tail};
use aish_plugin::{Value, error, json};

pub(crate) fn kmsg(a: &Value) -> Value {
    let Some(k) = kmsg_text() else { return error("no /proc/kmsg (not aios, or an older kernel)") };
    json!({ "text": tail(&k, a["grep"].as_str().unwrap_or(""), a["lines"].as_u64().unwrap_or(50) as usize) })
}

pub(crate) fn log(a: &Value) -> Value {
    let unit = a["unit"].as_str().unwrap_or("");
    if unit.is_empty() {
        let mut names: Vec<String> = std::fs::read_dir("/var/log")
            .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".log")).collect())
            .unwrap_or_default();
        names.sort();
        return json!({ "text": names.join("\n") + "\n" });
    }
    let full = if unit.contains('.') { unit.to_string() } else { format!("{}.service", unit) };
    let path = format!("/var/log/{}.log", full);
    match read(&path) {
        Some(t) => json!({ "path": path, "text": tail(&t, a["grep"].as_str().unwrap_or(""), a["lines"].as_u64().unwrap_or(50) as usize) }),
        None => error(format!("{}: no log", path)),
    }
}

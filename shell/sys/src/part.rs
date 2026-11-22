// part: カーネルの切れるところ (/proc/ai/parts) を、動かしたまま切る・入れる (ctl の part)
use super::{read, write_root};
use aish_plugin::{Value, error, json, s};
use std::path::Path;

pub(crate) fn part(a: &Value) -> Value {
    if !Path::new("/proc/ai/parts").exists() {
        return error("no /proc/ai/parts (not aios, or an older kernel)");
    }
    let name = s(a, "name");
    let mut changed = Value::Null;
    if let Some(off) = a["off"].as_bool() {
        if name.is_empty() {
            return error("name is needed with off");
        }
        if let Err(e) = write_root("/proc/ai/ctl", &format!("part {} {}", name, if off { "off" } else { "on" })) {
            return error(format!("{} (a device or aiproc cannot be changed while running; see /proc/kmsg)", e));
        }
        changed = json!({ "name": name, "off": off });
    }
    let p = match read("/proc/ai/parts").ok_or_else(|| "cannot read /proc/ai/parts".to_string()).and_then(|t| serde_json::from_str::<Value>(t.trim()).map_err(|e| e.to_string())) {
        Ok(p) => p,
        Err(e) => return error(e),
    };
    let list: Vec<&Value> = p["parts"].as_array().map(|v| v.iter().filter(|x| name.is_empty() || s(x, "name") == name).collect()).unwrap_or_default();
    let mut t = String::new();
    for x in &list {
        t.push_str(&format!("{:<18} {}{}\n", s(x, "name"), if x["off"].as_bool().unwrap_or(false) { "off" } else { "on " }, if x["runtime"].as_bool().unwrap_or(false) { "" } else { "  (boot only)" }));
    }
    json!({ "text": t, "parts": list, "changed": changed })
}

// delta: cmd の前と後の、カーネルの中の数のちがい (/proc/ai/mem、/proc/ai/fs、/proc/bkl)
use super::{ai_json, read, run, tail, timeout_of};
use aish_plugin::{Value, error, json, s};

pub(crate) fn delta(a: &Value, pwd: &str) -> Value {
    let snap = || -> Result<(Value, Value, String), String> {
        let m = ai_json("/proc/ai/mem")?;
        let f = ai_json("/proc/ai/fs")?;
        let b = read("/proc/bkl").and_then(|b| b.lines().find(|l| l.starts_with("all")).map(|l| l.trim_start_matches("all").trim().to_string())).unwrap_or_default();
        Ok((m, f, b))
    };
    let before = match snap() {
        Ok(x) => x,
        Err(e) => return error(e),
    };
    let r = match run(s(a, "cmd"), pwd, timeout_of(a)) {
        Ok(r) => r,
        Err(e) => return error(e),
    };
    let after = match snap() {
        Ok(x) => x,
        Err(e) => return error(e),
    };
    let text = delta_text(&before.0, &before.1, &after.0, &after.1);
    json!({ "text": text, "bkl": { "before": before.2, "after": after.2 }, "ran": { "status": r.0, "ms": r.3, "timeout": r.4, "out": tail(&r.1, "", 10), "err": tail(&r.2, "", 10) } })
}

/// 2 つの様子 (mem と fs) の、変わった数だけを「名前 before → after (+差)」で
fn delta_text(m0: &Value, f0: &Value, m1: &Value, f1: &Value) -> String {
    let mut t = String::new();
    let mut row = |name: &str, a: &Value, b: &Value| {
        if let (Some(x), Some(y)) = (a.as_u64(), b.as_u64())
            && x != y
        {
            t.push_str(&format!("{:<28} {:>12} → {:<12} ({:+})\n", name, x, y, y as i64 - x as i64));
        }
    };
    for (name, path) in [
        ("page cache pages", vec!["page_cache", "pages"]),
        ("page cache misses", vec!["page_cache", "misses"]),
        ("page cache shrunk", vec!["page_cache", "shrunk"]),
        ("page cache changed", vec!["page_cache", "changes"]),
        ("heap used", vec!["kernel", "heap", "used"]),
        ("heap peak", vec!["kernel", "heap", "peak"]),
        ("heap fails", vec!["kernel", "heap", "fails"]),
        ("heap grown", vec!["kernel", "heap", "grown"]),
        ("heap slow paths", vec!["kernel", "heap", "reclaims"]),
        ("heap emergencies", vec!["kernel", "heap", "emergencies"]),
        ("oom kills", vec!["kernel", "heap", "oom_kills"]),
        ("tmpfs inodes", vec!["tmpfs", "inodes"]),
        ("tmpfs pages", vec!["tmpfs", "pages"]),
        ("shared dirty", vec!["shared", "dirty"]),
        ("swap used", vec!["swap", "used"]),
        ("faults", vec!["faults", "all"]),
        ("ram free", vec!["ram", "free"]),
    ] {
        let (mut a, mut b) = (m0, m1);
        for k in &path {
            a = &a[*k];
            b = &b[*k];
        }
        row(name, a, b);
    }
    for (name, path) in [("paths hits", "hits"), ("paths misses", "misses"), ("paths sweeps", "sweeps"), ("paths invalidations", "invalidations"), ("paths entries", "entries")] {
        row(name, &f0["paths"][path], &f1["paths"][path]);
    }
    row("open files", &f0["files"]["open"], &f1["files"]["open"]);
    if let (Some(a), Some(b)) = (f0["mounts"].as_array(), f1["mounts"].as_array()) {
        for (x, y) in a.iter().zip(b) {
            if x["stats"]["cache"].is_object() {
                for k in ["blocks", "dirty_meta", "dirty_data", "hits", "misses", "readahead", "evictions", "flushes", "written_meta", "written_data", "commits"] {
                    row(&format!("{} cache {}", s(x, "target"), k), &x["stats"]["cache"][k], &y["stats"]["cache"][k]);
                }
            }
        }
    }
    if let (Some(a), Some(b)) = (f0["disk"].as_array(), f1["disk"].as_array()) {
        for (x, y) in a.iter().zip(b) {
            for k in ["reads", "read_sectors", "read_ms", "writes", "write_sectors", "write_ms"] {
                row(&format!("{} {}", s(x, "part"), k), &x[k], &y[k]);
            }
        }
    }
    if t.is_empty() {
        t.push_str("(nothing changed)\n");
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fail::errno_of;

    #[test]
    fn delta_rows() {
        let m0: Value = serde_json::from_str(include_str!("../testdata/mem.json")).unwrap();
        let f0: Value = serde_json::from_str(include_str!("../testdata/fs.json")).unwrap();
        let mut m1 = m0.clone();
        let mut f1 = f0.clone();
        m1["page_cache"]["pages"] = json!(2000);
        f1["mounts"][0]["stats"]["cache"]["hits"] = json!(6000);
        f1["disk"][1]["reads"] = json!(400);
        let t = delta_text(&m0, &f0, &m1, &f1);
        let hits = f0["mounts"][0]["stats"]["cache"]["hits"].as_u64().unwrap();
        let reads = f0["disk"][1]["reads"].as_u64().unwrap();
        assert!(t.contains(&format!("page cache pages             {:>12} → {:<12} ({:+})", m0["page_cache"]["pages"].as_u64().unwrap(), 2000, 2000 - m0["page_cache"]["pages"].as_i64().unwrap())), "{}", t);
        assert!(t.contains(&format!("/ cache hits                 {:>12} → 6000         ({:+})", hits, 6000 - hits as i64)), "{}", t);
        assert!(t.contains(&format!("/dev/vda reads               {:>12} → 400          ({:+})", reads, 400 - reads as i64)), "{}", t);
        assert_eq!(t.lines().count(), 3, "{}", t);
        assert_eq!(delta_text(&m0, &f0, &m0, &f0), "(nothing changed)\n");
        assert_eq!(errno_of("enomem"), Some(12));
        assert_eq!(errno_of("5"), Some(5));
    }
}

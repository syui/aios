// fw: ファイアウォール (/proc/ai/fw): nftables の基の鎖と決まりの当たった数と式、落としたもの、conntrack、trace (パケットを
// 流さずに決まりの道を試す)。nft があれば nft -a list ruleset も
use super::{json_lines, write_root};
use aish_plugin::{Value, error, json, s};
use std::path::Path;
use std::process::Command;

pub(crate) fn fw(a: &Value) -> Value {
    if !Path::new("/proc/ai/fw").exists() {
        return error("no /proc/ai/fw (not aios, or an older kernel)");
    }
    let mut trace_err = None;
    if let Some(t) = a["trace"].as_str().filter(|t| !t.trim().is_empty())
        && let Err(e) = write_root("/proc/ai/ctl", &format!("fw trace {}", t.trim()))
    {
        trace_err = Some(format!("{} (want: in|out eth0|lo tcp|udp|icmp SRC[:PORT] DST[:PORT] [syn|ack|synack|fin|rst])", e));
    }
    let lines = match json_lines("/proc/ai/fw") {
        Ok(l) => l,
        Err(e) => return error(e),
    };
    let ndrops = a["drops"].as_u64().unwrap_or(20) as usize;
    let want_ct = a["ct"].as_bool().unwrap_or(false);
    let pat = a["grep"].as_str().unwrap_or("");
    let u = |v: &Value| v.as_u64().unwrap_or(0);
    let mut t = String::new();
    let (mut chains, mut rules, mut drops, mut conns, mut trace, mut sets) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut head = Value::Null;
    let (mut ct, mut dtotal) = (Value::Null, 0);
    for l in lines {
        if l.get("fw").is_some() {
            head = l;
        } else if l.get("chain").is_some() {
            chains.push(l);
        } else if l.get("rule").is_some() {
            if pat.is_empty() || l.to_string().contains(pat) {
                rules.push(l);
            }
        } else if l.get("set").is_some() {
            sets.push(l);
        } else if let Some(n) = l.get("drops") {
            dtotal = n.as_u64().unwrap_or(0);
        } else if l.get("ago_ms").is_some() && l.get("hook").is_some() {
            if drops.len() < ndrops {
                drops.push(l);
            }
        } else if let Some(c) = l.get("conntrack") {
            ct = c.clone();
        } else if let Some(c) = l.get("ct") {
            if want_ct {
                conns.push(c.clone());
            }
        } else if let Some(x) = l.get("trace") {
            trace.push(x.as_str().unwrap_or("").to_string());
        }
    }
    t.push_str(&format!(
        "nftables: {}, conntrack {}, {} tables, {} rules (generation {})\n",
        if head["active"].as_bool().unwrap_or(false) { "active" } else { "no base chains (nothing is filtered)" },
        if head["conntrack"].as_bool().unwrap_or(false) { "on" } else { "off" },
        u(&head["tables"]),
        u(&head["rules"]),
        u(&head["generation"])
    ));
    for c in &chains {
        let name = s(c, "chain");
        if c.get("hook").is_some() {
            t.push_str(&format!(
                "chain {}  hook {} priority {} policy {}{}  seen {} to_policy {}\n",
                name,
                s(c, "hook"),
                c["priority"].as_i64().unwrap_or(0),
                s(c, "policy"),
                if c["dormant"].as_bool().unwrap_or(false) { " (dormant)" } else { "" },
                u(&c["seen"]),
                u(&c["to_policy"])
            ));
        } else {
            t.push_str(&format!("chain {}  (regular)\n", name));
        }
        for r in rules.iter().filter(|r| s(r, "rule") == name) {
            let ago = r["ago_ms"].as_u64().map_or(String::from("never"), |ms| format!("{} ms ago", ms));
            t.push_str(&format!("  handle {:<4} hits {:<6} {:<14} {}\n", u(&r["handle"]), u(&r["hits"]), ago, s(r, "expr")));
        }
    }
    for st in &sets {
        t.push_str(&format!("set {}  {} elements{}{}\n", s(st, "set"), u(&st["elements"]), if st["interval"].as_bool().unwrap_or(false) { " (interval)" } else { "" }, if st["map"].as_bool().unwrap_or(false) { " (map)" } else { "" }));
    }
    t.push_str(&format!("drops: {} in all{}\n", dtotal, if drops.is_empty() { "" } else { ", newest first:" }));
    for d in &drops {
        let ports = if d.get("sport").is_some() { format!(":{} > {}:{}", u(&d["sport"]), s(d, "dst"), u(&d["dport"])) } else { format!(" > {}", s(d, "dst")) };
        t.push_str(&format!(
            "  {} ms ago {} {}{} {} {}{}{} len {} by {}{}{}\n",
            u(&d["ago_ms"]),
            s(d, "hook"),
            if s(d, "iif").is_empty() { "" } else { "in " },
            if s(d, "iif").is_empty() { format!("out {}", s(d, "oif")) } else { s(d, "iif").to_string() },
            s(d, "proto"),
            s(d, "src"),
            ports,
            d.get("flags").map_or(String::new(), |f| format!(" [{}]", f.as_str().unwrap_or(""))),
            u(&d["len"]),
            s(d, "by"),
            d.get("reject").map_or(String::new(), |r| format!(", reject {}", r.as_str().unwrap_or(""))),
            d.get("uid").map_or(String::new(), |x| format!(", uid {}", x))
        ));
    }
    if ct.is_object() {
        let mut parts = Vec::new();
        if let Some(o) = ct.as_object() {
            for (k, v) in o {
                if k != "count" && k != "max" {
                    parts.push(format!("{} {}", k, v));
                }
            }
        }
        t.push_str(&format!("conntrack: {} / {}{}\n", u(&ct["count"]), u(&ct["max"]), if parts.is_empty() { String::new() } else { format!(" ({})", parts.join(", ")) }));
        for c in &conns {
            t.push_str(&format!(
                "  {} {} {}:{} > {}:{}{}{} expires {} s mark {}\n",
                s(c, "proto"),
                s(c, "state"),
                s(c, "src"),
                u(&c["sport"]),
                s(c, "dst"),
                u(&c["dport"]),
                if c["replied"].as_bool().unwrap_or(false) { "" } else { " [UNREPLIED]" },
                if c["assured"].as_bool().unwrap_or(false) { " [ASSURED]" } else { "" },
                u(&c["expires_s"]),
                u(&c["mark"])
            ));
        }
    }
    if a["trace"].as_str().is_some() {
        if let Some(e) = &trace_err {
            t.push_str(&format!("trace: {}\n", e));
        } else {
            for l in &trace {
                t.push_str(&format!("{}\n", l));
            }
        }
    }
    let mut out = json!({ "text": t, "chains": chains, "rules": rules, "drops": drops, "drops_total": dtotal, "conntrack": ct });
    if want_ct {
        out["ct"] = json!(conns);
    }
    if a["trace"].as_str().is_some() {
        out["trace"] = json!(trace);
        if let Some(e) = trace_err {
            out["trace_error"] = json!(e);
        }
    }
    if a["nft"].as_bool().unwrap_or(false) {
        let nft = ["/opt/c/bin/nft", "/usr/sbin/nft", "/usr/bin/nft"].into_iter().find(|p| Path::new(p).exists()).unwrap_or("nft");
        let o = if unsafe { libc::geteuid() } == 0 { Command::new(nft).args(["-a", "list", "ruleset"]).output() } else { Command::new("sudo").args(["-n", nft, "-a", "list", "ruleset"]).output() };
        out["nft"] = match o {
            Ok(o) if o.status.success() => json!(String::from_utf8_lossy(&o.stdout)),
            Ok(o) => json!(format!("nft: {}", String::from_utf8_lossy(&o.stderr).trim())),
            Err(e) => json!(format!("{}: {} (ap -S nftables)", nft, e)),
        };
    }
    out
}

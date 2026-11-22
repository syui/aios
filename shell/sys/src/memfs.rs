// mem (/proc/ai/mem: RAM、ヒープ、ページキャッシュ、tmpfs、スワップ、メモリの多いプロセス) と fs (/proc/ai/fs: マウントごとの
// ブロックキャッシュ・ジャーナル・inode の覚え、パスの覚え、開いているファイル、ディスクの読み書き)
use super::{ai_json, mib, pct};
use aish_plugin::{Value, error, json, s};

/// mem: /proc/ai/mem のまとめ
pub(crate) fn mem(a: &Value) -> Value {
    let mut m = match ai_json("/proc/ai/mem") {
        Ok(m) => m,
        Err(e) => return error(e),
    };
    let nfiles = a["files"].as_u64().unwrap_or(10) as usize;
    let nprocs = a["procs"].as_u64().unwrap_or(8) as usize;
    let t = mem_text(&m, nfiles, nprocs);
    if let Some(files) = m["page_cache"]["files"].as_array_mut() {
        files.truncate(nfiles);
    }
    if let Some(ps) = m["procs"].as_array_mut() {
        ps.truncate(nprocs);
    }
    m["text"] = json!(t);
    m
}

/// /proc/ai/mem の JSON を、人 (と AI) が読む形に
fn mem_text(m: &Value, nfiles: usize, nprocs: usize) -> String {
    let pg = m["page"].as_u64().unwrap_or(4096);
    let u = |v: &Value| v.as_u64().unwrap_or(0);
    let mut t = String::new();
    let r = &m["ram"];
    t.push_str(&format!("ram        {} total, {} free (reclaim below {})", mib(u(&r["total"]) * pg), mib(u(&r["free"]) * pg), mib(u(&r["min_free"]) * pg)));
    // 空きの、いちばん大きな続いたかたまり (buddy の大きさごとの数から。カーネルのヒープはこれで足す)
    if let Some((k, n)) = r["blocks"].as_array().and_then(|b| b.iter().enumerate().rev().find(|(_, n)| u(n) > 0)) {
        t.push_str(&format!(", largest free run {} x {}", mib((pg << k) as u64), u(n)));
    }
    t.push('\n');
    let h = &m["kernel"]["heap"];
    t.push_str(&format!(
        "kernel     heap {} / {} used (peak {}, free {} in {} blocks, largest {}, fails {}), stacks {}\n",
        mib(u(&h["used"])), mib(u(&h["size"])), mib(u(&h["peak"])), mib(u(&h["free"])), u(&h["free_blocks"]), mib(u(&h["largest_free"])), u(&h["fails"]), mib(u(&m["kernel"]["stacks"]))
    ));
    // 足りなくなったことがあれば (足した、遅い道、予備、oom で殺した)
    if u(&h["grown"]) + u(&h["reclaims"]) + u(&h["emergencies"]) + u(&h["oom_kills"]) > 0 {
        t.push_str(&format!(
            "           heap grew {} while running, {} slow paths, reserve used {} times ({} bytes in use now), oom killed {}\n",
            mib(u(&h["grown"])), u(&h["reclaims"]), u(&h["emergencies"]), u(&h["reserve_used"]), u(&h["oom_kills"])
        ));
    }
    let c = &m["page_cache"];
    t.push_str(&format!(
        "page cache {} ({} pages, max {}), misses {} ({} ms), shrunk {}, changed {}\n",
        mib(u(&c["pages"]) * pg), u(&c["pages"]), u(&c["max"]), u(&c["misses"]), u(&c["miss_ns"]) / 1_000_000, u(&c["shrunk"]), u(&c["changes"])
    ));
    if let Some(files) = c["files"].as_array() {
        for f in files.iter().take(nfiles) {
            let name = f["name"].as_str().map_or_else(|| format!("fs{}:ino{}", u(&f["fs"]), u(&f["ino"])), |n| n.to_string());
            t.push_str(&format!("  {:>9} {:>10}  {}\n", mib(u(&f["pages"]) * pg), mib(u(&f["size"])), name));
        }
    }
    t.push_str(&format!("shared     {} ({} dirty)\n", mib(u(&m["shared"]["pages"]) * pg), u(&m["shared"]["dirty"])));
    t.push_str(&format!("tmpfs      {} / {} limit\n", mib(u(&m["tmpfs"]["pages"]) * pg), mib(u(&m["tmpfs"]["limit"]) * pg)));
    let sw = &m["swap"];
    t.push_str(&format!("swap       {} / {} used (in {} out {} pages)\n", mib(u(&sw["used"]) * pg), mib(u(&sw["total"]) * pg), u(&sw["in"]), u(&sw["out"])));
    t.push_str(&format!("faults     {} ({} without the big lock)\n", u(&m["faults"]["all"]), u(&m["faults"]["fast"])));
    if let Some(ps) = m["procs"].as_array() {
        t.push_str("procs      rss / vsize\n");
        for p in ps.iter().take(nprocs) {
            t.push_str(&format!("  {:>6} {:>10} {:>10}  {}\n", u(&p["pid"]), mib(u(&p["rss"]) * pg), mib(u(&p["vsize"])), s(p, "name")));
        }
    }
    t
}

/// fs: /proc/ai/fs のまとめ
pub(crate) fn fs() -> Value {
    let mut f = match ai_json("/proc/ai/fs") {
        Ok(f) => f,
        Err(e) => return error(e),
    };
    f["text"] = json!(fs_text(&f));
    f
}

/// /proc/ai/fs の JSON を、人 (と AI) が読む形に
fn fs_text(f: &Value) -> String {
    let u = |v: &Value| v.as_u64().unwrap_or(0);
    let mut t = String::from("mounts\n");
    if let Some(ms) = f["mounts"].as_array() {
        for m in ms {
            t.push_str(&format!("  {:<12} {:<6} {:<12} {}", s(m, "target"), s(m, "type"), s(m, "source"), if m["ro"].as_bool().unwrap_or(false) { "ro" } else { "rw" }));
            let st = &m["stats"];
            if st.is_object() {
                if s(m, "type") == "tmpfs" {
                    t.push_str(&format!("  {} pages / {} limit", u(&st["pages"]), u(&st["limit"])));
                } else {
                    let c = &st["cache"];
                    let bs = u(&st["block_size"]).max(1);
                    t.push_str(&format!(
                        "\n    cache {} / {} blocks, dirty meta {} data {}, hit {}% ({} / {}), readahead {}, evicted {}, flushed {} (meta {} data {} blocks, last {} s ago), commits {}",
                        u(&c["blocks"]), u(&c["limit"]), u(&c["dirty_meta"]), u(&c["dirty_data"]), pct(u(&c["hits"]), u(&c["misses"])), u(&c["hits"]), u(&c["misses"]),
                        u(&c["readahead"]), u(&c["evictions"]), u(&c["flushes"]), u(&c["written_meta"]), u(&c["written_data"]), u(&c["last_flush_s"]), u(&c["commits"])
                    ));
                    if st["journal"].is_object() {
                        t.push_str(&format!("\n    journal {} blocks, next transaction {}", u(&st["journal"]["capacity"]), u(&st["journal"]["seq"])));
                    }
                    t.push_str(&format!(
                        "\n    inode cache {} / {}, name cache {} / {}, inodes in use {}, orphans {}; free {} of {} ({} inodes free)",
                        u(&st["metas"]["entries"]), u(&st["metas"]["max"]), u(&st["names"]["entries"]), u(&st["names"]["max"]), u(&st["inodes_in_use"]), u(&st["orphans"]),
                        mib(u(&st["blocks"]["free"]) * bs), mib(u(&st["blocks"]["total"]) * bs), u(&st["inodes"]["free"])
                    ));
                }
            }
            t.push('\n');
        }
    }
    let p = &f["paths"];
    t.push_str(&format!(
        "paths      {} cached ({} negative, max {}), hit {}% ({} / {}), swept {} times, invalidated {}\n",
        u(&p["entries"]), u(&p["negative"]), u(&p["max"]), pct(u(&p["hits"]), u(&p["misses"])), u(&p["hits"]), u(&p["misses"]), u(&p["sweeps"]), u(&p["invalidations"])
    ));
    t.push_str(&format!("files      {} open in {} processes\n", u(&f["files"]["open"]), u(&f["files"]["procs"])));
    if let Some(ds) = f["disk"].as_array() {
        for d in ds {
            t.push_str(&format!(
                "disk       {:<12} {} reads ({}, {} ms), {} writes ({}, {} ms)\n",
                s(d, "part"), u(&d["reads"]), mib(u(&d["read_sectors"]) * 512), u(&d["read_ms"]), u(&d["writes"]), mib(u(&d["write_sectors"]) * 512), u(&d["write_ms"])
            ));
        }
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mem_summary() {
        let m: Value = serde_json::from_str(include_str!("../testdata/mem.json")).unwrap();
        let t = mem_text(&m, 3, 2);
        assert!(t.contains("ram        2048.0 MiB total"), "{}", t);
        assert!(t.contains("kernel     heap 3.5 MiB / 64.0 MiB used"), "{}", t);
        assert!(t.contains("page cache 7.2 MiB (1844 pages, max 131072)"), "{}", t);
        assert!(t.contains("/usr/bin/aish"), "{}", t);
        assert_eq!(t.matches("/usr/").count(), 3, "{}", t);
        assert!(t.contains("procs      rss / vsize\n       7"), "{}", t);
        assert_eq!(t.lines().count(), 13, "{}", t);
        assert!(!t.contains("largest free run"), "{}", t);
        // buddy (kalloc) のかたまりの数があるカーネル: いちばん大きなものとその数
        let mut m = m;
        m["ram"]["blocks"] = json!([1, 0, 1, 0, 0, 0, 0, 0, 0, 1, 487]);
        let t = mem_text(&m, 3, 2);
        assert!(t.contains("(reclaim below 4.0 MiB), largest free run 4.0 MiB x 487\n"), "{}", t);
        m["ram"]["blocks"] = json!([3, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert!(mem_text(&m, 3, 2).contains("largest free run 8 KiB x 2\n"));
    }

    #[test]
    fn fs_summary() {
        let f: Value = serde_json::from_str(include_str!("../testdata/fs.json")).unwrap();
        let t = fs_text(&f);
        assert!(t.contains("  /            ext4   /dev/vda2    rw\n    cache 4774 / 16384 blocks"), "{}", t);
        assert!(t.contains("journal 16383 blocks"), "{}", t);
        assert!(t.contains("  /tmp         tmpfs  tmpfs        rw  49 pages / 262144 limit"), "{}", t);
        assert!(t.contains("paths      447 cached (349 negative, max 4096), hit 45%"), "{}", t);
        assert!(t.contains("files      62 open in 17 processes"), "{}", t);
        assert!(t.contains("disk       /dev/vda2"), "{}", t);
    }
}

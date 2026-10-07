// free: メモリとスワップの使いかた (/proc/meminfo から)
//   free [-k|-m|-g|-h]   既定は KiB
fn main() {
    // 読み手のいないパイプに書いたら (| head など)、ほかのコマンドと同じように静かに終わる
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let mut unit = 'k';
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "-k" | "--kibi" => unit = 'k',
            "-m" | "--mebi" => unit = 'm',
            "-g" | "--gibi" => unit = 'g',
            "-h" | "--human" => unit = 'h',
            _ => {
                eprintln!("usage: free [-k|-m|-g|-h]");
                std::process::exit(2);
            }
        }
    }
    let info = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let get = |k: &str| -> u64 {
        info.lines()
            .find_map(|l| l.strip_prefix(k).and_then(|r| r.strip_prefix(':')))
            .and_then(|r| r.split_whitespace().next())
            .and_then(|n| n.parse().ok())
            .unwrap_or(0)
    };
    let (total, free, avail) = (get("MemTotal"), get("MemFree"), get("MemAvailable"));
    let (stotal, sfree) = (get("SwapTotal"), get("SwapFree"));
    let f = |kb: u64| -> String {
        match unit {
            'm' => (kb / 1024).to_string(),
            'g' => (kb / 1024 / 1024).to_string(),
            'h' => human(kb),
            _ => kb.to_string(),
        }
    };
    println!("{:>15} {:>11} {:>11} {:>11} {:>11} {:>11}", "total", "used", "free", "shared", "buff/cache", "available");
    println!("{:<7} {:>7} {:>11} {:>11} {:>11} {:>11} {:>11}", "Mem:", f(total), f(total - free), f(free), f(0), f(0), f(avail));
    println!("{:<7} {:>7} {:>11} {:>11}", "Swap:", f(stotal), f(stotal - sfree), f(sfree));
}

fn human(kb: u64) -> String {
    let mut v = kb as f64;
    for u in ["Ki", "Mi", "Gi", "Ti"] {
        if v < 1024.0 {
            return if v < 10.0 && u != "Ki" { format!("{:.1}{}", v, u) } else { format!("{:.0}{}", v, u) };
        }
        v /= 1024.0;
    }
    format!("{:.0}Pi", v)
}

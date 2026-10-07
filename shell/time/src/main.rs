// aish-time: 長くかかったコマンドの時間と、しくじったときの終了コードを、終わったあとに 1 行で出す
// (zsh の REPORTTIME と同じ考え)。plugin aish-time [秒] で、それより長いものだけ時間を出す (既定 3 秒)
//   例: -- 12.3s
//       -- exit 1
//       -- 1m05s, exit 130
// 出すのは標準エラー (端末) で、プロンプトの前。aish --mcp では run の答えに ms と status があるので読まない
use aish_plugin::{Spec, json, s};
use std::time::Instant;

fn fmt(sec: f64) -> String {
    if sec < 60.0 {
        format!("{:.1}s", sec)
    } else if sec < 3600.0 {
        format!("{}m{:02}s", (sec / 60.0) as u64, sec as u64 % 60)
    } else {
        format!("{}h{:02}m", (sec / 3600.0) as u64, (sec as u64 % 3600) / 60)
    }
}

fn main() {
    let spec = Spec { name: "time", hooks: &["preexec", "precmd"], keys: &[], tools: &[] };
    let mut limit = 3.0f64;
    let mut start: Option<Instant> = None;
    aish_plugin::run(spec, |ev, v| {
        match ev {
            "hello" => {
                if let Some(n) = v["args"].as_array().and_then(|a| a.first()).and_then(|x| x.as_str()).and_then(|x| x.parse::<f64>().ok()) {
                    limit = n;
                }
            }
            "preexec" => {
                if !s(v, "line").trim().is_empty() {
                    start = Some(Instant::now());
                }
            }
            "precmd" => {
                // 動かしたものがなければ (Enter だけ、起きたとき) 出さない
                let Some(t) = start.take() else { return json!({}) };
                let sec = t.elapsed().as_secs_f64();
                let status = v["status"].as_i64().unwrap_or(0);
                let mut parts = Vec::new();
                if sec >= limit {
                    parts.push(fmt(sec));
                }
                // 130 (C-c) も出す: 止めたことがわかるように
                if status != 0 {
                    parts.push(format!("\x1b[31mexit {}\x1b[90m", status));
                }
                if !parts.is_empty() {
                    eprintln!("\x1b[90m-- {}\x1b[0m", parts.join(", "));
                }
            }
            _ => {}
        }
        json!({})
    });
}

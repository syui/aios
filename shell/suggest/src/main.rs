// aish-suggest: 打っている行に続く履歴を、グレーで出す (zsh-autosuggestions。aish の基本のプラグイン)
//   履歴は hello の histfile から読み、そのあとは preexec (動かした行) で足していく。
//   → や End、C-e で決めるのは aish がやる
use aish_plugin::{Spec, json, s};

fn main() {
    let spec = Spec { name: "suggest", hooks: &["suggest", "preexec"], keys: &[], tools: &[] };
    let mut history: Vec<String> = Vec::new();
    aish_plugin::run(spec, |ev, v| {
        match ev {
            "hello" => {
                history = std::fs::read_to_string(s(v, "histfile")).unwrap_or_default().lines().filter(|l| !l.is_empty()).map(String::from).collect();
            }
            "preexec" => {
                let l = s(v, "line").trim_end_matches('\n');
                if !l.trim().is_empty() && !l.contains('\n') {
                    history.retain(|h| h != l);
                    history.push(l.to_string());
                }
            }
            "suggest" => {
                // いちばん新しいもの
                let t = s(v, "line");
                if !t.is_empty()
                    && let Some(h) = history.iter().rev().find(|h| h.len() > t.len() && h.starts_with(t))
                {
                    return json!({ "suggest": &h[t.len()..] });
                }
            }
            _ => {}
        }
        json!({})
    });
}

// aish-powerline: powerline のプロンプト ([shell] リポジトリ)
//   ~/.aishrc に: plugin aish-powerline
// アイコンと区切りは aifont (Nerd Font と同じ位置) の文字。端末のフォントが要る
// ユーザーの色: ふつうは黄、ssh でつないでいるときは水色。git のディレクトリではブランチも出す
use aish_plugin::{Spec, Value, json, s, tilde};

const SEP: char = '\u{e0b0}';
const BRANCH: char = '\u{e0a0}';

fn main() {
    let spec = Spec { name: "powerline", hooks: &["prompt"], keys: &[] };
    aish_plugin::run(spec, |ev, v| match ev {
        "prompt" => json!({ "prompt": prompt(v) }),
        _ => json!({}),
    });
}

fn user_icon(user: &str) -> char {
    let is = |n: &str| user == n || user.starts_with(&format!("{}.", n));
    if is("ai") {
        '\u{e001}'
    } else if is("syui") {
        '\u{e002}'
    } else {
        '\u{276f}'
    }
}

/// pwd から上へたどって .git/HEAD のブランチ (切りはなした HEAD なら 7 桁)
fn git_branch(pwd: &str) -> Option<String> {
    let mut d = std::path::PathBuf::from(pwd);
    loop {
        if let Ok(h) = std::fs::read_to_string(d.join(".git/HEAD")) {
            let h = h.trim();
            return Some(match h.strip_prefix("ref: refs/heads/") {
                Some(b) => b.to_string(),
                None => h.chars().take(7).collect(),
            });
        }
        if !d.pop() {
            return None;
        }
    }
}

/// prompt: { pwd, home, user, ssh, ... } → プロンプトの文字列
fn prompt(v: &Value) -> String {
    let e = '\x1b';
    let c = if v["ssh"].as_bool().unwrap_or(false) { 36 } else { 33 };
    let user = s(v, "user");
    let pwd = s(v, "pwd");
    let dir = tilde(pwd, s(v, "home"));
    let mut p = format!("{e}[{c};40m {} {e}[30;48;5;234m{SEP}", user_icon(user));
    p += &format!("{e}[33;48;5;234m {user} {e}[38;5;234;48;5;236m{SEP}");
    p += &format!("{e}[37;48;5;236m {dir} ");
    match git_branch(pwd) {
        Some(b) => p += &format!("{e}[38;5;236;48;5;234m{SEP}{e}[36;48;5;234m {BRANCH} {b} {e}[0;38;5;234m{SEP}"),
        None => p += &format!("{e}[0;38;5;236m{SEP}"),
    }
    p += &format!("{e}[0m ");
    p
}

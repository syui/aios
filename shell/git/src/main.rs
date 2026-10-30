// aish-git: git のリポジトリの様子を 1 回で (端末なしの tools だけ)
//   git  ブランチと HEAD、変えたファイル、remote (とミラー) ごとの進み・おくれ、push していないコミットの
//        署名 (GPG か SSH か、どの鍵か) とコミットした人。設定 (user.email、user.signingkey、gpg.format) と
//        ちがうもの、remote のほうが進んでいるものは problems に
// クラウドの環境の git は Claude の名前と SSH の鍵で署名する設定なので、気づかずに push すると署名しなおして
// force push することになる (2026-10-30)。push の前にこれで見る
use aish_plugin::{Spec, Tool, Value, error, json, s};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const GIT: &str = r#"{"type":"object","properties":{
"dir":{"type":"string","description":"リポジトリ (既定: いまのディレクトリ)"},
"also":{"type":"array","items":{"type":"string"},"description":"remote のほかにくらべるところ (URL)。既定は git config aish.mirror (いくつでも)"},
"remote":{"type":"boolean","description":"remote とくらべる (git ls-remote。既定 true。false ならネットワークを使わない)"}}}"#;

/// push していないコミットを見るのはここまで
const MAX_COMMITS: usize = 30;

fn main() {
    let spec = Spec {
        name: "git",
        hooks: &[],
        keys: &[],
        tools: &[Tool {
            name: "git",
            desc: "git のリポジトリの様子を 1 回で: ブランチと HEAD、変えたファイル、remote (と git config aish.mirror のミラー) ごとの進み・おくれ (ls-remote なので fetch しない)、push していないコミットの署名 (gpg / ssh / none と鍵) とコミットした人。設定とちがう署名や人、remote のほうが進んでいるものは problems に。push の前に",
            input: GIT,
        }],
    };
    aish_plugin::run(spec, |ev, v| match ev {
        "tool" if s(v, "name") == "git" => state(v),
        "tool" => error(format!("{}: no such tool", s(v, "name"))),
        _ => json!({}),
    });
}

/// git を動かす (標準エラーは捨てる。署名を確かめられないときの知らせなど)
fn git(dir: &str, args: &[&str]) -> Option<String> {
    let o = Command::new("git").arg("-C").arg(dir).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim_end().to_string())
}

/// ls-remote は長くかかることがあるので、ms を過ぎたら止める
fn ls_remote(dir: &str, target: &str, branch: &str, ms: u64) -> Result<Option<String>, String> {
    let mut c = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["ls-remote", target, &format!("refs/heads/{}", branch)])
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let t0 = Instant::now();
    loop {
        match c.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if t0.elapsed() < Duration::from_millis(ms) => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = c.kill();
                let _ = c.wait();
                return Err(format!("no answer in {}s", ms / 1000));
            }
        }
    }
    let o = c.wait_with_output().map_err(|e| e.to_string())?;
    if !o.status.success() {
        return Err(String::from_utf8_lossy(&o.stderr).lines().last().unwrap_or("ls-remote failed").trim().to_string());
    }
    Ok(String::from_utf8_lossy(&o.stdout).split_whitespace().next().map(String::from))
}

/// コミットの署名の種類: コミットのオブジェクトの gpgsig を見る (確かめる設定がなくてもわかる)
fn sig_kind(raw: &str) -> &'static str {
    let head = raw.split("\n\n").next().unwrap_or("");
    if !head.contains("\ngpgsig ") {
        "none"
    } else if head.contains("BEGIN SSH SIGNATURE") {
        "ssh"
    } else if head.contains("BEGIN PGP SIGNATURE") {
        "gpg"
    } else {
        "x509"
    }
}

/// 鍵が同じか: 指紋 (40 桁) と長い ID (16 桁) をくらべる。gpg の 466554...! の ! はとる
fn same_key(want: &str, got: &[&str]) -> bool {
    let w = want.trim().trim_end_matches('!').to_ascii_uppercase();
    !w.is_empty() && got.iter().filter(|g| !g.is_empty()).any(|g| {
        let g = g.to_ascii_uppercase();
        w.ends_with(&g) || g.ends_with(&w)
    })
}

fn state(v: &Value) -> Value {
    let a = &v["args"];
    let dir = if s(a, "dir").is_empty() { s(v, "pwd").to_string() } else { s(a, "dir").to_string() };
    let Some(top) = git(&dir, &["rev-parse", "--show-toplevel"]) else {
        return error(format!("{}: not a git repository", dir));
    };
    let branch = git(&top, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    let head = git(&top, &["rev-parse", "HEAD"]).unwrap_or_default();
    let cfg = |k: &str| git(&top, &["config", "--get", k]).unwrap_or_default();
    let (name, email, key, format, sign) = (cfg("user.name"), cfg("user.email"), cfg("user.signingkey"), cfg("gpg.format"), cfg("commit.gpgsign"));
    let format = if format.is_empty() { "openpgp".to_string() } else { format };
    let want_sig = if format == "ssh" { "ssh" } else { "gpg" };
    let signing = sign == "true" || !key.is_empty();
    let mut problems: Vec<String> = Vec::new();
    // クラウドの環境の既定 (Claude の名前と SSH の鍵) のままならそう書く (持ち主の名前で署名するリポジトリでは、設定しなおす)
    let default = if name == "Claude" || email == "noreply@anthropic.com" { " (the environment's default identity)" } else { "" };
    let mut text = format!("{} {}  {} <{}>{}, signs: {}\n", branch, &head[..head.len().min(7)], name, email, default, if signing { format!("{} {}", want_sig, key) } else { "no".into() });

    // 変えたファイル
    let st = git(&top, &["status", "--porcelain=v1"]).unwrap_or_default();
    let changed: Vec<&str> = st.lines().collect();
    if !changed.is_empty() {
        text.push_str(&format!("changed ({}):\n", changed.len()));
        for l in changed.iter().take(20) {
            text.push_str(&format!("  {}\n", l));
        }
        if changed.len() > 20 {
            text.push_str(&format!("  ... {} more\n", changed.len() - 20));
        }
    }

    // remote とミラー: ls-remote でいまの先を見て、ここにあれば進み・おくれ
    let mut base: Option<String> = None;
    if a["remote"].as_bool().unwrap_or(true) && branch != "HEAD" {
        let mut targets: Vec<(String, String)> = git(&top, &["remote"]).unwrap_or_default().lines().map(|r| (r.to_string(), git(&top, &["remote", "get-url", r]).unwrap_or_default())).collect();
        let also: Vec<String> = match a["also"].as_array() {
            Some(x) => x.iter().filter_map(|u| u.as_str().map(String::from)).collect(),
            None => git(&top, &["config", "--get-all", "aish.mirror"]).unwrap_or_default().lines().map(String::from).collect(),
        };
        targets.extend(also.into_iter().filter(|u| !u.is_empty()).map(|u| (u.clone(), u)));
        if !targets.is_empty() {
            text.push_str("remotes:\n");
        }
        for (label, url) in &targets {
            let target = if label == url { url.as_str() } else { label.as_str() };
            let line = match ls_remote(&top, target, &branch, 15_000) {
                Err(e) => {
                    problems.push(format!("{}: {}", label, e));
                    format!("? ({})", e)
                }
                Ok(None) => format!("no {} there", branch),
                Ok(Some(sha)) if sha == head => "same".to_string(),
                Ok(Some(sha)) => match git(&top, &["rev-list", "--left-right", "--count", &format!("HEAD...{}", sha)]) {
                    Some(c) => {
                        let mut n = c.split_whitespace().map(|x| x.parse::<usize>().unwrap_or(0));
                        let (ahead, behind) = (n.next().unwrap_or(0), n.next().unwrap_or(0));
                        if behind > 0 {
                            problems.push(format!("{} has {} commit(s) that {} does not (merge them before pushing)", label, behind, branch));
                        }
                        if base.is_none() && behind == 0 {
                            base = Some(sha.clone());
                        }
                        match (ahead, behind) {
                            (a, 0) => format!("ahead {}", a),
                            (0, b) => format!("behind {}", b),
                            (a, b) => format!("ahead {}, behind {} (diverged)", a, b),
                        }
                    }
                    None => {
                        problems.push(format!("{} has {} at {} which is not here (fetch it)", label, branch, &sha[..sha.len().min(7)]));
                        format!("at {} (not here)", &sha[..sha.len().min(7)])
                    }
                },
            };
            text.push_str(&format!("  {:<8} {}  {}\n", if label == url { "(mirror)" } else { label.as_str() }, url, line));
        }
    }

    // push していないコミット: remote の同じ名前のブランチから先。なければ、どの remote のブランチにもないもの
    // (remote のブランチを知らなければ、うしろの 5 つを見せるだけ)
    let max = format!("--max-count={}", MAX_COMMITS);
    let remotes_known = !git(&top, &["for-each-ref", "--count=1", "refs/remotes"]).unwrap_or_default().is_empty();
    let (label, list) = match &base {
        Some(b) => ("not pushed", git(&top, &["rev-list", &max, &format!("{}..HEAD", b)])),
        None if remotes_known => ("on no remote branch", git(&top, &["rev-list", &max, "HEAD", "--not", "--remotes"])),
        None => ("recent", git(&top, &["rev-list", "--max-count=5", "HEAD"])),
    };
    let list = list.unwrap_or_default();
    let check = label != "recent";
    let commits: Vec<&str> = list.lines().collect();
    if !commits.is_empty() {
        text.push_str(&format!("{} ({}):\n", label, commits.len()));
    }
    for c in commits {
        let info = git(&top, &["log", "-1", "--format=%h%x00%cn <%ce>%x00%G?%x00%GK%x00%GF%x00%s", c]).unwrap_or_default();
        let f: Vec<&str> = info.split('\0').collect();
        let (short, who, gk, gf, subj) = (f.first().copied().unwrap_or(""), f.get(1).copied().unwrap_or(""), f.get(3).copied().unwrap_or(""), f.get(4).copied().unwrap_or(""), f.get(5).copied().unwrap_or(""));
        let kind = sig_kind(&git(&top, &["cat-file", "commit", c]).unwrap_or_default());
        let keyid = if kind == "gpg" { if gf.is_empty() { gk } else { gf } } else { "" };
        text.push_str(&format!("  {} {:<4} {} {}  {}\n", short, kind, if keyid.is_empty() { "-" } else { &keyid[keyid.len().saturating_sub(16)..] }, who, subj));
        // push していないものだけ確かめる (recent は前のものなので見せるだけ)
        if !check {
            continue;
        }
        if signing && kind == "none" {
            problems.push(format!("{} is not signed", short));
        } else if signing && kind != want_sig {
            problems.push(format!("{} is signed with {}, but this repository signs with {} (gpg.format {})", short, kind, want_sig, format));
        } else if kind == "gpg" && !key.is_empty() && !same_key(&key, &[gk, gf]) {
            problems.push(format!("{} is signed by {}, not by user.signingkey {}", short, if gk.is_empty() { "an unknown key" } else { gk }, key));
        }
        if !email.is_empty() && !who.ends_with(&format!("<{}>", email)) {
            problems.push(format!("{} was committed by {}, not {} <{}> (git config)", short, who, name, email));
        }
    }
    let mut r = json!({ "text": text, "ok": problems.is_empty() });
    if !problems.is_empty() {
        r["problems"] = json!(problems);
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert_eq!(sig_kind("tree x\nauthor a\ncommitter c\n\nmsg\n"), "none");
        assert_eq!(sig_kind("tree x\ncommitter c\ngpgsig -----BEGIN PGP SIGNATURE-----\n x\n -----END PGP SIGNATURE-----\n\nmsg"), "gpg");
        assert_eq!(sig_kind("tree x\ncommitter c\ngpgsig -----BEGIN SSH SIGNATURE-----\n x\n\nmsg"), "ssh");
        // 本文に書いてあるだけなら署名ではない
        assert_eq!(sig_kind("tree x\ncommitter c\n\ngpgsig -----BEGIN PGP SIGNATURE-----\n"), "none");
    }

    #[test]
    fn keys() {
        assert!(same_key("F33093BEA3A4BEA4521901A17CBAC5071C3EB395", &["7CBAC5071C3EB395", ""]));
        assert!(same_key("466554110F5881784365F04A924183B3813D5C25!", &["", "466554110f5881784365f04a924183b3813d5c25"]));
        assert!(!same_key("F33093BEA3A4BEA4521901A17CBAC5071C3EB395", &["924183B3813D5C25", ""]));
        assert!(!same_key("", &["7CBAC5071C3EB395"]));
    }
}

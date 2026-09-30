// which: PATH からコマンドを探して、見つかったパスを出す
//   which [-a] NAME...   -a は見つかったものをぜんぶ
use std::os::unix::fs::PermissionsExt;

fn main() {
    let mut all = false;
    let mut names = vec![];
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "-a" => all = true,
            _ => names.push(a),
        }
    }
    if names.is_empty() {
        eprintln!("usage: which [-a] name...");
        std::process::exit(2);
    }
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into());
    let mut missing = false;
    for n in &names {
        let found: Vec<String> = if n.contains('/') {
            vec![n.clone()].into_iter().filter(|p| executable(p)).collect()
        } else {
            path.split(':').map(|d| format!("{}/{}", if d.is_empty() { "." } else { d }, n)).filter(|p| executable(p)).collect()
        };
        if found.is_empty() {
            missing = true;
        }
        for p in found.iter().take(if all { usize::MAX } else { 1 }) {
            println!("{}", p);
        }
    }
    std::process::exit(missing as i32);
}

fn executable(p: &str) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

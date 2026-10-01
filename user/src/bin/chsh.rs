// chsh: ログインシェルを変える (/etc/passwd の 7 つ目)
//   chsh -s SHELL [USER]   SHELL は /etc/shells にあるもの。USER がなければ自分
//   chsh -l                使えるシェルの一覧
// root か、自分のシェルを変えるとき (passwd を書くので root で動かす: sudo chsh ...)
use std::fs;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let shells: Vec<String> = fs::read_to_string("/etc/shells").unwrap_or_default().lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty() && !l.starts_with('#')).collect();
    if args.first().map(|s| s.as_str()) == Some("-l") {
        for s in &shells {
            println!("{}", s);
        }
        return;
    }
    let (shell, user) = match args.as_slice() {
        [s, sh] if s == "-s" => (sh.clone(), None),
        [s, sh, u] if s == "-s" => (sh.clone(), Some(u.clone())),
        _ => die("usage: chsh -s SHELL [USER] | chsh -l"),
    };
    if !shells.contains(&shell) {
        die(&format!("chsh: {} is not in /etc/shells", shell));
    }
    let uid = unsafe { libc::getuid() };
    let me = fs::read_to_string("/etc/passwd").unwrap_or_default().lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.len() > 6 && f[2].parse() == Ok(uid)).then(|| f[0].to_string())
    });
    let user = user.or(me).unwrap_or_else(|| die("chsh: who are you?"));
    if uid != 0 && unsafe { libc::geteuid() } != 0 {
        die("chsh: must be root (sudo chsh -s SHELL USER)");
    }
    let text = fs::read_to_string("/etc/passwd").unwrap_or_else(|e| die(&format!("chsh: /etc/passwd: {}", e)));
    let mut found = false;
    let out: Vec<String> = text
        .lines()
        .map(|l| {
            let mut f: Vec<&str> = l.split(':').collect();
            if f.len() == 7 && f[0] == user {
                found = true;
                f[6] = &shell;
            }
            f.join(":")
        })
        .collect();
    if !found {
        die(&format!("chsh: no such user: {}", user));
    }
    // 書きかけで止まっても壊れないよう、別のファイルに書いてから置きかえる
    let tmp = "/etc/passwd.chsh";
    if let Err(e) = fs::write(tmp, out.join("\n") + "\n").and_then(|_| fs::rename(tmp, "/etc/passwd")) {
        die(&format!("chsh: {}", e));
    }
    println!("{}: {}", user, shell);
}

fn die(msg: &str) -> ! {
    eprintln!("{}", msg);
    std::process::exit(1);
}

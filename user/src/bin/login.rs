// login [-f USER]: ユーザーを確かめてシェルを動かす (root で動かす)
//   -f USER は確かめずにログインする (自動ログイン用)
#[path = "../lib/crypt.rs"]
mod crypt;
#[path = "../lib/term.rs"]
mod term;
#[path = "../lib/users.rs"]
mod users;

use std::io::{self, BufRead, Write};

fn main() {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("login: must be run as root");
        std::process::exit(1);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let forced = args.iter().position(|a| a == "-f").and_then(|i| args.get(i + 1)).cloned();
    let user = match forced {
        Some(name) => users::by_name(&name).unwrap_or_else(|| {
            eprintln!("login: no such user: {}", name);
            std::process::exit(1);
        }),
        None => loop {
            print!("aios login: ");
            io::stdout().flush().ok();
            let mut name = String::new();
            if io::stdin().lock().read_line(&mut name).unwrap_or(0) == 0 {
                std::process::exit(1);
            }
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            let pw = users::read_password("Password: ").unwrap_or_default();
            let ok = users::by_name(name).filter(|_| users::shadow_hash(name).is_some_and(|h| crypt::verify(&pw, &h)));
            match ok {
                Some(u) => break u,
                None => {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    println!("Login incorrect");
                }
            }
        },
    };
    // シリアルのコンソールは画面の大きさを知らない (24x80) ので、端末に聞いて合わせる
    term::fit(0);
    users::make_home(&user);
    if let Err(e) = users::become_user(&user) {
        eprintln!("login: {}", e);
        std::process::exit(1);
    }
    users::exec_shell(&user, true, None);
}

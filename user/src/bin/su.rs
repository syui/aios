// su [-] [USER] [-c CMD]: ほかのユーザーになる (setuid root)
//   root からは確かめない。それ以外はなる先のユーザーのパスワード
//   (root のパスワードはロックしてあるので、root になるには sudo を使う)
#[path = "../lib/crypt.rs"]
mod crypt;
#[path = "../lib/users.rs"]
mod users;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let login = args.iter().any(|a| a == "-" || a == "-l" || a == "--login");
    let cmd = args.iter().position(|a| a == "-c").and_then(|i| args.get(i + 1)).cloned();
    let skip: Vec<usize> = args.iter().position(|a| a == "-c").map(|i| vec![i, i + 1]).unwrap_or_default();
    let name = args
        .iter()
        .enumerate()
        .find(|(i, a)| !a.starts_with('-') && !skip.contains(i))
        .map(|(_, a)| a.clone())
        .unwrap_or_else(|| "root".into());

    let ruid = unsafe { libc::getuid() };
    let Some(target) = users::by_name(&name) else {
        eprintln!("su: user {} does not exist", name);
        std::process::exit(1);
    };
    if ruid != 0 {
        let hash = users::shadow_hash(&name).unwrap_or_default();
        let pw = users::read_password("Password: ").unwrap_or_default();
        if !crypt::verify(&pw, &hash) {
            std::thread::sleep(std::time::Duration::from_secs(1));
            eprintln!("su: Authentication failure");
            std::process::exit(1);
        }
    }
    if login {
        users::make_home(&target);
    }
    if let Err(e) = users::become_user(&target) {
        eprintln!("su: {}", e);
        std::process::exit(1);
    }
    users::exec_shell(&target, login, cmd.as_deref());
}

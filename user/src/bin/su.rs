// su [-] [USER] [-c CMD]: ほかのユーザーになる (setuid root)
//   root からは確かめない。wheel グループの人が root になるときも確かめない
//   (pam_wheel の trust と同じ。sudo-rs が入るまでのつなぎ)。それ以外はそのユーザーのパスワード
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
    let me = users::by_uid(ruid);
    let Some(target) = users::by_name(&name) else {
        eprintln!("su: user {} does not exist", name);
        std::process::exit(1);
    };
    let trusted = ruid == 0 || (target.uid == 0 && me.as_ref().is_some_and(|m| users::in_group(m, "wheel")));
    if !trusted {
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

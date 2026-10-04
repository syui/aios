// passwd [USER]: パスワードを変える (setuid root)
//   root は誰のでも、ほかは自分のだけ (今のパスワードを確かめる)
#[path = "../lib/crypt.rs"]
mod crypt;
#[path = "../lib/users.rs"]
mod users;

fn main() {
    let uid = unsafe { libc::getuid() };
    let me = users::by_uid(uid).unwrap_or_else(|| {
        eprintln!("passwd: who are you? (uid {})", uid);
        std::process::exit(1);
    });
    let target = std::env::args().nth(1).unwrap_or_else(|| me.name.clone());
    if target != me.name && uid != 0 {
        eprintln!("passwd: You may not view or modify password information for {}.", target);
        std::process::exit(1);
    }
    if users::by_name(&target).is_none() {
        eprintln!("passwd: user '{}' does not exist", target);
        std::process::exit(1);
    }
    println!("Changing password for {}.", target);
    if uid != 0 {
        let cur = users::shadow_hash(&target).unwrap_or_default();
        // ロックされている (! や *、空): いまのパスワードはないので、root に決めてもらう
        if !cur.starts_with('$') {
            eprintln!("passwd: {} has no password yet (locked). Set it as root: sudo passwd {}", target, target);
            std::process::exit(1);
        }
        let pw = users::read_password("Current password: ").unwrap_or_default();
        if !crypt::verify(&pw, &cur) {
            eprintln!("passwd: Authentication token manipulation error");
            std::process::exit(1);
        }
    }
    let a = users::read_password("New password: ").unwrap_or_default();
    let b = users::read_password("Retype new password: ").unwrap_or_default();
    if a != b {
        eprintln!("Sorry, passwords do not match.");
        std::process::exit(1);
    }
    if a.is_empty() {
        eprintln!("No password has been supplied.");
        std::process::exit(1);
    }
    if let Err(e) = users::set_shadow_hash(&target, &crypt::hash(&a)) {
        eprintln!("passwd: {}", e);
        std::process::exit(1);
    }
    println!("passwd: password updated successfully");
}

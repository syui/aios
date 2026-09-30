// systemctl: init (pid 1) にサービスの操作を頼む
//   systemctl [start|stop|restart|status|is-active|enable|disable|list-units|daemon-reload|poweroff|reboot] [unit...]
//   poweroff / reboot という名前で呼ばれたらそれを頼む
#[path = "../lib/unit.rs"]
mod unit;

use std::ffi::CString;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::symlink;
use std::process::exit;

/// init に 1 つ頼んで、(終了コード, 本文) を受け取る
fn ask(cmd: &str, arg: &str) -> (i32, String) {
    // 返事の FIFO は誰でも作れる /tmp に。init はその持ち主で頼んだ人を見分ける
    let reply = format!("/tmp/.systemctl.{}", std::process::id());
    let c = CString::new(reply.as_str()).unwrap();
    unsafe {
        libc::unlink(c.as_ptr());
        if libc::mknod(c.as_ptr(), libc::S_IFIFO | 0o600, 0) != 0 {
            eprintln!("systemctl: cannot create {}", reply);
            exit(1);
        }
    }
    let sent = fs::OpenOptions::new().write(true).open(unit::CTL).and_then(|mut f| writeln!(f, "{} {} {}", cmd, arg, reply));
    if let Err(e) = sent {
        let _ = fs::remove_file(&reply);
        eprintln!("systemctl: init is not reachable ({}): {}", unit::CTL, e);
        exit(1);
    }
    let mut text = String::new();
    let _ = fs::File::open(&reply).and_then(|mut f| f.read_to_string(&mut text));
    let _ = fs::remove_file(&reply);
    let (code, body) = text.split_once('\n').unwrap_or((&text, ""));
    (code.trim().parse().unwrap_or(1), body.to_string())
}

fn find_unit(name: &str) -> Option<unit::Unit> {
    unit::load_all().remove(&unit::full_name(name))
}

fn enable(name: &str) -> i32 {
    let Some(u) = find_unit(name) else {
        eprintln!("Failed to enable unit: Unit file {} does not exist.", unit::full_name(name));
        return 1;
    };
    if u.wanted_by.is_empty() {
        eprintln!("The unit files have no installation config (WantedBy=...).");
        return 1;
    }
    for t in &u.wanted_by {
        let dir = format!("/etc/systemd/system/{}.wants", t);
        let link = format!("{}/{}", dir, u.name);
        let _ = fs::create_dir_all(&dir);
        if fs::symlink_metadata(&link).is_err() {
            match symlink(&u.path, &link) {
                Ok(()) => println!("Created symlink {} → {}.", link, u.path),
                Err(e) => {
                    eprintln!("Failed to enable unit: {}: {}", link, e);
                    return 1;
                }
            }
        }
    }
    0
}

fn disable(name: &str) -> i32 {
    let full = unit::full_name(name);
    let Ok(rd) = fs::read_dir("/etc/systemd/system") else { return 0 };
    for e in rd.flatten() {
        let link = e.path().join(&full);
        if e.file_name().to_string_lossy().ends_with(".wants") && fs::symlink_metadata(&link).is_ok() {
            let _ = fs::remove_file(&link);
            println!("Removed {}.", link.display());
        }
    }
    0
}

fn main() {
    // 読み手のいないパイプに書いたら、ほかのコマンドと同じように静かに終わる
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let mut args: Vec<String> = std::env::args().collect();
    let prog = args.remove(0);
    let prog = prog.rsplit('/').next().unwrap_or(&prog).to_string();
    let now = args.iter().any(|a| a == "--now");
    args.retain(|a| !a.starts_with("--"));
    let (cmd, units) = match prog.as_str() {
        "poweroff" | "reboot" | "halt" => (prog.clone(), vec![]),
        _ => match args.split_first() {
            Some((c, rest)) => (c.clone(), rest.to_vec()),
            None => ("list-units".into(), vec![]),
        },
    };
    let mut code = 0;
    match cmd.as_str() {
        "enable" | "disable" => {
            for u in &units {
                code |= if cmd == "enable" { enable(u) } else { disable(u) };
                if now {
                    let (c, out) = ask(if cmd == "enable" { "start" } else { "stop" }, u);
                    print!("{}", out);
                    code |= c;
                }
            }
            if code == 0 {
                ask("daemon-reload", "-");
            }
        }
        "list-units" | "daemon-reload" | "poweroff" | "reboot" | "halt" => {
            let (c, out) = ask(&cmd, "-");
            print!("{}", out);
            code = c;
        }
        "start" | "stop" | "restart" | "status" | "is-active" => {
            if units.is_empty() {
                eprintln!("Too few arguments.");
                exit(1);
            }
            for u in &units {
                let (c, out) = ask(&cmd, u);
                print!("{}", out);
                code = code.max(c);
            }
        }
        _ => {
            eprintln!("Unknown command verb '{}'.", cmd);
            code = 1;
        }
    }
    exit(code);
}

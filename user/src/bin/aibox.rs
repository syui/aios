// aibox: コマンドを砂場 (landlock と seccomp) の中で動かす
//   aibox [-w PATH]... [-n PORT]... [--no-net] [--tmp] [--root] [--deny SYSCALL,...] [--kill SYSCALL,...] [-v] [--] CMD [ARG]...
//   読む・動かすのはどこでも。書く (作る・消す・名前を変える) のは、いまのディレクトリ、/tmp、/dev と -w の下だけ。
//   --no-net でネットワークを分ける (NET の namespace: 中どうしの 127.0.0.1 の TCP のほかは、TCP も UDP も
//   どこへもとどかない)。-n PORT でその口だけつなげる (TCP だけ。--no-net がなくても、-n があればそれだけ)。
//   ホスト名も分ける (UTS の namespace。中で変えても外には見えない)。プロセスの番号も分ける (PID の namespace:
//   CMD は中の 1 番で、外のプロセスは見えず kill もできない。aibox は外で待って、CMD の終わりかたで終わる)。
//   --tmp で /tmp を自分だけのもの (空の tmpfs) にする (マウントの namespace。外の /tmp は見えず、中で作ったものは
//   終わると消える)。マウントの表はいつも分ける。
//   --root で中では root (uid 0 / gid 0) に見せる (ユーザーの namespace: 本当は外の自分のままなので、できることは
//   ふえない。root でないと動かないと言うインストーラなどを砂場で動かすため)
//   システムコール: ptrace、mount、モジュールの読みこみ、reboot など、カーネルの深いところにさわるもの
//   (seccomp.rs の DEFAULT_DENY) は EPERM。--deny で足し (名前か番号)、--kill のものは呼んだら止める
//   砂場は子にも引き継がれ、外せない。sudo (setuid) も効かなくなる
//   CMD がなければ $SHELL (なければ /bin/sh)。-w の ~ はホーム
//   /etc/claude-code/managed-mcp.json は aish --mcp をこれで起こす (Claude が動かすものはみな砂場の中)。
//   root のする操作は aios do (aiosd が wheel の人かを見てする) を通す
#[path = "../lib/landlock.rs"]
mod landlock;
#[path = "../lib/seccomp.rs"]
mod seccomp;

use std::os::unix::process::CommandExt;

/// ユーザーの namespace に入り、外の自分を中の root に見せる (gid_map の前に setgroups を deny: Linux と同じ決まり)
fn enter_userns() -> std::io::Result<()> {
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    if unsafe { libc::unshare(libc::CLONE_NEWUSER) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    std::fs::write("/proc/self/uid_map", format!("0 {} 1\n", uid))?;
    std::fs::write("/proc/self/setgroups", "deny")?;
    std::fs::write("/proc/self/gid_map", format!("0 {} 1\n", gid))?;
    Ok(())
}

fn usage() -> ! {
    eprintln!("usage: aibox [-w PATH]... [-n PORT]... [--no-net] [--tmp] [--root] [--deny SYSCALL,...] [--kill SYSCALL,...] [-v] [--] CMD [ARG]...");
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut write = landlock::default_write();
    let mut ports: Option<Vec<u16>> = None;
    let mut verbose = false;
    let mut private_tmp = false;
    let mut as_root = false;
    let mut deny: Vec<u32> = seccomp::DEFAULT_DENY.iter().filter_map(|n| seccomp::number(n)).collect();
    let mut kill: Vec<u32> = vec![];
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-w" => {
                i += 1;
                let w = args.get(i).cloned().unwrap_or_else(|| usage());
                // ~ はホーム (JSON の設定から呼ばれても、シェルが広げないので)
                let w = match (w.strip_prefix('~'), std::env::var("HOME")) {
                    (Some(rest), Ok(h)) if rest.is_empty() || rest.starts_with('/') => format!("{}{}", h, rest),
                    _ => w,
                };
                write.push(w);
            }
            "-n" => {
                i += 1;
                let p = args.get(i).and_then(|p| p.parse().ok()).unwrap_or_else(|| usage());
                ports.get_or_insert_with(Vec::new).push(p);
            }
            "--no-net" => {
                ports.get_or_insert_with(Vec::new);
            }
            "--tmp" => private_tmp = true,
            "--root" => as_root = true,
            "--deny" | "--kill" => {
                let opt = args[i].clone();
                i += 1;
                for n in args.get(i).unwrap_or_else(|| usage()).split(',').filter(|n| !n.is_empty()) {
                    let Some(v) = seccomp::number(n) else {
                        eprintln!("aibox: {}: unknown system call", n);
                        std::process::exit(2);
                    };
                    if opt == "--deny" { deny.push(v) } else { kill.push(v) }
                }
            }
            "-v" => verbose = true,
            "-h" | "--help" => usage(),
            "--" => {
                i += 1;
                break;
            }
            a if a.starts_with('-') => usage(),
            _ => break,
        }
        i += 1;
    }
    let cmd: Vec<String> = if i < args.len() { args[i..].to_vec() } else { vec![std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())] };
    // --root: ユーザーの namespace を作って、自分の uid / gid を中の 0 に (ほかの namespace より先に)
    if as_root && let Err(e) = enter_userns() {
        eprintln!("aibox: user namespace: {}", e);
        std::process::exit(1);
    }
    // マウントの表を分けて、--tmp なら /tmp に空の tmpfs を (landlock の前に: 砂場の中からはマウントできない)
    unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if unsafe { libc::unshare(libc::CLONE_NEWNS) } != 0 {
        eprintln!("aibox: unshare: {}", std::io::Error::last_os_error());
        std::process::exit(1);
    }
    if private_tmp {
        let r = unsafe { libc::mount(c"tmpfs".as_ptr(), c"/tmp".as_ptr(), c"tmpfs".as_ptr(), 0, std::ptr::null()) };
        if r != 0 {
            eprintln!("aibox: mount /tmp: {}", std::io::Error::last_os_error());
            std::process::exit(1);
        }
    }
    match landlock::restrict(&write, ports.as_deref()) {
        Ok(missing) => {
            for m in &missing {
                eprintln!("aibox: {}: not found (not writable)", m);
            }
            if verbose {
                let net = match &ports {
                    None => "any".to_string(),
                    Some(p) if p.is_empty() => "none".to_string(),
                    Some(p) => p.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(","),
                };
                eprintln!("aibox: write {} / tcp {}", write.iter().filter(|w| !missing.contains(w)).cloned().collect::<Vec<_>>().join(" "), net);
            }
        }
        Err(e) => {
            eprintln!("aibox: {}", e);
            std::process::exit(1);
        }
    }
    // namespace を分ける (no_new_privs のあとで): UTS はいつも、NET は --no-net (ポートの指定なし) のとき
    let mut ns = libc::CLONE_NEWUTS | libc::CLONE_NEWPID;
    if ports.as_ref().is_some_and(|p| p.is_empty()) {
        ns |= libc::CLONE_NEWNET;
    }
    if unsafe { libc::unshare(ns) } != 0 {
        eprintln!("aibox: unshare: {}", std::io::Error::last_os_error());
        std::process::exit(1);
    }
    // システムコールをしぼる (landlock が no_new_privs をつけたあとで)
    if let Err(e) = seccomp::restrict(&deny, &kill) {
        eprintln!("aibox: {}", e);
        std::process::exit(1);
    }
    if verbose {
        eprintln!("aibox: seccomp deny {} kill {}", deny.len(), kill.len());
    }
    // 中のプログラムが「どこに書けるか」を知れるように (aish --mcp が Permission denied のときに教える)
    let writable: Vec<String> = write.iter().filter(|w| std::path::Path::new(w).exists()).cloned().collect();
    // PID の namespace は、このあと作る子から: 子 (中の 1 番) が CMD になり、ここは待つ
    let child = unsafe { libc::fork() };
    if child < 0 {
        eprintln!("aibox: fork: {}", std::io::Error::last_os_error());
        std::process::exit(1);
    }
    if child == 0 {
        let e = std::process::Command::new(&cmd[0]).args(&cmd[1..]).env("AIBOX_WRITE", writable.join(":")).exec();
        eprintln!("aibox: {}: {}", cmd[0], e);
        std::process::exit(127);
    }
    wait_child(child)
}

static CHILD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

extern "C" fn forward(sig: libc::c_int) {
    let c = CHILD.load(std::sync::atomic::Ordering::Relaxed);
    if c > 0 {
        unsafe { libc::kill(c, sig) };
    }
}

/// 子 (CMD) を待ち、同じ終わりかたで終わる。端末の Ctrl-C などは子にも届くのでここでは無視し、
/// TERM と HUP (ここだけに来たもの) は子に渡す
fn wait_child(child: libc::pid_t) -> ! {
    CHILD.store(child, std::sync::atomic::Ordering::Relaxed);
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_IGN);
        libc::signal(libc::SIGQUIT, libc::SIG_IGN);
        libc::signal(libc::SIGTERM, forward as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, forward as *const () as libc::sighandler_t);
    }
    loop {
        let mut st = 0;
        let r = unsafe { libc::waitpid(child, &mut st, 0) };
        if r < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            std::process::exit(1);
        }
        if libc::WIFEXITED(st) {
            std::process::exit(libc::WEXITSTATUS(st));
        }
        if libc::WIFSIGNALED(st) {
            // 同じシグナルで終わる (呼んだシェルが「Killed」などと言えるように)
            let sig = libc::WTERMSIG(st);
            unsafe {
                libc::signal(sig, libc::SIG_DFL);
                libc::kill(libc::getpid(), sig);
            }
            std::process::exit(128 + sig);
        }
    }
}

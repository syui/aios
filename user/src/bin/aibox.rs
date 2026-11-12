// aibox: コマンドを砂場 (landlock と seccomp) の中で動かす
//   aibox [-w PATH]... [-W PATH]... [-n PORT]... [--no-net] [--tmp] [--root] [--deny SYSCALL,...] [--kill SYSCALL,...] [-v] [--] CMD [ARG]...
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
//   -W PATH は -w と同じだが、なければだまって飛ばす (~/.cargo のように、入れたら使うもの。-w はなければ知らせる)
//   CMD がなければ $SHELL (なければ /bin/sh)。-w と -W の ~ はホーム
//   /etc/claude-code/managed-mcp.json は aish --mcp をこれで起こす (Claude が動かすものはみな砂場の中)。
//   root のする操作は aios do (aiosd が wheel の人かを見てする) を通す
//
//   aibox [-b HOST:PATH[:ro]]... [-e K[=V]]... [-u USER] [--entrypoint CMD] [--no-net] [-n PORT]... [-v] --image IMAGE [ARG]...
//   container image (OCI) をそのまま動かす (docker run のかわり。root も docker もいらない)。IMAGE は
//   ap image -o X.tar で作ったもの (OCI のアーカイブ。置き場に入れる)、OCI layout のディレクトリ (パスは / ./ ../ で
//   始めるか .tar で終わる)、置き場の名前
//   (ghcr.io/syui/aios-node:26.13.1、alpine:3.22。なければ ap image pull で取る)。層は ~/.cache/aios/rootfs に一度だけひろげる。
//   ユーザーの namespace で自分を image の User (の uid と gid) に見せ、マウントの namespace で image の根を読むだけにして
//   chroot する。/tmp と /dev は空の tmpfs (/dev は null zero full random urandom tty だけ)、/proc はこの中のもの、
//   /etc/hosts と /etc/resolv.conf は外のもの、家 (/etc/passwd の) が空のディレクトリなら tmpfs。-b で外のパスを中へ
//   (:ro で読むだけ)。書けるのは /tmp、/dev、家、-b のところだけ (landlock)。PID と UTS (ホスト名は image の名前) も分け、
//   システムコールも同じようにしぼる。IMAGE のあとはみな ARG (docker run と同じ): image の Cmd のかわり (Entrypoint のあとに)
#[path = "../lib/landlock.rs"]
mod landlock;
#[path = "../lib/ocistore.rs"]
mod ocistore;
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
    eprintln!("usage: aibox [-w PATH]... [-W PATH]... [-n PORT]... [--no-net] [--tmp] [--root] [--deny SYSCALL,...] [--kill SYSCALL,...] [-v] [--] CMD [ARG]...");
    eprintln!("       aibox [-b HOST:PATH[:ro]]... [-e K[=V]]... [-u USER] [--entrypoint CMD] [--no-net] [-n PORT]... [-v] --image IMAGE [ARG]...");
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
    // -W のもの (なくても知らせない)
    let mut quiet: Vec<String> = vec![];
    let mut img = Image::default();
    let mut host_only = false;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            o @ ("-w" | "-W") => {
                host_only = true;
                let quietly = o == "-W";
                i += 1;
                let w = args.get(i).cloned().unwrap_or_else(|| usage());
                // ~ はホーム (JSON の設定から呼ばれても、シェルが広げないので)
                let w = match (w.strip_prefix('~'), std::env::var("HOME")) {
                    (Some(rest), Ok(h)) if rest.is_empty() || rest.starts_with('/') => format!("{}{}", h, rest),
                    _ => w,
                };
                if quietly {
                    quiet.push(w.clone());
                }
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
            "--root" => {
                host_only = true;
                as_root = true;
            }
            o @ ("--image" | "-b" | "-e" | "-u" | "--entrypoint") => {
                i += 1;
                let v = args.get(i).cloned().unwrap_or_else(|| usage());
                match o {
                    // IMAGE のあとはみな image へ (docker run と同じ。-- があれば飛ばす)
                    "--image" => {
                        img.image = Some(v);
                        i += 1;
                        if args.get(i).map(String::as_str) == Some("--") {
                            i += 1;
                        }
                        break;
                    }
                    "-b" => {
                        let mut f = v.splitn(3, ':');
                        let (h, c, m) = (f.next().unwrap_or(""), f.next().unwrap_or(""), f.next().unwrap_or(""));
                        if h.is_empty() || !c.starts_with('/') || !matches!(m, "" | "ro" | "rw") {
                            eprintln!("aibox: -b {}: HOST:PATH[:ro] (PATH is absolute)", v);
                            std::process::exit(2);
                        }
                        let h = std::fs::canonicalize(h).unwrap_or_else(|e| {
                            eprintln!("aibox: -b {}: {}", h, e);
                            std::process::exit(2);
                        });
                        img.binds.push((h.display().to_string(), c.to_string(), m == "ro"));
                    }
                    "-e" => img.env.push(v),
                    "-u" => img.user = Some(v),
                    _ => img.entrypoint = Some(v),
                }
            }
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
    if img.image.is_some() {
        if host_only {
            eprintln!("aibox: -w, -W and --root are not for --image (use -b HOST:PATH)");
            std::process::exit(2);
        }
        run_image(&img, &args[i..], ports.as_deref(), &deny, &kill, verbose);
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
            for m in missing.iter().filter(|m| !quiet.contains(m)) {
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

// ---- --image: container image を動かす ----

#[derive(Default)]
struct Image {
    image: Option<String>,
    /// (外のパス, 中のパス, 読むだけ)
    binds: Vec<(String, String, bool)>,
    env: Vec<String>,
    user: Option<String>,
    entrypoint: Option<String>,
}

fn fail(what: impl std::fmt::Display) -> ! {
    eprintln!("aibox: {}", what);
    std::process::exit(125);
}

fn cstr(s: &str) -> std::ffi::CString {
    std::ffi::CString::new(s).unwrap_or_else(|_| fail(format!("{}: NUL in path", s)))
}

fn mount(src: Option<&str>, target: &std::path::Path, fstype: Option<&str>, flags: libc::c_ulong) -> std::io::Result<()> {
    let src = src.map(cstr);
    let t = cstr(&target.display().to_string());
    let ty = fstype.map(cstr);
    let r = unsafe { libc::mount(src.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()), t.as_ptr(), ty.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()), flags, std::ptr::null()) };
    if r != 0 { Err(std::io::Error::last_os_error()) } else { Ok(()) }
}

/// IMAGE を置き場 (かディレクトリ) の manifest の記述に: (どこにあるか, 記述, 名前)
fn find_image(arg: &str) -> (ocistore::Layout, serde_json::Value, String) {
    let p = std::path::Path::new(arg);
    // パスは / ./ ../ で始まるか .tar で終わるもの (ほかは名前: 置き場か registry)
    let path = arg.starts_with('/') || arg.starts_with("./") || arg.starts_with("../") || arg.ends_with(".tar");
    if path && !p.exists() {
        fail(format!("{}: no such file or directory", arg));
    }
    if path && p.is_dir() {
        let l = ocistore::Layout { dir: p.to_path_buf() };
        let d = ocistore::pick(&l.index()).unwrap_or_else(|| fail(format!("{}: no image in index.json", arg)));
        let name = p.file_name().map_or("image".into(), |n| n.to_string_lossy().to_string());
        return (l, d, name);
    }
    let st = ocistore::store();
    if path {
        let got = ocistore::import_archive(p, &st).unwrap_or_else(|e| fail(format!("{}: {}", arg, e)));
        let (name, d) = got.into_iter().next().unwrap_or_else(|| fail(format!("{}: no image", arg)));
        return (st, d, name);
    }
    let r = ocistore::Ref::parse(arg);
    let mut names = vec![arg.to_string(), r.full()];
    if r.registry == "docker.io" && r.repo.starts_with("library/") {
        names.push(format!("localhost/{}:{}", r.short(), r.tag));
    }
    for _ in 0..2 {
        if let Some((n, d)) = names.iter().find_map(|n| st.find(n).map(|d| (n.clone(), d))) {
            return (st, d, n);
        }
        // 置き場になければ registry から (ap image pull)
        let ok = std::process::Command::new("aipkg").args(["image", "pull", arg]).status().is_ok_and(|s| s.success());
        if !ok {
            fail(format!("{}: cannot pull", arg));
        }
    }
    fail(format!("{}: not found", arg))
}

fn run_image(o: &Image, args: &[String], ports: Option<&[u16]>, deny: &[u32], kill: &[u32], verbose: bool) -> ! {
    use std::os::unix::fs::PermissionsExt;
    let (layout, desc, name) = find_image(o.image.as_deref().unwrap());
    let (m, md) = ocistore::manifest(&layout, &desc).unwrap_or_else(|e| fail(format!("{}: {}", name, e)));
    let cfg = m["config"]["digest"].as_str().and_then(|d| layout.read_json(d).ok()).unwrap_or_else(|| fail(format!("{}: no config", name)));
    let root = ocistore::rootfs(&layout, &m, &md, &cfg, &mut |s| eprintln!("aibox: {}: {}", name, s)).unwrap_or_else(|e| fail(format!("{}: {}", name, e)));
    let c = &cfg["config"];
    let strs = |v: &serde_json::Value| -> Vec<String> { v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default() };

    // だれとして: -u か image の User (名前か番号、:group)。なければ root
    let user = o.user.clone().or_else(|| c["User"].as_str().filter(|u| !u.is_empty()).map(String::from)).unwrap_or_else(|| "0".into());
    let (u, g) = match user.split_once(':') {
        Some((u, g)) => (u.to_string(), Some(g.to_string())),
        None => (user.clone(), None),
    };
    let pw = ocistore::passwd(&root, &u);
    let uid: u32 = u.parse().ok().or(pw.as_ref().map(|p| p.0)).unwrap_or_else(|| fail(format!("{}: no user {} in /etc/passwd", name, u)));
    let gid: u32 = match &g {
        Some(g) => g.parse().ok().or_else(|| ocistore::group(&root, g)).unwrap_or_else(|| fail(format!("{}: no group {} in /etc/group", name, g))),
        None => pw.as_ref().map_or(uid, |p| p.1),
    };
    let home = pw.as_ref().map(|p| p.2.clone()).filter(|h| !h.is_empty()).unwrap_or_else(|| if uid == 0 { "/root".into() } else { "/".into() });
    let host = ocistore::Ref::parse(&name).short().chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(63).collect::<String>();
    let host = if host.is_empty() { "aibox".to_string() } else { host };

    // 動かすもの: Entrypoint (か --entrypoint) と、ARG (なければ Cmd)
    let mut argv: Vec<String> = match &o.entrypoint {
        Some(e) => vec![e.clone()],
        None => strs(&c["Entrypoint"]),
    };
    argv.extend(if args.is_empty() { strs(&c["Cmd"]) } else { args.to_vec() });
    if argv.is_empty() {
        fail(format!("{}: no command (no Entrypoint or Cmd; give ARG)", name));
    }
    let workdir = c["WorkingDir"].as_str().filter(|w| !w.is_empty()).unwrap_or("/").to_string();

    // 環境: image の Env、PATH と HOME と HOSTNAME (なければ)、外の TERM、-e
    let mut env: Vec<(String, String)> = strs(&c["Env"]).iter().filter_map(|e| e.split_once('=').map(|(k, v)| (k.to_string(), v.to_string()))).collect();
    let set = |env: &mut Vec<(String, String)>, k: &str, v: String, over: bool| match env.iter_mut().find(|(n, _)| n == k) {
        Some(e) if over => e.1 = v,
        Some(_) => {}
        None => env.push((k.to_string(), v)),
    };
    set(&mut env, "PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into(), false);
    set(&mut env, "HOSTNAME", host.clone(), true);
    set(&mut env, "HOME", home.clone(), false);
    if let Ok(t) = std::env::var("TERM") {
        set(&mut env, "TERM", t, false);
    }
    for e in &o.env {
        match e.split_once('=') {
            Some((k, v)) => set(&mut env, k, v.to_string(), true),
            None => {
                if let Ok(v) = std::env::var(e) {
                    set(&mut env, e, v, true)
                }
            }
        }
    }
    if verbose {
        eprintln!("aibox: {} {} uid {} gid {} home {} cwd {}", name, &md[..19], uid, gid, home, workdir);
        eprintln!("aibox: root {}", root.display());
    }

    // ユーザーの namespace: 外の自分を中の uid と gid に (それより先に作る。中ではマウントと chroot ができる)
    let (me, mygid) = unsafe { (libc::getuid(), libc::getgid()) };
    let userns = || -> std::io::Result<()> {
        if unsafe { libc::unshare(libc::CLONE_NEWUSER) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        std::fs::write("/proc/self/uid_map", format!("{} {} 1\n", uid, me))?;
        std::fs::write("/proc/self/setgroups", "deny")?;
        std::fs::write("/proc/self/gid_map", format!("{} {} 1\n", gid, mygid))?;
        if unsafe { libc::unshare(libc::CLONE_NEWNS) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    };
    userns().unwrap_or_else(|e| fail(format!("namespace: {}", e)));
    let _ = mount(None, std::path::Path::new("/"), None, libc::MS_REC | libc::MS_PRIVATE);

    // -b の中のパス (なければ作る: Docker と同じ。根はまだ書ける)
    let mut binds = vec![];
    for (h, p, ro) in &o.binds {
        let t = ocistore::path_in(&root, p).unwrap_or_else(|| fail(format!("-b {}: bad path", p)));
        if !t.exists() {
            let made = if std::path::Path::new(h).is_dir() {
                std::fs::create_dir_all(&t)
            } else {
                t.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| std::fs::write(&t, b""))
            };
            made.unwrap_or_else(|e| fail(format!("-b {}: {}", p, e)));
        }
        binds.push((h.clone(), t, p.clone(), *ro));
    }
    let need = |r: std::io::Result<()>, what: &str| r.unwrap_or_else(|e| fail(format!("mount {}: {}", what, e)));
    // 根を自分の上に (あとで読むだけに)
    need(mount(Some(&root.display().to_string()), &root, None, libc::MS_BIND | libc::MS_REC), "root");
    // /dev: 空の tmpfs に、外のデバイスをいくつか (ファイルの bind)
    let dev = root.join("dev");
    need(mount(Some("tmpfs"), &dev, Some("tmpfs"), libc::MS_NOSUID | libc::MS_NOEXEC), "/dev");
    let _ = std::fs::set_permissions(&dev, std::fs::Permissions::from_mode(0o755));
    for d in ["null", "zero", "full", "random", "urandom", "tty"] {
        let h = format!("/dev/{}", d);
        if std::path::Path::new(&h).exists() && std::fs::write(dev.join(d), b"").is_ok() {
            need(mount(Some(&h), &dev.join(d), None, libc::MS_BIND), &h);
        }
    }
    for (l, t) in [("fd", "/proc/self/fd"), ("stdin", "/proc/self/fd/0"), ("stdout", "/proc/self/fd/1"), ("stderr", "/proc/self/fd/2")] {
        let _ = std::os::unix::fs::symlink(t, dev.join(l));
    }
    if std::fs::create_dir(dev.join("shm")).is_ok() {
        let _ = std::fs::set_permissions(dev.join("shm"), std::fs::Permissions::from_mode(0o1777));
    }
    // /tmp
    let tmp = ocistore::path_in(&root, "tmp").unwrap_or_else(|| root.join("tmp"));
    need(mount(Some("tmpfs"), &tmp, Some("tmpfs"), libc::MS_NOSUID | libc::MS_NODEV), "/tmp");
    let mut write = vec!["/tmp".to_string(), "/dev".to_string()];
    // 家が空のディレクトリなら書ける tmpfs に (distroless の /home/nonroot、/root)
    if home != "/" && let Some(h) = ocistore::path_in(&root, &home) {
        if std::fs::read_dir(&h).is_ok_and(|mut d| d.next().is_none()) {
            need(mount(Some("tmpfs"), &h, Some("tmpfs"), libc::MS_NOSUID | libc::MS_NODEV), &home);
            let _ = std::fs::set_permissions(&h, std::fs::Permissions::from_mode(0o700));
            write.push(home.clone());
        }
    }
    // 名前を引くもの (外のもの)
    for f in ["/etc/hosts", "/etc/resolv.conf"] {
        let t = root.join(&f[1..]);
        if std::path::Path::new(f).is_file() && t.is_file() {
            need(mount(Some(f), &t, None, libc::MS_BIND), f);
        }
    }
    for (h, t, p, ro) in &binds {
        need(mount(Some(h), t, None, libc::MS_BIND | libc::MS_REC), p);
        if *ro {
            need(mount(None, t, None, libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY), p);
        } else {
            write.push(p.clone());
        }
    }
    need(mount(None, &root, None, libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY), "root (ro)");

    // PID と UTS (と --no-net なら NET) を分けて、子が中の 1 番になる
    let mut ns = libc::CLONE_NEWPID | libc::CLONE_NEWUTS;
    if ports.is_some_and(|p| p.is_empty()) {
        ns |= libc::CLONE_NEWNET;
    }
    if unsafe { libc::unshare(ns) } != 0 {
        fail(format!("unshare: {}", std::io::Error::last_os_error()));
    }
    unsafe { libc::sethostname(host.as_ptr().cast(), host.len()) };
    let child = unsafe { libc::fork() };
    if child < 0 {
        fail(format!("fork: {}", std::io::Error::last_os_error()));
    }
    if child == 0 {
        if let Err(e) = mount(Some("proc"), &root.join("proc"), Some("proc"), libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC) {
            eprintln!("aibox: mount /proc: {}", e);
        }
        let r = cstr(&root.display().to_string());
        if unsafe { libc::chroot(r.as_ptr()) } != 0 {
            fail(format!("chroot: {}", std::io::Error::last_os_error()));
        }
        let w = cstr(&workdir);
        if unsafe { libc::chdir(w.as_ptr()) } != 0 {
            fail(format!("{}: {}", workdir, std::io::Error::last_os_error()));
        }
        match landlock::restrict(&write, ports) {
            Ok(missing) => {
                if verbose {
                    eprintln!("aibox: write {}", write.iter().filter(|w| !missing.contains(w)).cloned().collect::<Vec<_>>().join(" "));
                }
            }
            Err(e) => fail(e),
        }
        // chroot も止める (pivot_root でなく chroot で入れたので、中で chroot できると外の / へ出られる)
        let mut deny = deny.to_vec();
        deny.extend(seccomp::number("chroot"));
        if let Err(e) = seccomp::restrict(&deny, kill) {
            fail(e);
        }
        let e = std::process::Command::new(&argv[0]).args(&argv[1..]).env_clear().envs(env).exec();
        eprintln!("aibox: {}: {}", argv[0], e);
        std::process::exit(127);
    }
    wait_child(child)
}

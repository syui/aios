// /etc/passwd, /etc/group, /etc/shadow を読み書きし、ユーザーになる
#![allow(dead_code)]
use std::ffi::CString;
use std::fs;
use std::io::{self, BufRead, Write};

pub struct User {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: String,
    pub shell: String,
}

pub fn users() -> Vec<User> {
    fs::read_to_string("/etc/passwd")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            (f.len() >= 7).then(|| User {
                name: f[0].into(),
                uid: f[2].parse().unwrap_or(u32::MAX),
                gid: f[3].parse().unwrap_or(u32::MAX),
                home: f[5].into(),
                shell: if f[6].is_empty() { "/bin/sh".into() } else { f[6].into() },
            })
        })
        .collect()
}

pub fn by_name(name: &str) -> Option<User> {
    users().into_iter().find(|u| u.name == name)
}

pub fn by_uid(uid: u32) -> Option<User> {
    users().into_iter().find(|u| u.uid == uid)
}

/// (グループ名, gid, メンバー)
pub fn groups() -> Vec<(String, u32, Vec<String>)> {
    fs::read_to_string("/etc/group")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split(':').collect();
            (f.len() >= 4).then(|| (f[0].into(), f[2].parse().unwrap_or(u32::MAX), f[3].split(',').filter(|m| !m.is_empty()).map(String::from).collect()))
        })
        .collect()
}

/// user が入っているグループ (主グループも含む)
pub fn groups_of(u: &User) -> Vec<u32> {
    let mut g = vec![u.gid];
    for (_, gid, members) in groups() {
        if members.contains(&u.name) && !g.contains(&gid) {
            g.push(gid);
        }
    }
    g
}

pub fn in_group(u: &User, group: &str) -> bool {
    groups().iter().any(|(n, gid, members)| n == group && (*gid == u.gid || members.contains(&u.name)))
}

pub fn shadow_hash(name: &str) -> Option<String> {
    fs::read_to_string("/etc/shadow").ok()?.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.first() == Some(&name)).then(|| f.get(1).unwrap_or(&"!").to_string())
    })
}

/// shadow の name の行のハッシュを書きかえる (一時ファイルに書いて rename)
pub fn set_shadow_hash(name: &str, hash: &str) -> io::Result<()> {
    let days = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() / 86400);
    let text = fs::read_to_string("/etc/shadow").unwrap_or_default();
    let mut found = false;
    let mut out = String::new();
    for l in text.lines() {
        let mut f: Vec<String> = l.split(':').map(String::from).collect();
        if f.first().is_some_and(|n| n == name) {
            f.resize(9.max(f.len()), String::new());
            f[1] = hash.into();
            f[2] = days.to_string();
            found = true;
        }
        out.push_str(&f.join(":"));
        out.push('\n');
    }
    if !found {
        out.push_str(&format!("{}:{}:{}::::::\n", name, hash, days));
    }
    let tmp = "/etc/shadow.new";
    {
        let mut f = fs::OpenOptions::new().write(true).create(true).truncate(true).open(tmp)?;
        f.write_all(out.as_bytes())?;
    }
    let c = CString::new(tmp).unwrap();
    unsafe {
        libc::chmod(c.as_ptr(), 0o600);
        libc::chown(c.as_ptr(), 0, 0);
    }
    fs::rename(tmp, "/etc/shadow")
}

/// 端末の echo を止めてパスワードを読む
pub fn read_password(prompt: &str) -> Option<String> {
    print!("{}", prompt);
    io::stdout().flush().ok();
    let mut t: libc::termios = unsafe { std::mem::zeroed() };
    let tty = unsafe { libc::tcgetattr(0, &mut t) } == 0;
    if tty {
        let mut quiet = t;
        quiet.c_lflag &= !libc::ECHO;
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &quiet) };
    }
    let mut line = String::new();
    let n = io::stdin().lock().read_line(&mut line).unwrap_or(0);
    if tty {
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) };
        println!();
    }
    (n > 0).then(|| line.trim_end_matches(['\n', '\r']).to_string())
}

/// ホームがなければ作り、/etc/skel のファイル (.aishrc など) を写す (pam_mkhomedir と同じ)
pub fn make_home(u: &User) {
    if u.home.is_empty() || u.home == "/" || fs::metadata(&u.home).is_ok() {
        return;
    }
    if fs::create_dir_all(&u.home).is_err() {
        return;
    }
    let own = |path: &str, mode: libc::mode_t| {
        let c = CString::new(path).unwrap();
        unsafe {
            libc::chown(c.as_ptr(), u.uid, u.gid);
            libc::chmod(c.as_ptr(), mode);
        }
    };
    own(&u.home, 0o700);
    for e in fs::read_dir("/etc/skel").into_iter().flatten().flatten() {
        if e.file_type().is_ok_and(|t| t.is_file()) {
            let dst = format!("{}/{}", u.home, e.file_name().to_string_lossy());
            if fs::copy(e.path(), &dst).is_ok() {
                own(&dst, 0o644);
            }
        }
    }
}

/// u になる: グループ、gid、uid の順に変える (root でなければ失敗する)
pub fn become_user(u: &User) -> Result<(), String> {
    let g: Vec<libc::gid_t> = groups_of(u);
    unsafe {
        if libc::setgroups(g.len(), g.as_ptr()) != 0 {
            return Err(format!("setgroups: {}", io::Error::last_os_error()));
        }
        if libc::setgid(u.gid) != 0 {
            return Err(format!("setgid: {}", io::Error::last_os_error()));
        }
        if libc::setuid(u.uid) != 0 {
            return Err(format!("setuid: {}", io::Error::last_os_error()));
        }
    }
    Ok(())
}

/// u のシェルを動かす。login なら argv[0] を "-sh" にしてホームへ
pub fn exec_shell(u: &User, login: bool, cmd: Option<&str>) -> ! {
    let mut env: Vec<(String, String)> = std::env::vars().filter(|(k, _)| k == "TERM").collect();
    env.push(("HOME".into(), u.home.clone()));
    env.push(("USER".into(), u.name.clone()));
    env.push(("LOGNAME".into(), u.name.clone()));
    env.push(("SHELL".into(), u.shell.clone()));
    env.push(("PATH".into(), "/usr/bin:/bin".into()));
    if login {
        let _ = std::env::set_current_dir(&u.home).or_else(|_| std::env::set_current_dir("/"));
    }
    let base = u.shell.rsplit('/').next().unwrap_or("sh");
    let arg0 = if login { format!("-{}", base) } else { base.to_string() };
    let mut args = vec![CString::new(arg0).unwrap()];
    if let Some(c) = cmd {
        args.push(CString::new("-c").unwrap());
        args.push(CString::new(c).unwrap());
    }
    let mut argv: Vec<*const libc::c_char> = args.iter().map(|a| a.as_ptr()).collect();
    argv.push(std::ptr::null());
    let envs: Vec<CString> = env.iter().map(|(k, v)| CString::new(format!("{}={}", k, v)).unwrap()).collect();
    let mut envp: Vec<*const libc::c_char> = envs.iter().map(|e| e.as_ptr()).collect();
    envp.push(std::ptr::null());
    let sh = CString::new(u.shell.as_str()).unwrap();
    unsafe { libc::execve(sh.as_ptr(), argv.as_ptr(), envp.as_ptr()) };
    eprintln!("{}: {}", u.shell, io::Error::last_os_error());
    // ログインシェル (brush など) が消えていても入れるよう、/bin/sh で動かす
    if u.shell != "/bin/sh" {
        eprintln!("falling back to /bin/sh");
        let arg0 = CString::new(if login { "-sh" } else { "sh" }).unwrap();
        argv[0] = arg0.as_ptr();
        unsafe { libc::execve(c"/bin/sh".as_ptr(), argv.as_ptr(), envp.as_ptr()) };
    }
    std::process::exit(127);
}

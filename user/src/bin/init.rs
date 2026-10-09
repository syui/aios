// aios の init (pid 1): systemd のユニットファイルでサービスを起こし、見守る
//
//   /usr/lib/systemd/system, /etc/systemd/system の *.service を読み、
//   /etc/systemd/system/multi-user.target.wants にあるものを起動する。
//   systemctl とは /run/aiinit.ctl (FIFO) で話す。
#[path = "../lib/kmod.rs"]
mod kmod;
#[path = "../lib/sysctl.rs"]
mod sysctl;
#[path = "../lib/unit.rs"]
mod unit;
#[path = "../lib/users.rs"]
mod users;

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::io::Write;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use unit::{Restart, Type, Unit};

#[derive(Clone, Copy, PartialEq)]
enum St {
    Inactive,
    /// oneshot を実行中
    Activating,
    Active,
    Failed,
}

struct Svc {
    unit: Unit,
    pid: Option<i32>,
    st: St,
    since: u64,
    exit: Option<i32>,
    restart_at: Option<Instant>,
    /// stop を頼まれた (再起動しない)
    stopping: bool,
    restarts: u32,
}

struct Init {
    svcs: BTreeMap<String, Svc>,
    /// これから起動する予定のもの (After= の相手が入っていたら先に起動する)
    pending: std::collections::BTreeSet<String>,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap_or_default()
}

fn find_exe(prog: &str) -> Option<String> {
    if prog.starts_with('/') {
        return Some(prog.to_string());
    }
    ["/usr/bin", "/bin"].iter().map(|d| format!("{}/{}", d, prog)).find(|p| fs::metadata(p).is_ok())
}

/// サービスのプロセスを作る
fn spawn(u: &Unit) -> Result<i32, String> {
    let exe = u.exec_start.first().and_then(|p| find_exe(p)).ok_or("ExecStart not found")?;
    let args: Vec<CString> = u.exec_start.iter().map(|a| cstr(a)).collect();
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    // /etc/profile と同じ (/opt/c/bin は [c] のパッケージ: aiwm の exec で firefox などを名前だけで動かせるように)
    env.insert("PATH".into(), "/usr/local/bin:/usr/bin:/bin:/opt/c/bin".into());
    env.insert("HOME".into(), "/root".into());
    env.insert("USER".into(), "root".into());
    env.insert("TERM".into(), "vt100".into());
    // User=: そのユーザーの HOME などと、/run/user/UID (XDG_RUNTIME_DIR)
    let mut who: Option<users::User> = None;
    if let Some(name) = &u.user {
        let mut usr = users::by_name(name).ok_or(format!("User={}: no such user", name))?;
        if let Some(g) = &u.group {
            usr.gid = users::groups().into_iter().find(|(n, _, _)| n == g).map(|(_, gid, _)| gid).ok_or(format!("Group={}: no such group", g))?;
        }
        users::make_home(&usr);
        let run = format!("/run/user/{}", usr.uid);
        let _ = fs::create_dir_all(&run);
        let c = cstr(&run);
        unsafe {
            libc::chown(c.as_ptr(), usr.uid, usr.gid);
            libc::chmod(c.as_ptr(), 0o700);
        }
        env.insert("HOME".into(), usr.home.clone());
        env.insert("USER".into(), usr.name.clone());
        env.insert("LOGNAME".into(), usr.name.clone());
        env.insert("SHELL".into(), usr.shell.clone());
        env.insert("XDG_RUNTIME_DIR".into(), run);
        who = Some(usr);
    }
    for (k, v) in &u.env {
        env.insert(k.clone(), v.clone());
    }
    let envs: Vec<CString> = env.iter().map(|(k, v)| cstr(&format!("{}={}", k, v))).collect();
    let log = format!("/var/log/{}.log", u.name);
    let (exe, log) = (cstr(&exe), cstr(&log));
    // WorkingDirectory=~ はホーム
    let workdir = match u.workdir.as_deref() {
        Some("~") => Some(cstr(env.get("HOME").map_or("/", |h| h.as_str()))),
        w => w.map(cstr),
    };
    let tty = u.tty;

    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err("fork failed".into());
    }
    if pid > 0 {
        return Ok(pid);
    }
    // 子
    unsafe {
        libc::setsid();
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        let (inp, out) = if tty {
            let c = libc::open(c"/dev/console".as_ptr(), libc::O_RDWR);
            (c, c)
        } else {
            let n = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY);
            let l = libc::open(log.as_ptr(), libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND, 0o644);
            (n, if l >= 0 { l } else { libc::open(c"/dev/console".as_ptr(), libc::O_WRONLY) })
        };
        libc::dup2(inp, 0);
        libc::dup2(out, 1);
        libc::dup2(out, 2);
        if inp > 2 {
            libc::close(inp);
        }
        if out > 2 && out != inp {
            libc::close(out);
        }
        if let Some(usr) = &who {
            if let Err(e) = users::become_user(usr) {
                eprintln!("init: {}", e);
                libc::_exit(217); // systemd の EXIT_USER
            }
        }
        if let Some(d) = &workdir {
            libc::chdir(d.as_ptr());
        }
        let mut argv: Vec<*const libc::c_char> = args.iter().map(|a| a.as_ptr()).collect();
        argv.push(std::ptr::null());
        let mut envp: Vec<*const libc::c_char> = envs.iter().map(|e| e.as_ptr()).collect();
        envp.push(std::ptr::null());
        libc::execve(exe.as_ptr(), argv.as_ptr(), envp.as_ptr());
        libc::_exit(203); // systemd の EXIT_EXEC
    }
}

impl Init {
    fn load() -> Init {
        let svcs = unit::load_all()
            .into_iter()
            .map(|(n, u)| (n, Svc { unit: u, pid: None, st: St::Inactive, since: now_secs(), exit: None, restart_at: None, stopping: false, restarts: 0 }))
            .collect();
        Init { svcs, pending: Default::default() }
    }

    /// name を起動する。依存 (Wants/Requires) も起動し、After にある oneshot は終わるまで待つ
    fn start(&mut self, name: &str, depth: u32) -> Result<(), String> {
        let Some(s) = self.svcs.get(name) else { return Err(format!("Unit {} not found.", name)) };
        if matches!(s.st, St::Active | St::Activating) || depth > 16 {
            return Ok(());
        }
        let u = s.unit.clone();
        self.pending.remove(name);
        // ConditionPathExists=: 合わなければ起動しないで飛ばす (失敗ではない)
        let unmet = u.cond_paths.iter().find(|p| match p.strip_prefix('!') {
            Some(q) => fs::metadata(q).is_ok(),
            None => fs::metadata(p).is_err(),
        });
        if let Some(p) = unmet {
            println!("init: {}: skipped (ConditionPathExists={})", name, p);
            return Ok(());
        }
        for a in &u.after {
            if self.pending.contains(a) {
                let _ = self.start(a, depth + 1);
            }
        }
        for d in &u.wants {
            if self.svcs.contains_key(d) {
                let _ = self.start(d, depth + 1);
            }
        }
        for a in &u.after {
            self.wait_oneshot(a);
        }
        let s = self.svcs.get_mut(name).unwrap();
        s.stopping = false;
        s.restart_at = None;
        match spawn(&u) {
            Ok(pid) => {
                s.pid = Some(pid);
                s.st = if u.typ == Type::Oneshot { St::Activating } else { St::Active };
                s.since = now_secs();
                s.exit = None;
                Ok(())
            }
            Err(e) => {
                s.st = St::Failed;
                Err(format!("{}: {}", name, e))
            }
        }
    }

    /// name が実行中の oneshot なら終わるまで待つ
    fn wait_oneshot(&mut self, name: &str) {
        let Some(pid) = self.svcs.get(name).filter(|s| s.st == St::Activating).and_then(|s| s.pid) else { return };
        let mut status = 0;
        if unsafe { libc::waitpid(pid, &mut status, 0) } == pid {
            self.exited(pid, status);
        }
    }

    fn stop(&mut self, name: &str) -> Result<(), String> {
        let s = self.svcs.get_mut(name).ok_or(format!("Unit {} not found.", name))?;
        s.stopping = true;
        s.restart_at = None;
        if let Some(pid) = s.pid {
            unsafe { libc::kill(pid, libc::SIGTERM) };
            let mut status = 0;
            if unsafe { libc::waitpid(pid, &mut status, 0) } == pid {
                self.exited(pid, status);
            }
        }
        let s = self.svcs.get_mut(name).unwrap();
        if s.st != St::Failed {
            s.st = St::Inactive;
        }
        Ok(())
    }

    /// 子が終わった
    fn exited(&mut self, pid: i32, status: i32) {
        let Some(s) = self.svcs.values_mut().find(|s| s.pid == Some(pid)) else { return };
        let code = if libc::WIFSIGNALED(status) { 128 + libc::WTERMSIG(status) } else { libc::WEXITSTATUS(status) };
        s.pid = None;
        s.exit = Some(code);
        s.since = now_secs();
        let ok = code == 0 || s.unit.ignore_failure;
        s.st = match (s.unit.typ, ok) {
            _ if s.stopping => St::Inactive,
            (Type::Oneshot, true) => St::Active, // oneshot は終わっても active (RemainAfterExit 相当)
            (_, true) => St::Inactive,
            (_, false) => St::Failed,
        };
        let again = !s.stopping
            && match s.unit.restart {
                Restart::Always => true,
                Restart::OnFailure => !ok,
                Restart::No => false,
            };
        if again {
            s.restarts += 1;
            s.restart_at = Some(Instant::now() + Duration::from_secs_f64(s.unit.restart_sec));
        }
    }

    fn tick(&mut self) {
        let due: Vec<String> = self.svcs.iter().filter(|(_, s)| s.restart_at.is_some_and(|t| t <= Instant::now())).map(|(n, _)| n.clone()).collect();
        for n in due {
            let _ = self.start(&n, 0);
        }
    }

    fn state_str(s: &Svc) -> String {
        let sub = match (s.st, s.pid) {
            (St::Active, Some(_)) => "running",
            (St::Active, None) => "exited",
            (St::Activating, _) => "start",
            (St::Failed, _) => "failed",
            (St::Inactive, _) => "dead",
        };
        let st = match s.st {
            St::Inactive => "inactive",
            St::Activating => "activating",
            St::Active => "active",
            St::Failed => "failed",
        };
        format!("{} ({})", st, sub)
    }

    fn status(&self, name: &str) -> String {
        let Some(s) = self.svcs.get(name) else { return format!("Unit {} could not be found.\n", name) };
        let enabled = if unit::enabled().iter().any(|e| e == name) { "enabled" } else { "disabled" };
        let mut o = format!("{} {} - {}\n", if s.st == St::Active { "●" } else { "○" }, name, s.unit.description);
        o += &format!("     Loaded: loaded ({}; {})\n", s.unit.path, enabled);
        o += &format!("     Active: {} since {}\n", Init::state_str(s), unit::fmt_time(s.since));
        if let Some(pid) = s.pid {
            o += &format!("   Main PID: {} ({})\n", pid, s.unit.exec_start.first().map_or("", |p| p.rsplit('/').next().unwrap_or(p)));
        } else if let Some(code) = s.exit {
            o += &format!("       Exit: status={}\n", code);
        }
        if s.restarts > 0 {
            o += &format!("   Restarts: {}\n", s.restarts);
        }
        if let Ok(log) = fs::read_to_string(format!("/var/log/{}.log", name)) {
            let lines: Vec<&str> = log.lines().collect();
            if !lines.is_empty() {
                o += "\n";
                for l in &lines[lines.len().saturating_sub(5)..] {
                    o += &format!("{}\n", l);
                }
            }
        }
        o
    }

    fn list(&self) -> String {
        let mut o = format!("{:<28} {:<10} {:<10} {}\n", "UNIT", "ACTIVE", "SUB", "DESCRIPTION");
        for (n, s) in &self.svcs {
            let full = Init::state_str(s);
            let (a, sub) = full.split_once(' ').unwrap_or((&full, ""));
            o += &format!("{:<28} {:<10} {:<10} {}\n", n, a, sub.trim_matches(['(', ')']), s.unit.description);
        }
        o
    }

    /// systemctl からの 1 行: "cmd unit reply-fifo"
    fn command(&mut self, line: &str) {
        let w: Vec<&str> = line.split_whitespace().collect();
        let Some((&reply, rest)) = w.split_last() else { return };
        let (cmd, arg) = (rest.first().copied().unwrap_or(""), rest.get(1).map(|a| unit::full_name(a)).unwrap_or_default());
        // 変える操作は root だけ (返事の FIFO の持ち主が頼んだ人)
        let requester = std::fs::metadata(reply).map(|m| std::os::unix::fs::MetadataExt::uid(&m)).unwrap_or(u32::MAX);
        let read_only = matches!(cmd, "status" | "is-active" | "list-units");
        if !read_only && requester != 0 {
            reply_to(reply, "Access denied (run as root).\n", 4);
            return;
        }
        let (out, code) = match cmd {
            "start" => res(self.start(&arg, 0)),
            "stop" => res(self.stop(&arg)),
            "restart" => res(self.stop(&arg).and_then(|_| self.start(&arg, 0))),
            "status" => {
                let known = self.svcs.contains_key(&arg);
                let active = self.svcs.get(&arg).is_some_and(|s| s.st == St::Active);
                (self.status(&arg), if !known { 4 } else if active { 0 } else { 3 })
            }
            "is-active" => {
                let s = self.svcs.get(&arg).map_or("inactive".to_string(), |s| Init::state_str(s));
                let st = s.split(' ').next().unwrap_or("").to_string();
                (st.clone() + "\n", if st == "active" { 0 } else { 3 })
            }
            "list-units" => (self.list(), 0),
            "daemon-reload" => {
                let fresh = unit::load_all();
                for (n, u) in fresh {
                    match self.svcs.get_mut(&n) {
                        Some(s) => s.unit = u,
                        None => {
                            self.svcs.insert(n, Svc { unit: u, pid: None, st: St::Inactive, since: now_secs(), exit: None, restart_at: None, stopping: false, restarts: 0 });
                        }
                    }
                }
                (String::new(), 0)
            }
            "poweroff" | "reboot" | "halt" => {
                reply_to(reply, "", 0);
                self.shutdown(cmd == "reboot");
            }
            _ => (format!("Unknown command {}\n", cmd), 1),
        };
        reply_to(reply, &out, code);
    }

    fn shutdown(&mut self, reboot: bool) -> ! {
        println!("aios: stopping services");
        let names: Vec<String> = self.svcs.iter().filter(|(_, s)| s.pid.is_some()).map(|(n, _)| n.clone()).collect();
        for n in names {
            let _ = self.stop(&n);
        }
        unsafe {
            libc::sync();
            libc::reboot(if reboot { libc::RB_AUTOBOOT } else { libc::RB_POWER_OFF });
        }
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

fn res(r: Result<(), String>) -> (String, i32) {
    match r {
        Ok(()) => (String::new(), 0),
        Err(e) => (format!("{}\n", e), 1),
    }
}

/// 返事は 1 行目に終了コード、そのあとに本文
fn reply_to(path: &str, out: &str, code: i32) {
    if let Ok(mut f) = fs::OpenOptions::new().write(true).open(path) {
        let _ = write!(f, "{}\n{}", code, out);
    }
}

fn main() {
    if std::process::id() != 1 {
        eprintln!("init: must be run as pid 1");
        std::process::exit(1);
    }
    // ホスト名: /etc/hostname をカーネルに (uname の nodename。aios apply の host.name も変える)
    if let Ok(h) = std::fs::read_to_string("/etc/hostname") {
        let h = h.trim();
        if !h.is_empty() {
            unsafe { libc::sethostname(h.as_ptr() as *const libc::c_char, h.len()) };
        }
    }
    // モジュール: /etc/modules-load.d/*.conf に書いてあるドライバを起こす (systemd-modules-load と同じ)
    for m in kmod::boot_list() {
        match kmod::load(&m) {
            Ok(()) => println!("init: module {}", m),
            Err(e) => println!("init: module {}: {}", m, e),
        }
    }
    // カーネルの値: /etc/sysctl.conf と /etc/sysctl.d/*.conf を /proc/sys に (systemd-sysctl と同じ)
    for f in sysctl::boot_files() {
        for e in sysctl::apply_file(&f) {
            println!("init: sysctl: {}", e);
        }
    }
    let mut init = Init::load();
    let enabled = unit::enabled();
    println!("aios init: {} units, {} enabled", init.svcs.len(), enabled.len());
    init.pending = enabled.iter().cloned().collect();
    for n in &enabled {
        if let Err(e) = init.start(n, 0) {
            println!("init: {}", e);
        }
    }

    // systemctl の窓口
    let ctl = cstr(unit::CTL);
    let fd = unsafe {
        libc::unlink(ctl.as_ptr());
        libc::mknod(ctl.as_ptr(), libc::S_IFIFO | 0o622, 0);
        libc::chmod(ctl.as_ptr(), 0o622);
        libc::open(ctl.as_ptr(), libc::O_RDWR)
    };
    let mut buf = String::new();
    loop {
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        let n = unsafe { libc::poll(&mut pfd, 1, 100) };
        if n > 0 && pfd.revents & libc::POLLIN != 0 {
            let mut b = [0u8; 4096];
            let k = unsafe { libc::read(fd, b.as_mut_ptr() as *mut _, b.len()) };
            if k > 0 {
                buf.push_str(&String::from_utf8_lossy(&b[..k as usize]));
                while let Some(i) = buf.find('\n') {
                    let line: String = buf.drain(..=i).collect();
                    init.command(line.trim());
                }
            }
        }
        // 終わった子 (サービスも、親をなくした子も) を刈り取る
        loop {
            let mut status = 0;
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if pid <= 0 {
                break;
            }
            init.exited(pid, status);
        }
        init.tick();
        let _ = std::io::stdout().flush();
    }
}

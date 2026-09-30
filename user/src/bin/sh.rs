// aios の小さなシェル: パイプ (|)、入出力のつけかえ (< >)、; と &&、cd / exit
use std::ffi::CString;
use std::io::{self, BufRead, Write};

fn find(cmd: &str) -> Option<CString> {
    if cmd.contains('/') {
        return CString::new(cmd).ok();
    }
    let path = std::env::var("PATH").unwrap_or_else(|_| "/bin".into());
    path.split(':')
        .map(|d| format!("{}/{}", d, cmd))
        .find(|p| std::fs::metadata(p).is_ok_and(|m| m.is_file()))
        .and_then(|p| CString::new(p).ok())
}

struct Cmd {
    args: Vec<String>,
    stdin: Option<String>,
    stdout: Option<String>,
}

fn parse(line: &str) -> Vec<Cmd> {
    line.split('|')
        .map(|part| {
            let mut c = Cmd { args: vec![], stdin: None, stdout: None };
            let mut words = part.split_whitespace();
            while let Some(w) = words.next() {
                match w {
                    "<" => c.stdin = words.next().map(String::from),
                    ">" => c.stdout = words.next().map(String::from),
                    _ => c.args.push(w.to_string()),
                }
            }
            c
        })
        .collect()
}

fn redirect(path: &str, fd: i32, flags: i32) -> bool {
    let p = CString::new(path).unwrap();
    let f = unsafe { libc::open(p.as_ptr(), flags, 0o644) };
    if f < 0 {
        eprintln!("sh: {}: {}", path, io::Error::last_os_error());
        return false;
    }
    unsafe {
        libc::dup2(f, fd);
        libc::close(f);
    }
    true
}

fn run(cmds: Vec<Cmd>) -> i32 {
    let n = cmds.len();
    let mut pids = vec![];
    let mut prev_read = -1;
    for (i, c) in cmds.into_iter().enumerate() {
        let mut fds = [-1, -1];
        if i + 1 < n && unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
            eprintln!("sh: pipe: {}", io::Error::last_os_error());
            break;
        }
        let prog = c.args.first().and_then(|a| find(a));
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            unsafe {
                // Rust は SIGPIPE を無視するが、子には既定の動作で渡す
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
                if prev_read >= 0 {
                    libc::dup2(prev_read, 0);
                    libc::close(prev_read);
                }
                if fds[1] >= 0 {
                    libc::dup2(fds[1], 1);
                    libc::close(fds[1]);
                    libc::close(fds[0]);
                }
            }
            if let Some(f) = &c.stdin {
                if !redirect(f, 0, libc::O_RDONLY) {
                    unsafe { libc::_exit(1) };
                }
            }
            if let Some(f) = &c.stdout {
                if !redirect(f, 1, libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC) {
                    unsafe { libc::_exit(1) };
                }
            }
            let Some(prog) = prog else {
                eprintln!("sh: {}: command not found", c.args.first().map_or("", |s| s));
                unsafe { libc::_exit(127) };
            };
            let args: Vec<CString> = c.args.iter().map(|a| CString::new(a.as_str()).unwrap()).collect();
            let mut argv: Vec<*const libc::c_char> = args.iter().map(|a| a.as_ptr()).collect();
            argv.push(std::ptr::null());
            let env: Vec<CString> = std::env::vars().map(|(k, v)| CString::new(format!("{k}={v}")).unwrap()).collect();
            let mut envp: Vec<*const libc::c_char> = env.iter().map(|e| e.as_ptr()).collect();
            envp.push(std::ptr::null());
            unsafe {
                libc::execve(prog.as_ptr(), argv.as_ptr(), envp.as_ptr());
            }
            eprintln!("sh: {}: {}", c.args[0], io::Error::last_os_error());
            unsafe { libc::_exit(126) };
        }
        if prev_read >= 0 {
            unsafe { libc::close(prev_read) };
        }
        if fds[1] >= 0 {
            unsafe { libc::close(fds[1]) };
        }
        prev_read = fds[0];
        if pid > 0 {
            pids.push(pid);
        }
    }
    let mut last = 0;
    for pid in pids {
        let mut st = 0;
        unsafe { libc::waitpid(pid, &mut st, 0) };
        last = if libc::WIFSIGNALED(st) { 128 + libc::WTERMSIG(st) } else { libc::WEXITSTATUS(st) };
    }
    last
}

fn main() {
    let stdin = io::stdin();
    let mut status = 0;
    loop {
        let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
        print!("{} {} ", cwd, if status == 0 { "%" } else { "!" });
        io::stdout().flush().ok();

        let mut line = String::new();
        if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 {
            println!();
            return;
        }
        let line = line.trim();
        for list in line.split(';') {
            for (j, cmd) in list.split("&&").enumerate() {
                if j > 0 && status != 0 {
                    break;
                }
                status = run_line(cmd.trim(), status);
            }
        }
    }
}

fn run_line(line: &str, status: i32) -> i32 {
    if line.is_empty() || line.starts_with('#') {
        return status;
    }
    let mut words = line.split_whitespace();
    match words.next() {
        Some("exit") => std::process::exit(words.next().and_then(|s| s.parse().ok()).unwrap_or(status)),
        Some("cd") => {
            let dir = words.next().unwrap_or("/");
            match std::env::set_current_dir(dir) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("cd: {}: {}", dir, e);
                    1
                }
            }
        }
        _ => run(parse(line)),
    }
}

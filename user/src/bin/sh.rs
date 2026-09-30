// aios の小さなシェル
//   パイプ |、つけかえ < > >>、並べる ; &&、クォート ' " \、変数 $VAR $?
//   組み込み: cd, exit, export
use std::ffi::CString;
use std::io::{self, BufRead, Write};

#[derive(Debug, PartialEq)]
enum Tok {
    Word(String),
    Pipe,
    Lt,
    Gt,
    GtGt,
}

fn var(name: &str, status: i32) -> String {
    match name {
        "?" => status.to_string(),
        "$" => std::process::id().to_string(),
        _ => std::env::var(name).unwrap_or_default(),
    }
}

/// $NAME / ${NAME} / $? を読む。c は '$' の次の位置
fn read_var(cs: &[char], i: &mut usize, status: i32) -> String {
    if *i < cs.len() && cs[*i] == '{' {
        let start = *i + 1;
        let end = cs[start..].iter().position(|&c| c == '}').map_or(cs.len(), |p| start + p);
        *i = (end + 1).min(cs.len());
        return var(&cs[start..end].iter().collect::<String>(), status);
    }
    if *i < cs.len() && (cs[*i] == '?' || cs[*i] == '$') {
        *i += 1;
        return var(&cs[*i - 1].to_string(), status);
    }
    let start = *i;
    while *i < cs.len() && (cs[*i].is_alphanumeric() || cs[*i] == '_') {
        *i += 1;
    }
    if start == *i {
        return "$".into();
    }
    var(&cs[start..*i].iter().collect::<String>(), status)
}

fn lex(line: &str, status: i32) -> Result<Vec<Tok>, String> {
    let cs: Vec<char> = line.chars().collect();
    let mut toks = vec![];
    let mut word: Option<String> = None;
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        i += 1;
        let op = match c {
            '|' => Some(Tok::Pipe),
            '<' => Some(Tok::Lt),
            '>' if cs.get(i) == Some(&'>') => {
                i += 1;
                Some(Tok::GtGt)
            }
            '>' => Some(Tok::Gt),
            '#' if word.is_none() => break,
            _ => None,
        };
        if let Some(op) = op {
            if let Some(w) = word.take() {
                toks.push(Tok::Word(w));
            }
            toks.push(op);
            continue;
        }
        match c {
            ' ' | '\t' => {
                if let Some(w) = word.take() {
                    toks.push(Tok::Word(w));
                }
            }
            '\'' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match cs.get(i) {
                        Some('\'') => break,
                        Some(&c) => w.push(c),
                        None => return Err("unterminated '".into()),
                    }
                    i += 1;
                }
                i += 1;
            }
            '"' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match cs.get(i) {
                        Some('"') => break,
                        Some('\\') if matches!(cs.get(i + 1), Some('"' | '\\' | '$')) => {
                            w.push(cs[i + 1]);
                            i += 1;
                        }
                        Some('$') => {
                            i += 1;
                            w.push_str(&read_var(&cs, &mut i, status));
                            continue;
                        }
                        Some(&c) => w.push(c),
                        None => return Err("unterminated \"".into()),
                    }
                    i += 1;
                }
                i += 1;
            }
            '\\' => {
                if let Some(&n) = cs.get(i) {
                    word.get_or_insert_with(String::new).push(n);
                    i += 1;
                }
            }
            '$' => {
                let v = read_var(&cs, &mut i, status);
                word.get_or_insert_with(String::new).push_str(&v);
            }
            '~' if word.is_none() && matches!(cs.get(i), None | Some('/' | ' ')) => {
                word = Some(var("HOME", status));
            }
            c => word.get_or_insert_with(String::new).push(c),
        }
    }
    if let Some(w) = word {
        toks.push(Tok::Word(w));
    }
    Ok(toks)
}

#[derive(Default)]
struct Cmd {
    args: Vec<String>,
    stdin: Option<String>,
    /// (path, append)
    stdout: Option<(String, bool)>,
}

/// 1 本のパイプライン (| でつながったもの)
fn pipeline(toks: &[Tok]) -> Result<Vec<Cmd>, String> {
    let mut cmds = vec![Cmd::default()];
    let mut it = toks.iter();
    while let Some(t) = it.next() {
        let cur = cmds.last_mut().unwrap();
        match t {
            Tok::Word(w) => cur.args.push(w.clone()),
            Tok::Pipe => cmds.push(Cmd::default()),
            Tok::Lt | Tok::Gt | Tok::GtGt => {
                let Some(Tok::Word(f)) = it.next() else { return Err("missing file for redirection".into()) };
                match t {
                    Tok::Lt => cur.stdin = Some(f.clone()),
                    _ => cur.stdout = Some((f.clone(), *t == Tok::GtGt)),
                }
            }
        }
    }
    if cmds.iter().any(|c| c.args.is_empty()) {
        return Err("syntax error near |".into());
    }
    Ok(cmds)
}

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

fn spawn(cmds: Vec<Cmd>) -> i32 {
    let n = cmds.len();
    let mut pids = vec![];
    let mut prev_read = -1;
    for (i, c) in cmds.into_iter().enumerate() {
        let mut fds = [-1, -1];
        if i + 1 < n && unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
            eprintln!("sh: pipe: {}", io::Error::last_os_error());
            break;
        }
        let prog = find(&c.args[0]);
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
            if let Some((f, append)) = &c.stdout {
                let mode = if *append { libc::O_APPEND } else { libc::O_TRUNC };
                if !redirect(f, 1, libc::O_WRONLY | libc::O_CREAT | mode) {
                    unsafe { libc::_exit(1) };
                }
            }
            let Some(prog) = prog else {
                eprintln!("sh: {}: command not found", c.args[0]);
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

fn builtin(args: &[String], status: i32) -> Option<i32> {
    match args[0].as_str() {
        "exit" => std::process::exit(args.get(1).and_then(|s| s.parse().ok()).unwrap_or(status)),
        "cd" => {
            let home = var("HOME", status);
            let dir = args.get(1).map_or(home.as_str(), |s| s.as_str());
            Some(match std::env::set_current_dir(dir) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("cd: {}: {}", dir, e);
                    1
                }
            })
        }
        "export" => {
            for a in &args[1..] {
                if let Some((k, v)) = a.split_once('=') {
                    unsafe { std::env::set_var(k, v) };
                }
            }
            Some(0)
        }
        _ => None,
    }
}

/// クォートの外にある ; と && で区切る。bool は「前が成功したときだけ」
fn split_list(line: &str) -> Vec<(String, bool)> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut and = false;
    let mut quote = None;
    let mut cs = line.chars().peekable();
    while let Some(c) = cs.next() {
        match (quote, c) {
            (None, '\\') => {
                cur.push(c);
                if let Some(n) = cs.next() {
                    cur.push(n);
                }
                continue;
            }
            (None, '\'' | '"') => quote = Some(c),
            (Some(q), _) if q == c => quote = None,
            (None, ';') => {
                out.push((std::mem::take(&mut cur), and));
                and = false;
                continue;
            }
            (None, '&') if cs.peek() == Some(&'&') => {
                cs.next();
                out.push((std::mem::take(&mut cur), and));
                and = true;
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    out.push((cur, and));
    out
}

fn run(line: &str, mut status: i32) -> i32 {
    let mut skip = false;
    for (part, after_and) in split_list(line) {
        if !after_and {
            skip = false;
        }
        if skip || (after_and && status != 0) {
            skip = true;
            continue;
        }
        // 変数は実行する直前に展開する
        let toks = match lex(&part, status) {
            Ok(t) if t.is_empty() => continue,
            Ok(t) => t,
            Err(e) => {
                eprintln!("sh: {}", e);
                status = 2;
                continue;
            }
        };
        status = match pipeline(&toks) {
            Ok(cmds) if cmds.len() == 1 => match builtin(&cmds[0].args, status) {
                Some(s) => s,
                None => spawn(cmds),
            },
            Ok(cmds) => spawn(cmds),
            Err(e) => {
                eprintln!("sh: {}", e);
                2
            }
        };
    }
    status
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
        status = run(line.trim(), status);
    }
}

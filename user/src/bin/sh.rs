// aios の小さなシェル (sh -c CMD、sh FILE も)
//   パイプ |、つけかえ < > >>、並べる ; && &、クォート ' " \、変数 $VAR $? $! $0..$9 $# "$@"
//   組み込み: cd, exit, export, exec, jobs, fg, bg, wait, kill %N
//   対話するときはジョブ制御: パイプラインごとにプロセスグループを作り、Ctrl-Z で止めて fg / bg で戻す
use std::ffi::CString;
use std::io::{self, Write};

#[derive(Debug, PartialEq)]
enum Tok {
    Word(String),
    Pipe,
    Lt,
    Gt,
    GtGt,
}

/// 位置パラメータ ($0 と $1 以降)
static mut ARGS: Vec<String> = Vec::new();

fn params() -> &'static [String] {
    unsafe { &*(&raw const ARGS) }
}

fn set_params(v: Vec<String>) {
    unsafe { *(&raw mut ARGS) = v };
}

fn var(name: &str, status: i32) -> String {
    match name {
        "?" => status.to_string(),
        "$" => std::process::id().to_string(),
        "#" => params().len().saturating_sub(1).to_string(),
        "!" => match unsafe { LAST_BG } {
            0 => String::new(),
            p => p.to_string(),
        },
        "@" | "*" => params().get(1..).unwrap_or(&[]).join(" "),
        n if n.chars().all(|c| c.is_ascii_digit()) => n.parse::<usize>().ok().and_then(|i| params().get(i)).cloned().unwrap_or_default(),
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
    if *i < cs.len() && matches!(cs[*i], '?' | '$' | '#' | '@' | '*' | '!' | '0'..='9') {
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
                let mut empty_at = false;
                let w = word.get_or_insert_with(String::new);
                loop {
                    match cs.get(i) {
                        Some('"') => break,
                        Some('\\') if matches!(cs.get(i + 1), Some('"' | '\\' | '$')) => {
                            w.push(cs[i + 1]);
                            i += 1;
                        }
                        Some('$') if cs.get(i + 1) == Some(&'@') => {
                            // "$@" は引数ごとに別の語になる
                            i += 2;
                            let ps = params().get(1..).unwrap_or(&[]);
                            for (k, p) in ps.iter().enumerate() {
                                if k > 0 {
                                    toks.push(Tok::Word(std::mem::take(w)));
                                }
                                w.push_str(p);
                            }
                            if ps.is_empty() && w.is_empty() && cs.get(i) == Some(&'"') {
                                // 引数がなければ "$@" は何も残さない
                                empty_at = true;
                                break;
                            }
                            continue;
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
                if empty_at {
                    word = None;
                }
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

fn spawn(cmds: Vec<Cmd>, bg: bool, text: &str) -> i32 {
    let n = cmds.len();
    let job_ctl = interactive();
    let mut pgid = 0;
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
                if job_ctl {
                    // パイプラインごとに 1 つのプロセスグループ。前で動くならそれを端末の前に
                    libc::setpgid(0, pgid);
                    if !bg {
                        libc::tcsetpgrp(0, libc::getpgrp());
                    }
                } else if bg && c.stdin.is_none() && prev_read < 0 {
                    // ジョブ制御のないうしろのジョブは端末を読まない
                    redirect("/dev/null", 0, libc::O_RDONLY);
                }
                // Rust は SIGPIPE を、シェルは SIGINT/SIGQUIT (と止めるシグナル) を無視するが、
                // 子には既定の動作で渡す
                for sig in [libc::SIGPIPE, libc::SIGINT, libc::SIGQUIT, libc::SIGTSTP, libc::SIGTTIN, libc::SIGTTOU] {
                    libc::signal(sig, libc::SIG_DFL);
                }
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
            if job_ctl {
                if pgid == 0 {
                    pgid = pid;
                }
                // 子と同じことを親でもする (どちらが先に動いても揃うように)
                unsafe { libc::setpgid(pid, pgid) };
            }
            pids.push(pid);
        }
    }
    if pids.is_empty() {
        return 1;
    }
    let id = jobs().iter().map(|j| j.id).max().unwrap_or(0) + 1;
    let last_pid = *pids.last().unwrap();
    jobs().push(Job { id, pgid, pids: pids.into_iter().map(|p| (p, None)).collect(), cmd: text.to_string(), state: JobState::Running, notified: true, tmodes: None });
    if bg {
        unsafe { LAST_BG = last_pid };
        if interactive() {
            eprintln!("[{}] {}", id, last_pid);
        }
        return 0;
    }
    foreground(jobs().len() - 1)
}

// ---- ジョブ ----

#[derive(Clone, Copy, PartialEq)]
enum JobState {
    Running,
    Stopped,
    Done(i32),
}

struct Job {
    id: usize,
    /// プロセスグループ (ジョブ制御がなければ 0)
    pgid: i32,
    /// (pid, 終わっていれば終了ステータス)
    pids: Vec<(i32, Option<i32>)>,
    cmd: String,
    state: JobState,
    /// 状態の変化をもう知らせたか
    notified: bool,
    /// 止まったときの端末の設定 (fg で戻す)
    tmodes: Option<libc::termios>,
}

static mut JOBS: Vec<Job> = Vec::new();
static mut INTERACTIVE: bool = false;
static mut LAST_BG: i32 = 0;
/// シェル自身の端末の設定
static mut SHELL_TMODES: Option<libc::termios> = None;

fn jobs() -> &'static mut Vec<Job> {
    unsafe { &mut *(&raw mut JOBS) }
}

fn interactive() -> bool {
    unsafe { INTERACTIVE }
}

fn exit_code(st: i32) -> i32 {
    if libc::WIFSIGNALED(st) { 128 + libc::WTERMSIG(st) } else { libc::WEXITSTATUS(st) }
}

/// waitpid で分かったことをジョブに書く
fn record(pid: i32, st: i32) {
    for j in jobs().iter_mut() {
        let Some(k) = j.pids.iter().position(|(p, _)| *p == pid) else { continue };
        if libc::WIFSTOPPED(st) {
            j.state = JobState::Stopped;
            j.notified = false;
        } else if libc::WIFCONTINUED(st) {
            j.state = JobState::Running;
        } else {
            j.pids[k].1 = Some(exit_code(st));
            if j.pids.iter().all(|(_, s)| s.is_some()) {
                // パイプラインのステータスは最後のコマンドのもの
                j.state = JobState::Done(j.pids.last().unwrap().1.unwrap());
                j.notified = false;
            }
        }
        return;
    }
}

fn set_tmodes(t: &Option<libc::termios>) {
    if let Some(t) = t {
        unsafe { libc::tcsetattr(0, libc::TCSADRAIN, t) };
    }
}

/// 前のジョブが終わるか止まるまで待つ
fn foreground(idx: usize) -> i32 {
    let status = loop {
        let j = &mut jobs()[idx];
        match j.state {
            JobState::Done(s) => {
                jobs().remove(idx);
                break s;
            }
            JobState::Stopped => {
                j.notified = true;
                if interactive() {
                    let mut t: libc::termios = unsafe { std::mem::zeroed() };
                    if unsafe { libc::tcgetattr(0, &mut t) } == 0 {
                        j.tmodes = Some(t);
                    }
                    eprintln!("\n[{}]+  {:<24}{}", j.id, state_text(j.state), j.cmd);
                }
                break 128 + libc::SIGTSTP;
            }
            JobState::Running => {}
        }
        let Some(&(pid, _)) = j.pids.iter().find(|(_, s)| s.is_none()) else {
            j.state = JobState::Done(0);
            continue;
        };
        let mut st = 0;
        let r = unsafe { libc::waitpid(pid, &mut st, libc::WUNTRACED) };
        if r > 0 {
            record(r, st);
        } else if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            // もう待てない子 (誰かが先に回収した)
            if let Some(e) = jobs()[idx].pids.iter_mut().find(|(p, _)| *p == pid) {
                e.1 = Some(0);
            }
            let j = &mut jobs()[idx];
            if j.pids.iter().all(|(_, s)| s.is_some()) {
                j.state = JobState::Done(j.pids.last().unwrap().1.unwrap());
            }
        }
        // Ctrl-C でシェルのハンドラが動くと EINTR で戻るので、待ちなおす
    };
    if interactive() {
        // 端末をシェルに戻し、設定も戻す
        unsafe { libc::tcsetpgrp(0, libc::getpgrp()) };
        set_tmodes(unsafe { &*(&raw const SHELL_TMODES) });
    }
    status
}

/// うしろのジョブの様子を集めて、変わったものを知らせる (プロンプトの前に)
fn report_jobs() {
    loop {
        let mut st = 0;
        let r = unsafe { libc::waitpid(-1, &mut st, libc::WNOHANG | libc::WUNTRACED | libc::WCONTINUED) };
        if r <= 0 {
            break;
        }
        record(r, st);
    }
    jobs().retain_mut(|j| {
        if j.notified {
            return true;
        }
        j.notified = true;
        if j.state != JobState::Running {
            eprintln!("[{}]+  {:<24}{}", j.id, state_text(j.state), j.cmd);
        }
        !matches!(j.state, JobState::Done(_))
    });
}

/// jobs などに出す状態
fn state_text(s: JobState) -> String {
    match s {
        JobState::Running => "Running".into(),
        JobState::Stopped => "Stopped".into(),
        JobState::Done(0) => "Done".into(),
        JobState::Done(s) if s > 128 => match s - 128 {
            libc::SIGHUP => "Hangup".into(),
            libc::SIGINT => "Interrupt".into(),
            libc::SIGQUIT => "Quit".into(),
            libc::SIGKILL => "Killed".into(),
            libc::SIGSEGV => "Segmentation fault".into(),
            libc::SIGPIPE => "Broken pipe".into(),
            libc::SIGTERM => "Terminated".into(),
            n => format!("Signal {}", n),
        },
        JobState::Done(s) => format!("Exit {}", s),
    }
}

/// %N / %% / 省略 (いちばん新しいジョブ) からジョブの添字
fn find_job(arg: Option<&String>) -> Result<usize, String> {
    let js = jobs();
    let spec = arg.map(|s| s.as_str()).unwrap_or("%%");
    let idx = match spec {
        "%%" | "%+" | "%" => js.len().checked_sub(1),
        s => s.strip_prefix('%').and_then(|n| n.parse::<usize>().ok()).and_then(|n| js.iter().position(|j| j.id == n)),
    };
    idx.ok_or_else(|| format!("{}: no such job", spec))
}

fn job_builtin(args: &[String]) -> Option<i32> {
    let name = args[0].as_str();
    let res = match name {
        "jobs" => {
            report_jobs_quiet();
            for j in jobs().iter() {
                println!("[{}]   {:<24}{}", j.id, state_text(j.state), j.cmd);
            }
            // 終わったものは一度見せたら消す
            jobs().retain(|j| !matches!(j.state, JobState::Done(_)));
            Ok(0)
        }
        "fg" | "bg" => find_job(args.get(1)).map(|idx| {
            let j = &mut jobs()[idx];
            let target = if j.pgid > 0 { -j.pgid } else { j.pids[0].0 };
            if name == "fg" {
                println!("{}", j.cmd);
                if interactive() && j.pgid > 0 {
                    unsafe { libc::tcsetpgrp(0, j.pgid) };
                    set_tmodes(&j.tmodes.take());
                }
                j.state = JobState::Running;
                unsafe { libc::kill(target, libc::SIGCONT) };
                foreground(idx)
            } else {
                println!("[{}]+ {} &", j.id, j.cmd);
                j.state = JobState::Running;
                unsafe { libc::kill(target, libc::SIGCONT) };
                0
            }
        }),
        "wait" => {
            let mut s = 0;
            while let Some(idx) = jobs().iter().position(|j| j.state == JobState::Running) {
                s = foreground_quiet(idx);
            }
            Ok(s)
        }
        // kill %N はジョブのグループへ。それ以外は /bin/kill に任せる
        "kill" if args.iter().any(|a| a.starts_with('%')) => {
            let sig = args.get(1).filter(|a| a.starts_with('-')).map(|a| signal_number(&a[1..]));
            let sig = match sig {
                Some(Some(s)) => s,
                Some(None) => {
                    eprintln!("kill: {}: invalid signal", args[1]);
                    return Some(1);
                }
                None => libc::SIGTERM,
            };
            let mut st = 0;
            for a in args[1..].iter().filter(|a| a.starts_with('%')) {
                match find_job(Some(a)) {
                    Ok(idx) => {
                        let j = &jobs()[idx];
                        let target = if j.pgid > 0 { -j.pgid } else { j.pids[0].0 };
                        unsafe { libc::kill(target, sig) };
                        if sig != libc::SIGCONT && j.state == JobState::Stopped {
                            unsafe { libc::kill(target, libc::SIGCONT) };
                        }
                    }
                    Err(e) => {
                        eprintln!("kill: {}", e);
                        st = 1;
                    }
                }
            }
            Ok(st)
        }
        _ => return None,
    };
    Some(res.unwrap_or_else(|e| {
        eprintln!("{}: {}", name, e);
        1
    }))
}

fn signal_number(s: &str) -> Option<i32> {
    if let Ok(n) = s.parse() {
        return Some(n);
    }
    let s = s.strip_prefix("SIG").unwrap_or(s);
    Some(match s {
        "HUP" => libc::SIGHUP,
        "INT" => libc::SIGINT,
        "QUIT" => libc::SIGQUIT,
        "KILL" => libc::SIGKILL,
        "TERM" => libc::SIGTERM,
        "STOP" => libc::SIGSTOP,
        "TSTP" => libc::SIGTSTP,
        "CONT" => libc::SIGCONT,
        "USR1" => libc::SIGUSR1,
        "USR2" => libc::SIGUSR2,
        _ => return None,
    })
}

/// jobs の前に、終わったものの状態だけ集める (知らせは jobs の表示で)
fn report_jobs_quiet() {
    loop {
        let mut st = 0;
        let r = unsafe { libc::waitpid(-1, &mut st, libc::WNOHANG | libc::WUNTRACED | libc::WCONTINUED) };
        if r <= 0 {
            break;
        }
        record(r, st);
    }
    for j in jobs().iter_mut() {
        j.notified = true;
    }
}

/// wait 用: 端末は動かさずに待つ
fn foreground_quiet(idx: usize) -> i32 {
    let was = unsafe { INTERACTIVE };
    unsafe { INTERACTIVE = false };
    let s = foreground(idx);
    unsafe { INTERACTIVE = was };
    s
}

fn builtin(args: &[String], status: i32) -> Option<i32> {
    if let Some(s) = job_builtin(args) {
        return Some(s);
    }
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
        "exec" if args.len() > 1 => {
            let Some(prog) = find(&args[1]) else {
                eprintln!("sh: {}: command not found", args[1]);
                std::process::exit(127);
            };
            let cargs: Vec<CString> = args[1..].iter().map(|a| CString::new(a.as_str()).unwrap()).collect();
            let mut argv: Vec<*const libc::c_char> = cargs.iter().map(|a| a.as_ptr()).collect();
            argv.push(std::ptr::null());
            let env: Vec<CString> = std::env::vars().map(|(k, v)| CString::new(format!("{k}={v}")).unwrap()).collect();
            let mut envp: Vec<*const libc::c_char> = env.iter().map(|e| e.as_ptr()).collect();
            envp.push(std::ptr::null());
            unsafe {
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
                libc::execve(prog.as_ptr(), argv.as_ptr(), envp.as_ptr());
            }
            eprintln!("sh: {}: {}", args[1], io::Error::last_os_error());
            std::process::exit(126);
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

/// クォートの外にある ; && & で区切る。(コマンド, 前が成功したときだけ, うしろで動かす)
fn split_list(line: &str) -> Vec<(String, bool, bool)> {
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
                out.push((std::mem::take(&mut cur), and, false));
                and = false;
                continue;
            }
            (None, '&') if cs.peek() == Some(&'&') => {
                cs.next();
                out.push((std::mem::take(&mut cur), and, false));
                and = true;
                continue;
            }
            (None, '&') => {
                out.push((std::mem::take(&mut cur), and, true));
                and = false;
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    out.push((cur, and, false));
    out
}

fn run(line: &str, mut status: i32) -> i32 {
    let mut skip = false;
    for (part, after_and, bg) in split_list(line) {
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
        let text = part.trim();
        status = match pipeline(&toks) {
            Ok(cmds) if cmds.len() == 1 && !bg => match builtin(&cmds[0].args, status) {
                Some(s) => s,
                None => spawn(cmds, bg, text),
            },
            Ok(cmds) => spawn(cmds, bg, text),
            Err(e) => {
                eprintln!("sh: {}", e);
                2
            }
        };
    }
    status
}

fn main() {
    // sh -c CMD / sh FILE
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.get(1).filter(|a| *a == "-c").map(|_| 1) {
        let cmd = args.get(i + 1).cloned().unwrap_or_default();
        // sh -c CMD NAME ARGS... は NAME が $0
        let mut ps: Vec<String> = args.get(i + 2..).unwrap_or(&[]).to_vec();
        if ps.is_empty() {
            ps.push(args[0].clone());
        }
        set_params(ps);
        std::process::exit(run(&cmd, 0));
    }
    if let Some(file) = args.get(1).filter(|a| !a.starts_with('-')) {
        set_params(args[1..].to_vec());
        let text = match std::fs::read_to_string(file) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("sh: {}: {}", file, e);
                std::process::exit(127);
            }
        };
        let mut status = 0;
        for line in text.lines() {
            status = run(line.trim(), status);
        }
        std::process::exit(status);
    }
    set_params(vec![args[0].clone()]);
    // 対話するシェルは Ctrl-C で終わらず (打ちかけの行を捨てて新しいプロンプトへ)、
    // 自分のグループを端末の前に出す
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_sigint as *const () as usize;
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
        for sig in [libc::SIGQUIT, libc::SIGTSTP, libc::SIGTTIN, libc::SIGTTOU] {
            libc::signal(sig, libc::SIG_IGN);
        }
        // 端末があればジョブ制御をする
        if libc::isatty(0) == 1 {
            INTERACTIVE = true;
            libc::setpgid(0, 0);
            libc::tcsetpgrp(0, libc::getpgrp());
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) == 0 {
                *(&raw mut SHELL_TMODES) = Some(t);
            }
        }
    }
    let mut status = 0;
    loop {
        report_jobs();
        let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
        let mark = if status != 0 { "!" } else if unsafe { libc::geteuid() } == 0 { "#" } else { "%" };
        print!("{} {} ", cwd, mark);
        io::stdout().flush().ok();

        let line = match read_line() {
            Ok(Some(l)) => l,
            Ok(None) => {
                println!();
                return;
            }
            Err(_) => {
                println!();
                status = 130;
                continue;
            }
        };
        status = run(line.trim(), status);
    }
}

extern "C" fn on_sigint(_: libc::c_int) {}

/// 端末から 1 行。Ctrl-C (SIGINT で read が EINTR) なら Err、終わりなら None
fn read_line() -> io::Result<Option<String>> {
    let mut buf = Vec::new();
    loop {
        let mut c = 0u8;
        let n = unsafe { libc::read(0, &mut c as *mut u8 as *mut libc::c_void, 1) };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n == 0 {
            return Ok(if buf.is_empty() { None } else { Some(String::from_utf8_lossy(&buf).into_owned()) });
        }
        buf.push(c);
        if c == b'\n' {
            return Ok(Some(String::from_utf8_lossy(&buf).into_owned()));
        }
    }
}

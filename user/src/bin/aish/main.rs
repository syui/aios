// aish: aios のシェル (POSIX sh のだいたい + 対話の機能)。aish -c CMD、aish FILE ARGS...、対話も
//   /bin/sh は aish へのリンク (bash と同じやり方)。sh として呼ばれたら POSIX の sh として
//   設定は $ENV だけ読み、aish として呼ばれたら /etc/aishrc と ~/.aishrc を読む
//   パイプ |、つけかえ < > >> <> >| N>&M N<&- <<EOF <<-EOF、並べる ; && || &、! パイプライン
//   if / while / until / for / case / { } / ( )、関数 NAME() { ... } (local, return)
//   展開: ~ $NAME ${NAME} ${NAME:-x} ${NAME:=x} ${NAME:+x} ${NAME:?x} ${#NAME} ${NAME#p} ${NAME##p} ${NAME%p} ${NAME%%p}
//         $(cmd) `cmd` $((式)) "$@"、IFS で分ける、ワイルドカード * ? [...]
//   組み込み: cd pwd exit export unset set shift read local eval . source echo test [ true false :
//             return break continue exec command type、ジョブ: jobs fg bg wait kill %N
//   set -e (失敗で終わる)、set -x (実行するコマンドを見せる)
//   bash と zsh から: ブレース展開 {a,b} {1..3}、[[ ... ]] (== のパターン、=~ の正規表現、&& || ! ( ) < >)、
//   ** (下のディレクトリぜんぶ)、配列 a=(x y) a+=(z) $a ${a[@]} ${a[*]} ${a[N]} ${#a}
//   (番号は zsh と同じ 1 から。setopt ksharrays で bash と同じ 0 から)、path=(...) は $PATH とつながる、
//   ${x:t} ${x:h} ${x:r} ${x:e} ${x:l} ${x:u} (zsh)、${x:OFF:LEN} (bash)、*.zsh(N) (.) (/) (@) (zsh のワイルドカードの修飾)、
//   setopt / unsetopt (nullglob と ksharrays が効く)、typeset / declare、{ echo a } (zsh。} の前の ; はいらない)、
//   zmodload zstyle autoload compinit compdef と bindkey -M はなにもしない (.zshrc をそのまま読めるように)、$OSTYPE
//   対話するときはジョブ制御: パイプラインごとにプロセスグループを作り、Ctrl-Z で止めて fg / bg で戻す
//   対話するときは行の編集と履歴 (edit.rs)、alias
//   読むファイル: ログインのとき /etc/profile, ~/.profile。対話するとき aish は /etc/aishrc, ~/.aishrc、
//   sh は $ENV
mod edit;
mod expand;
mod glob;
mod jobs;
mod mcp;
mod trap;
mod parse;
mod plugin;

use expand::Mode;
use jobs::{exit_code, interactive, jobs};
use parse::{AndOr, Cmd, Compound, Error, List, Parser, Pipeline, RKind, Redir};
use std::collections::HashMap;
use std::sync::OnceLock;

/// 呼ばれた名前 ("aish" か "sh")。エラーの頭にも使う
static NAME: OnceLock<&'static str> = OnceLock::new();

fn shell_name() -> &'static str {
    NAME.get().copied().unwrap_or("aish")
}
use std::ffi::CString;
use std::io::{self, Write};
use std::rc::Rc;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Flow {
    None,
    Break(u32),
    Continue(u32),
    Return,
}

/// 展開したつけかえ
#[derive(Clone)]
enum RT {
    File(String, i32),
    Dup(i32),
    Close,
    Here(String),
}

/// 展開を終えた外のコマンド
struct Ready {
    args: Vec<String>,
    assigns: Vec<(String, String)>,
    /// 配列の代入 (名前, 足すか, 要素)。コマンドのないときだけ
    arrays: Vec<(String, bool, Vec<String>)>,
    redirs: Vec<(i32, RT)>,
}

/// パイプラインの 1 つ (子の中で動かす)
enum Part {
    Ready(Ready),
    Ast(Cmd),
    AndOr(AndOr),
}

pub struct Shell {
    /// export していない変数 (export したものは環境変数)
    vars: HashMap<String, String>,
    funcs: HashMap<String, Rc<Compound>>,
    /// $0 と $1 以降
    pub params: Vec<String>,
    pub status: i32,
    pub pid: i32,
    pub errexit: bool,
    pub xtrace: bool,
    flow: Flow,
    /// if / while の条件、! の中 (set -e で終わらない)
    cond: u32,
    loops: u32,
    /// 関数ごとに local で変えたもの (名前, 前の値, export されていたか)
    locals: Vec<Vec<(String, Option<String>, bool)>>,
    /// . で読んでいる深さ (return できる)
    sourcing: u32,
    /// $( ) の終了ステータス (代入だけのコマンドの $? に)
    subst_status: Option<i32>,
    /// <(...) >(...) の (親が持つ口, 子の pid)。その行が終わったら閉じて待つ
    procsubs: Vec<(i32, i32)>,
    /// ジョブの表示に使う、いま動かしているもののソース
    text: String,
    /// alias NAME=VALUE
    aliases: HashMap<String, String>,
    /// いま展開している alias (自分自身をくりかえし展開しない)
    expanding: Vec<String>,
    /// プラグイン (plugin.rs。対話するシェルだけ)
    plugins: plugin::Plugins,
    /// 履歴のファイル (プラグインに教える)
    histfile: Option<String>,
    /// setopt でつけたもの (zsh の名前を小文字にして _ を除いたもの。nullglob と ksharrays が効く)
    opts: std::collections::BTreeSet<String>,
    /// 配列 (a=(x y z))。path は $PATH とつながっている (zsh と同じ)
    arrays: HashMap<String, Vec<String>>,
}

const BUILTINS: &[&str] = &[
    ":", "true", "false", "[[", "setopt", "unsetopt", "typeset", "declare", "zmodload", "zstyle", "autoload", "compinit", "compdef", "cd", "pwd", "exit", "export", "unset", "set", "shift", "read", "local", "eval", ".", "source", "echo", "test", "[", "return",
    "break", "continue", "exec", "command", "type", "umask", "jobs", "fg", "bg", "wait", "alias", "unalias", "plugin", "bindkey", "trap", "tool",
];

fn is_builtin(args: &[String]) -> bool {
    BUILTINS.contains(&args[0].as_str()) || (args[0] == "kill" && args.iter().any(|a| a.starts_with('%')))
}

fn flush() {
    io::stdout().flush().ok();
    io::stderr().flush().ok();
}

fn exit_shell(st: i32) -> ! {
    // EXIT の trap (trap を置いたシェルのプロセスだけ)
    trap::run_exit();
    flush();
    std::process::exit(st)
}

/// エラーの文 ("(os error N)" を除く)
fn err_text(e: &io::Error) -> String {
    let s = e.to_string();
    match s.find(" (os error") {
        Some(i) => s[..i].to_string(),
        None => s,
    }
}

fn last_err() -> String {
    err_text(&io::Error::last_os_error())
}

/// 動かせるファイルか (PATH で探すとき。動かせないファイルは飛ばして先を探す。bash と同じ)
pub fn is_exec(p: &str) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.is_file()) && unsafe { libc::access(cstr(p).as_ptr(), libc::X_OK) } == 0
}

fn cstr(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap()
}

impl Shell {
    fn new(params: Vec<String>) -> Shell {
        Shell {
            vars: HashMap::new(),
            funcs: HashMap::new(),
            params,
            status: 0,
            pid: std::process::id() as i32,
            errexit: false,
            xtrace: false,
            flow: Flow::None,
            cond: 0,
            loops: 0,
            locals: vec![],
            sourcing: 0,
            subst_status: None,
            procsubs: Vec::new(),
            text: String::new(),
            aliases: HashMap::new(),
            expanding: vec![],
            plugins: plugin::Plugins::default(),
            histfile: None,
            opts: Default::default(),
            arrays: HashMap::new(),
        }
        // zsh と bash の $OSTYPE (.zshrc の case $OSTYPE in linux*) で分けられるように)
        .with_var("OSTYPE", "linux-musl")
    }

    fn with_var(mut self, k: &str, v: &str) -> Shell {
        if self.get_var(k).is_none() {
            self.vars.insert(k.into(), v.into());
        }
        self
    }

    /// 配列 (なければ None)。path は $PATH を : で分けたもの
    pub fn array(&self, k: &str) -> Option<Vec<String>> {
        if k == "path" {
            return Some(self.get_var("PATH").unwrap_or_default().split(':').filter(|x| !x.is_empty()).map(String::from).collect());
        }
        self.arrays.get(k).cloned()
    }

    pub fn set_array(&mut self, k: &str, v: Vec<String>) {
        if k == "path" {
            let p = v.join(":");
            return self.set_var("PATH", &p);
        }
        self.vars.remove(k);
        self.arrays.insert(k.to_string(), v);
    }

    /// 配列の番号の始まり (zsh は 1、setopt ksharrays なら bash と同じ 0)
    pub fn array_base(&self) -> i64 {
        if self.opts.contains("ksharrays") { 0 } else { 1 }
    }

    /// 当たらないワイルドカードを消すか (setopt nullglob)
    pub fn nullglob(&self) -> bool {
        self.opts.contains("nullglob")
    }

    // ---- 変数 ----

    pub fn get_var(&self, k: &str) -> Option<String> {
        self.vars.get(k).cloned().or_else(|| std::env::var(k).ok())
    }

    pub fn set_var(&mut self, k: &str, v: &str) {
        self.arrays.remove(k);
        if std::env::var_os(k).is_some() {
            unsafe { std::env::set_var(k, v) };
        } else {
            self.vars.insert(k.to_string(), v.to_string());
        }
    }

    fn export(&mut self, k: &str, v: Option<String>) {
        let v = v.or_else(|| self.vars.get(k).cloned());
        self.vars.remove(k);
        if let Some(v) = v {
            unsafe { std::env::set_var(k, v) };
        }
    }

    fn unset(&mut self, k: &str) {
        self.vars.remove(k);
        self.arrays.remove(k);
        unsafe { std::env::remove_var(k) };
    }

    // ---- 動かす ----

    /// ソースを 1 コマンドずつ読んで動かす (sh FILE、sh -c、. 、eval)
    fn run_source(&mut self, src: &str, name: &str) -> i32 {
        let mut p = Parser::new(src);
        loop {
            match p.complete_command() {
                Ok(Some(list)) => {
                    self.run_list(&list);
                    if self.flow != Flow::None {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let msg = match e {
                        Error::Incomplete => "syntax error: unexpected end of file".into(),
                        Error::Syntax(s) => s,
                    };
                    eprintln!("{}: {}", name, msg);
                    self.status = 2;
                    break;
                }
            }
        }
        self.status
    }

    /// 来たシグナルの trap を動かす ($? は変えない)
    fn run_traps(&mut self) {
        for cmd in trap::take_pending() {
            let st = self.status;
            self.run_source(&cmd, "trap");
            self.status = st;
        }
    }

    fn run_list(&mut self, list: &List) -> i32 {
        self.run_traps();
        for item in list {
            if self.flow != Flow::None || mcp::stopped() {
                break;
            }
            self.text = item.text.clone();
            if item.bg {
                let parts = if item.ao.rest.is_empty() && !item.ao.first.neg {
                    item.ao.first.cmds.iter().cloned().map(Part::Ast).collect()
                } else {
                    vec![Part::AndOr(item.ao.clone())]
                };
                let text = item.text.clone();
                self.status = self.spawn(parts, true, &text);
            } else {
                self.status = self.run_andor(&item.ao);
            }
            self.run_traps();
        }
        self.status
    }

    fn run_andor(&mut self, ao: &AndOr) -> i32 {
        let mut st = self.run_pipeline(&ao.first, ao.rest.is_empty());
        for (k, (and, p)) in ao.rest.iter().enumerate() {
            if self.flow != Flow::None || mcp::stopped() {
                break;
            }
            if (*and && st != 0) || (!*and && st == 0) {
                continue;
            }
            st = self.run_pipeline(p, k + 1 == ao.rest.len());
        }
        st
    }

    /// last: && || の最後 (set -e で見る)
    fn run_pipeline(&mut self, p: &Pipeline, last: bool) -> i32 {
        let subs = self.procsubs.len();
        let st = self.run_pipeline1(p, last);
        // この行で作った <(...) >(...): 口を閉じて (読む子は SIGPIPE、書く子は EOF で終わる)、待つ
        for (fd, pid) in self.procsubs.split_off(subs) {
            unsafe {
                libc::close(fd);
                let mut w = 0;
                while libc::waitpid(pid, &mut w, 0) < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {}
            }
        }
        st
    }

    fn run_pipeline1(&mut self, p: &Pipeline, last: bool) -> i32 {
        if !last || p.neg {
            self.cond += 1;
        }
        let start = p.time.then(|| (std::time::Instant::now(), cpu_times()));
        let mut st = if p.cmds.len() == 1 {
            self.run_cmd(&p.cmds[0])
        } else {
            let text = self.text.clone();
            self.spawn(p.cmds.iter().cloned().map(Part::Ast).collect(), false, &text)
        };
        if !last || p.neg {
            self.cond -= 1;
        }
        if let Some((t0, (u0, s0))) = start {
            let (u1, s1) = cpu_times();
            let f = |d: f64| format!("{}m{:.3}s", (d / 60.0) as u64, d % 60.0);
            eprintln!("\nreal\t{}\nuser\t{}\nsys\t{}", f(t0.elapsed().as_secs_f64()), f(u1 - u0), f(s1 - s0));
        }
        if p.neg {
            st = (st == 0) as i32;
        }
        self.status = st;
        if self.errexit && st != 0 && last && !p.neg && self.cond == 0 && self.flow == Flow::None {
            exit_shell(st);
        }
        st
    }

    fn run_cmd(&mut self, cmd: &Cmd) -> i32 {
        match cmd {
            Cmd::Func(name, body) => {
                self.funcs.insert(name.clone(), body.clone());
                0
            }
            Cmd::Compound(c, _) if matches!(**c, Compound::Subshell(_)) => {
                let text = self.text.clone();
                self.spawn(vec![Part::Ast(cmd.clone())], false, &text)
            }
            Cmd::Compound(c, redirs) => match self.expand_redirs(redirs) {
                Ok(r) => self.with_redirs(&r, |sh| sh.run_compound(c)),
                Err(e) => {
                    eprintln!("{}: {}", shell_name(), e);
                    1
                }
            },
            Cmd::Simple { assigns, words, redirs } => self.run_simple(assigns, words, redirs),
        }
    }

    /// 単純なコマンドの語、代入、つけかえを展開する
    fn prepare(&mut self, assigns: &[(String, String)], words: &[String], redirs: &[Redir]) -> Result<Ready, String> {
        self.subst_status = None;
        let words = self.expand_alias(words);
        let mut args = vec![];
        // [[ ... ]] の中は分けず、ワイルドカードも広げない (== の右のパターンの印は残す)
        let cond = words.first().is_some_and(|w| w == "[[");
        for (k, w) in words.iter().enumerate() {
            if cond && (k == 0 || k + 1 == words.len()) {
                args.push(w.clone());
            } else if cond {
                args.push(self.expand(w, Mode::Pattern)?.pop().unwrap_or_default());
            } else {
                args.extend(self.expand(w, Mode::Fields)?);
            }
        }
        let mut avals = vec![];
        let mut arrays = vec![];
        for (k, w) in assigns {
            if let Some(items) = w.strip_prefix(parse::ARRAY) {
                let mut vals = vec![];
                for x in items.split(parse::SEP).filter(|x| !x.is_empty()) {
                    vals.extend(self.expand(x, Mode::Fields)?);
                }
                let (name, add) = match k.strip_suffix('+') {
                    Some(n) => (n.to_string(), true),
                    None => (k.clone(), false),
                };
                arrays.push((name, add, vals));
                continue;
            }
            avals.push((k.clone(), self.expand_one(w)?));
        }
        let redirs = self.expand_redirs(redirs)?;
        if self.xtrace {
            let a: Vec<String> = avals.iter().map(|(k, v)| format!("{}={}", k, v)).chain(args.iter().cloned()).collect();
            eprintln!("+ {}", a.join(" "));
        }
        Ok(Ready { args, assigns: avals, arrays, redirs })
    }

    /// 行の編集に渡す、補完などのための様子
    fn edit_ctx(&self) -> edit::Ctx {
        let home = self.get_var("HOME").unwrap_or_default();
        let mut cmds: Vec<String> = BUILTINS.iter().map(|s| s.to_string()).collect();
        cmds.extend(self.aliases.keys().cloned());
        cmds.extend(self.funcs.keys().cloned());
        let mut vars: Vec<String> = self.vars.keys().cloned().collect();
        vars.extend(std::env::vars().map(|(k, _)| k));
        let pwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
        edit::Ctx { cmds, vars, path: self.get_var("PATH").unwrap_or_default(), home, pwd }
    }

    /// 最初の語が alias なら置きかえる (引用していない語だけ)。
    /// 値が 1 つの単純なコマンドなら語を並べかえ、ほか (| ; && など) なら eval にまかせる
    fn expand_alias(&mut self, words: &[String]) -> Vec<String> {
        let Some(first) = words.first() else { return vec![] };
        let Some(value) = self.aliases.get(first).cloned().filter(|_| !self.expanding.contains(first)) else {
            return words.to_vec();
        };
        if let Ok(list) = Parser::new(&format!("{}\n", value)).program()
            && let [item] = list.as_slice()
            && item.ao.rest.is_empty()
            && !item.bg
            && !item.ao.first.neg
            && let [Cmd::Simple { assigns, words: aw, redirs }] = item.ao.first.cmds.as_slice()
            && assigns.is_empty()
            && redirs.is_empty()
            && !aw.is_empty()
        {
            // 置きかえた先の最初の語も alias かもしれない (自分自身はのぞく)
            self.expanding.push(first.clone());
            let mut out = self.expand_alias(aw);
            self.expanding.pop();
            out.extend(words[1..].iter().cloned());
            return out;
        }
        let mut src = value;
        for w in &words[1..] {
            src.push(' ');
            src.push_str(w);
        }
        vec!["eval".into(), quote(&src)]
    }

    fn run_simple(&mut self, assigns: &[(String, String)], words: &[String], redirs: &[Redir]) -> i32 {
        let r = match self.prepare(assigns, words, redirs) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{}: {}", shell_name(), e);
                if !interactive() && self.sourcing == 0 && self.locals.is_empty() {
                    exit_shell(1);
                }
                return 1;
            }
        };
        if r.args.is_empty() {
            for (k, v) in &r.assigns {
                self.set_var(k, v);
            }
            for (k, add, vals) in &r.arrays {
                let mut v = if *add { self.array(k).unwrap_or_default() } else { vec![] };
                v.extend(vals.iter().cloned());
                self.set_array(k, v);
            }
            let st = self.with_redirs(&r.redirs, |_| 0);
            return if st != 0 { st } else { self.subst_status.unwrap_or(0) };
        }
        if let Some(body) = self.funcs.get(&r.args[0]).cloned() {
            let args = r.args.clone();
            return self.with_temp_vars(&r.assigns, |sh| sh.with_redirs(&r.redirs, |sh| sh.call(&body, &args)));
        }
        if is_builtin(&r.args) {
            // exec だけならつけかえはそのまま残す
            if r.args[0] == "exec" && r.args.len() == 1 {
                flush();
                for (fd, t) in &r.redirs {
                    if !apply_redir(*fd, t) {
                        return 1;
                    }
                }
                return 0;
            }
            let args = r.args.clone();
            return self.with_temp_vars(&r.assigns, |sh| sh.with_redirs(&r.redirs, |sh| sh.builtin(&args)));
        }
        // コマンドが見つからない: not_found のプラグインにまかせる (答えに status があれば、それが結果)
        if self.plugins.wants("not_found") && self.find(&r.args[0]).is_none() {
            // env: いまの環境 (export したものと、このコマンドの前の VAR=x)。プラグインは起こされたときの環境しか
            // 知らないので、あとで変えた PATH などもここで渡す
            let mut env: serde_json::Map<String, serde_json::Value> = std::env::vars().map(|(k, v)| (k, serde_json::Value::String(v))).collect();
            for (k, v) in &r.assigns {
                env.insert(k.clone(), serde_json::Value::String(v.clone()));
            }
            let ev = serde_json::json!({
                "args": r.args,
                "line": self.text,
                "pwd": std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default(),
                "status": self.status,
                "env": env,
            });
            if let Some(st) = self.plugins.ask("not_found", ev).and_then(|v| v["status"].as_i64()) {
                return st as i32;
            }
        }
        let text = self.text.clone();
        self.spawn(vec![Part::Ready(r)], false, &text)
    }

    fn run_compound(&mut self, c: &Compound) -> i32 {
        match c {
            Compound::Brace(l) | Compound::Subshell(l) => self.run_list(l),
            Compound::If(arms, els) => {
                for (cond, body) in arms {
                    self.cond += 1;
                    let st = self.run_list(cond);
                    self.cond -= 1;
                    if self.flow != Flow::None {
                        return st;
                    }
                    if st == 0 {
                        return self.run_list(body);
                    }
                }
                match els {
                    Some(e) => self.run_list(e),
                    None => 0,
                }
            }
            Compound::While(cond, body, until) => {
                let mut last = 0;
                self.loops += 1;
                loop {
                    self.cond += 1;
                    let st = self.run_list(cond);
                    self.cond -= 1;
                    if mcp::stopped() {
                        break;
                    }
                    if self.flow != Flow::None || (st == 0) == *until {
                        if self.loop_flow() {
                            continue;
                        }
                        break;
                    }
                    last = self.run_list(body);
                    if self.flow != Flow::None && !self.loop_flow() {
                        break;
                    }
                }
                self.loops -= 1;
                self.status = last;
                last
            }
            Compound::For(name, items, body) => {
                let vals = match items {
                    None => self.params.get(1..).unwrap_or(&[]).to_vec(),
                    Some(ws) => {
                        let mut v = vec![];
                        for w in ws {
                            match self.expand(w, Mode::Fields) {
                                Ok(x) => v.extend(x),
                                Err(e) => {
                                    eprintln!("{}: {}", shell_name(), e);
                                    return 1;
                                }
                            }
                        }
                        v
                    }
                };
                let mut last = 0;
                self.loops += 1;
                for v in vals {
                    self.set_var(name, &v);
                    last = self.run_list(body);
                    if self.flow != Flow::None && !self.loop_flow() {
                        break;
                    }
                }
                self.loops -= 1;
                self.status = last;
                last
            }
            Compound::Case(w, arms) => {
                let s: Vec<char> = match self.expand_one(w) {
                    Ok(s) => s.chars().collect(),
                    Err(e) => {
                        eprintln!("{}: {}", shell_name(), e);
                        return 1;
                    }
                };
                for (pats, body) in arms {
                    for p in pats {
                        let pat: Vec<char> = match self.expand(p, Mode::Pattern) {
                            Ok(mut v) => v.pop().unwrap_or_default().chars().collect(),
                            Err(e) => {
                                eprintln!("{}: {}", shell_name(), e);
                                return 1;
                            }
                        };
                        if glob::glob_match(&pat, &s) {
                            return self.run_list(body);
                        }
                    }
                }
                0
            }
        }
    }

    /// ループの中で break / continue を受けとる。続けるなら true
    fn loop_flow(&mut self) -> bool {
        match self.flow {
            Flow::Break(n) => {
                self.flow = if n > 1 { Flow::Break(n - 1) } else { Flow::None };
                false
            }
            Flow::Continue(n) if n > 1 => {
                self.flow = Flow::Continue(n - 1);
                false
            }
            Flow::Continue(_) => {
                self.flow = Flow::None;
                true
            }
            _ => false,
        }
    }

    fn call(&mut self, body: &Rc<Compound>, args: &[String]) -> i32 {
        let mut ps = vec![self.params[0].clone()];
        ps.extend_from_slice(&args[1..]);
        let saved = std::mem::replace(&mut self.params, ps);
        let saved_loops = std::mem::replace(&mut self.loops, 0);
        self.locals.push(vec![]);
        let mut st = self.run_compound(body);
        if self.flow == Flow::Return {
            self.flow = Flow::None;
            st = self.status;
        }
        for (k, old, exported) in self.locals.pop().unwrap().into_iter().rev() {
            self.vars.remove(&k);
            match (old, exported) {
                (Some(v), true) => unsafe { std::env::set_var(&k, v) },
                (Some(v), false) => {
                    unsafe { std::env::remove_var(&k) };
                    self.vars.insert(k, v);
                }
                (None, _) => unsafe { std::env::remove_var(&k) },
            }
        }
        self.loops = saved_loops;
        self.params = saved;
        self.status = st;
        st
    }

    /// VAR=x cmd: cmd の間だけ変数を変える
    fn with_temp_vars(&mut self, assigns: &[(String, String)], f: impl FnOnce(&mut Shell) -> i32) -> i32 {
        let saved: Vec<(String, Option<String>, Option<String>)> =
            assigns.iter().map(|(k, _)| (k.clone(), self.vars.get(k).cloned(), std::env::var(k).ok())).collect();
        for (k, v) in assigns {
            self.set_var(k, v);
        }
        let st = f(self);
        for (k, var, env) in saved {
            self.vars.remove(&k);
            unsafe { std::env::remove_var(&k) };
            if let Some(v) = var {
                self.vars.insert(k.clone(), v);
            }
            if let Some(v) = env {
                unsafe { std::env::set_var(&k, v) };
            }
        }
        st
    }

    // ---- つけかえ ----

    fn expand_redirs(&mut self, redirs: &[Redir]) -> Result<Vec<(i32, RT)>, String> {
        let mut out = vec![];
        for r in redirs {
            let t = match &r.kind {
                RKind::File(w, flags) => RT::File(self.expand_one(w)?, *flags),
                RKind::Dup(w) => {
                    let n = self.expand_one(w)?;
                    if n == "-" {
                        RT::Close
                    } else {
                        RT::Dup(n.parse().map_err(|_| format!("{}: bad file descriptor", n))?)
                    }
                }
                RKind::Here(body, expand) => {
                    let b = body.borrow().clone();
                    RT::Here(if *expand { self.expand_heredoc(&b)? } else { b })
                }
                RKind::HereStr(w) => RT::Here(self.expand_one(w)? + "\n"),
            };
            out.push((r.fd, t));
        }
        Ok(out)
    }

    /// heredoc の中: $ ` \ だけを扱う (" や ' はそのまま)
    fn expand_heredoc(&mut self, s: &str) -> Result<String, String> {
        let mut w = String::from("\"");
        for c in s.chars() {
            if c == '"' {
                w.push_str("\\\"");
            } else {
                w.push(c);
            }
        }
        w.push('"');
        // \" を " に戻す以外は " の中と同じ
        self.expand_one(&w)
    }

    /// つけかえて f を動かし、もとに戻す (組み込みや { } で)
    fn with_redirs(&mut self, r: &[(i32, RT)], f: impl FnOnce(&mut Shell) -> i32) -> i32 {
        if r.is_empty() {
            return f(self);
        }
        flush();
        let mut saved = vec![];
        let mut ok = true;
        for (fd, t) in r {
            if !saved.iter().any(|(s, _)| s == fd) {
                let s = unsafe { libc::fcntl(*fd, libc::F_DUPFD_CLOEXEC, 10) };
                saved.push((*fd, s));
            }
            if !apply_redir(*fd, t) {
                ok = false;
                break;
            }
        }
        let st = if ok { f(self) } else { 1 };
        flush();
        for (fd, s) in saved.into_iter().rev() {
            unsafe {
                if s >= 0 {
                    libc::dup2(s, fd);
                    libc::close(s);
                } else {
                    libc::close(fd);
                }
            }
        }
        st
    }

    // ---- 子のプロセス ----

    fn spawn(&mut self, parts: Vec<Part>, bg: bool, text: &str) -> i32 {
        let n = parts.len();
        let job_ctl = interactive();
        let mut pgid = 0;
        let mut pids = vec![];
        let mut prev_read = -1;
        for (i, part) in parts.into_iter().enumerate() {
            let mut fds = [-1, -1];
            if i + 1 < n && unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
                eprintln!("{}: pipe: {}", shell_name(), last_err());
                break;
            }
            flush();
            let pid = unsafe { libc::fork() };
            if pid == 0 {
                unsafe {
                    if job_ctl {
                        // パイプラインごとに 1 つのプロセスグループ。前で動くならそれを端末の前に
                        libc::setpgid(0, pgid);
                        if !bg {
                            libc::tcsetpgrp(0, libc::getpgrp());
                        }
                    } else if bg && prev_read < 0 {
                        // ジョブ制御のないうしろのジョブは端末を読まない
                        apply_redir(0, &RT::File("/dev/null".into(), libc::O_RDONLY));
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
                    jobs::INTERACTIVE = false;
                }
                jobs().clear();
                let st = self.in_child(part);
                exit_shell(st);
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
            } else {
                eprintln!("{}: fork: {}", shell_name(), last_err());
            }
        }
        jobs::add_job(pgid, pids, text, bg)
    }

    /// 子の中で動かす。外のコマンドなら exec して戻らない
    fn in_child(&mut self, part: Part) -> i32 {
        match part {
            Part::Ready(r) => self.exec_ready(r),
            Part::AndOr(ao) => self.run_andor(&ao),
            Part::Ast(Cmd::Compound(c, redirs)) => match self.expand_redirs(&redirs) {
                Ok(r) => {
                    for (fd, t) in &r {
                        if !apply_redir(*fd, t) {
                            return 1;
                        }
                    }
                    self.run_compound(&c)
                }
                Err(e) => {
                    eprintln!("{}: {}", shell_name(), e);
                    1
                }
            },
            Part::Ast(Cmd::Simple { assigns, words, redirs }) => match self.prepare(&assigns, &words, &redirs) {
                Ok(r) if r.args.is_empty() || self.funcs.contains_key(&r.args[0]) || is_builtin(&r.args) => {
                    for (fd, t) in &r.redirs {
                        if !apply_redir(*fd, t) {
                            return 1;
                        }
                    }
                    let Ready { args, assigns, .. } = r;
                    for (k, v) in &assigns {
                        self.set_var(k, v);
                    }
                    if args.is_empty() {
                        return 0;
                    }
                    match self.funcs.get(&args[0]).cloned() {
                        Some(body) => self.call(&body, &args),
                        None => self.builtin(&args),
                    }
                }
                Ok(r) => self.exec_ready(r),
                Err(e) => {
                    eprintln!("{}: {}", shell_name(), e);
                    1
                }
            },
            Part::Ast(cmd) => self.run_cmd(&cmd),
        }
    }

    fn find(&self, cmd: &str) -> Option<String> {
        if cmd.contains('/') {
            return Some(cmd.to_string());
        }
        let path = self.get_var("PATH").unwrap_or_else(|| "/usr/bin:/bin".into());
        path.split(':')
            .map(|d| format!("{}/{}", if d.is_empty() { "." } else { d }, cmd))
            .find(|p| is_exec(p))
    }

    fn exec_ready(&mut self, r: Ready) -> i32 {
        for (fd, t) in &r.redirs {
            if !apply_redir(*fd, t) {
                return 1;
            }
        }
        for (k, v) in &r.assigns {
            unsafe { std::env::set_var(k, v) };
        }
        let Some(prog) = self.find(&r.args[0]) else {
            eprintln!("{}: {}: command not found", shell_name(), r.args[0]);
            return 127;
        };
        self.execve(&prog, &r.args);
        eprintln!("{}: {}: {}", shell_name(), r.args[0], last_err());
        126
    }

    /// 環境は export したもの
    fn execve(&self, prog: &str, args: &[String]) {
        let prog = cstr(prog);
        let cargs: Vec<CString> = args.iter().map(|a| cstr(a)).collect();
        let mut argv: Vec<*const libc::c_char> = cargs.iter().map(|a| a.as_ptr()).collect();
        argv.push(std::ptr::null());
        let env: Vec<CString> = std::env::vars().map(|(k, v)| cstr(&format!("{k}={v}"))).collect();
        let mut envp: Vec<*const libc::c_char> = env.iter().map(|e| e.as_ptr()).collect();
        envp.push(std::ptr::null());
        flush();
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            libc::execve(prog.as_ptr(), argv.as_ptr(), envp.as_ptr());
        }
    }

    /// <(...) (write なら >(...)): 子で動かし、その出力 (入力) とつないだパイプの口の名前 /dev/fd/N
    pub fn proc_subst(&mut self, src: &str, write: bool) -> String {
        let list = match Parser::new(src).program() {
            Ok(l) => l,
            Err(e) => {
                eprintln!("{}: <(...): {}", shell_name(), match e {
                    Error::Incomplete => "syntax error: unexpected end of file".into(),
                    Error::Syntax(s) => s,
                });
                return String::new();
            }
        };
        let mut fds = [-1, -1];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
            return String::new();
        }
        // 親が持つ口と、子が使う口
        let (mine, theirs, to) = if write { (fds[1], fds[0], 0) } else { (fds[0], fds[1], 1) };
        flush();
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            unsafe {
                libc::close(mine);
                libc::dup2(theirs, to);
                libc::close(theirs);
                for sig in [libc::SIGINT, libc::SIGQUIT, libc::SIGTSTP, libc::SIGTTIN, libc::SIGTTOU] {
                    libc::signal(sig, libc::SIG_DFL);
                }
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
                jobs::INTERACTIVE = false;
            }
            jobs().clear();
            let st = self.run_list(&list);
            exit_shell(st);
        }
        unsafe { libc::close(theirs) };
        if pid < 0 {
            unsafe { libc::close(mine) };
            return String::new();
        }
        // 小さい番号のままだと exec 3< <(...) の 3 とぶつかる (行の終わりに閉じてしまう)。bash と同じく 60 から上へ
        let high = unsafe { libc::fcntl(mine, libc::F_DUPFD, 60) };
        let mine = if high >= 0 {
            unsafe { libc::close(mine) };
            high
        } else {
            mine
        };
        self.procsubs.push((mine, pid));
        format!("/dev/fd/{}", mine)
    }

    /// $( ): 子で動かして、出力の最後の改行を除いたもの
    pub fn command_subst(&mut self, src: &str) -> String {
        let list = match Parser::new(src).program() {
            Ok(l) => l,
            Err(e) => {
                eprintln!("{}: $(...): {}", shell_name(), match e {
                    Error::Incomplete => "syntax error: unexpected end of file".into(),
                    Error::Syntax(s) => s,
                });
                self.subst_status = Some(2);
                return String::new();
            }
        };
        let mut fds = [-1, -1];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
            return String::new();
        }
        flush();
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            unsafe {
                libc::close(fds[0]);
                libc::dup2(fds[1], 1);
                libc::close(fds[1]);
                for sig in [libc::SIGINT, libc::SIGQUIT, libc::SIGTSTP, libc::SIGTTIN, libc::SIGTTOU] {
                    libc::signal(sig, libc::SIG_DFL);
                }
                jobs::INTERACTIVE = false;
            }
            jobs().clear();
            let st = self.run_list(&list);
            exit_shell(st);
        }
        unsafe { libc::close(fds[1]) };
        let mut out = vec![];
        let mut buf = [0u8; 4096];
        loop {
            let n = unsafe { libc::read(fds[0], buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n > 0 {
                out.extend_from_slice(&buf[..n as usize]);
            } else if n == 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                break;
            }
        }
        unsafe { libc::close(fds[0]) };
        if pid > 0 {
            let mut st = 0;
            while unsafe { libc::waitpid(pid, &mut st, 0) } < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {}
            self.subst_status = Some(exit_code(st));
        }
        let mut s = String::from_utf8_lossy(&out).into_owned();
        while s.ends_with('\n') {
            s.pop();
        }
        s
    }

    // ---- 組み込み ----

    fn builtin(&mut self, args: &[String]) -> i32 {
        if let Some(s) = jobs::job_builtin(args) {
            return s;
        }
        let a = &args[1..];
        let name = args[0].as_str();
        match name {
            ":" | "true" => 0,
            "false" => 1,
            "exit" => {
                let st = a.first().and_then(|s| s.parse().ok()).unwrap_or(self.status);
                // aish --mcp では、サーバーは終わらずにその run だけを終える
                if mcp::PID.load(std::sync::atomic::Ordering::Relaxed) == unsafe { libc::getpid() } {
                    self.flow = Flow::Return;
                    return st & 0xff;
                }
                exit_shell(st & 0xff)
            }
            "cd" => {
                // -L / -P (POSIX)。aish の cd はいつもリンクをたどったあとの場所 (カーネルの getcwd) なので、どちらも同じ
                let a: Vec<String> = {
                    let k = a.iter().take_while(|x| matches!(x.as_str(), "-L" | "-P" | "-LP" | "-PL")).count();
                    let k = if a.get(k).is_some_and(|x| x == "--") { k + 1 } else { k };
                    a[k..].to_vec()
                };
                let dir = match a.first().map(|s| s.as_str()) {
                    None => self.get_var("HOME").unwrap_or_else(|| "/".into()),
                    Some("-") => {
                        let d = self.get_var("OLDPWD").unwrap_or_default();
                        println!("{}", d);
                        d
                    }
                    Some(d) => d.to_string(),
                };
                let old = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
                match std::env::set_current_dir(&dir) {
                    Ok(()) => {
                        let new = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
                        self.export("OLDPWD", Some(old.clone()));
                        self.export("PWD", Some(new.clone()));
                        if new != old {
                            self.plugins.tell("chpwd", serde_json::json!({ "pwd": new, "old": old }));
                            // zsh と同じく、chpwd という関数があれば動かす
                            if let Some(body) = self.funcs.get("chpwd").cloned() {
                                return self.call(&body, &["chpwd".to_string()]);
                            }
                        }
                        0
                    }
                    Err(e) => {
                        eprintln!("cd: {}: {}", dir, err_text(&e));
                        1
                    }
                }
            }
            "trap" => trap::builtin(a),
            "umask" => {
                match a.first() {
                    None => {
                        let m = unsafe { libc::umask(0) };
                        unsafe { libc::umask(m) };
                        println!("{:04o}", m);
                    }
                    Some(m) => match u32::from_str_radix(m, 8) {
                        Ok(m) if m <= 0o777 => unsafe {
                            libc::umask(m as libc::mode_t);
                        },
                        _ => {
                            eprintln!("umask: {}: invalid octal number", m);
                            return 1;
                        }
                    },
                }
                0
            }
            "pwd" => {
                // -L / -P も (どちらもリンクをたどったあとの場所)
                println!("{}", std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default());
                0
            }
            "export" => {
                if a.is_empty() || a[0] == "-p" {
                    let mut vs: Vec<(String, String)> = std::env::vars().collect();
                    vs.sort();
                    for (k, v) in vs {
                        println!("export {}={}", k, quote(&v));
                    }
                    return 0;
                }
                for x in a {
                    match x.split_once('=') {
                        Some((k, v)) => self.export(k, Some(v.to_string())),
                        None => self.export(x, None),
                    }
                }
                0
            }
            "unset" => {
                let mut funcs = false;
                for x in a {
                    match x.as_str() {
                        "-f" => funcs = true,
                        "-v" => funcs = false,
                        _ if funcs => {
                            self.funcs.remove(x);
                        }
                        _ => self.unset(x),
                    }
                }
                0
            }
            "set" => self.set(a),
            "shift" => {
                let n: usize = a.first().and_then(|s| s.parse().ok()).unwrap_or(1);
                if n + 1 > self.params.len() {
                    eprintln!("shift: shift count out of range");
                    return 1;
                }
                self.params.drain(1..1 + n);
                0
            }
            "read" => self.read(a),
            // zsh: setopt NAME (no を前につけると外す)。名前は大文字小文字と _ を気にしない
            "setopt" | "unsetopt" => {
                if a.is_empty() {
                    for o in &self.opts {
                        println!("{}", o);
                    }
                    return 0;
                }
                for x in a {
                    let mut n: String = x.chars().filter(|c| *c != '_').collect::<String>().to_lowercase();
                    let mut on = name == "setopt";
                    if let Some(r) = n.strip_prefix("no").filter(|r| !r.is_empty() && *r != "tify") {
                        n = r.to_string();
                        on = !on;
                    }
                    if on {
                        self.opts.insert(n);
                    } else {
                        self.opts.remove(&n);
                    }
                }
                0
            }
            // zsh の補完とモジュールの設定: aish では補完はプラグインなので、なにもしない
            "zmodload" | "zstyle" | "autoload" | "compinit" | "compdef" => 0,
            // typeset / declare: 関数の中なら local、外なら代入。-x は export。ほかの印 (-U -r -i -g ...) は気にしない
            "typeset" | "declare" => {
                let flags: String = a.iter().filter(|x| x.starts_with('-') || x.starts_with('+')).flat_map(|x| x.chars().skip(1)).collect();
                let names: Vec<String> = a.iter().filter(|x| !x.starts_with('-') && !x.starts_with('+')).cloned().collect();
                let global = flags.contains('g') || self.locals.is_empty();
                for x in &names {
                    let (k, v) = match x.split_once('=') {
                        Some((k, v)) => (k.to_string(), Some(v.to_string())),
                        None => (x.clone(), None),
                    };
                    if !parse::valid_name(&k) {
                        eprintln!("{}: {}: not a valid identifier", name, k);
                        return 1;
                    }
                    if !global {
                        let frame = self.locals.len() - 1;
                        if !self.locals[frame].iter().any(|(n, ..)| *n == k) {
                            let exported = std::env::var_os(&k).is_some();
                            let old = self.get_var(&k);
                            self.locals[frame].push((k.clone(), old, exported));
                        }
                    }
                    if flags.contains('x') {
                        self.export(&k, v);
                    } else if let Some(v) = v {
                        self.set_var(&k, &v);
                    } else if !global {
                        self.set_var(&k, "");
                    }
                }
                0
            }
            "local" => {
                let Some(frame) = self.locals.len().checked_sub(1) else {
                    eprintln!("local: can only be used in a function");
                    return 1;
                };
                for x in a {
                    let (k, v) = match x.split_once('=') {
                        Some((k, v)) => (k.to_string(), Some(v.to_string())),
                        None => (x.clone(), None),
                    };
                    if !parse::valid_name(&k) {
                        eprintln!("local: {}: not a valid identifier", k);
                        return 1;
                    }
                    if !self.locals[frame].iter().any(|(n, ..)| *n == k) {
                        let exported = std::env::var_os(&k).is_some();
                        let old = self.get_var(&k);
                        self.locals[frame].push((k.clone(), old, exported));
                    }
                    self.set_var(&k, &v.unwrap_or_default());
                }
                0
            }
            "plugin" => {
                // plugin NAME [ARGS...] / plugin (一覧)
                let Some(name) = a.first() else {
                    print!("{}", self.plugins.describe());
                    return 0;
                };
                let home = self.get_var("HOME").unwrap_or_default();
                let extra = serde_json::json!({ "histfile": self.histfile.clone().unwrap_or_default() });
                let (pv, path) = (self.get_var("AISH_PLUGIN_PATH"), self.get_var("PATH").unwrap_or_default());
                match self.plugins.load(name, &a[1..], &home, pv, &path, extra) {
                    Ok(()) => 0,
                    Err(e) => {
                        eprintln!("plugin: {}", e);
                        1
                    }
                }
            }
            "tool" => {
                // tool NAME [JSON | KEY=VALUE...]: プラグインのツールをシェルから呼ぶ (VALUE は JSON として読めればそれ、
                // ほかは文字)。MCP の客がツールの新しい引数をまだ知らないとき (つなぎなおすまで) も run から使える
                let Some(name) = a.first() else {
                    for (plugin, t) in self.plugins.tools() {
                        println!("{}\t(aish-{}) {}", t["name"].as_str().unwrap_or(""), plugin, t["description"].as_str().unwrap_or(""));
                    }
                    return 0;
                };
                let args = match &a[1..] {
                    [j] if j.trim_start().starts_with('{') => match serde_json::from_str::<serde_json::Value>(j) {
                        Ok(v) => v,
                        Err(e) => {
                            eprintln!("tool: {}: {}", j, e);
                            return 2;
                        }
                    },
                    kv => {
                        let mut m = serde_json::Map::new();
                        for x in kv {
                            let Some((k, v)) = x.split_once('=') else {
                                eprintln!("usage: tool NAME [JSON | KEY=VALUE...]");
                                return 2;
                            };
                            m.insert(k.into(), serde_json::from_str(v).unwrap_or_else(|_| serde_json::Value::String(v.into())));
                        }
                        serde_json::Value::Object(m)
                    }
                };
                let pwd = std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default();
                let ev = serde_json::json!({ "args": args, "pwd": pwd, "home": self.get_var("HOME").unwrap_or_default(), "histfile": self.histfile.clone().unwrap_or_default() });
                let Some(r) = self.plugins.tool(name, ev) else {
                    eprintln!("tool: {}: no such tool (or the plugin stopped)", name);
                    return 1;
                };
                // 本文は標準出力、残り (行の数などの JSON) は標準エラーへ ($(...) やパイプで本文だけ使える)
                let (text, meta) = mcp::render_split(&r);
                print!("{}", text);
                if !text.is_empty() && !text.ends_with('\n') {
                    println!();
                }
                if !meta.is_empty() {
                    eprintln!("{}", meta);
                }
                if r.get("error").is_some() { 1 } else { 0 }
            }
            "bindkey" => match a {
                [] => {
                    print!("{}", self.plugins.describe_keys());
                    0
                }
                [r, key] if r == "-r" => {
                    self.plugins.unbind(key);
                    0
                }
                // zsh の bindkey -M (メニューのキー) や、zsh の widget 名 (: のないもの) は、aish にはないので黙って通す
                // (↑ ↓ の履歴の部分一致は aish にはじめからある)
                [m, ..] if m == "-M" || m == "-e" || m == "-v" => 0,
                [_, target] if !target.contains(':') => 0,
                [key, target] => match self.plugins.bindkey(key, target) {
                    Ok(()) => 0,
                    Err(e) => {
                        eprintln!("bindkey: {}", e);
                        1
                    }
                },
                _ => {
                    eprintln!("usage: bindkey KEY PLUGIN:WIDGET | bindkey -r KEY | bindkey");
                    2
                }
            },
            "alias" => {
                if a.is_empty() {
                    let mut v: Vec<_> = self.aliases.iter().collect();
                    v.sort();
                    for (k, x) in v {
                        println!("alias {}={}", k, quote(x));
                    }
                    return 0;
                }
                let mut st = 0;
                for x in a {
                    match x.split_once('=') {
                        Some((k, v)) if !k.is_empty() => {
                            self.aliases.insert(k.to_string(), v.to_string());
                        }
                        _ => match self.aliases.get(x) {
                            Some(v) => println!("alias {}={}", x, quote(v)),
                            None => {
                                eprintln!("alias: {}: not found", x);
                                st = 1;
                            }
                        },
                    }
                }
                st
            }
            "unalias" => {
                if a.first().map(|s| s.as_str()) == Some("-a") {
                    self.aliases.clear();
                    return 0;
                }
                let mut st = 0;
                for x in a {
                    if self.aliases.remove(x).is_none() {
                        eprintln!("unalias: {}: not found", x);
                        st = 1;
                    }
                }
                st
            }
            "eval" => {
                let src = a.join(" ");
                self.run_source(&src, "eval")
            }
            "." | "source" => {
                let Some(file) = a.first() else {
                    eprintln!("{}: filename argument required", name);
                    return 2;
                };
                let path = if file.contains('/') { Some(file.clone()) } else { self.find(file).or(Some(file.clone())) };
                let text = match std::fs::read_to_string(path.as_deref().unwrap()) {
                    Ok(t) => t,
                    Err(e) => {
                        eprintln!("{}: {}: {}", name, file, err_text(&e));
                        return 1;
                    }
                };
                let saved = (a.len() > 1).then(|| {
                    let mut ps = vec![self.params[0].clone()];
                    ps.extend_from_slice(&a[1..]);
                    std::mem::replace(&mut self.params, ps)
                });
                self.sourcing += 1;
                let st = self.run_source(&text, file);
                self.sourcing -= 1;
                if self.flow == Flow::Return {
                    self.flow = Flow::None;
                }
                if let Some(p) = saved {
                    self.params = p;
                }
                st
            }
            "return" => {
                if self.locals.is_empty() && self.sourcing == 0 {
                    eprintln!("return: can only `return' from a function or sourced script");
                    return 1;
                }
                let st = a.first().and_then(|s| s.parse().ok()).unwrap_or(self.status);
                self.status = st;
                self.flow = Flow::Return;
                st
            }
            "break" | "continue" => {
                if self.loops == 0 {
                    eprintln!("{}: only meaningful in a loop", name);
                    return 0;
                }
                let n = a.first().and_then(|s| s.parse().ok()).unwrap_or(1u32).clamp(1, self.loops);
                self.flow = if name == "break" { Flow::Break(n) } else { Flow::Continue(n) };
                0
            }
            "echo" => echo(a),
            "[[" => {
                let mut a = a.to_vec();
                if a.last().map(|s| s.as_str()) != Some("]]") {
                    eprintln!("[[: missing `]]'");
                    return 2;
                }
                a.pop();
                test_with(&a, true)
            }
            "test" | "[" => {
                let mut a = a.to_vec();
                if name == "[" {
                    if a.last().map(|s| s.as_str()) != Some("]") {
                        eprintln!("[: missing `]'");
                        return 2;
                    }
                    a.pop();
                }
                test(&a)
            }
            "exec" => {
                let Some(prog) = self.find(&a[0]) else {
                    eprintln!("{}: {}: command not found", shell_name(), a[0]);
                    exit_shell(127);
                };
                self.execve(&prog, a);
                eprintln!("{}: {}: {}", shell_name(), a[0], last_err());
                exit_shell(126)
            }
            "command" | "type" => {
                let verbose = name == "type";
                let list: Vec<&String> = a.iter().filter(|x| !x.starts_with('-')).collect();
                if name == "command" && a.first().map(|s| s.as_str()) != Some("-v") && a.first().map(|s| s.as_str()) != Some("-V") {
                    // command CMD ...: 関数を飛ばして動かす
                    if a.is_empty() {
                        return 0;
                    }
                    if is_builtin(a) {
                        return self.builtin(a);
                    }
                    let text = self.text.clone();
                    return self.spawn(vec![Part::Ready(Ready { args: a.to_vec(), assigns: vec![], arrays: vec![], redirs: vec![] })], false, &text);
                }
                let mut st = 0;
                for x in list {
                    if parse::is_reserved(x) {
                        if verbose { println!("{} is a shell keyword", x) } else { println!("{}", x) }
                    } else if self.funcs.contains_key(x) {
                        if verbose { println!("{} is a function", x) } else { println!("{}", x) }
                    } else if BUILTINS.contains(&x.as_str()) {
                        if verbose { println!("{} is a shell builtin", x) } else { println!("{}", x) }
                    } else if let Some(p) = self.find(x).filter(|p| std::fs::metadata(p).is_ok()) {
                        if verbose { println!("{} is {}", x, p) } else { println!("{}", p) }
                    } else {
                        if verbose {
                            eprintln!("type: {}: not found", x);
                        }
                        st = 1;
                    }
                }
                st
            }
            _ => {
                eprintln!("{}: {}: not a builtin", shell_name(), name);
                1
            }
        }
    }

    fn set(&mut self, a: &[String]) -> i32 {
        if a.is_empty() {
            let mut vs: Vec<(String, String)> = self.vars.iter().map(|(k, v)| (k.clone(), v.clone())).chain(std::env::vars()).collect();
            vs.sort();
            for (k, v) in vs {
                println!("{}={}", k, quote(&v));
            }
            return 0;
        }
        let mut i = 0;
        while i < a.len() {
            let x = &a[i];
            if x == "--" {
                i += 1;
                break;
            }
            let on = x.starts_with('-');
            if !(on || x.starts_with('+')) || x.len() < 2 {
                break;
            }
            if &x[1..] == "o" {
                // set -o errexit / xtrace
                i += 1;
                match a.get(i).map(|s| s.as_str()) {
                    Some("errexit") => self.errexit = on,
                    Some("xtrace") => self.xtrace = on,
                    Some(_) | None => {}
                }
            } else {
                for c in x[1..].chars() {
                    match c {
                        'e' => self.errexit = on,
                        'x' => self.xtrace = on,
                        'u' | 'f' | 'h' | 'm' | 'b' | 'C' | 'v' | 'n' => {}
                        _ => {
                            eprintln!("set: -{}: invalid option", c);
                            return 2;
                        }
                    }
                }
            }
            i += 1;
        }
        if i < a.len() || a.last().is_some_and(|x| x == "--") {
            let mut ps = vec![self.params[0].clone()];
            ps.extend_from_slice(&a[i..]);
            self.params = ps;
        }
        0
    }

    /// read [-r] [-p PROMPT] [NAME...]
    fn read(&mut self, a: &[String]) -> i32 {
        let mut raw = false;
        let mut names = vec![];
        let mut i = 0;
        while i < a.len() {
            match a[i].as_str() {
                "-r" => raw = true,
                "-p" => {
                    i += 1;
                    eprint!("{}", a.get(i).map(|s| s.as_str()).unwrap_or(""));
                }
                x => names.push(x.to_string()),
            }
            i += 1;
        }
        if names.is_empty() {
            names.push("REPLY".into());
        }
        let mut line = String::new();
        let mut got = false;
        let mut bytes = vec![];
        loop {
            let mut c = 0u8;
            let n = unsafe { libc::read(0, &mut c as *mut u8 as *mut libc::c_void, 1) };
            if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                return 130;
            }
            if n <= 0 {
                break;
            }
            got = true;
            if c == b'\n' {
                bytes.push(b'\n');
                break;
            }
            bytes.push(c);
        }
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let ended = text.ends_with('\n');
        let mut cs = text.trim_end_matches('\n').chars().peekable();
        // \ をはずす (-r でなければ)。\ で守った文字は分けない印 (\u{0}) をつける
        let mut protected = vec![];
        while let Some(c) = cs.next() {
            if c == '\\' && !raw {
                if let Some(n) = cs.next() {
                    line.push(n);
                    protected.push(true);
                }
                continue;
            }
            line.push(c);
            protected.push(false);
        }
        let ifs = self.get_var("IFS").unwrap_or_else(|| " \t\n".into());
        let chars: Vec<char> = line.chars().collect();
        let is_sep = |k: usize| !protected[k] && ifs.contains(chars[k]);
        let mut k = 0;
        // 前の空白を飛ばす
        while k < chars.len() && is_sep(k) && chars[k].is_whitespace() {
            k += 1;
        }
        for (ni, name) in names.iter().enumerate() {
            let mut v = String::new();
            if ni + 1 == names.len() {
                // 最後の名前に残りぜんぶ (後ろの IFS の空白は除く)
                let mut end = chars.len();
                while end > k && is_sep(end - 1) && chars[end - 1].is_whitespace() {
                    end -= 1;
                }
                v = chars[k.min(end)..end].iter().collect();
                k = chars.len();
            } else {
                while k < chars.len() && !is_sep(k) {
                    v.push(chars[k]);
                    k += 1;
                }
                // 区切りを 1 つ (と空白) 飛ばす
                let mut seen_hard = false;
                while k < chars.len() && is_sep(k) {
                    if !chars[k].is_whitespace() {
                        if seen_hard {
                            break;
                        }
                        seen_hard = true;
                    }
                    k += 1;
                }
            }
            if !parse::valid_name(name) {
                eprintln!("read: {}: not a valid identifier", name);
                return 2;
            }
            self.set_var(name, &v);
        }
        if got && (ended || !line.is_empty()) { 0 } else { 1 }
    }
}

/// export や set で見せるクォート
fn quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "_-./:,+@%".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// つけかえる (fd を置きかえる)。失敗したら知らせて false
fn apply_redir(fd: i32, t: &RT) -> bool {
    unsafe {
        match t {
            RT::File(path, flags) => {
                let p = cstr(path);
                let f = libc::open(p.as_ptr(), *flags, 0o666);
                if f < 0 {
                    eprintln!("{}: {}: {}", shell_name(), path, last_err());
                    return false;
                }
                if f != fd {
                    libc::dup2(f, fd);
                    libc::close(f);
                }
            }
            RT::Dup(n) => {
                if *n != fd && libc::dup2(*n, fd) < 0 {
                    eprintln!("{}: {}: {}", shell_name(), n, last_err());
                    return false;
                }
            }
            RT::Close => {
                libc::close(fd);
            }
            RT::Here(text) => {
                // 小さければパイプに、大きければ /tmp の消したファイルに
                let b = text.as_bytes();
                let f = if b.len() <= 4096 {
                    let mut p = [-1, -1];
                    if libc::pipe(p.as_mut_ptr()) < 0 {
                        return false;
                    }
                    libc::write(p[1], b.as_ptr() as *const libc::c_void, b.len());
                    libc::close(p[1]);
                    p[0]
                } else {
                    let path = cstr(&format!("/tmp/.sh-here-{}-{}", std::process::id(), fd));
                    let f = libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CREAT | libc::O_TRUNC, 0o600);
                    if f < 0 {
                        eprintln!("{}: heredoc: {}", shell_name(), last_err());
                        return false;
                    }
                    libc::unlink(path.as_ptr());
                    libc::write(f, b.as_ptr() as *const libc::c_void, b.len());
                    libc::lseek(f, 0, libc::SEEK_SET);
                    f
                };
                if f != fd {
                    libc::dup2(f, fd);
                    libc::close(f);
                }
            }
        }
    }
    true
}

fn echo(a: &[String]) -> i32 {
    let mut newline = true;
    let mut escapes = false;
    let mut i = 0;
    while i < a.len() && a[i].len() > 1 && a[i].starts_with('-') && a[i][1..].chars().all(|c| "neE".contains(c)) {
        for c in a[i][1..].chars() {
            match c {
                'n' => newline = false,
                'e' => escapes = true,
                _ => escapes = false,
            }
        }
        i += 1;
    }
    let mut out = a[i..].join(" ");
    if escapes {
        let mut s = String::new();
        let mut cs = out.chars().peekable();
        while let Some(c) = cs.next() {
            if c != '\\' {
                s.push(c);
                continue;
            }
            match cs.next() {
                Some('n') => s.push('\n'),
                Some('t') => s.push('\t'),
                Some('r') => s.push('\r'),
                Some('a') => s.push('\x07'),
                Some('b') => s.push('\x08'),
                Some('e') => s.push('\x1b'),
                Some('v') => s.push('\x0b'),
                Some('f') => s.push('\x0c'),
                Some('\\') => s.push('\\'),
                Some('c') => {
                    newline = false;
                    break;
                }
                Some('0') => {
                    let mut v = 0u32;
                    for _ in 0..3 {
                        match cs.peek() {
                            Some(d) if d.is_digit(8) => {
                                v = v * 8 + d.to_digit(8).unwrap();
                                cs.next();
                            }
                            _ => break,
                        }
                    }
                    s.push(char::from_u32(v).unwrap_or('?'));
                }
                Some(o) => {
                    s.push('\\');
                    s.push(o);
                }
                None => s.push('\\'),
            }
        }
        out = s;
    }
    if newline {
        out.push('\n');
    }
    let mut so = io::stdout().lock();
    if so.write_all(out.as_bytes()).and_then(|_| so.flush()).is_err() {
        return 1;
    }
    0
}

// ---- test / [ ----

fn test(a: &[String]) -> i32 {
    test_with(a, false)
}

/// dbl: [[ ]] のもの (&& || で、== != の右はパターン、=~ は正規表現。語にはワイルドカードの印が残っている)
fn test_with(a: &[String], dbl: bool) -> i32 {
    let mut t = Test { a, i: 0, dbl };
    if a.is_empty() {
        return 1;
    }
    match t.or() {
        Ok(v) if t.i == a.len() => !v as i32,
        Ok(_) => {
            eprintln!("test: {}: unexpected argument", a[t.i]);
            2
        }
        Err(e) => {
            eprintln!("test: {}", e);
            2
        }
    }
}

struct Test<'a> {
    a: &'a [String],
    i: usize,
    dbl: bool,
}

const UNARY: &[&str] = &["-e", "-f", "-d", "-r", "-w", "-x", "-s", "-L", "-h", "-z", "-n", "-b", "-c", "-p", "-S", "-t", "-g", "-u", "-k", "-O", "-G"];
const BINARY: &[&str] = &["=", "==", "!=", "<", ">", "-eq", "-ne", "-lt", "-le", "-gt", "-ge", "-nt", "-ot", "-ef"];

impl Test<'_> {
    fn get(&self, k: usize) -> Option<&str> {
        self.a.get(self.i + k).map(|s| s.as_str())
    }

    fn or(&mut self) -> Result<bool, String> {
        let mut v = self.and()?;
        while self.get(0) == Some(if self.dbl { "||" } else { "-o" }) {
            self.i += 1;
            let r = self.and()?;
            v = v || r;
        }
        Ok(v)
    }

    fn and(&mut self) -> Result<bool, String> {
        let mut v = self.not()?;
        while self.get(0) == Some(if self.dbl { "&&" } else { "-a" }) {
            self.i += 1;
            let r = self.not()?;
            v = v && r;
        }
        Ok(v)
    }

    fn not(&mut self) -> Result<bool, String> {
        // "!" の後ろに何かあるときだけ否定
        if self.get(0) == Some("!") && self.get(1).is_some() && !(self.get(1).is_some_and(|x| BINARY.contains(&x)) && self.get(2).is_some()) {
            self.i += 1;
            return Ok(!self.not()?);
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<bool, String> {
        let Some(x) = self.get(0).map(|s| s.to_string()) else { return Err("argument expected".into()) };
        let x = x.as_str();
        let rem = self.a.len() - self.i;
        // 二項 (3 つ以上残っていて真ん中が演算子) を先に
        if rem >= 3
            && let (Some(op), Some(r)) = (self.get(1), self.get(2))
            && (BINARY.contains(&op) || (self.dbl && op == "=~"))
        {
            let (l, op, r) = (x.to_string(), op.to_string(), r.to_string());
            self.i += 3;
            if self.dbl {
                let l = glob::unmark(&l);
                return match op.as_str() {
                    "=" | "==" | "!=" => {
                        let m = glob::glob_match(&r.chars().collect::<Vec<_>>(), &l.chars().collect::<Vec<_>>());
                        Ok(m == (op != "!="))
                    }
                    "=~" => regex::Regex::new(&glob::unmark(&r)).map(|re| re.is_match(&l)).map_err(|e| format!("=~: {}", e)),
                    _ => binary_test(&l, &op, &glob::unmark(&r)),
                };
            }
            return binary_test(&l, &op, &r);
        }
        if x == "(" && rem >= 2 {
            self.i += 1;
            let v = self.or()?;
            if self.get(0) != Some(")") {
                return Err("missing `)'".into());
            }
            self.i += 1;
            return Ok(v);
        }
        if UNARY.contains(&x)
            && let Some(arg) = self.get(1)
        {
            let (op, arg) = (x.to_string(), if self.dbl { glob::unmark(arg) } else { arg.to_string() });
            self.i += 2;
            return Ok(unary_test(&op, &arg));
        }
        self.i += 1;
        Ok(!x.is_empty())
    }
}

fn unary_test(op: &str, arg: &str) -> bool {
    let md = || std::fs::metadata(arg);
    let access = |m: i32| {
        let p = cstr(arg);
        unsafe { libc::access(p.as_ptr(), m) == 0 }
    };
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    match op {
        "-e" => md().is_ok(),
        "-f" => md().is_ok_and(|m| m.is_file()),
        "-d" => md().is_ok_and(|m| m.is_dir()),
        "-s" => md().is_ok_and(|m| m.len() > 0),
        "-L" | "-h" => std::fs::symlink_metadata(arg).is_ok_and(|m| m.file_type().is_symlink()),
        "-b" => md().is_ok_and(|m| m.file_type().is_block_device()),
        "-c" => md().is_ok_and(|m| m.file_type().is_char_device()),
        "-p" => md().is_ok_and(|m| m.file_type().is_fifo()),
        "-S" => md().is_ok_and(|m| m.file_type().is_socket()),
        "-u" => md().is_ok_and(|m| m.mode() & 0o4000 != 0),
        "-g" => md().is_ok_and(|m| m.mode() & 0o2000 != 0),
        "-k" => md().is_ok_and(|m| m.mode() & 0o1000 != 0),
        "-O" => md().is_ok_and(|m| m.uid() == unsafe { libc::geteuid() }),
        "-G" => md().is_ok_and(|m| m.gid() == unsafe { libc::getegid() }),
        "-r" => access(libc::R_OK),
        "-w" => access(libc::W_OK),
        "-x" => access(libc::X_OK),
        "-z" => arg.is_empty(),
        "-n" => !arg.is_empty(),
        "-t" => arg.parse().is_ok_and(|fd: i32| unsafe { libc::isatty(fd) } == 1),
        _ => false,
    }
}

fn binary_test(l: &str, op: &str, r: &str) -> Result<bool, String> {
    let num = |s: &str| s.trim().parse::<i64>().map_err(|_| format!("{}: integer expression expected", s));
    use std::os::unix::fs::MetadataExt;
    let mtime = |p: &str| std::fs::metadata(p).ok().map(|m| (m.mtime(), m.mtime_nsec()));
    Ok(match op {
        "=" | "==" => l == r,
        "!=" => l != r,
        "<" => l < r,
        ">" => l > r,
        "-eq" => num(l)? == num(r)?,
        "-ne" => num(l)? != num(r)?,
        "-lt" => num(l)? < num(r)?,
        "-le" => num(l)? <= num(r)?,
        "-gt" => num(l)? > num(r)?,
        "-ge" => num(l)? >= num(r)?,
        "-nt" => matches!((mtime(l), mtime(r)), (Some(a), Some(b)) if a > b) || (mtime(l).is_some() && mtime(r).is_none()),
        "-ot" => matches!((mtime(l), mtime(r)), (Some(a), Some(b)) if a < b) || (mtime(l).is_none() && mtime(r).is_some()),
        "-ef" => match (std::fs::metadata(l), std::fs::metadata(r)) {
            (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        },
        _ => return Err(format!("{}: unknown operator", op)),
    })
}

// ---- 入口 ----

fn main() {
    let args: Vec<String> = std::env::args().collect();
    // 呼ばれた名前: sh (と -sh) なら POSIX の sh として
    let base = args.first().map_or("aish", |a| a.rsplit('/').next().unwrap_or(a).trim_start_matches('-'));
    let _ = NAME.set(if base == "sh" { "sh" } else { "aish" });
    // sh -c CMD [NAME ARGS...]: NAME が $0
    if args.get(1).is_some_and(|a| a == "-c") {
        let cmd = args.get(2).cloned().unwrap_or_default();
        let mut ps: Vec<String> = args.get(3..).unwrap_or(&[]).to_vec();
        if ps.is_empty() {
            ps.push(args[0].clone());
        }
        let mut sh = Shell::new(ps);
        trap::set_shell(&mut sh);
        let st = sh.run_source(&cmd, shell_name());
        exit_shell(st);
    }
    // aish --mcp [--json] [RC...]: MCP のサーバー (mcp.rs)
    if args.get(1).is_some_and(|a| a == "--mcp") {
        Shell::new(vec![args[0].clone()]).mcp(&args[2..]);
    }
    // sh [-e] [-x] FILE ARGS...
    let mut sh = Shell::new(vec![args[0].clone()]);
    trap::set_shell(&mut sh);
    // PWD は今のディレクトリ (受けついだものが違えばなおす。POSIX のシェルと同じ)
    if let Ok(cwd) = std::env::current_dir() {
        let cwd = cwd.display().to_string();
        if sh.get_var("PWD").as_deref() != Some(cwd.as_str()) {
            sh.export("PWD", Some(cwd));
        }
    }
    let mut i = 1;
    while let Some(a) = args.get(i).filter(|a| a.len() > 1 && a.starts_with('-') && a[1..].chars().all(|c| "exs".contains(c))) {
        sh.errexit |= a.contains('e');
        sh.xtrace |= a.contains('x');
        i += 1;
    }
    if let Some(file) = args.get(i) {
        sh.params = args[i..].to_vec();
        let text = match std::fs::read_to_string(file) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("{}: {}: {}", shell_name(), file, e);
                std::process::exit(127);
            }
        };
        let st = sh.run_source(&text, file);
        exit_shell(st);
    }
    // ログインシェル (argv[0] が -sh) は /etc/profile と ~/.profile を読む
    let home = sh.get_var("HOME").unwrap_or_default();
    if args[0].starts_with('-') {
        for f in ["/etc/profile".to_string(), format!("{}/.profile", home)] {
            if std::path::Path::new(&f).is_file() {
                sh.builtin(&[".".into(), f]);
            }
        }
    }
    // 対話するシェルは設定も読む: alias や PS1 など
    //   aish: /etc/aishrc と ~/.aishrc (zsh の /etc/zsh/zshrc と ~/.zshrc のように)
    //   sh:   $ENV だけ (POSIX)
    if unsafe { libc::isatty(0) } == 1 {
        // 履歴のファイル (プラグインにも教えるので、設定を読む前に決める。設定で HISTFILE を変えたら、あとで読みなおす)
        let file = sh.get_var("HISTFILE").unwrap_or_else(|| format!("{}/.aish_history", home));
        sh.histfile = (!home.is_empty() || file.starts_with('/')).then_some(file);
        let rcs = if shell_name() == "sh" {
            sh.get_var("ENV").and_then(|e| sh.expand_one(&e).ok()).into_iter().collect()
        } else {
            vec!["/etc/aishrc".to_string(), format!("{}/.aishrc", home)]
        };
        for rc in rcs {
            if std::path::Path::new(&rc).is_file() {
                sh.builtin(&[".".into(), rc]);
            }
        }
    }
    sh.interactive();
}

extern "C" fn on_sigint(_: libc::c_int) {}

impl Shell {
    fn interactive(&mut self) {
        // 対話するシェルは Ctrl-C で終わらず (打ちかけの行を捨てて新しいプロンプトへ)、
        // 自分のグループを端末の前に出す
        let tty = unsafe { libc::isatty(0) == 1 };
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_sigint as *const () as usize;
            libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
            if tty {
                for sig in [libc::SIGQUIT, libc::SIGTSTP, libc::SIGTTIN, libc::SIGTTOU] {
                    libc::signal(sig, libc::SIG_IGN);
                }
                // 端末があればジョブ制御をする
                jobs::INTERACTIVE = true;
                libc::setpgid(0, 0);
                libc::tcsetpgrp(0, libc::getpgrp());
                let mut t: libc::termios = std::mem::zeroed();
                if libc::tcgetattr(0, &mut t) == 0 {
                    *(&raw mut jobs::SHELL_TMODES) = Some(t);
                }
            }
        }
        // 履歴: $HISTFILE (なければ ~/.aish_history)、$HISTSIZE 行
        let mut ed = edit::Editor::new();
        if tty {
            let size = self.get_var("HISTSIZE").and_then(|n| n.parse().ok()).unwrap_or(10000);
            ed.load(self.histfile.clone(), size);
        }
        let mut buf = String::new();
        loop {
            if buf.is_empty() {
                jobs::report_jobs();
            }
            let read = if tty {
                if buf.is_empty() {
                    self.plugins.tell("precmd", serde_json::json!({ "status": self.status }));
                }
                let p = if buf.is_empty() { self.prompt() } else { self.get_var("PS2").unwrap_or_else(|| "> ".into()) };
                let ctx = self.edit_ctx();
                match ed.read(&p, &ctx, &mut self.plugins) {
                    edit::Input::Line(l) => Ok(Some(l)),
                    edit::Input::Silent(l) => {
                        // 履歴に残さずに動かす (プラグインの機能の run と silent)
                        if let Ok(list) = Parser::new(&l).program() {
                            self.run_list(&list);
                            self.flow = Flow::None;
                        }
                        continue;
                    }
                    edit::Input::Eof => Ok(None),
                    edit::Input::Interrupt => Err(io::Error::from(io::ErrorKind::Interrupted)),
                }
            } else {
                read_line()
            };
            let line = match read {
                Ok(Some(l)) => l,
                Ok(None) => {
                    if !buf.is_empty() {
                        eprintln!("{}: syntax error: unexpected end of file", shell_name());
                        exit_shell(2);
                    }
                    if tty {
                        eprintln!();
                    }
                    exit_shell(self.status);
                }
                Err(_) => {
                    if !tty {
                        eprintln!();
                    }
                    buf.clear();
                    self.status = 130;
                    continue;
                }
            };
            buf.push_str(&line);
            if !buf.ends_with('\n') {
                buf.push('\n');
            }
            match Parser::new(&buf).program() {
                Ok(list) => {
                    if tty {
                        ed.add(&buf);
                        let pwd = std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default();
                        self.plugins.tell("preexec", serde_json::json!({ "line": buf.trim_end_matches('\n'), "pwd": pwd }));
                    }
                    buf.clear();
                    self.run_list(&list);
                    self.flow = Flow::None;
                }
                Err(Error::Incomplete) => {}
                Err(Error::Syntax(e)) => {
                    eprintln!("{}: {}", shell_name(), e);
                    buf.clear();
                    self.status = 2;
                }
            }
        }
    }
}

impl Shell {
    /// プロンプト: PS1 (\u \h \w \W \$ \n \\ \e \[ \] が使える)。なければ「ディレクトリ %」(失敗のあとは !)。
    /// そのあと bash と同じく $NAME や $(cmd) を展開する (zsh の PROMPT_SUBST)
    fn prompt(&mut self) -> String {
        let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
        let root = unsafe { libc::geteuid() } == 0;
        // prompt のプラグインがあれば、それが作る
        if self.plugins.wants("prompt") {
            let host = std::fs::read_to_string("/etc/hostname").unwrap_or_default();
            let ev = serde_json::json!({
                "pwd": cwd,
                "home": self.get_var("HOME").unwrap_or_default(),
                "user": self.get_var("USER").or_else(|| self.get_var("LOGNAME")).unwrap_or_default(),
                "host": host.trim().split('.').next().unwrap_or(""),
                "status": self.status,
                "root": root,
                "ssh": self.get_var("SSH_CONNECTION").is_some(),
                "jobs": jobs().len(),
            });
            if let Some(p) = self.plugins.ask("prompt", ev).and_then(|r| r["prompt"].as_str().map(String::from)) {
                return p;
            }
        }
        let Some(ps1) = self.get_var("PS1") else {
            let mark = if self.status != 0 { "!" } else if root { "#" } else { "%" };
            return format!("{} {} ", cwd, mark);
        };
        let home = self.get_var("HOME").unwrap_or_default();
        let short = if !home.is_empty() && (cwd == home || cwd.starts_with(&format!("{}/", home))) { format!("~{}", &cwd[home.len()..]) } else { cwd.clone() };
        let mut out = String::new();
        let mut cs = ps1.chars();
        while let Some(c) = cs.next() {
            if c != '\\' {
                out.push(c);
                continue;
            }
            match cs.next() {
                Some('u') => out.push_str(&self.get_var("USER").or_else(|| self.get_var("LOGNAME")).unwrap_or_else(|| if root { "root".into() } else { "?".into() })),
                Some('h') | Some('H') => {
                    let h = std::fs::read_to_string("/etc/hostname").unwrap_or_default();
                    out.push_str(h.trim().split('.').next().unwrap_or(""));
                }
                Some('w') => out.push_str(&short),
                Some('W') => out.push_str(if short == "~" || cwd == "/" { &short } else { short.rsplit('/').next().unwrap_or("") }),
                Some('$') => out.push(if root { '#' } else { '$' }),
                // 色: \e は ESC、\[ \] は幅に数えないところ (bash と同じ)
                Some('e') => out.push('\x1b'),
                Some('[') => out.push('\x01'),
                Some(']') => out.push('\x02'),
                Some('n') => out.push('\n'),
                Some('\\') => out.push('\\'),
                Some(o) => {
                    out.push('\\');
                    out.push(o);
                }
                None => out.push('\\'),
            }
        }
        if out.contains('$') || out.contains('`') {
            // 展開しても $? は変えない
            let st = self.status;
            if let Ok(e) = self.expand_one(&out) {
                out = e;
            }
            self.status = st;
        }
        out
    }
}

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

/// time 用: このシェルと終わった子の CPU 時間 (user, sys) の秒
fn cpu_times() -> (f64, f64) {
    let sec = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    let mut u = (0.0, 0.0);
    for who in [libc::RUSAGE_SELF, libc::RUSAGE_CHILDREN] {
        let mut r: libc::rusage = unsafe { std::mem::zeroed() };
        if unsafe { libc::getrusage(who, &mut r) } == 0 {
            u.0 += sec(r.ru_utime);
            u.1 += sec(r.ru_stime);
        }
    }
    u
}

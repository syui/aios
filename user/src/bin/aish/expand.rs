// 語の展開: ~、$NAME ${...} $(...) `...` $((...))、クォート、IFS で分ける、ワイルドカード
use crate::Shell;
use crate::glob::{self, GLOB_ONE, GLOB_SET, GLOB_STAR};
use crate::parse;

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    /// コマンドの引数: 分けて、ワイルドカードを広げる
    Fields,
    /// 代入の右辺、case の語、つけかえ先、heredoc: 1 つの文字列のまま
    Single,
    /// case のパターン、${x#pat}: クォートの外の * ? [ を印にしたまま
    Pattern,
}

/// 展開の途中: できた語と作りかけの語
struct Out {
    mode: Mode,
    ifs: String,
    fields: Vec<String>,
    cur: String,
    /// 作りかけの語が (空でも) ある ("" や '' があった)
    has: bool,
}

impl Out {
    fn lit(&mut self, c: char, quoted: bool) {
        if !quoted && self.mode != Mode::Single {
            match c {
                '*' => return self.cur.push(GLOB_STAR),
                '?' => return self.cur.push(GLOB_ONE),
                '[' => return self.cur.push(GLOB_SET),
                _ => {}
            }
        }
        self.cur.push(c);
    }

    fn quoted(&mut self, s: &str) {
        self.cur.push_str(s);
        self.has = true;
    }

    /// クォートの外の展開の結果: IFS で分け、* ? [ は広げる
    fn unquoted(&mut self, s: &str) {
        if self.mode != Mode::Fields {
            for c in s.chars() {
                self.lit(c, false);
            }
            return;
        }
        for c in s.chars() {
            if self.ifs.contains(c) {
                self.split();
            } else {
                self.lit(c, false);
            }
        }
    }

    fn split(&mut self) {
        if self.has || !self.cur.is_empty() {
            self.fields.push(std::mem::take(&mut self.cur));
        }
        self.has = false;
    }
}

/// i から始まる open に対応する close の位置 (クォートと \ を飛ばす)
fn matching(cs: &[char], i: usize, open: char, close: char) -> usize {
    let mut depth = 0;
    let mut j = i;
    while j < cs.len() {
        match cs[j] {
            '\\' => j += 1,
            '\'' if open == '(' => {
                j += 1;
                while j < cs.len() && cs[j] != '\'' {
                    j += 1;
                }
            }
            '"' => {
                j += 1;
                while j < cs.len() && cs[j] != '"' {
                    if cs[j] == '\\' {
                        j += 1;
                    }
                    j += 1;
                }
            }
            c if c == open => depth += 1,
            c if c == close => {
                depth -= 1;
                if depth == 0 {
                    return j;
                }
            }
            _ => {}
        }
        j += 1;
    }
    cs.len()
}

/// ブレース展開 (bash と zsh): a{b,c}d → abd acd、{1..3} → 1 2 3、{a..c} → a b c。
/// クォートの中、${...}、$(...) の中は見ない。広げるものがなければ None
pub fn braces(w: &str) -> Option<Vec<String>> {
    let cs: Vec<char> = w.chars().collect();
    let mut i = 0;
    while i < cs.len() {
        match cs[i] {
            '\\' => i += 2,
            '\'' => {
                i += 1;
                while i < cs.len() && cs[i] != '\'' {
                    i += 1;
                }
                i += 1;
            }
            '"' => {
                i += 1;
                while i < cs.len() && cs[i] != '"' {
                    if cs[i] == '\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i += 1;
            }
            '$' if matches!(cs.get(i + 1), Some('{' | '(')) => {
                let (o, c) = if cs[i + 1] == '{' { ('{', '}') } else { ('(', ')') };
                i = matching(&cs, i + 1, o, c) + 1;
            }
            '{' => {
                // 対になる } と、いちばん外の , を探す
                let mut depth = 0;
                let mut j = i;
                let mut commas = vec![];
                let mut end = None;
                while j < cs.len() {
                    match cs[j] {
                        '\\' => j += 1,
                        '\'' | '"' => {
                            let q = cs[j];
                            j += 1;
                            while j < cs.len() && cs[j] != q {
                                j += 1;
                            }
                        }
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                end = Some(j);
                                break;
                            }
                        }
                        ',' if depth == 1 => commas.push(j),
                        _ => {}
                    }
                    j += 1;
                }
                let Some(e) = end else { return None };
                let pre: String = cs[..i].iter().collect();
                let post: String = cs[e + 1..].iter().collect();
                let inner: String = cs[i + 1..e].iter().collect();
                let items: Vec<String> = if !commas.is_empty() {
                    let mut v = vec![];
                    let mut st = i + 1;
                    for &c in commas.iter().chain(std::iter::once(&e)) {
                        v.push(cs[st..c].iter().collect());
                        st = c + 1;
                    }
                    v
                } else if let Some(r) = range(&inner) {
                    r
                } else {
                    // {x} や ${ でないただの { は、そのまま (find -exec {} など)。その先を見る
                    i += 1;
                    continue;
                };
                let mut out = vec![];
                for it in items {
                    let w = format!("{}{}{}", pre, it, post);
                    match braces(&w) {
                        Some(v) => out.extend(v),
                        None => out.push(w),
                    }
                }
                return Some(out);
            }
            _ => i += 1,
        }
    }
    None
}

/// 語の終わりの zsh の修飾 (N) (.) (/) (@) とその前
pub fn glob_qualifier(w: &str) -> Option<(&str, String)> {
    let base = w.strip_suffix(')')?;
    let k = base.rfind('(')?;
    let q = &base[k + 1..];
    if q.is_empty() || !q.chars().all(|c| matches!(c, 'N' | '.' | '/' | '@')) {
        return None;
    }
    let base = &base[..k];
    // ワイルドカードのない語 (a(/) や top.rs(.)) にも、zsh と同じく修飾をつけられる
    (!base.is_empty()).then(|| (base, q.to_string()))
}

/// {1..5} {5..1} {01..10} {a..e} {1..10..2} の中身
fn range(s: &str) -> Option<Vec<String>> {
    let p: Vec<&str> = s.split("..").collect();
    if !(2..=3).contains(&p.len()) {
        return None;
    }
    let step: i64 = p.get(2).map_or(Some(1), |x| x.parse::<i64>().ok().map(|n| n.abs().max(1)))?;
    if let (Ok(a), Ok(b)) = (p[0].parse::<i64>(), p[1].parse::<i64>()) {
        // 0 で始まるものは桁をそろえる
        let width = if (p[0].starts_with('0') && p[0].len() > 1) || (p[1].starts_with('0') && p[1].len() > 1) { p[0].len().max(p[1].len()) } else { 0 };
        let n = ((a - b).abs() / step + 1) as usize;
        if n > 100_000 {
            return None;
        }
        let dir = if a <= b { step } else { -step };
        return Some((0..n).map(|k| format!("{:0w$}", a + dir * k as i64, w = width)).collect());
    }
    let (a, b): (Vec<char>, Vec<char>) = (p[0].chars().collect(), p[1].chars().collect());
    if a.len() == 1 && b.len() == 1 && a[0].is_ascii_alphabetic() && b[0].is_ascii_alphabetic() {
        let (x, y) = (a[0] as i64, b[0] as i64);
        let n = ((x - y).abs() / step + 1) as usize;
        let dir = if x <= y { step } else { -step };
        return Some((0..n).map(|k| char::from_u32((x + dir * k as i64) as u32).unwrap_or('?').to_string()).collect());
    }
    None
}

impl Shell {
    /// 語を展開する。Fields なら 0 個以上の語、ほかは 1 つ
    pub fn expand(&mut self, w: &str, mode: Mode) -> Result<Vec<String>, String> {
        // ブレース展開は引数のときだけ、ほかの展開の前に
        if mode == Mode::Fields
            && let Some(ws) = braces(w)
        {
            let mut out = vec![];
            for x in ws {
                out.extend(self.expand_nobrace(&x)?);
            }
            return Ok(out);
        }
        self.expand_nobrace_mode(w, mode)
    }

    fn expand_nobrace(&mut self, w: &str) -> Result<Vec<String>, String> {
        self.expand_nobrace_mode(w, Mode::Fields)
    }

    fn expand_nobrace_mode(&mut self, w: &str, mode: Mode) -> Result<Vec<String>, String> {
        // zsh のワイルドカードの修飾: *.zsh(N) (当たらなければ消す)、(.) (/) (@)
        let (w, qual) = match glob_qualifier(w) {
            Some((base, q)) if mode == Mode::Fields => (base, q),
            _ => (w, String::new()),
        };
        let ifs = self.get_var("IFS").unwrap_or_else(|| " \t\n".into());
        let mut o = Out { mode, ifs, fields: vec![], cur: String::new(), has: false };
        let cs: Vec<char> = w.chars().collect();
        let mut i = 0;
        // ~ と ~/...
        if cs.first() == Some(&'~') && matches!(cs.get(1), None | Some('/')) {
            o.quoted(&self.get_var("HOME").unwrap_or_default());
            i = 1;
        }
        while i < cs.len() {
            let c = cs[i];
            i += 1;
            match c {
                '\\' => {
                    if let Some(&n) = cs.get(i) {
                        o.lit(n, true);
                        o.has = true;
                        i += 1;
                    }
                }
                '\'' => {
                    let end = cs[i..].iter().position(|&c| c == '\'').map_or(cs.len(), |p| i + p);
                    o.quoted(&cs[i..end].iter().collect::<String>());
                    i = end + 1;
                }
                '"' => {
                    let had = o.has;
                    o.has = true;
                    let mut only_empty_at = false;
                    let before = (o.fields.len(), o.cur.len());
                    while i < cs.len() && cs[i] != '"' {
                        let c = cs[i];
                        i += 1;
                        match c {
                            '\\' if matches!(cs.get(i), Some('$' | '`' | '"' | '\\' | '\n')) => {
                                if cs[i] != '\n' {
                                    o.lit(cs[i], true);
                                }
                                i += 1;
                            }
                            '$' if cs.get(i) == Some(&'@') => {
                                // "$@" は引数ごとに別の語
                                i += 1;
                                let ps = self.params.get(1..).unwrap_or(&[]).to_vec();
                                for (k, p) in ps.iter().enumerate() {
                                    if k > 0 {
                                        o.fields.push(std::mem::take(&mut o.cur));
                                    }
                                    o.cur.push_str(p);
                                }
                                if ps.is_empty() {
                                    only_empty_at = true;
                                }
                            }
                            '$' => i = self.dollar(&cs, i, &mut o, true)?,
                            '`' => i = self.backquote(&cs, i, &mut o, true),
                            c => o.lit(c, true),
                        }
                    }
                    i += 1;
                    // "$@" だけで引数がなければ、語を作らない
                    if only_empty_at && before == (o.fields.len(), o.cur.len()) {
                        o.has = had;
                    }
                }
                '$' => i = self.dollar(&cs, i, &mut o, false)?,
                '`' => i = self.backquote(&cs, i, &mut o, false),
                // <(...) >(...): コマンドとパイプでつないで、その口の名前 (/dev/fd/N)
                '<' | '>' if cs.get(i) == Some(&'(') => {
                    let end = matching(&cs, i, '(', ')');
                    let src: String = cs[i + 1..end.min(cs.len())].iter().collect();
                    let v = self.proc_subst(&src, c == '>');
                    o.quoted(&v);
                    i = end + 1;
                }
                c => o.lit(c, false),
            }
        }
        o.split();
        let fields = o.fields;
        Ok(match mode {
            Mode::Fields => {
                let null = self.nullglob() || qual.contains('N');
                let mut v: Vec<String> = fields.iter().flat_map(|f| if null { glob::glob_or_none(f) } else { glob::glob(f) }).collect();
                // 修飾 (.) はふつうのファイル、(/) はディレクトリ、(@) はリンクだけ
                if qual.contains(['.', '/', '@']) {
                    v.retain(|p| {
                        let m = std::fs::symlink_metadata(p);
                        (qual.contains('.') && m.as_ref().is_ok_and(|m| m.is_file()))
                            || (qual.contains('/') && std::fs::metadata(p).is_ok_and(|m| m.is_dir()))
                            || (qual.contains('@') && m.as_ref().is_ok_and(|m| m.file_type().is_symlink()))
                    });
                }
                v
            }
            Mode::Single => vec![fields.join(" ")],
            Mode::Pattern => vec![fields.join(" ")],
        })
    }

    /// 1 つの文字列に
    pub fn expand_one(&mut self, w: &str) -> Result<String, String> {
        Ok(self.expand(w, Mode::Single)?.pop().unwrap_or_default())
    }

    /// `...` (i は ` の次)
    fn backquote(&mut self, cs: &[char], mut i: usize, o: &mut Out, quoted: bool) -> usize {
        let mut src = String::new();
        while i < cs.len() && cs[i] != '`' {
            if cs[i] == '\\' && matches!(cs.get(i + 1), Some('$' | '`' | '\\')) {
                i += 1;
            }
            src.push(cs[i]);
            i += 1;
        }
        let v = self.command_subst(&src);
        if quoted { o.quoted(&v) } else { o.unquoted(&v) }
        i + 1
    }

    /// $ の後ろ (i は $ の次)。続きの位置を返す
    fn dollar(&mut self, cs: &[char], i: usize, o: &mut Out, quoted: bool) -> Result<usize, String> {
        let put = |sh: &mut Shell, o: &mut Out, v: &str| {
            let _ = sh;
            if quoted { o.quoted(v) } else { o.unquoted(v) }
        };
        match cs.get(i) {
            // ${v//$'\n'/,} のように ${...} の中に残った $'...' (語の中のものは字句のときにほどいてある)
            Some('\'') if !quoted => {
                let mut k = i + 1;
                let mut raw = String::new();
                while k < cs.len() && cs[k] != '\'' {
                    if cs[k] == '\\' && k + 1 < cs.len() {
                        raw.push(cs[k]);
                        k += 1;
                    }
                    raw.push(cs[k]);
                    k += 1;
                }
                o.quoted(&parse::ansi_c(&raw));
                Ok(k + 1)
            }
            Some('(') if cs.get(i + 1) == Some(&'(') => {
                // $(( 式 ))
                let end = matching(cs, i, '(', ')');
                let inner: String = cs[i + 2..end.saturating_sub(1).max(i + 2)].iter().collect();
                let text = self.expand_one(&inner)?;
                let v = self.arith(&text)?;
                put(self, o, &v.to_string());
                Ok(end + 1)
            }
            Some('[') => {
                // $[ 式 ] (古い bash の $(( )))
                let end = matching(cs, i, '[', ']');
                let inner: String = cs[i + 1..end.min(cs.len())].iter().collect();
                let text = self.expand_one(&inner)?;
                let v = self.arith(&text)?;
                put(self, o, &v.to_string());
                Ok(end + 1)
            }
            Some('(') => {
                let end = matching(cs, i, '(', ')');
                let src: String = cs[i + 1..end.min(cs.len())].iter().collect();
                // $(< FILE): ファイルの中身 (cat を起こさない。bash と同じ)
                if let Some(f) = src.trim().strip_prefix('<').filter(|f| !f.starts_with(['<', '('])) {
                    let f = self.expand_one(f.trim())?;
                    let v = match std::fs::read(&f) {
                        Ok(b) => String::from_utf8_lossy(&b).trim_end_matches('\n').to_string(),
                        Err(e) => {
                            eprintln!("{}: {}: {}", crate::shell_name(), f, crate::err_text(&e));
                            self.subst_status = Some(1);
                            String::new()
                        }
                    };
                    put(self, o, &v);
                    return Ok(end + 1);
                }
                let v = self.command_subst(&src);
                put(self, o, &v);
                Ok(end + 1)
            }
            Some('{') => {
                let end = matching(cs, i, '{', '}');
                let inner: String = cs[i + 1..end.min(cs.len())].iter().collect();
                self.param_expr(&inner, o, quoted)?;
                Ok(end + 1)
            }
            Some('@' | '*') if !quoted => {
                let ps = self.params.get(1..).unwrap_or(&[]).to_vec();
                for p in ps {
                    o.unquoted(&p);
                    o.split();
                }
                Ok(i + 1)
            }
            Some(&c) if matches!(c, '?' | '$' | '#' | '!' | '*' | '-') || c.is_ascii_digit() => {
                let v = self.special(&c.to_string()).unwrap_or_default();
                put(self, o, &v);
                Ok(i + 1)
            }
            Some(&c) if c.is_ascii_alphabetic() || c == '_' => {
                let mut j = i;
                while j < cs.len() && (cs[j].is_ascii_alphanumeric() || cs[j] == '_') {
                    j += 1;
                }
                let name: String = cs[i..j].iter().collect();
                let name = self.deref(&name).to_string();
                // 配列 ($path は $PATH ではなく path の配列): zsh と同じく、要素ごとに別の語 ("" の中ならつなぐ)
                if name != "path" || self.arrays.contains_key("path") {
                    if let Some(a) = self.arrays.get(&name).cloned() {
                        // bash (ksharrays): $a は最初の要素だけ
                        if self.opts.contains("ksharrays") {
                            put(self, o, a.first().map_or("", |s| s.as_str()));
                        } else {
                            self.put_array(o, &a, quoted, false);
                        }
                        return Ok(j);
                    }
                } else if let Some(a) = self.array("path") {
                    self.put_array(o, &a, quoted, false);
                    return Ok(j);
                }
                if let Some(m) = self.assoc.get(&name) {
                    let v = m.iter().find(|(k, _)| k == "0").map(|(_, v)| v.clone()).unwrap_or_default();
                    put(self, o, &v);
                    return Ok(j);
                }
                let v = match self.special(&name) {
                    Some(v) => v,
                    None if self.nounset => return Err(format!("{}: unbound variable", name)),
                    None => String::new(),
                };
                put(self, o, &v);
                Ok(j)
            }
            _ => {
                o.lit('$', true);
                Ok(i)
            }
        }
    }

    /// "$*" や "${a[*]}" をつなぐ字: IFS の最初の字 (IFS がなければ空白、空なら何もなし)
    fn ifs_sep(&self) -> String {
        match self.get_var("IFS") {
            None => " ".into(),
            Some(v) => v.chars().next().map(String::from).unwrap_or_default(),
        }
    }

    /// $? $# $1 ${10} など (なければ None)
    pub fn special(&self, name: &str) -> Option<String> {
        Some(match name {
            "?" => self.status.to_string(),
            "$" => self.pid.to_string(),
            "#" => self.params.len().saturating_sub(1).to_string(),
            "!" => match crate::jobs::last_bg() {
                0 => return None,
                p => p.to_string(),
            },
            "-" => {
                let mut f = String::new();
                if self.errexit {
                    f.push('e');
                }
                if self.xtrace {
                    f.push('x');
                }
                f
            }
            "@" => self.params.get(1..).unwrap_or(&[]).join(" "),
            // "$*" は IFS の最初の字でつなぐ
            "*" => self.params.get(1..).unwrap_or(&[]).join(&self.ifs_sep()),
            // 読むたびに変わるもの (bash と zsh)。代入されていればそちら
            "RANDOM" if self.get_var("RANDOM").is_none() => (crate::rand_u32() & 0x7fff).to_string(),
            "SECONDS" if self.get_var("SECONDS").is_none() => self.started.elapsed().as_secs().to_string(),
            // いまのプロセス ($$ はサブシェルでも親のまま、こちらは本当の pid)
            "BASHPID" => unsafe { libc::getpid() }.to_string(),
            "EPOCHSECONDS" => std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()).to_string(),
            n if n.chars().all(|c| c.is_ascii_digit()) => return n.parse::<usize>().ok().and_then(|i| self.params.get(i)).cloned(),
            n => return self.get_var(n),
        })
    }

    /// ${...} の中
    /// 配列を語に: 1 つずつ別の語 ("" の中の [@] も)。join なら空白でつないで 1 つ ("" の中の [*] と $a)
    fn put_array(&mut self, o: &mut Out, a: &[String], quoted: bool, at: bool) {
        if quoted && !at {
            return o.quoted(&a.join(&self.ifs_sep()));
        }
        if quoted {
            for (k, x) in a.iter().enumerate() {
                if k > 0 {
                    o.fields.push(std::mem::take(&mut o.cur));
                }
                o.quoted(x);
            }
            return;
        }
        // クォートの外: zsh と同じく、要素はそれ以上分けず、ワイルドカードも広げない。空の要素は消える
        for (k, x) in a.iter().filter(|x| !x.is_empty()).enumerate() {
            if k > 0 {
                o.split();
            }
            o.quoted(x);
        }
    }

    fn param_expr(&mut self, s: &str, o: &mut Out, quoted: bool) -> Result<(), String> {
        // nameref (declare -n): 前の名前を指す先にかえる (${#r[@]} ${r[1]} ${r:-x} など)
        let renamed;
        let s = if self.namerefs.is_empty() {
            s
        } else {
            let pre = if s.starts_with('#') && s.len() > 1 { 1 } else { 0 };
            let end = s[pre..].find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).map_or(s.len(), |e| e + pre);
            match self.namerefs.contains_key(&s[pre..end]) {
                true => {
                    renamed = format!("{}{}{}", &s[..pre], self.deref(&s[pre..end]), &s[end..]);
                    renamed.as_str()
                }
                false => s,
            }
        };
        let put = |o: &mut Out, v: &str| if quoted { o.quoted(v) } else { o.unquoted(v) };
        // ${!...}: ${!a[@]} 番号 (連想配列ならキー)、${!pre*} ${!pre@} その名前で始まる変数、${!name} 間接参照
        if let Some(r) = s.strip_prefix('!')
            && !r.is_empty()
        {
            if let Some(n) = r.strip_suffix("[@]").or_else(|| r.strip_suffix("[*]"))
                && parse::valid_name(n)
            {
                let keys: Vec<String> = match self.assoc.get(n) {
                    Some(m) => m.iter().map(|(k, _)| k.clone()).collect(),
                    None => {
                        let len = self.array(n).map_or(usize::from(self.get_var(n).is_some()), |a| a.len());
                        let b = self.array_base();
                        (0..len).map(|i| (i as i64 + b).to_string()).collect()
                    }
                };
                self.put_array(o, &keys, quoted, r.ends_with("[@]"));
                return Ok(());
            }
            if let Some(p) = r.strip_suffix('*').or_else(|| r.strip_suffix('@'))
                && parse::valid_name(p)
            {
                let mut names: Vec<String> = self.vars.keys().cloned().chain(std::env::vars().map(|(k, _)| k)).chain(self.arrays.keys().cloned()).chain(self.assoc.keys().cloned()).filter(|k| k.starts_with(p)).collect();
                names.sort();
                names.dedup();
                self.put_array(o, &names, quoted, r.ends_with('@'));
                return Ok(());
            }
            if parse::valid_name(r) || r.chars().all(|c| c.is_ascii_digit()) || r == "#" {
                let target = self.special(r).unwrap_or_default();
                if target.is_empty() {
                    return Ok(());
                }
                return self.param_expr(&target, o, quoted);
            }
        }
        // 連想配列: ${m[k]} ${m[@]} ${#m[@]} ${#m[k]}
        {
            let (count, body) = match s.strip_prefix('#') {
                Some(b) if !b.is_empty() => (true, b),
                _ => (false, s),
            };
            if let Some((n, r)) = body.split_once('[')
                && r.ends_with(']')
                && let Some(m) = self.assoc.get(n).cloned()
            {
                let sub = &r[..r.len() - 1];
                match sub {
                    "@" | "*" if count => put(o, &m.len().to_string()),
                    "@" | "*" => {
                        let vals: Vec<String> = m.into_iter().map(|(_, v)| v).collect();
                        self.put_array(o, &vals, quoted, sub == "@");
                    }
                    k => {
                        let k = self.expand_one(k)?;
                        let v = m.iter().find(|(x, _)| *x == k).map(|(_, v)| v.clone()).unwrap_or_default();
                        put(o, &if count { v.chars().count().to_string() } else { v });
                    }
                }
                return Ok(());
            }
        }
        // 配列の要素ごとに: ${a[@]/p/r} ${a[@]^} ${a[@],,} ${a[@]#p} ${a[@]%p} ${a[@]@Q} (bash)
        if let Some((n, rest)) = s.split_once("[@]").or_else(|| s.split_once("[*]"))
            && parse::valid_name(n)
            && rest.starts_with(['/', '^', ',', '#', '%', '@'])
        {
            const EL: &str = "_AISH_EL";
            let a = match self.assoc.get(n) {
                Some(m) => m.iter().map(|(_, v)| v.clone()).collect(),
                None => self.array(n).unwrap_or_else(|| self.get_var(n).map(|v| vec![v]).unwrap_or_default()),
            };
            let mut r = vec![];
            for x in a {
                self.vars.insert(EL.into(), x);
                let mut t = Out { mode: Mode::Single, ifs: String::new(), fields: vec![], cur: String::new(), has: false };
                let res = self.param_expr(&format!("{}{}", EL, rest), &mut t, true);
                self.vars.remove(EL);
                res?;
                r.push(t.cur);
            }
            self.put_array(o, &r, quoted, s.contains("[@]"));
            return Ok(());
        }
        // 配列の一部: ${a[@]:OFF} ${a[@]:OFF:LEN} (bash。番号は 0 から)
        if let Some((n, rest)) = s.split_once("[@]:").or_else(|| s.split_once("[*]:"))
            && parse::valid_name(n)
        {
            let a = self.array(n).unwrap_or_default();
            let (off, len) = match rest.split_once(':') {
                Some((x, y)) => (x, Some(y)),
                None => (rest, None),
            };
            let off = self.expand_one(off)?;
            let off = self.arith(&off)?;
            let l = a.len() as i64;
            let start = if off < 0 { (l + off).max(0) } else { off.min(l) } as usize;
            let end = match len {
                Some(x) => {
                    let x = self.expand_one(x)?;
                    let x = self.arith(&x)?;
                    if x < 0 { (l + x).max(start as i64) as usize } else { (start + x as usize).min(a.len()) }
                }
                None => a.len(),
            };
            let part = a[start..end.max(start)].to_vec();
            self.put_array(o, &part, quoted, s.contains("[@]"));
            return Ok(());
        }
        // 配列: ${a[@]} ${a[*]} ${a[N]} ${#a[@]} ${#a}
        {
            let (count, body) = match s.strip_prefix('#') {
                Some(b) if !b.is_empty() => (true, b),
                _ => (false, s),
            };
            let (name, sub) = match body.split_once('[') {
                Some((n, r)) if r.ends_with(']') => (n, Some(&r[..r.len() - 1])),
                _ => (body, None),
            };
            if parse::valid_name(name) && (sub.is_some() || self.arrays.contains_key(name) || (name == "path" && count)) {
                let a = self.array(name).unwrap_or_else(|| self.get_var(name).map(|v| vec![v]).unwrap_or_default());
                let ksh = self.opts.contains("ksharrays");
                match (count, sub) {
                    // bash (ksharrays): ${a} は最初の要素、${#a} はその長さ
                    (true, None) if ksh => put(o, &a.first().map_or(0, |s| s.chars().count()).to_string()),
                    (false, None) if ksh => put(o, a.first().map_or("", |s| s.as_str())),
                    (true, None | Some("@" | "*")) => put(o, &a.len().to_string()),
                    (false, None) => self.put_array(o, &a, quoted, false),
                    (false, Some("@")) => self.put_array(o, &a, quoted, true),
                    (false, Some("*")) => self.put_array(o, &a, quoted, false),
                    (c, Some(ix)) => {
                        let ix = self.expand_one(ix)?;
                        let n: i64 = self.arith(&ix)?;
                        let k = if n < 0 { a.len() as i64 + n } else { n - self.array_base() };
                        let v = usize::try_from(k).ok().and_then(|k| a.get(k)).cloned().unwrap_or_default();
                        put(o, &if c { v.chars().count().to_string() } else { v });
                    }
                }
                return Ok(());
            }
        }
        // ${#@} ${#*}: 引数の数
        if s == "#@" || s == "#*" {
            put(o, &self.params.len().saturating_sub(1).to_string());
            return Ok(());
        }
        // ${#NAME}: 長さ
        if let Some(name) = s.strip_prefix('#').filter(|n| !n.is_empty()) {
            let v = self.special(name).unwrap_or_default();
            put(o, &v.chars().count().to_string());
            return Ok(());
        }
        let cs: Vec<char> = s.chars().collect();
        let mut j = 0;
        if cs.first().is_some_and(|c| matches!(c, '?' | '$' | '#' | '!' | '@' | '*' | '-')) {
            j = 1;
        } else {
            while j < cs.len() && (cs[j].is_ascii_alphanumeric() || cs[j] == '_') {
                j += 1;
            }
        }
        let name: String = cs[..j].iter().collect();
        if name.is_empty() {
            return Err(format!("${{{}}}: bad substitution", s));
        }
        let rest: String = cs[j..].iter().collect();
        let val = self.special(&name);
        if rest.is_empty() {
            if quoted && name == "@" {
                // "${@}" も "$@" と同じ
                let ps = self.params.get(1..).unwrap_or(&[]).to_vec();
                for (k, p) in ps.iter().enumerate() {
                    if k > 0 {
                        o.fields.push(std::mem::take(&mut o.cur));
                    }
                    o.cur.push_str(p);
                }
                return Ok(());
            }
            if val.is_none() && self.nounset && !matches!(name.as_str(), "@" | "*" | "!" | "#" | "?" | "$" | "-") {
                return Err(format!("{}: unbound variable", name));
            }
            put(o, &val.unwrap_or_default());
            return Ok(());
        }
        // bash の大文字・小文字: ${x^^} ${x^} ${x,,} ${x,}
        if matches!(rest.as_str(), "^^" | "^" | ",," | ",") {
            let v = val.unwrap_or_default();
            let mut cs = v.chars();
            let r = match rest.as_str() {
                "^^" => v.to_uppercase(),
                ",," => v.to_lowercase(),
                "^" => cs.next().map_or(String::new(), |c| c.to_uppercase().chain(cs).collect()),
                _ => cs.next().map_or(String::new(), |c| c.to_lowercase().chain(cs).collect()),
            };
            put(o, &r);
            return Ok(());
        }
        // bash の ${x@Q} (シェルでそのまま読めるようにクォート) ${x@U} ${x@L} ${x@u}
        if let Some(op) = rest.strip_prefix('@') {
            let v = val.unwrap_or_default();
            let r = match op {
                "Q" => format!("'{}'", v.replace('\'', "'\\''")),
                "U" => v.to_uppercase(),
                "L" => v.to_lowercase(),
                "u" => {
                    let mut cs = v.chars();
                    cs.next().map_or(String::new(), |c| c.to_uppercase().chain(cs).collect())
                }
                "E" | "P" | "A" | "a" | "K" => v,
                _ => return Err(format!("${{{}}}: bad substitution", s)),
            };
            put(o, &r);
            return Ok(());
        }
        // bash の置きかえ: ${x/pat/rep} (はじめの 1 つ) ${x//pat/rep} (ぜんぶ) ${x/#pat/rep} (前) ${x/%pat/rep} (後ろ)
        if let Some(r) = rest.strip_prefix('/') {
            let (mode, r) = match r.chars().next() {
                Some('/') => ('a', &r[1..]),
                Some('#') => ('b', &r[1..]),
                Some('%') => ('e', &r[1..]),
                _ => ('1', r),
            };
            // pat と rep を分ける / (\/ はそのまま)
            let rc: Vec<char> = r.chars().collect();
            let mut k = 0;
            while k < rc.len() && rc[k] != '/' {
                if rc[k] == '\\' {
                    k += 1;
                }
                k += 1;
            }
            let pat_src: String = rc[..k.min(rc.len())].iter().collect();
            let rep_src: String = if k < rc.len() { rc[k + 1..].iter().collect() } else { String::new() };
            let v = val.unwrap_or_default();
            let pat: Vec<char> = self.expand(&pat_src, Mode::Pattern)?.pop().unwrap_or_default().chars().collect();
            let rep = self.expand_one(&rep_src)?;
            let vc: Vec<char> = v.chars().collect();
            let n = vc.len();
            let mut out = String::new();
            let r = match mode {
                'b' => match (0..=n).rev().find(|&e| glob::glob_match(&pat, &vc[..e])) {
                    Some(e) => format!("{}{}", rep, vc[e..].iter().collect::<String>()),
                    None => v.clone(),
                },
                'e' => match (0..=n).find(|&b| glob::glob_match(&pat, &vc[b..])) {
                    Some(b) => format!("{}{}", vc[..b].iter().collect::<String>(), rep),
                    None => v.clone(),
                },
                _ => {
                    // 前から、いちばん長く合うところを置きかえる (all ならくり返す)
                    let mut i = 0;
                    let mut done = false;
                    while i < n {
                        if !done && !pat.is_empty() && let Some(e) = (i + 1..=n).rev().find(|&e| glob::glob_match(&pat, &vc[i..e])) {
                            out.push_str(&rep);
                            i = e;
                            if mode == '1' {
                                done = true;
                            }
                            continue;
                        }
                        out.push(vc[i]);
                        i += 1;
                    }
                    out
                }
            };
            put(o, &r);
            return Ok(());
        }
        // zsh の修飾 ${x:t} (最後の要素) :h (その前) :r (拡張子を除く) :e (拡張子) :l :u (小文字 / 大文字)。:t:r のように続けてよい
        if let Some(m) = rest.strip_prefix(':')
            && !m.is_empty()
            && m.split(':').all(|x| x.len() == 1 && "htrelu".contains(x))
        {
            let mut v = val.unwrap_or_default();
            for x in m.split(':') {
                v = match x {
                    "t" => v.rsplit('/').next().unwrap_or("").to_string(),
                    "h" => match v.rfind('/') {
                        Some(0) => "/".into(),
                        Some(k) => v[..k].to_string(),
                        None => ".".into(),
                    },
                    "r" => match v.rfind('.') {
                        Some(k) if !v[k..].contains('/') => v[..k].to_string(),
                        _ => v,
                    },
                    "e" => match v.rfind('.') {
                        Some(k) if !v[k..].contains('/') => v[k + 1..].to_string(),
                        _ => String::new(),
                    },
                    "l" => v.to_lowercase(),
                    _ => v.to_uppercase(),
                };
            }
            put(o, &v);
            return Ok(());
        }
        // bash の ${x:OFFSET} ${x:OFFSET:LENGTH} (文字の番号。負は終わりから)
        if let Some(m) = rest.strip_prefix(':')
            && m.starts_with(|c: char| c.is_ascii_alphanumeric() || matches!(c, ' ' | '(' | '$' | '_' | ':'))
        {
            let v: Vec<char> = val.unwrap_or_default().chars().collect();
            let (off, len) = match m.split_once(':') {
                Some((a, b)) => (a, Some(b)),
                None => (m, None),
            };
            let off = self.expand_one(off)?;
            let off = if off.trim().is_empty() { 0 } else { self.arith(&off)? };
            let n = v.len() as i64;
            let start = if off < 0 { (n + off).max(0) } else { off.min(n) } as usize;
            let end = match len {
                Some(l) => {
                    let l = self.expand_one(l)?;
                    let l = self.arith(&l)?;
                    if l < 0 { ((n + l).max(start as i64)) as usize } else { (start + l as usize).min(v.len()) }
                }
                None => v.len(),
            };
            put(o, &v[start..end.max(start)].iter().collect::<String>());
            return Ok(());
        }
        for op in [":-", ":=", ":+", ":?", "-", "=", "+", "?", "##", "#", "%%", "%"] {
            let Some(word) = rest.strip_prefix(op) else { continue };
            let colon = op.starts_with(':');
            // : つきは空も「ない」とみなす
            let unset = match &val {
                None => true,
                Some(v) => colon && v.is_empty(),
            };
            match op.trim_start_matches(':') {
                "-" => {
                    if unset {
                        let w = self.expand_one(word)?;
                        put(o, &w);
                    } else {
                        put(o, &val.unwrap());
                    }
                }
                "=" => {
                    if unset {
                        let w = self.expand_one(word)?;
                        self.set_var(&name, &w);
                        put(o, &w);
                    } else {
                        put(o, &val.unwrap());
                    }
                }
                "+" => {
                    if !unset {
                        let w = self.expand_one(word)?;
                        put(o, &w);
                    }
                }
                "?" => {
                    if unset {
                        let w = self.expand_one(word)?;
                        return Err(format!("{}: {}", name, if w.is_empty() { "parameter null or not set" } else { &w }));
                    }
                    put(o, &val.unwrap());
                }
                pat_op => {
                    let v = val.unwrap_or_default();
                    let pat: Vec<char> = self.expand(word, Mode::Pattern)?.pop().unwrap_or_default().chars().collect();
                    let vc: Vec<char> = v.chars().collect();
                    let n = vc.len();
                    let cut = match pat_op {
                        // 前から短い / 長い
                        "#" => (0..=n).find(|&k| glob::glob_match(&pat, &vc[..k])).map(|k| vc[k..].iter().collect()),
                        "##" => (0..=n).rev().find(|&k| glob::glob_match(&pat, &vc[..k])).map(|k| vc[k..].iter().collect()),
                        // 後ろから短い / 長い
                        "%" => (0..=n).rev().find(|&k| glob::glob_match(&pat, &vc[k..])).map(|k| vc[..k].iter().collect()),
                        _ => (0..=n).find(|&k| glob::glob_match(&pat, &vc[k..])).map(|k| vc[..k].iter().collect()),
                    };
                    put(o, &cut.unwrap_or(v));
                }
            }
            return Ok(());
        }
        Err(format!("${{{}}}: bad substitution", s))
    }

    // ---- $(( )) ----

    pub fn arith(&mut self, s: &str) -> Result<i64, String> {
        let toks = arith_lex(s)?;
        let mut p = Arith { toks, i: 0 };
        let v = p.assign(self)?;
        if p.i < p.toks.len() {
            return Err(format!("{}: syntax error in expression", s.trim()));
        }
        Ok(v)
    }

    fn arith_var(&self, name: &str) -> i64 {
        let v = self.get_var(name).unwrap_or_default();
        let v = v.trim();
        if v.is_empty() { 0 } else { parse_num(v).unwrap_or(0) }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum AT {
    Num(i64),
    Name(String),
    Op(String),
}

fn parse_num(s: &str) -> Option<i64> {
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        i64::from_str_radix(h, 16).ok()
    } else if s.len() > 1 && s.starts_with('0') {
        i64::from_str_radix(&s[1..], 8).ok()
    } else {
        s.parse().ok()
    }
}

fn arith_lex(s: &str) -> Result<Vec<AT>, String> {
    let cs: Vec<char> = s.chars().collect();
    let mut v = vec![];
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        if c.is_whitespace() {
            i += 1;
        } else if c.is_ascii_digit() {
            let st = i;
            while i < cs.len() && cs[i].is_ascii_alphanumeric() {
                i += 1;
            }
            // BASE#DIGITS (bash: 16#ff 2#101)
            if i < cs.len() && cs[i] == '#' {
                let base: u32 = cs[st..i].iter().collect::<String>().parse().map_err(|_| "bad base".to_string())?;
                let ds = i + 1;
                i += 1;
                while i < cs.len() && cs[i].is_ascii_alphanumeric() {
                    i += 1;
                }
                let d: String = cs[ds..i].iter().collect();
                if !(2..=36).contains(&base) {
                    return Err(format!("{}#{}: invalid arithmetic base", base, d));
                }
                v.push(AT::Num(i64::from_str_radix(&d, base).map_err(|_| format!("{}#{}: value too great for base", base, d))?));
                continue;
            }
            let t: String = cs[st..i].iter().collect();
            v.push(AT::Num(parse_num(&t).ok_or_else(|| format!("{}: bad number", t))?));
        } else if c.is_ascii_alphabetic() || c == '_' {
            let st = i;
            while i < cs.len() && (cs[i].is_ascii_alphanumeric() || cs[i] == '_') {
                i += 1;
            }
            v.push(AT::Name(cs[st..i].iter().collect()));
        } else {
            let three: String = cs[i..(i + 3).min(cs.len())].iter().collect();
            let op = ["**", "<<=", ">>=", "<=", ">=", "==", "!=", "&&", "||", "<<", ">>", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", "++", "--"]
                .into_iter()
                .find(|o| three.starts_with(o))
                .map(|o| o.to_string())
                .unwrap_or_else(|| c.to_string());
            if !"+-*/%<>=!&|^~?:(),".contains(op.chars().next().unwrap()) {
                return Err(format!("{}: syntax error in expression", s.trim()));
            }
            i += op.chars().count();
            v.push(AT::Op(op));
        }
    }
    Ok(v)
}

struct Arith {
    toks: Vec<AT>,
    i: usize,
}

impl Arith {
    fn peek_op(&self) -> Option<&str> {
        match self.toks.get(self.i) {
            Some(AT::Op(o)) => Some(o),
            _ => None,
        }
    }

    fn eat(&mut self, op: &str) -> bool {
        if self.peek_op() == Some(op) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn assign(&mut self, sh: &mut Shell) -> Result<i64, String> {
        // NAME = 式 / NAME += 式 ...
        if let (Some(AT::Name(n)), Some(AT::Op(op))) = (self.toks.get(self.i).cloned(), self.toks.get(self.i + 1).cloned())
            && matches!(op.as_str(), "=" | "+=" | "-=" | "*=" | "/=" | "%=" | "<<=" | ">>=" | "&=" | "|=" | "^=")
        {
            self.i += 2;
            let r = self.assign(sh)?;
            let l = sh.arith_var(&n);
            let v = if op == "=" { r } else { binop(&op[..op.len() - 1], l, r)? };
            sh.set_var(&n, &v.to_string());
            return Ok(v);
        }
        let mut v = self.ternary(sh)?;
        while self.eat(",") {
            v = self.assign(sh)?;
        }
        Ok(v)
    }

    fn ternary(&mut self, sh: &mut Shell) -> Result<i64, String> {
        let c = self.binary(sh, 0)?;
        if self.eat("?") {
            let a = self.assign(sh)?;
            if !self.eat(":") {
                return Err("expected : in ?:".into());
            }
            let b = self.ternary(sh)?;
            return Ok(if c != 0 { a } else { b });
        }
        Ok(c)
    }

    fn binary(&mut self, sh: &mut Shell, min: u8) -> Result<i64, String> {
        let mut l = self.unary(sh)?;
        loop {
            let Some(op) = self.peek_op().map(|s| s.to_string()) else { break };
            let p = match op.as_str() {
                "||" => 1,
                "&&" => 2,
                "|" => 3,
                "^" => 4,
                "&" => 5,
                "==" | "!=" => 6,
                "<" | ">" | "<=" | ">=" => 7,
                "<<" | ">>" => 8,
                "+" | "-" => 9,
                "*" | "/" | "%" => 10,
                "**" => 11,
                _ => break,
            };
            if p < min {
                break;
            }
            self.i += 1;
            // ** は右から (2**3**2 は 2**9)
            let r = self.binary(sh, if p == 11 { p } else { p + 1 })?;
            l = binop(&op, l, r)?;
        }
        Ok(l)
    }

    fn unary(&mut self, sh: &mut Shell) -> Result<i64, String> {
        for op in ["++", "--"] {
            if self.eat(op)
                && let Some(AT::Name(n)) = self.toks.get(self.i).cloned()
            {
                self.i += 1;
                let v = sh.arith_var(&n) + if op == "++" { 1 } else { -1 };
                sh.set_var(&n, &v.to_string());
                return Ok(v);
            }
        }
        if self.eat("-") {
            return Ok(self.unary(sh)?.wrapping_neg());
        }
        if self.eat("+") {
            return self.unary(sh);
        }
        if self.eat("!") {
            return Ok((self.unary(sh)? == 0) as i64);
        }
        if self.eat("~") {
            return Ok(!self.unary(sh)?);
        }
        if self.eat("(") {
            let v = self.assign(sh)?;
            if !self.eat(")") {
                return Err("expected )".into());
            }
            return Ok(v);
        }
        match self.toks.get(self.i).cloned() {
            Some(AT::Num(n)) => {
                self.i += 1;
                Ok(n)
            }
            Some(AT::Name(n)) => {
                self.i += 1;
                let v = sh.arith_var(&n);
                // NAME++ / NAME--
                for op in ["++", "--"] {
                    if self.eat(op) {
                        sh.set_var(&n, &(v + if op == "++" { 1 } else { -1 }).to_string());
                    }
                }
                Ok(v)
            }
            _ => Err("syntax error in expression".into()),
        }
    }
}

fn binop(op: &str, l: i64, r: i64) -> Result<i64, String> {
    Ok(match op {
        "+" => l.wrapping_add(r),
        "-" => l.wrapping_sub(r),
        "*" => l.wrapping_mul(r),
        "**" if r < 0 => return Err("exponent less than 0".into()),
        "**" => l.wrapping_pow(r.min(u32::MAX as i64) as u32),
        "/" | "%" if r == 0 => return Err("division by 0".into()),
        "/" => l.wrapping_div(r),
        "%" => l.wrapping_rem(r),
        "<<" => l.wrapping_shl(r as u32),
        ">>" => l.wrapping_shr(r as u32),
        "<" => (l < r) as i64,
        ">" => (l > r) as i64,
        "<=" => (l <= r) as i64,
        ">=" => (l >= r) as i64,
        "==" => (l == r) as i64,
        "!=" => (l != r) as i64,
        "&" => l & r,
        "|" => l | r,
        "^" => l ^ r,
        "&&" => (l != 0 && r != 0) as i64,
        "||" => (l != 0 || r != 0) as i64,
        _ => return Err(format!("{}: bad operator", op)),
    })
}

// 語の展開: ~、$NAME ${...} $(...) `...` $((...))、クォート、IFS で分ける、ワイルドカード
use crate::Shell;
use crate::glob::{self, GLOB_ONE, GLOB_SET, GLOB_STAR};

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

impl Shell {
    /// 語を展開する。Fields なら 0 個以上の語、ほかは 1 つ
    pub fn expand(&mut self, w: &str, mode: Mode) -> Result<Vec<String>, String> {
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
                c => o.lit(c, false),
            }
        }
        o.split();
        let fields = o.fields;
        Ok(match mode {
            Mode::Fields => fields.iter().flat_map(|f| glob::glob(f)).collect(),
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
            Some('(') if cs.get(i + 1) == Some(&'(') => {
                // $(( 式 ))
                let end = matching(cs, i, '(', ')');
                let inner: String = cs[i + 2..end.saturating_sub(1).max(i + 2)].iter().collect();
                let text = self.expand_one(&inner)?;
                let v = self.arith(&text)?;
                put(self, o, &v.to_string());
                Ok(end + 1)
            }
            Some('(') => {
                let end = matching(cs, i, '(', ')');
                let src: String = cs[i + 1..end.min(cs.len())].iter().collect();
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
                let v = self.get_var(&name).unwrap_or_default();
                put(self, o, &v);
                Ok(j)
            }
            _ => {
                o.lit('$', true);
                Ok(i)
            }
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
            "@" | "*" => self.params.get(1..).unwrap_or(&[]).join(" "),
            n if n.chars().all(|c| c.is_ascii_digit()) => return n.parse::<usize>().ok().and_then(|i| self.params.get(i)).cloned(),
            n => return self.get_var(n),
        })
    }

    /// ${...} の中
    fn param_expr(&mut self, s: &str, o: &mut Out, quoted: bool) -> Result<(), String> {
        let put = |o: &mut Out, v: &str| if quoted { o.quoted(v) } else { o.unquoted(v) };
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
            put(o, &val.unwrap_or_default());
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
            let op = ["<<=", ">>=", "<=", ">=", "==", "!=", "&&", "||", "<<", ">>", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", "++", "--"]
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
                _ => break,
            };
            if p < min {
                break;
            }
            self.i += 1;
            let r = self.binary(sh, p + 1)?;
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

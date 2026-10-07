// 字句と構文: ソースを木 (List) にする。語はクォートを残したまま持ち、実行の直前に展開する (expand.rs)
//
//   list      := and_or ((';' | '&' | 改行) and_or)*
//   and_or    := pipeline (('&&' | '||') 改行* pipeline)*
//   pipeline  := ['!'] command ('|' 改行* command)*
//   command   := simple | compound redir* | NAME '(' ')' 改行* compound
//   compound  := if / while / until / for / case / '{' list '}' / '(' list ')'
use std::cell::RefCell;
use std::rc::Rc;

/// 展開する前の語 (ソースのまま)
pub type Word = String;

#[derive(Clone, Debug)]
pub struct Redir {
    pub fd: i32,
    pub kind: RKind,
}

#[derive(Clone, Debug)]
pub enum RKind {
    /// < > >> <> >| と open の flags
    File(Word, i32),
    /// >&N <&N (N が - なら閉じる)
    Dup(Word),
    /// <<EOF の中身 (読み終わるまでは空)。bool は展開するか
    Here(Rc<RefCell<String>>, bool),
    /// <<< 語 (bash と zsh の here-string): 語を展開して、終わりに改行をつけたもの
    HereStr(Word),
}

#[derive(Clone, Debug)]
pub enum Cmd {
    Simple { assigns: Vec<(String, Word)>, words: Vec<Word>, redirs: Vec<Redir> },
    Compound(Rc<Compound>, Vec<Redir>),
    Func(String, Rc<Compound>),
}

#[derive(Debug)]
pub enum Compound {
    Brace(List),
    Subshell(List),
    /// (条件, 本体) の並びと else
    If(Vec<(List, List)>, Option<List>),
    /// 条件, 本体, until か
    While(List, List, bool),
    For(String, Option<Vec<Word>>, List),
    Case(Word, Vec<(Vec<Word>, List)>),
    /// (( 式 )): 0 でなければ成功 (bash と zsh)
    Arith(String),
    /// for (( 初め; 条件; 次 )) do ... done
    ArithFor(String, String, String, List),
}

#[derive(Clone, Debug)]
pub struct Pipeline {
    pub neg: bool,
    /// time: 終わったら、かかった時間 (real / user / sys) を標準エラーに (bash と同じ形)
    pub time: bool,
    pub cmds: Vec<Cmd>,
}

#[derive(Clone, Debug)]
pub struct AndOr {
    pub first: Pipeline,
    /// (&& なら true, パイプライン)
    pub rest: Vec<(bool, Pipeline)>,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub ao: AndOr,
    pub bg: bool,
    /// ジョブの表示に使うソース
    pub text: String,
    /// はじまりの行 ($LINENO。1 から、0 はわからない)
    pub line: usize,
}

pub type List = Vec<Item>;

#[derive(Debug)]
pub enum Error {
    /// 続きの行が要る (クォートや if が閉じていない)
    Incomplete,
    Syntax(String),
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Word(String),
    /// && || ;; ; & | ( ) と改行 ("\n")
    Op(&'static str),
    /// fd とつけかえの記号 (< > >> <& >& << <<- <<< <> >|)
    Redir(i32, &'static str),
    Eof,
}

const RESERVED: &[&str] = &["if", "then", "elif", "else", "fi", "while", "until", "do", "done", "for", "in", "case", "esac", "{", "}", "!"];

pub struct Parser {
    src: Vec<char>,
    pos: usize,
    peeked: Option<(Tok, usize)>,
    /// 改行で読む heredoc: (区切り, タブを消すか, 入れ物)
    heredocs: Vec<(String, bool, Rc<RefCell<String>>)>,
    /// { の中にいる深さ (zsh と同じく、{ echo a } の } を ; なしでも閉じとして読むため)
    braces: usize,
    /// 行の数えかけ (位置, その位置の行)
    lines: (usize, usize),
}

/// 配列の代入 a=(x y) の値の印: ARRAY のあとに、要素の語 (展開する前) を SEP でつないだもの
pub const ARRAY: char = '\u{f0010}';
pub const SEP: char = '\u{f0011}';

fn is_meta(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | ';' | '&' | '|' | '<' | '>' | '(' | ')')
}

fn is_name(s: &str) -> bool {
    let mut cs = s.chars();
    matches!(cs.next(), Some(c) if c.is_ascii_alphabetic() || c == '_') && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl Parser {
    /// 行を数えない ($LINENO を変えない)
    pub fn no_lines(&mut self) {
        self.lines = (0, 0);
    }

    /// 位置 p の行 (前から数える。たいてい前に進むだけなので続きから)
    fn line_at(&mut self, p: usize) -> usize {
        if self.lines.1 == 0 {
            return 0;
        }
        if p < self.lines.0 {
            self.lines = (0, 1);
        }
        let p = p.min(self.src.len());
        self.lines.1 += self.src[self.lines.0..p].iter().filter(|&&c| c == '\n').count();
        self.lines.0 = p;
        self.lines.1
    }

    pub fn new(src: &str) -> Parser {
        Parser { src: src.chars().collect(), pos: 0, peeked: None, heredocs: vec![], braces: 0, lines: (0, 1) }
    }

    // ---- 字句 ----

    fn at(&self, k: usize) -> Option<char> {
        self.src.get(self.pos + k).copied()
    }

    fn next_tok(&mut self) -> Result<(Tok, usize), Error> {
        // 空白、\ 改行、コメント
        loop {
            match self.at(0) {
                Some(' ' | '\t') => self.pos += 1,
                Some('\\') if self.at(1) == Some('\n') => self.pos += 2,
                Some('#') => {
                    while self.at(0).is_some_and(|c| c != '\n') {
                        self.pos += 1;
                    }
                }
                _ => break,
            }
        }
        let start = self.pos;
        let Some(c) = self.at(0) else { return Ok((Tok::Eof, start)) };
        if c == '\n' {
            self.pos += 1;
            self.read_heredocs()?;
            return Ok((Tok::Op("\n"), start));
        }
        // 2>file の 2
        let mut k = 0;
        while self.at(k).is_some_and(|c| c.is_ascii_digit()) {
            k += 1;
        }
        let fd = if k > 0 && matches!(self.at(k), Some('<' | '>')) {
            let n: String = self.src[self.pos..self.pos + k].iter().collect();
            self.pos += k;
            n.parse().ok()
        } else {
            None
        };
        let c = self.at(0).unwrap();
        // <(...) >(...) は語 (コマンドの出力や入力をファイルの名前に)
        if (c == '<' || c == '>') && self.at(1) == Some('(') && fd.is_none() {
            let w = self.scan_word()?;
            return Ok((Tok::Word(w), start));
        }
        if c == '<' || c == '>' {
            let two: String = [Some(c), self.at(1), self.at(2)].iter().flatten().collect();
            let op: &'static str = if two.starts_with("<<<") {
                "<<<"
            } else if two.starts_with("<<-") {
                "<<-"
            } else {
                ["<<", "<&", "<>", ">>", ">&", ">|"].into_iter().find(|o| two.starts_with(o)).unwrap_or(if c == '<' { "<" } else { ">" })
            };
            self.pos += op.len();
            let fd = fd.unwrap_or(if c == '<' { 0 } else { 1 });
            return Ok((Tok::Redir(fd, op), start));
        }
        for op in ["&&", "||", ";;", ";", "&", "|", "(", ")"] {
            if self.src[self.pos..].iter().take(op.len()).copied().eq(op.chars()) {
                self.pos += op.len();
                return Ok((Tok::Op(op), start));
            }
        }
        let w = self.scan_word()?;
        Ok((Tok::Word(w), start))
    }

    /// クォートや $( ) を含めて、区切りまでの 1 語をそのまま
    fn scan_word(&mut self) -> Result<String, Error> {
        let mut w = String::new();
        while let Some(c) = self.at(0) {
            if (c == '<' || c == '>') && self.at(1) == Some('(') && w.is_empty() {
                self.scan_dollar(&mut w)?;
                continue;
            }
            if is_meta(c) {
                // zsh のワイルドカードの修飾 *.zsh(N) は、語のつづき
                if c == '(' && !w.is_empty() && !w.ends_with('=') {
                    let mut k = 1;
                    while self.at(k).is_some_and(|x| matches!(x, 'N' | '.' | '/' | '@')) {
                        k += 1;
                    }
                    if k > 1 && self.at(k) == Some(')') && self.at(k + 1).is_none_or(is_meta) {
                        w.extend(&self.src[self.pos..=self.pos + k]);
                        self.pos += k + 1;
                        continue;
                    }
                }
                break;
            }
            match c {
                '\\' => {
                    match self.at(1) {
                        Some('\n') => {}
                        Some(n) => {
                            w.push('\\');
                            w.push(n);
                        }
                        None => w.push('\\'),
                    }
                    self.pos += 2;
                }
                '\'' => {
                    let end = self.find('\'', self.pos + 1)?;
                    w.extend(&self.src[self.pos..=end]);
                    self.pos = end + 1;
                }
                '"' => {
                    w.push('"');
                    self.pos += 1;
                    self.scan_dquote(&mut w)?;
                }
                '$' | '`' => self.scan_dollar(&mut w)?,
                _ => {
                    w.push(c);
                    self.pos += 1;
                }
            }
        }
        Ok(w)
    }

    fn find(&self, c: char, from: usize) -> Result<usize, Error> {
        self.src[from..].iter().position(|&x| x == c).map(|p| from + p).ok_or(Error::Incomplete)
    }

    /// " の中 (閉じる " まで w に足す)
    fn scan_dquote(&mut self, w: &mut String) -> Result<(), Error> {
        loop {
            match self.at(0) {
                None => return Err(Error::Incomplete),
                Some('"') => {
                    w.push('"');
                    self.pos += 1;
                    return Ok(());
                }
                Some('\\') => {
                    w.push('\\');
                    if let Some(n) = self.at(1) {
                        w.push(n);
                    }
                    self.pos += 2;
                }
                // "..." の中の $'...' はそのままの文字 (bash と同じ)
                Some('$') if self.at(1) == Some('\'') => {
                    w.push('$');
                    self.pos += 1;
                }
                Some('$' | '`') => self.scan_dollar(w)?,
                Some(c) => {
                    w.push(c);
                    self.pos += 1;
                }
            }
        }
    }

    /// $( ) $(( )) ${ } ` ` を対応する閉じまで
    fn scan_dollar(&mut self, w: &mut String) -> Result<(), Error> {
        let c = self.at(0).unwrap();
        // $'...' (bash と zsh): \n \t \xHH \uHHHH などをほどいて、'...' の文字として
        if c == '$' && self.at(1) == Some('\'') {
            let mut k = self.pos + 2;
            let mut raw = String::new();
            loop {
                match self.src.get(k) {
                    None => return Err(Error::Incomplete),
                    Some('\\') => {
                        raw.push('\\');
                        if let Some(&n) = self.src.get(k + 1) {
                            raw.push(n);
                        }
                        k += 2;
                    }
                    Some('\'') => break,
                    Some(&x) => {
                        raw.push(x);
                        k += 1;
                    }
                }
            }
            self.pos = k + 1;
            w.push('\'');
            w.push_str(&ansi_c(&raw).replace('\'', "'\\''"));
            w.push('\'');
            return Ok(());
        }
        if c == '`' {
            let mut i = self.pos + 1;
            loop {
                match self.src.get(i) {
                    None => return Err(Error::Incomplete),
                    Some('\\') => i += 2,
                    Some('`') => break,
                    _ => i += 1,
                }
            }
            w.extend(&self.src[self.pos..=i]);
            self.pos = i + 1;
            return Ok(());
        }
        let (open, close) = match self.at(1) {
            Some('(') => ('(', ')'),
            Some('{') => ('{', '}'),
            _ => {
                w.push('$');
                self.pos += 1;
                return Ok(());
            }
        };
        let start = self.pos;
        self.pos += 2;
        let mut depth = 1;
        while depth > 0 {
            match self.at(0) {
                None => return Err(Error::Incomplete),
                Some('\\') => self.pos += 2,
                Some('\'') if open == '(' => self.pos = self.find('\'', self.pos + 1)? + 1,
                Some('"') => {
                    self.pos += 1;
                    let mut tmp = String::new();
                    self.scan_dquote(&mut tmp)?;
                }
                Some('$' | '`') if self.at(0) == Some('`') || matches!(self.at(1), Some('(' | '{')) => {
                    let mut tmp = String::new();
                    self.scan_dollar(&mut tmp)?;
                }
                Some(x) if x == open => {
                    depth += 1;
                    self.pos += 1;
                }
                Some(x) if x == close => {
                    depth -= 1;
                    self.pos += 1;
                }
                Some(_) => self.pos += 1,
            }
        }
        w.extend(&self.src[start..self.pos]);
        Ok(())
    }

    /// 改行の後ろに続く heredoc の中身を読む
    fn read_heredocs(&mut self) -> Result<(), Error> {
        for (delim, strip, body) in std::mem::take(&mut self.heredocs) {
            let mut text = String::new();
            loop {
                if self.pos >= self.src.len() {
                    return Err(Error::Incomplete);
                }
                let end = self.src[self.pos..].iter().position(|&c| c == '\n').map_or(self.src.len(), |p| self.pos + p);
                let mut line: String = self.src[self.pos..end].iter().collect();
                self.pos = (end + 1).min(self.src.len());
                if strip {
                    line = line.trim_start_matches('\t').to_string();
                }
                if line == delim {
                    break;
                }
                text.push_str(&line);
                text.push('\n');
            }
            *body.borrow_mut() = text;
        }
        Ok(())
    }

    fn peek(&mut self) -> Result<&Tok, Error> {
        if self.peeked.is_none() {
            self.peeked = Some(self.next_tok()?);
        }
        Ok(&self.peeked.as_ref().unwrap().0)
    }

    /// 先読みした字句のところのソースがこれで始まるか
    fn peeked_raw_is(&self, s: &str) -> bool {
        let Some((_, start)) = self.peeked else { return false };
        s.chars().enumerate().all(|(k, c)| self.src.get(start + k) == Some(&c))
    }

    /// (( ... )) を読む (先読みした ( から)。中はそのまま文字で
    fn take_arith(&mut self) -> Result<String, Error> {
        let (_, start) = self.peeked.take().unwrap();
        let mut i = start + 2;
        let mut depth = 0;
        loop {
            match self.src.get(i) {
                None => return Err(Error::Incomplete),
                Some('(') => depth += 1,
                Some(')') if depth > 0 => depth -= 1,
                Some(')') if self.src.get(i + 1) == Some(&')') => break,
                Some(')') => return Err(Error::Syntax("syntax error: `))' expected".into())),
                _ => {}
            }
            i += 1;
        }
        let e: String = self.src[start + 2..i].iter().collect();
        self.pos = i + 2;
        Ok(e)
    }

    fn take(&mut self) -> Result<(Tok, usize), Error> {
        self.peek()?;
        Ok(self.peeked.take().unwrap())
    }

    /// 次がこの予約語か
    fn is_word(&mut self, w: &str) -> Result<bool, Error> {
        Ok(matches!(self.peek()?, Tok::Word(x) if x == w))
    }

    fn is_op(&mut self, o: &str) -> Result<bool, Error> {
        Ok(matches!(self.peek()?, Tok::Op(x) if *x == o))
    }

    fn expect_word(&mut self, w: &str) -> Result<(), Error> {
        self.skip_newlines()?;
        if self.is_word(w)? {
            self.take()?;
            return Ok(());
        }
        Err(self.unexpected(w))
    }

    fn unexpected(&mut self, want: &str) -> Error {
        match self.peek() {
            Ok(Tok::Eof) => Error::Incomplete,
            Ok(t) => Error::Syntax(format!("syntax error near {} (expected {})", show(t), want)),
            Err(e) => e,
        }
    }

    fn skip_newlines(&mut self) -> Result<(), Error> {
        while self.is_op("\n")? {
            self.take()?;
        }
        Ok(())
    }

    // ---- 構文 ----

    /// 改行までの 1 つの完全なコマンド。終わりなら None
    pub fn complete_command(&mut self) -> Result<Option<List>, Error> {
        self.skip_newlines()?;
        if *self.peek()? == Tok::Eof {
            return Ok(None);
        }
        let list = self.list(&[], true)?;
        match self.take()?.0 {
            Tok::Op("\n") | Tok::Eof => Ok(Some(list)),
            t => Err(Error::Syntax(format!("syntax error near {}", show(&t)))),
        }
    }

    /// ぜんぶ (sh -c、eval、$( ) の中)
    pub fn program(&mut self) -> Result<List, Error> {
        let mut all = vec![];
        while let Some(l) = self.complete_command()? {
            all.extend(l);
        }
        Ok(all)
    }

    /// ends の予約語、) 、;; 、終わりの前まで。top なら改行でも終わる
    fn list(&mut self, ends: &[&str], top: bool) -> Result<List, Error> {
        let mut items = vec![];
        loop {
            if !top {
                self.skip_newlines()?;
            }
            match self.peek()? {
                Tok::Eof | Tok::Op(")" | ";;") => break,
                Tok::Op("\n") if top => break,
                Tok::Word(w) if ends.contains(&w.as_str()) => break,
                _ => {}
            }
            let start = self.peeked.as_ref().unwrap().1;
            let line = self.line_at(start);
            let ao = self.and_or()?;
            let end = self.pos.min(self.peeked.as_ref().map_or(self.pos, |p| p.1));
            let text: String = self.src[start..end].iter().collect::<String>().trim().to_string();
            let bg = match self.peek()? {
                Tok::Op("&") => {
                    self.take()?;
                    true
                }
                Tok::Op(";") => {
                    self.take()?;
                    false
                }
                _ => false,
            };
            items.push(Item { ao, bg, text, line });
        }
        Ok(items)
    }

    fn and_or(&mut self) -> Result<AndOr, Error> {
        let first = self.pipeline()?;
        let mut rest = vec![];
        loop {
            let and = match self.peek()? {
                Tok::Op("&&") => true,
                Tok::Op("||") => false,
                _ => break,
            };
            self.take()?;
            self.skip_newlines()?;
            rest.push((and, self.pipeline()?));
        }
        Ok(AndOr { first, rest })
    }

    fn pipeline(&mut self) -> Result<Pipeline, Error> {
        let time = self.is_word("time")?;
        if time {
            self.take()?;
        }
        let neg = self.is_word("!")?;
        if neg {
            self.take()?;
        }
        let mut cmds = vec![self.command()?];
        while self.is_op("|")? {
            self.take()?;
            self.skip_newlines()?;
            cmds.push(self.command()?);
        }
        Ok(Pipeline { neg, time, cmds })
    }

    fn command(&mut self) -> Result<Cmd, Error> {
        let t = self.peek()?.clone();
        let compound = match &t {
            // (( 式 ))
            Tok::Op("(") if self.peeked_raw_is("((") => Some(Compound::Arith(self.take_arith()?)),
            Tok::Op("(") => {
                self.take()?;
                let l = self.list(&[], false)?;
                if !self.is_op(")")? {
                    return Err(self.unexpected(")"));
                }
                self.take()?;
                Some(Compound::Subshell(l))
            }
            Tok::Word(w) => match w.as_str() {
                "{" => {
                    self.take()?;
                    self.braces += 1;
                    let l = self.list(&["}"], false);
                    self.braces -= 1;
                    let l = l?;
                    self.expect_word("}")?;
                    Some(Compound::Brace(l))
                }
                "if" => Some(self.if_clause()?),
                "[[" => return self.cond(),
                "while" | "until" => {
                    self.take()?;
                    let cond = self.list(&["do"], false)?;
                    self.expect_word("do")?;
                    let body = self.list(&["done"], false)?;
                    self.expect_word("done")?;
                    Some(Compound::While(cond, body, w == "until"))
                }
                "for" => Some(self.for_clause()?),
                "case" => Some(self.case_clause()?),
                "function" => {
                    self.take()?;
                    let Tok::Word(name) = self.take()?.0 else { return Err(Error::Syntax("function: missing name".into())) };
                    if self.is_op("(")? {
                        self.take()?;
                        if !self.is_op(")")? {
                            return Err(self.unexpected(")"));
                        }
                        self.take()?;
                    }
                    return self.func_body(name);
                }
                _ => None,
            },
            Tok::Eof => return Err(Error::Incomplete),
            Tok::Redir(..) => None,
            t => return Err(Error::Syntax(format!("syntax error near {}", show(t)))),
        };
        if let Some(c) = compound {
            let redirs = self.redirs()?;
            return Ok(Cmd::Compound(Rc::new(c), redirs));
        }
        self.simple()
    }

    /// [[ ... ]] (bash と zsh): ]] までの語をそのまま。&& || ( ) < > も語にする (コマンドを区切らない)。
    /// =~ の右は空白まで ( ) | もふくめて 1 語。動かすのは組み込みの [[ (main.rs)
    fn cond(&mut self) -> Result<Cmd, Error> {
        self.take()?; // [[
        let mut words = vec!["[[".to_string()];
        loop {
            while matches!(self.at(0), Some(' ' | '\t' | '\n')) || (self.at(0) == Some('\\') && self.at(1) == Some('\n')) {
                self.pos += if self.at(0) == Some('\\') { 2 } else { 1 };
            }
            let Some(c) = self.at(0) else { return Err(Error::Incomplete) };
            if c == ']' && self.at(1) == Some(']') && self.at(2).is_none_or(is_meta) {
                self.pos += 2;
                words.push("]]".into());
                break;
            }
            if words.last().is_some_and(|w| w == "=~") {
                // 正規表現: クォートの外の空白まで
                let mut w = String::new();
                while let Some(c) = self.at(0) {
                    if matches!(c, ' ' | '\t' | '\n') {
                        break;
                    }
                    match c {
                        '\'' | '"' => {
                            let end = self.find(c, self.pos + 1)?;
                            w.extend(&self.src[self.pos..=end]);
                            self.pos = end + 1;
                        }
                        '\\' => {
                            w.extend(self.src[self.pos..(self.pos + 2).min(self.src.len())].iter());
                            self.pos += 2;
                        }
                        _ => {
                            w.push(c);
                            self.pos += 1;
                        }
                    }
                }
                words.push(w);
                continue;
            }
            if let Some(op) = ["&&", "||", "(", ")", "<", ">"].into_iter().find(|o| self.src[self.pos..].iter().take(o.len()).copied().eq(o.chars())) {
                self.pos += op.len();
                words.push(op.into());
                continue;
            }
            let w = self.scan_word()?;
            if w.is_empty() {
                return Err(Error::Syntax(format!("syntax error near {} in [[", c)));
            }
            words.push(w);
        }
        let redirs = self.redirs()?;
        Ok(Cmd::Simple { assigns: vec![], words, redirs })
    }

    fn func_body(&mut self, name: String) -> Result<Cmd, Error> {
        self.skip_newlines()?;
        match self.command()? {
            Cmd::Compound(body, r) if r.is_empty() => Ok(Cmd::Func(name, body)),
            // f() { ...; } > out: 呼ぶたびにつけかえる (中身を { } でくるむ)
            c @ Cmd::Compound(..) => {
                let ao = AndOr { first: Pipeline { neg: false, time: false, cmds: vec![c] }, rest: vec![] };
                Ok(Cmd::Func(name, Rc::new(Compound::Brace(vec![Item { ao, bg: false, text: String::new(), line: 0 }]))))
            }
            _ => Err(Error::Syntax(format!("{}: function body must be a compound command", name))),
        }
    }

    fn redirs(&mut self) -> Result<Vec<Redir>, Error> {
        let mut v = vec![];
        while let Tok::Redir(..) = self.peek()? {
            v.push(self.redir()?);
        }
        Ok(v)
    }

    fn redir(&mut self) -> Result<Redir, Error> {
        let Tok::Redir(fd, op) = self.take()?.0 else { unreachable!() };
        let target = match self.take()?.0 {
            Tok::Word(w) => w,
            Tok::Eof => return Err(Error::Incomplete),
            t => return Err(Error::Syntax(format!("syntax error near {} (expected a file after {})", show(&t), op))),
        };
        let kind = match op {
            "<" => RKind::File(target, libc::O_RDONLY),
            ">" | ">|" => RKind::File(target, libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC),
            ">>" => RKind::File(target, libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND),
            "<>" => RKind::File(target, libc::O_RDWR | libc::O_CREAT),
            "<&" | ">&" => RKind::Dup(target),
            "<<<" => RKind::HereStr(target),
            _ => {
                // heredoc: 区切りがクォートされていれば中は展開しない
                let quoted = target.contains(['\'', '"', '\\']);
                let delim: String = target.chars().filter(|c| !matches!(c, '\'' | '"' | '\\')).collect();
                let body = Rc::new(RefCell::new(String::new()));
                self.heredocs.push((delim, op == "<<-", body.clone()));
                RKind::Here(body, !quoted)
            }
        };
        Ok(Redir { fd, kind })
    }

    fn simple(&mut self) -> Result<Cmd, Error> {
        let mut assigns = vec![];
        let mut words: Vec<Word> = vec![];
        let mut redirs = vec![];
        loop {
            match self.peek()? {
                Tok::Redir(..) => redirs.push(self.redir()?),
                Tok::Word(w) => {
                    let w = w.clone();
                    // declare / local などの引数の配列: declare -A m=([a]=1) (語の中に配列の印を入れて渡す)
                    if words.first().is_some_and(|c| matches!(c.as_str(), "declare" | "typeset" | "local" | "readonly" | "export"))
                        && let Some(k) = w.strip_suffix('=')
                        && is_name(k.strip_suffix('+').unwrap_or(k))
                    {
                        self.take()?;
                        if self.is_op("(")? {
                            self.take()?;
                            let mut items = vec![];
                            loop {
                                match self.take()?.0 {
                                    Tok::Op(")") => break,
                                    Tok::Op("\n") => {}
                                    Tok::Word(x) => items.push(x),
                                    Tok::Eof => return Err(Error::Incomplete),
                                    t => return Err(Error::Syntax(format!("syntax error near {} in an array", show(&t)))),
                                }
                            }
                            let v: String = std::iter::once(ARRAY).chain(items.join(&SEP.to_string()).chars()).collect();
                            words.push(format!("{}={}", k, v));
                        } else {
                            words.push(w);
                        }
                        continue;
                    }
                    // 配列: a=(x y z) と a+=(w) (bash と zsh)。キーの + は足すこと
                    if words.is_empty()
                        && let Some(k) = w.strip_suffix('=')
                        && is_name(k.strip_suffix('+').unwrap_or(k))
                    {
                        self.take()?;
                        if self.is_op("(")? {
                            self.take()?;
                            let mut items = vec![];
                            loop {
                                match self.take()?.0 {
                                    Tok::Op(")") => break,
                                    Tok::Op("\n") => {}
                                    Tok::Word(x) => items.push(x),
                                    Tok::Eof => return Err(Error::Incomplete),
                                    t => return Err(Error::Syntax(format!("syntax error near {} in an array", show(&t)))),
                                }
                            }
                            let v: String = std::iter::once(ARRAY).chain(items.join(&SEP.to_string()).chars()).collect();
                            assigns.push((k.to_string(), v));
                            continue;
                        }
                        if is_name(k) {
                            assigns.push((k.to_string(), String::new()));
                            continue;
                        }
                        words.push(w);
                        continue;
                    }
                    // NAME=v、NAME+=v (足す)、NAME[i]=v (配列の 1 つ。連想配列ならキー)、NAME[i]+=v
                    if words.is_empty()
                        && let Some((k, _)) = w.split_once('=')
                        && is_assign_key(k)
                    {
                        self.take()?;
                        assigns.push((k.to_string(), w[k.len() + 1..].to_string()));
                        continue;
                    }
                    // zsh: { echo a } の } (語の始めから。; や改行なしで) は、{ の中なら閉じ
                    if w == "}" && self.braces > 0 && !words.is_empty() {
                        break;
                    }
                    self.take()?;
                    // NAME ( ) { ... } は関数
                    if words.is_empty() && redirs.is_empty() && assigns.is_empty() && self.is_op("(")? && is_name(&w) {
                        self.take()?;
                        if !self.is_op(")")? {
                            return Err(self.unexpected(")"));
                        }
                        self.take()?;
                        return self.func_body(w);
                    }
                    words.push(w);
                }
                _ => break,
            }
        }
        if words.is_empty() && assigns.is_empty() && redirs.is_empty() {
            return Err(self.unexpected("a command"));
        }
        Ok(Cmd::Simple { assigns, words, redirs })
    }

    fn if_clause(&mut self) -> Result<Compound, Error> {
        self.take()?; // if
        let mut arms = vec![];
        let mut els = None;
        loop {
            let cond = self.list(&["then"], false)?;
            self.expect_word("then")?;
            let body = self.list(&["elif", "else", "fi"], false)?;
            arms.push((cond, body));
            self.skip_newlines()?;
            match self.take()?.0 {
                Tok::Word(w) if w == "elif" => continue,
                Tok::Word(w) if w == "else" => {
                    els = Some(self.list(&["fi"], false)?);
                    self.expect_word("fi")?;
                    break;
                }
                Tok::Word(w) if w == "fi" => break,
                Tok::Eof => return Err(Error::Incomplete),
                t => return Err(Error::Syntax(format!("syntax error near {} (expected fi)", show(&t)))),
            }
        }
        Ok(Compound::If(arms, els))
    }

    fn for_clause(&mut self) -> Result<Compound, Error> {
        self.take()?; // for
        // for (( 初め; 条件; 次 ))
        if matches!(self.peek()?, Tok::Op("(")) && self.peeked_raw_is("((") {
            let e = self.take_arith()?;
            let mut parts = e.splitn(3, ';').map(|x| x.trim().to_string());
            let (a, b, c) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
            self.skip_newlines()?;
            if self.is_op(";")? {
                self.take()?;
            }
            self.skip_newlines()?;
            let body = if self.is_word("{")? {
                self.take()?;
                self.braces += 1;
                let l = self.list(&["}"], false);
                self.braces -= 1;
                let l = l?;
                self.expect_word("}")?;
                l
            } else {
                self.expect_word("do")?;
                let l = self.list(&["done"], false)?;
                self.expect_word("done")?;
                l
            };
            return Ok(Compound::ArithFor(a, b, c, body));
        }
        let name = match self.take()?.0 {
            Tok::Word(w) if is_name(&w) => w,
            Tok::Eof => return Err(Error::Incomplete),
            t => return Err(Error::Syntax(format!("for: bad variable name {}", show(&t)))),
        };
        let mut items = None;
        self.skip_newlines()?;
        if self.is_word("in")? {
            self.take()?;
            let mut v = vec![];
            while let Tok::Word(w) = self.peek()? {
                v.push(w.clone());
                self.take()?;
            }
            items = Some(v);
        }
        if self.is_op(";")? {
            self.take()?;
        }
        self.expect_word("do")?;
        let body = self.list(&["done"], false)?;
        self.expect_word("done")?;
        Ok(Compound::For(name, items, body))
    }

    fn case_clause(&mut self) -> Result<Compound, Error> {
        self.take()?; // case
        let word = match self.take()?.0 {
            Tok::Word(w) => w,
            Tok::Eof => return Err(Error::Incomplete),
            t => return Err(Error::Syntax(format!("case: syntax error near {}", show(&t)))),
        };
        self.expect_word("in")?;
        let mut arms = vec![];
        loop {
            self.skip_newlines()?;
            if self.is_word("esac")? {
                self.take()?;
                break;
            }
            if self.is_op("(")? {
                self.take()?;
            }
            let mut pats = vec![];
            loop {
                match self.take()?.0 {
                    Tok::Word(w) => pats.push(w),
                    Tok::Eof => return Err(Error::Incomplete),
                    t => return Err(Error::Syntax(format!("case: syntax error near {}", show(&t)))),
                }
                match self.take()?.0 {
                    Tok::Op("|") => continue,
                    Tok::Op(")") => break,
                    Tok::Eof => return Err(Error::Incomplete),
                    t => return Err(Error::Syntax(format!("case: syntax error near {}", show(&t)))),
                }
            }
            let body = self.list(&["esac"], false)?;
            arms.push((pats, body));
            self.skip_newlines()?;
            if self.is_op(";;")? {
                self.take()?;
            } else if !self.is_word("esac")? {
                return Err(self.unexpected(";; or esac"));
            }
        }
        Ok(Compound::Case(word, arms))
    }
}

fn show(t: &Tok) -> String {
    match t {
        Tok::Word(w) => format!("`{}'", w),
        Tok::Op("\n") => "newline".into(),
        Tok::Op(o) => format!("`{}'", o),
        Tok::Redir(_, o) => format!("`{}'", o),
        Tok::Eof => "end of file".into(),
    }
}

/// 予約語か (type などで)
pub fn is_reserved(w: &str) -> bool {
    RESERVED.contains(&w)
}

/// 変数名として使えるか
/// $'...' の中のエスケープ
pub fn ansi_c(s: &str) -> String {
    let cs: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let hex = |cs: &[char], i: usize, max: usize| -> (u32, usize) {
        let mut v = 0;
        let mut n = 0;
        while n < max && i + n < cs.len() && cs[i + n].is_ascii_hexdigit() {
            v = v * 16 + cs[i + n].to_digit(16).unwrap();
            n += 1;
        }
        (v, n)
    };
    while i < cs.len() {
        if cs[i] != '\\' || i + 1 >= cs.len() {
            out.push(cs[i]);
            i += 1;
            continue;
        }
        i += 1;
        let c = cs[i];
        i += 1;
        match c {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'a' => out.push('\x07'),
            'b' => out.push('\x08'),
            'e' | 'E' => out.push('\x1b'),
            'f' => out.push('\x0c'),
            'v' => out.push('\x0b'),
            '\\' | '\'' | '"' | '?' => out.push(c),
            'x' => {
                let (v, n) = hex(&cs, i, 2);
                i += n;
                out.push(char::from_u32(v).unwrap_or('?'));
            }
            'u' | 'U' => {
                let (v, n) = hex(&cs, i, if c == 'u' { 4 } else { 8 });
                i += n;
                out.push(char::from_u32(v).unwrap_or('?'));
            }
            '0'..='7' => {
                let mut v = c.to_digit(8).unwrap();
                let mut n = 0;
                while n < 2 && i < cs.len() && cs[i].is_digit(8) {
                    v = v * 8 + cs[i].to_digit(8).unwrap();
                    i += 1;
                    n += 1;
                }
                out.push(char::from_u32(v).unwrap_or('?'));
            }
            'c' if i < cs.len() => {
                out.push(char::from_u32((cs[i] as u32) & 0x1f).unwrap_or('?'));
                i += 1;
            }
            _ => {
                out.push('\\');
                out.push(c);
            }
        }
    }
    out
}

/// 代入の左: NAME、NAME+、NAME[...]、NAME[...]+
pub fn is_assign_key(k: &str) -> bool {
    let k = k.strip_suffix('+').unwrap_or(k);
    match k.split_once('[') {
        Some((n, r)) => is_name(n) && r.ends_with(']') && r.len() > 1,
        None => is_name(k),
    }
}

pub fn valid_name(s: &str) -> bool {
    is_name(s)
}

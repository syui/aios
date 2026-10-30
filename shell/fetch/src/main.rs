// aish-fetch: HTTP で取ってきて、JSON なら道 (.a.b[0].c) で取り出す (端末なしの tools だけ)
//   fetch  url (か file) の中身。path で一部、fields で配列の 1 つずつを 1 行 (name=x  status=y) に
// curl | python3 -c 'import json ...' のかわり (aish --mcp で Claude が使う。GitHub の API、PDS のレコードなど)。
// 取ってくるのは curl (aios にも開発のマシンにもある。プロキシの設定も curl のもの)
use aish_plugin::{Spec, Tool, Value, error, json, s};
use std::io::Write;
use std::process::{Command, Stdio};

const FETCH: &str = r#"{"type":"object","properties":{
"url":{"type":"string","description":"取ってくるもの (https://... file://... など curl が読めるもの)"},
"file":{"type":"string","description":"url のかわりに、手元のファイル (JSON なら path と fields も効く)"},
"path":{"type":"string","description":"JSON の取り出し (jq に似た形): .a.b[0].c、.items[].name (配列のみな)、.a[-1] (うしろから)。なければ全部"},
"fields":{"type":"array","items":{"type":"string"},"description":"path の答え (配列ならその 1 つずつ) を 1 行に: [\"name\",\"status\",\"head_commit.message\"] → name=x  status=y  head_commit.message=z"},
"method":{"type":"string","description":"GET (既定)、POST、PUT、DELETE など"},
"body":{"type":"string","description":"送るもの (POST など。JSON なら Content-Type も JSON に)"},
"headers":{"type":"object","description":"足すヘッダー ({\"Accept\": \"application/json\"})"},
"timeout_ms":{"type":"integer","description":"これを過ぎたらやめる (既定 30000、50000 まで)"}}}"#;

/// 答えの text はここまで (長ければ切って、path や fields でしぼるように言う)
const MAX_TEXT: usize = 16_000;

fn main() {
    let spec = Spec {
        name: "fetch",
        hooks: &[],
        keys: &[],
        tools: &[Tool {
            name: "fetch",
            desc: "url (か file) の中身を取ってくる (curl)。JSON なら path (.a.b[0].c、.items[].name) で取り出し、fields で配列の 1 つずつを 1 行 (name=x  status=y) に。{status, type, text}。curl | python3 -c json のかわり",
            input: FETCH,
        }],
    };
    aish_plugin::run(spec, |ev, v| match ev {
        "tool" if s(v, "name") == "fetch" => fetch(v),
        "tool" => error(format!("{}: no such tool", s(v, "name"))),
        _ => json!({}),
    });
}

fn fetch(v: &Value) -> Value {
    let a = &v["args"];
    let (body, mut r) = if !s(a, "file").is_empty() {
        let p = std::path::Path::new(s(a, "file"));
        let p = if p.is_absolute() { p.to_path_buf() } else { std::path::Path::new(s(v, "pwd")).join(p) };
        match std::fs::read(&p) {
            Ok(b) => (b, json!({})),
            Err(e) => return error(format!("{}: {}", p.display(), e)),
        }
    } else if !s(a, "url").is_empty() {
        match curl(a) {
            Ok(x) => x,
            Err(e) => return error(e),
        }
    } else {
        return error("give url or file");
    };
    r["bytes"] = json!(body.len());
    let text = String::from_utf8_lossy(&body);
    let path = s(a, "path");
    let fields: Vec<&str> = a["fields"].as_array().map(|f| f.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
    let out = match serde_json::from_str::<Value>(&text) {
        Ok(doc) => match select(&doc, path) {
            Ok(found) => {
                if !path.is_empty() {
                    r["count"] = json!(found.len());
                }
                show(&found, &fields)
            }
            Err(e) => return error(e),
        },
        // JSON でなければそのまま (path や fields は使えない)
        Err(_) if path.is_empty() && fields.is_empty() => text.into_owned(),
        Err(_) => return error(format!("not JSON ({}), so path and fields do not apply", r["type"].as_str().unwrap_or("?"))),
    };
    r["text"] = json!(cut(out));
    r
}

/// curl で取ってくる: (中身, {status, type}) か、curl のしくじり
fn curl(a: &Value) -> Result<(Vec<u8>, Value), String> {
    let ms = a["timeout_ms"].as_u64().unwrap_or(30_000).min(50_000);
    let mut c = Command::new("curl");
    // 終わりに、HTTP の番号と Content-Type を 1 行で (中身と分けるしるしをつけて)
    c.args(["-sS", "-L", "--compressed", "--max-time"]).arg(format!("{:.1}", ms as f64 / 1000.0)).args(["-w", "\n\x01%{http_code}\t%{content_type}"]);
    let method = s(a, "method");
    if !method.is_empty() && !method.eq_ignore_ascii_case("GET") {
        c.args(["-X", &method.to_uppercase()]);
    }
    if let Some(h) = a["headers"].as_object() {
        for (k, v) in h {
            c.arg("-H").arg(format!("{}: {}", k, v.as_str().map_or(v.to_string(), String::from)));
        }
    }
    let body = s(a, "body");
    if !body.is_empty() {
        if serde_json::from_str::<Value>(body).is_ok() && !a["headers"].as_object().is_some_and(|h| h.keys().any(|k| k.eq_ignore_ascii_case("content-type"))) {
            c.args(["-H", "Content-Type: application/json"]);
        }
        c.args(["--data-binary", "@-"]);
    }
    c.arg(s(a, "url")).stdin(if body.is_empty() { Stdio::null() } else { Stdio::piped() }).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().map_err(|e| format!("curl: {}", e))?;
    if let Some(mut i) = child.stdin.take() {
        let _ = i.write_all(body.as_bytes());
    }
    let o = child.wait_with_output().map_err(|e| format!("curl: {}", e))?;
    let mut out = o.stdout;
    let Some(at) = out.windows(2).rposition(|w| w == b"\n\x01") else {
        return Err(format!("curl: {}", String::from_utf8_lossy(&o.stderr).trim()));
    };
    let tail = String::from_utf8_lossy(&out[at + 2..]).into_owned();
    out.truncate(at);
    let (code, ty) = tail.split_once('\t').unwrap_or((tail.as_str(), ""));
    let code: u64 = code.trim().parse().unwrap_or(0);
    if code == 0 {
        // つながらなかった、時間切れなど (file:// も 0 だが、そのときは curl が成功を返す)
        if !o.status.success() {
            return Err(format!("curl: {}", String::from_utf8_lossy(&o.stderr).trim()));
        }
    }
    let mut r = json!({ "status": code, "type": ty });
    if code >= 400 {
        r["error"] = json!(format!("HTTP {}", code));
    }
    Ok((out, r))
}

/// 道の 1 つ
#[derive(Debug, PartialEq)]
enum Step {
    Key(String),
    Index(i64),
    Each,
}

/// .a.b[0].c、.a[]、.a[-1]、.["a.b"]、a.b (前の . はなくてもよい)
fn parse(path: &str) -> Result<Vec<Step>, String> {
    let cs: Vec<char> = path.trim().chars().collect();
    let mut steps = Vec::new();
    let mut i = 0;
    while i < cs.len() {
        match cs[i] {
            '.' => i += 1,
            '[' => {
                let end = cs[i..].iter().position(|&c| c == ']').map(|e| e + i).ok_or_else(|| format!("{}: no ]", path))?;
                let inner: String = cs[i + 1..end].iter().collect();
                let inner = inner.trim();
                steps.push(if inner.is_empty() {
                    Step::Each
                } else if let Some(k) = inner.strip_prefix('"').and_then(|k| k.strip_suffix('"')) {
                    Step::Key(k.to_string())
                } else {
                    Step::Index(inner.parse().map_err(|_| format!("{}: [{}] is not a number", path, inner))?)
                });
                i = end + 1;
            }
            _ => {
                let end = cs[i..].iter().position(|&c| c == '.' || c == '[').map_or(cs.len(), |e| e + i);
                steps.push(Step::Key(cs[i..end].iter().collect()));
                i = end;
            }
        }
    }
    Ok(steps)
}

/// 道で取り出す ([] があれば、いくつも)。ないところは null
fn select(doc: &Value, path: &str) -> Result<Vec<Value>, String> {
    let mut cur = vec![doc.clone()];
    for st in parse(path)? {
        cur = cur
            .into_iter()
            .flat_map(|v| match (&st, v) {
                (Step::Key(k), Value::Object(mut m)) => vec![m.remove(k).unwrap_or(Value::Null)],
                (Step::Index(n), Value::Array(mut a)) => {
                    let k = if *n < 0 { a.len() as i64 + n } else { *n };
                    vec![usize::try_from(k).ok().filter(|k| *k < a.len()).map_or(Value::Null, |k| a.swap_remove(k))]
                }
                (Step::Each, Value::Array(a)) => a,
                (Step::Each, Value::Object(m)) => m.into_iter().map(|(_, v)| v).collect(),
                _ => vec![Value::Null],
            })
            .collect();
    }
    Ok(cur)
}

/// 答えを読む形に: 文字はそのまま、ほかは JSON (オブジェクトと配列は字下げ)。fields があれば 1 つを 1 行に
fn show(found: &[Value], fields: &[&str]) -> String {
    // 道の答えが 1 つの配列なら、その 1 つずつ
    let items: Vec<&Value> = match found {
        [Value::Array(a)] if !fields.is_empty() => a.iter().collect(),
        _ => found.iter().collect(),
    };
    let mut out = String::new();
    for it in items {
        if fields.is_empty() {
            match it {
                Value::String(t) => out.push_str(t),
                Value::Object(_) | Value::Array(_) => out.push_str(&serde_json::to_string_pretty(it).unwrap_or_default()),
                _ => out.push_str(&it.to_string()),
            }
        } else {
            let line: Vec<String> = fields
                .iter()
                .map(|f| {
                    let v = select(it, f).ok().and_then(|mut x| (x.len() == 1).then(|| x.remove(0))).unwrap_or(Value::Null);
                    // 1 行に: 文字の中の改行は空白に
                    let v = match v {
                        Value::String(t) => t.lines().next().unwrap_or("").to_string(),
                        v => v.to_string(),
                    };
                    format!("{}={}", f.trim_start_matches('.'), v)
                })
                .collect();
            out.push_str(&line.join("  "));
        }
        out.push('\n');
    }
    out
}

fn cut(t: String) -> String {
    if t.len() <= MAX_TEXT {
        return t;
    }
    let mut end = MAX_TEXT;
    while !t.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n... ({} bytes more; narrow it with path or fields)\n", &t[..end], t.len() - end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> Value {
        json!({ "runs": [{ "name": "unix", "status": "completed", "head": { "msg": "a\nb" } }, { "name": "pkg", "status": "queued", "head": { "msg": "c" } }], "n": 2, "a.b": 1 })
    }

    #[test]
    fn paths() {
        assert_eq!(parse(".a.b[0].c").unwrap(), vec![Step::Key("a".into()), Step::Key("b".into()), Step::Index(0), Step::Key("c".into())]);
        assert_eq!(parse("a[]").unwrap(), vec![Step::Key("a".into()), Step::Each]);
        assert_eq!(parse(r#".["a.b"]"#).unwrap(), vec![Step::Key("a.b".into())]);
        assert!(parse(".a[x]").is_err());
        assert!(parse(".a[0").is_err());
    }

    #[test]
    fn selects() {
        let d = doc();
        assert_eq!(select(&d, ".runs[0].name").unwrap(), vec![json!("unix")]);
        assert_eq!(select(&d, ".runs[-1].status").unwrap(), vec![json!("queued")]);
        assert_eq!(select(&d, ".runs[].name").unwrap(), vec![json!("unix"), json!("pkg")]);
        assert_eq!(select(&d, r#".["a.b"]"#).unwrap(), vec![json!(1)]);
        assert_eq!(select(&d, ".nosuch.x").unwrap(), vec![Value::Null]);
        assert_eq!(select(&d, "").unwrap(), vec![d.clone()]);
    }

    #[test]
    fn shows() {
        let d = doc();
        assert_eq!(show(&select(&d, ".runs").unwrap(), &["name", "status", "head.msg"]), "name=unix  status=completed  head.msg=a\nname=pkg  status=queued  head.msg=c\n");
        assert_eq!(show(&select(&d, ".runs[].name").unwrap(), &[]), "unix\npkg\n");
        assert_eq!(show(&select(&d, ".n").unwrap(), &[]), "2\n");
    }
}

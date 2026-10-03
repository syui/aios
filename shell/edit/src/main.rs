// aish-edit: ファイルを確かに読み書きする (aish の基本のプラグイン。端末なしの tools だけ)
//   read   行の番号つきで読む
//   edit   old をぴったり new に置きかえる (見つからない、いくつもある、ならしくじる)
//   write  まるごと書く (なければ作る)
//   undo   edit / write のまえに戻す
// aish --mcp で Claude が使う。取り消すための写しはこのプログラムのメモリーにだけ持つ
// (ディスクに書かないので、リポジトリやイメージに入ることはない。aish が終わると消える)
use aish_plugin::{Spec, Tool, Value, error, json, s};
use std::path::{Path, PathBuf};

const READ: &str = r#"{"type":"object","properties":{"path":{"type":"string"},"offset":{"type":"integer","description":"何行目から (1 から。既定 1)"},"limit":{"type":"integer","description":"何行 (既定 2000)"}},"required":["path"]}"#;
const EDIT: &str = r#"{"type":"object","properties":{"path":{"type":"string"},"old":{"type":"string","description":"置きかえるもの (ファイルにぴったり 1 つあること)"},"new":{"type":"string"},"all":{"type":"boolean","description":"いくつもあれば全部 (既定 false)"}},"required":["path","old","new"]}"#;
const WRITE: &str = r#"{"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}"#;
const UNDO: &str = r#"{"type":"object","properties":{"path":{"type":"string","description":"このファイルの最後の変更を戻す (なければ、いちばん新しい変更)"}}}"#;

/// 取り消すための写しの数
const KEEP: usize = 100;

/// 変える前のファイル (なかったなら None)
struct Snap {
    path: PathBuf,
    before: Option<Vec<u8>>,
}

fn main() {
    let spec = Spec {
        name: "edit",
        hooks: &[],
        keys: &[],
        tools: &[
            Tool { name: "read", desc: "ファイルを行の番号つきで読む。{path, lines (全部の行数), text}", input: READ },
            Tool { name: "edit", desc: "ファイルの old をぴったり new に置きかえる。old が見つからないか、いくつもある (all でない) ならしくじる。undo で戻せる", input: EDIT },
            Tool { name: "write", desc: "ファイルをまるごと書く (なければディレクトリごと作る)。undo で戻せる", input: WRITE },
            Tool { name: "undo", desc: "edit / write のまえに戻す (aish が動いているあいだの 100 回まで)", input: UNDO },
        ],
    };
    let mut snaps: Vec<Snap> = Vec::new();
    aish_plugin::run(spec, |ev, v| match ev {
        "tool" => {
            let a = &v["args"];
            let path = resolve(s(v, "pwd"), s(a, "path"));
            match s(v, "name") {
                "read" => read(&path, a),
                "edit" => edit(&path, a, &mut snaps),
                "write" => write(&path, s(a, "content").as_bytes(), &mut snaps),
                "undo" => undo(&path, s(a, "path").is_empty(), &mut snaps),
                n => error(format!("{}: no such tool", n)),
            }
        }
        _ => json!({}),
    });
}

/// 相対パスはシェルのいまのディレクトリから
fn resolve(pwd: &str, p: &str) -> PathBuf {
    let p = Path::new(p);
    if p.is_absolute() { p.to_path_buf() } else { Path::new(pwd).join(p) }
}

fn read(path: &Path, a: &Value) -> Value {
    let text = match std::fs::read(path) {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(e) => return error(format!("{}: {}", path.display(), e)),
    };
    let from = a["offset"].as_u64().unwrap_or(1).max(1) as usize;
    let limit = a["limit"].as_u64().unwrap_or(2000) as usize;
    let lines: Vec<&str> = text.lines().collect();
    let mut out = String::new();
    for (i, l) in lines.iter().enumerate().skip(from - 1).take(limit) {
        out.push_str(&format!("{:6}\t{}\n", i + 1, l));
    }
    json!({ "path": path.display().to_string(), "lines": lines.len(), "text": out })
}

fn edit(path: &Path, a: &Value, snaps: &mut Vec<Snap>) -> Value {
    let (old, new) = (s(a, "old"), s(a, "new"));
    if old.is_empty() {
        return error("old is empty");
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => return error(format!("{}: {}", path.display(), e)),
    };
    let n = text.matches(old).count();
    let all = a["all"].as_bool().unwrap_or(false);
    if n == 0 {
        return error(format!("{}: old not found", path.display()));
    }
    if n > 1 && !all {
        return error(format!("{}: old found {} times (make it longer, or all: true)", path.display(), n));
    }
    let out = if all { text.replace(old, new) } else { text.replacen(old, new, 1) };
    match write(path, out.as_bytes(), snaps) {
        r if r.get("error").is_some() => r,
        _ => json!({ "path": path.display().to_string(), "replaced": if all { n } else { 1 } }),
    }
}

fn write(path: &Path, content: &[u8], snaps: &mut Vec<Snap>) -> Value {
    let before = std::fs::read(path).ok();
    if let Some(d) = path.parent()
        && let Err(e) = std::fs::create_dir_all(d)
    {
        return error(format!("{}: {}", d.display(), e));
    }
    if let Err(e) = std::fs::write(path, content) {
        return error(format!("{}: {}", path.display(), e));
    }
    let created = before.is_none();
    snaps.push(Snap { path: path.to_path_buf(), before });
    if snaps.len() > KEEP {
        snaps.remove(0);
    }
    json!({ "path": path.display().to_string(), "bytes": content.len(), "created": created })
}

fn undo(path: &Path, latest: bool, snaps: &mut Vec<Snap>) -> Value {
    let Some(i) = snaps.iter().rposition(|x| latest || x.path == path) else {
        return error("nothing to undo");
    };
    let snap = snaps.remove(i);
    let r = match &snap.before {
        Some(b) => std::fs::write(&snap.path, b),
        None => std::fs::remove_file(&snap.path),
    };
    match r {
        Ok(()) => json!({ "path": snap.path.display().to_string(), "removed": snap.before.is_none() }),
        Err(e) => error(format!("{}: {}", snap.path.display(), e)),
    }
}

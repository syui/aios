// aish-claude: コマンドが見つからなかった行を Claude に渡す (aish の基本のプラグイン)
//   ふつうのコマンドは aish がそのまま動かす。見つからなかったとき (打ちまちがい、やりたいことを言葉で書いた) だけ、
//   その行といまのディレクトリを `claude -p` に渡し、答えをそのまま端末に出す。Ctrl-C で止まる。
//   claude がなければ何もしない (aish がいつもどおり "command not found")。AISH_CLAUDE=0 でも何もしない
// Claude は aish --mcp (aios では /etc/claude-code/managed-mcp.json) で同じ aios を触れる
use aish_plugin::{Spec, Value, json, s};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

fn main() {
    let spec = Spec { name: "claude", hooks: &["not_found"], keys: &[], tools: &[] };
    aish_plugin::run(spec, |ev, v| match ev {
        "not_found" => ask(v),
        _ => json!({}),
    });
}

/// PATH の中の claude
fn claude(path: &str) -> Option<std::path::PathBuf> {
    path.split(':').filter(|d| !d.is_empty()).map(|d| std::path::Path::new(d).join("claude")).find(|p| p.is_file())
}

fn ask(v: &Value) -> Value {
    // シェルのいまの環境 (aish が not_found の env に入れる。なければプラグインが起こされたときのもの)
    let env: Vec<(String, String)> = match v["env"].as_object() {
        Some(m) => m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect(),
        None => std::env::vars().collect(),
    };
    let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    if get("AISH_CLAUDE") == Some("0") {
        return json!({});
    }
    let Some(bin) = claude(get("PATH").unwrap_or("/usr/bin:/bin")) else { return json!({}) };
    let line = s(v, "line").trim().to_string();
    let pwd = s(v, "pwd");
    if line.is_empty() {
        return json!({});
    }
    // 端末へ出す (プラグインの標準出力は aish との話に使っているので)
    let Ok(tty) = std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty") else { return json!({}) };
    let (Ok(o), Ok(e), Ok(i)) = (tty.try_clone(), tty.try_clone(), tty.try_clone()) else { return json!({}) };
    let first = v["args"][0].as_str().unwrap_or("").to_string();
    let _ = writeln_tty(&tty, &format!("\x1b[2maish: {}: not found → claude\x1b[0m", first));
    let prompt = format!(
        "aios のシェル aish で、いまのディレクトリ {pwd} で次の行が打たれたが、コマンドが見つからなかった:\n\n    {line}\n\n\
         打ちまちがいなら正しいコマンドを、やりたいことを言葉で書いたのなら、そのやり方を短く答えて。\
         動かす必要があれば aish の MCP (run など) で動かしてよい。"
    );
    let mut cmd = Command::new(bin);
    cmd.env_clear().envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    cmd.arg("-p").arg(prompt).current_dir(if pwd.is_empty() { "." } else { pwd }).stdin(Stdio::from(i)).stdout(Stdio::from(o)).stderr(Stdio::from(e));
    // プラグインは Ctrl-C を無視して起こされるので、claude には戻す (Ctrl-C で止められるように)
    unsafe {
        cmd.pre_exec(|| {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            Ok(())
        });
    }
    match cmd.status() {
        Ok(st) => json!({ "status": st.code().unwrap_or(130) }),
        Err(_) => json!({}),
    }
}

fn writeln_tty(mut t: &std::fs::File, s: &str) -> std::io::Result<()> {
    use std::io::Write;
    writeln!(t, "{}", s)
}

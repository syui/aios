// resize: 端末に画面の大きさを聞いて、端末の行と列に設定する (xterm の resize と同じ)。
// tmux のペインやウィンドウの大きさを変えたあとに。sh に読ませる形で出す:
//   eval "$(resize)"   で COLUMNS と LINES も合わせる
#[path = "../lib/term.rs"]
mod term;

fn main() {
    match term::fit(0) {
        Some((rows, cols)) => println!("COLUMNS={};\nLINES={};\nexport COLUMNS LINES;", cols, rows),
        None => {
            eprintln!("resize: the terminal did not report its size");
            std::process::exit(1);
        }
    }
}

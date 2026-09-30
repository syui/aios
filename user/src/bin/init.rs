// aios の最初のプロセス
use std::time::Instant;

fn main() {
    let t = Instant::now();
    println!("aios init (pid {})", std::process::id());

    let args: Vec<String> = std::env::args().collect();
    println!("args: {:?}", args);
    for (k, v) in std::env::vars() {
        println!("env: {}={}", k, v);
    }

    let mut v: Vec<u64> = (1..=100_000).collect();
    v.reverse();
    v.sort();
    println!("sum 1..=100000 = {}", v.iter().sum::<u64>());

    let words = ["aios", "unix", "rust", "arm"];
    let s = words.iter().map(|w| w.to_uppercase()).collect::<Vec<_>>().join(" ");
    println!("{}", s);

    println!("elapsed {:?}", t.elapsed());
}

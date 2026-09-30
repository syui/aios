fn main() {
    let args: Vec<String> = std::env::args().collect();
    println!("hello (pid {}): args {:?}", std::process::id(), &args[1..]);
    std::process::exit(args.len() as i32);
}

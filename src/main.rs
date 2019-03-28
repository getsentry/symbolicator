fn main() {
    if let Err(e) = symbolicator::app::run_main() {
        println!("{}", e);
        std::process::exit(1);
    }
}

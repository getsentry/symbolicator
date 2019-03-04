use std::process::exit;
use symbolicator;

fn main() {
    if let Err(e) = symbolicator::app::run_main() {
        println!("{}", e);
        exit(1);
    }
}

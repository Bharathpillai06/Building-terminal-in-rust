#[allow(unused_imports)]
use std::io::{self, Write};

fn main() {
    // TODO: Uncomment the code below to pass the first stage
    let mut x = true;
    while x{
    let mut command = String::new();
    print!("$ ");
    io::stdout().flush().unwrap();

    io::stdin().read_line(&mut command).unwrap();
    if command == ""{
        x = false
    }
    else{
    println!("{}: command not found", command.trim());
    }

    }
}

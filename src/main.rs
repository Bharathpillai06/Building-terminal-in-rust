#[allow(unused_imports)]
use std::io::{self, Write};

fn main() {
    // TODO: Uncomment the code below to pass the first stage
    loop     
    {
    let mut command = String::new();
    print!("$ ");
    io::stdout().flush().unwrap();

    io::stdin().read_line(&mut command).unwrap();
    if command.is_empty(){
        continue;
    } 
    if command.trim() == "exit"{
        break;
    }
    if &command.trim_left()[0..4] == "echo"{
        println!("{}",&command[4..]);
        continue;
        
    }
    println!("{}: command not found", command.trim());

    }
}

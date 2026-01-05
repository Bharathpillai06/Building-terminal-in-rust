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
        print!("{}",&command[4..].trim_left());
        continue;
        
    }
    // shows command would be interpreted if it were used
    if &command.trim_left()[0..4] == "type"{

        match command[4..].trim_left(){

            "echo" | "exit" | "type"=> {
                println!("{}: is a shell builtin",  &command[4..].trim_left());
            }
             _ => {
                println!("{}: not found",  &command[4..].trim_left());
             }
        } 
    }
    println!("{}: command not found", command.trim());

    }
}


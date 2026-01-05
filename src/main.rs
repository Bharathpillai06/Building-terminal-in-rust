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
    command.trim_end();

    if command.is_empty(){
        continue;
    } 
    if command.trim() == "exit"{
        break;
    }
    if command.starts_with("echo"){
        println!("{}",&command[4..].trim_left());
        continue;
        
    }
    // shows command would be interpreted if it were used
    if command.starts_with("type"){
        let arg = command[4..].trim_start();
        match arg{

            "echo" | "exit" | "type"=> {
                println!("{}: is a shell builtin",  arg);
            }
             _ => {
                println!("{}: not found",  arg);
             }
        } 
        continue;
    }
    println!("{}: command not found", command.trim());

    }
}


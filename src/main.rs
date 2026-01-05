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
    let line = command.trim_end();

    if line.is_empty(){
        continue;
    } 
    if line.trim() == "exit"{
        break;
    }
    if line.starts_with("echo"){
        print!("{}",&command[4..].trim_start());
        continue;
        
    }
    // shows command would be interpreted if it were used
    if line.starts_with("type"){
        let arg = line[4..].trim_start();
        match arg{

            "echo" | "exit" | "type"=> {
                print!("{} is a shell builtin",  arg);
            }
             _ => {
                print!("{} not found",  arg);
             }
        } 
        continue;
    }
    print!("{}: command not found", line.trim());

    }
}


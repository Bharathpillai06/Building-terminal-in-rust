#[allow(unused_imports)]
use std::io::{self, Write};
use is_executable::IsExecutable;
use std::env;

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
        println!("{}",&line[4..].trim_start());
        continue;
        
    }
    // shows command would be interpreted if it were used and Locate executable files using path
    if line.starts_with("type"){
        let arg = line[4..].trim_start();
        let valid_options = ["exit", "type", "echo"];

        if valid_options.contains(&arg){
           println!("{} is a shell builtin",  arg);
           continue;
        }


        let mut found = false;

        if let Some(paths) = env::var_os("PATH") {
            for dir in env::split_paths(&paths) 
            {
                let candidate = dir.join(arg);

                if candidate.is_file() && candidate.is_executable()
                {
                    println!("{arg} is {}", candidate.display());
                    found = true;
                    break;
                }
            }
        }
        if !found 
        {
        println!("{arg} not found");
        }

        continue;

    }



if let Some(paths) = env::var_os("PATH") {
        let args: Vec<&str> = line.split_whitespace().collect();

        for dir in env::split_paths(&paths) 
        {
            let candidate = dir.join(args[0]);

            if candidate.is_file() && candidate.is_executable()
            {
                println!("Program was passed {} args (including program name)." ,args.len());
                println!("Arg #0 (program name): {} ",candidate.display());
                let mut i =1;
                while i < args.len() -1{
                    println!("Arg #{}: {}",i , args[i]);
                    i = i+1;
                }
                break;
            }
        }
        continue;
    }


    println!("{}: command not found", line.trim());


    }
}


#[allow(unused_imports)]
use std::io::{self, Write};
use std::env;
use std::process::Command;
use is_executable::IsExecutable;

fn main() {
    loop {
        let mut input = String::new();

        print!("$ ");
        io::stdout().flush().unwrap();

        // Exit on EOF
        if io::stdin().read_line(&mut input).unwrap() == 0 {
            break;
        }

        let line = input.trim_end();
        if line.is_empty() {
            continue;
        }

        // Split into tokens: command + args
        let parts: Vec<&str> = line.split_whitespace().collect();
        let cmd = parts[0];
        let args = &parts[1..];

        // Builtins
        if cmd == "exit" {
            break;
        }
        
        if cmd == "pwd"{
            let path = env::current_dir().unwrap();
            println!("{}", path.display());
            continue;

        }

        if cmd == "echo" {
            println!("{}", args.join(" "));
            continue;
        }

        if cmd == "type" {
            let target = args[0];
            let builtins = ["exit", "echo", "type", "pwd"];
            if builtins.contains(&target) {
                println!("{target} is a shell builtin");
                continue;
            }

            if let Some(full_path) = find_executable_in_path(target) {
                println!("{target} is {}", full_path.display());
            } else {
                println!("{target} not found");
            }
            continue;
        }

        // External programs
        if let Some(full_path) = find_executable_in_path(cmd) {
            // Run the program and let it print to stdout/stderr normally
            let status = Command::new(parts[0])
                .args(args)
                .status();

            
        } 
        else {
            println!("{cmd}: command not found");
        }


    }
}



// Searches PATH for an executable named `name`.
// Returns the full path if found + executable.
fn find_executable_in_path(name: &str) -> Option<std::path::PathBuf> {
    let paths = env::var_os("PATH")?;

    for dir in env::split_paths(&paths) {
        let candidate = dir.join(name);
        if candidate.is_file() && candidate.is_executable() {
            return Some(candidate);
        }
    }
    None
}

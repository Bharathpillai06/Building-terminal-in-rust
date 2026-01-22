#[allow(unused_imports)]
use std::io::{self, Write};
use std::env;
use std::process::Command;
use is_executable::IsExecutable;
use std::path::Path;
use std::fs::File;
use std::process::Stdio;

fn main() {
    loop {
        let mut input = String::new();

        print!("$ ");
        io::stdout().flush().unwrap();

        if io::stdin().read_line(&mut input).unwrap() == 0 {
            break;
        }

        let line = input.trim_end();
        if line.is_empty() {
            continue;
        }

        // Split into tokens: command + args (supports quotes + backslash escaping)
        let parts: Vec<String> = {
            let mut args: Vec<String> = Vec::new();
            let mut current = String::new();

            let mut in_single = false;
            let mut in_double = false;
            let mut backslash = false;

            let dq_escapable = ['\\', '"', '$', '`'];

            for ch in line.chars() {
                if backslash {
                    if in_single {
                        current.push('\\');
                        current.push(ch);
                    } else if in_double {
                        if dq_escapable.contains(&ch) {
                            current.push(ch);
                        } else {
                            current.push('\\');
                            current.push(ch);
                        }
                    } else {
                        current.push(ch);
                    }
                    backslash = false;
                    continue;
                }

                if ch == '\\' && !in_single {
                    backslash = true;
                    continue;
                }

                if ch == '\'' && !in_double {
                    in_single = !in_single;
                    continue;
                }
                if ch == '"' && !in_single {
                    in_double = !in_double;
                    continue;
                }

                if !in_single && !in_double && ch.is_whitespace() {
                    if !current.is_empty() {
                        args.push(current);
                        current = String::new();
                    }
                    continue;
                }

                current.push(ch);
            }

            if backslash {
                current.push('\\');
            }

            if !current.is_empty() {
                args.push(current);
            }

            args
        };

        let cmd = parts[0].as_str();

        if cmd == "exit" {
            break;
        }

        // ---------- NEW: parse redirections ----------
        // We treat redirections as shell syntax, not program arguments.
        let mut clean_args: Vec<String> = Vec::new();
        let mut stdout_path: Option<String> = None;
        let mut stderr_path: Option<String> = None;

        let mut i = 1;
        while i < parts.len() {
            match parts[i].as_str() {
                ">" | "1>" => {
                    if i + 1 >= parts.len() {
                        eprintln!("{cmd}: syntax error near unexpected token `newline`");
                        clean_args.clear();
                        break;
                    }
                    stdout_path = Some(parts[i + 1].clone());
                    i += 2;
                }
                "2>" => {
                    if i + 1 >= parts.len() {
                        eprintln!("{cmd}: syntax error near unexpected token `newline`");
                        clean_args.clear();
                        break;
                    }
                    stderr_path = Some(parts[i + 1].clone());
                    i += 2;
                }
                _ => {
                    clean_args.push(parts[i].clone());
                    i += 1;
                }
            }
        }

        if clean_args.is_empty() && (stdout_path.is_some() || stderr_path.is_some()) {
            // malformed redirection handled above
            continue;
        }

        // Create stderr file if requested (even if nothing is written)
        // This matches the tester: `echo hello 2> file` must create/truncate file.
        let mut stderr_file_for_builtins: Option<File> = match &stderr_path {
            Some(p) => match File::create(p) {
                Ok(f) => Some(f),
                Err(e) => {
                    eprintln!("{cmd}: {e}");
                    None
                }
            },
            None => None,
        };

        // Helper: write errors either to redirected stderr file or real stderr
        let mut write_err = |msg: &str| {
            if let Some(f) = stderr_file_for_builtins.as_mut() {
                let _ = f.write_all(msg.as_bytes());
            } else {
                eprint!("{msg}");
            }
        };

        // ---------- builtins ----------
        if cmd == "pwd" {
            match env::current_dir() {
                Ok(path) => println!("{}", path.display()),
                Err(e) => write_err(&format!("pwd: {e}\n")),
            }
            continue;
        }

        if cmd == "echo" {
            println!("{}", clean_args.join(" "));
            continue;
        }

        if cmd == "type" {
            if clean_args.is_empty() {
                write_err("type: missing operand\n");
                continue;
            }
            let target = clean_args[0].as_str();
            let builtins = ["exit", "echo", "type", "pwd", "cd"];
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

        if cmd == "cd" {
            if clean_args.is_empty() {
                continue;
            }

            let dest = clean_args[0].as_str();

            let target_path = if dest == "~" {
                match env::home_dir() {
                    Some(home) => home,
                    None => {
                        write_err("cd: ~: No such file or directory\n");
                        continue;
                    }
                }
            } else {
                Path::new(dest).to_path_buf()
            };

            if env::set_current_dir(&target_path).is_err() {
                write_err(&format!("cd: {}: No such file or directory\n", dest));
            }
            continue;
        }

        // ---------- external programs ----------
        if find_executable_in_path(cmd).is_some() {
            let mut command = Command::new(cmd);
            command.args(clean_args.iter());

            // stdout redirect (existing feature)
            if let Some(p) = &stdout_path {
                match File::create(p) {
                    Ok(f) => {
                        command.stdout(Stdio::from(f));
                    }
                    Err(e) => {
                        write_err(&format!("{cmd}: {e}\n"));
                        continue;
                    }
                }
            }

            // stderr redirect (NEW feature)
            if let Some(p) = &stderr_path {
                match File::create(p) {
                    Ok(f) => {
                        command.stderr(Stdio::from(f));
                    }
                    Err(e) => {
                        // if we can't open the stderr file, error goes to real stderr (or builtin redirect)
                        write_err(&format!("{cmd}: {e}\n"));
                        continue;
                    }
                }
            }

            if let Err(e) = command.status() {
                // this is an exec/spawn error from *your* shell, so respect 2> too
                write_err(&format!("{cmd}: {e}\n"));
            }
        } else {
            println!("{cmd}: command not found");
        }
    }
}

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

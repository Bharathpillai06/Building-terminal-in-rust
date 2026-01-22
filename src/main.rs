#[allow(unused_imports)]
use std::io::{self, Write};
use std::env;
use std::process::{Command, Stdio};
use is_executable::IsExecutable;
use std::path::Path;
use std::fs::{File, OpenOptions};

#[derive(Debug, Clone)]
enum StdoutRedirect {
    Inherit,
    Truncate(String), // > or 1>
    Append(String),   // >> or 1>>
}

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

        // ---------- tokenization (quotes + backslash escaping) ----------
        let parts: Vec<String> = {
            let mut args = Vec::new();
            let mut current = String::new();

            let mut in_single = false;
            let mut in_double = false;
            let mut backslash = false;

            // In double quotes, backslash only escapes: \ " $ `
            let dq_escapable = ['\\', '"', '$', '`'];

            for ch in line.chars() {
                if backslash {
                    if in_single {
                        // Backslash is literal inside single quotes
                        current.push('\\');
                        current.push(ch);
                    } else if in_double {
                        // In double quotes, backslash only escapes specific chars
                        if dq_escapable.contains(&ch) {
                            current.push(ch);
                        } else {
                            current.push('\\');
                            current.push(ch);
                        }
                    } else {
                        // Outside quotes, backslash escapes next char
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

        // ---------- parse redirections ----------
        let mut clean_args: Vec<String> = Vec::new();
        let mut stdout_redirect = StdoutRedirect::Inherit;
        let mut stderr_path: Option<String> = None;

        let mut i = 1;
        let mut syntax_error = false;

        while i < parts.len() {
            match parts[i].as_str() {
                ">" | "1>" => {
                    if i + 1 >= parts.len() {
                        eprintln!("{cmd}: syntax error near unexpected token `newline`");
                        syntax_error = true;
                        break;
                    }
                    stdout_redirect = StdoutRedirect::Truncate(parts[i + 1].clone());
                    i += 2;
                }
                ">>" | "1>>" => {
                    if i + 1 >= parts.len() {
                        eprintln!("{cmd}: syntax error near unexpected token `newline`");
                        syntax_error = true;
                        break;
                    }
                    stdout_redirect = StdoutRedirect::Append(parts[i + 1].clone());
                    i += 2;
                }
                "2>" => {
                    if i + 1 >= parts.len() {
                        eprintln!("{cmd}: syntax error near unexpected token `newline`");
                        syntax_error = true;
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

        if syntax_error {
            continue;
        }

        // Create stderr file early if requested (even if unused), matching tester behavior.
        let mut stderr_file: Option<File> = match &stderr_path {
            Some(p) => match File::create(p) {
                Ok(f) => Some(f),
                Err(e) => {
                    eprintln!("{cmd}: {e}");
                    None
                }
            },
            None => None,
        };

        let mut write_err = |msg: &str| {
            if let Some(f) = stderr_file.as_mut() {
                let _ = f.write_all(msg.as_bytes());
            } else {
                eprint!("{msg}");
            }
        };

        // ---------- builtins ----------
        if cmd == "pwd" {
            match env::current_dir() {
                Ok(p) => println!("{}", p.display()),
                Err(e) => write_err(&format!("pwd: {e}\n")),
            }
            continue;
        }

        if cmd == "echo" {
            let out = clean_args.join(" ");
            match &stdout_redirect {
                StdoutRedirect::Inherit => {
                    println!("{out}");
                }
                StdoutRedirect::Truncate(path) => {
                    if let Err(e) = std::fs::write(path, format!("{out}\n")) {
                        write_err(&format!("echo: {e}\n"));
                    }
                }
                StdoutRedirect::Append(path) => {
                    let mut f = match OpenOptions::new().create(true).append(true).open(path) {
                        Ok(f) => f,
                        Err(e) => {
                            write_err(&format!("echo: {e}\n"));
                            continue;
                        }
                    };
                    if let Err(e) = writeln!(f, "{out}") {
                        write_err(&format!("echo: {e}\n"));
                    }
                }
            }
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
            } else if let Some(p) = find_executable_in_path(target) {
                println!("{target} is {}", p.display());
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
            let target = if dest == "~" {
                match env::home_dir() {
                    Some(h) => h,
                    None => {
                        write_err("cd: ~: No such file or directory\n");
                        continue;
                    }
                }
            } else {
                Path::new(dest).to_path_buf()
            };

            if env::set_current_dir(&target).is_err() {
                write_err(&format!("cd: {}: No such file or directory\n", dest));
            }
            continue;
        }

        // ---------- external commands ----------
        if find_executable_in_path(cmd).is_some() {
            let mut command = Command::new(cmd);
            command.args(clean_args.iter());

            // stdout redirect
            match &stdout_redirect {
                StdoutRedirect::Inherit => {}
                StdoutRedirect::Truncate(path) => {
                    match File::create(path) {
                        Ok(f) => {
                            command.stdout(Stdio::from(f));
                        }
                        Err(e) => {
                            write_err(&format!("{cmd}: {e}\n"));
                            continue;
                        }
                    }
                }
                StdoutRedirect::Append(path) => {
                    match OpenOptions::new().create(true).append(true).open(path) {
                        Ok(f) => {
                            command.stdout(Stdio::from(f));
                        }
                        Err(e) => {
                            write_err(&format!("{cmd}: {e}\n"));
                            continue;
                        }
                    }
                }
            }

            // stderr redirect
            if let Some(path) = &stderr_path {
                match File::create(path) {
                    Ok(f) => {
                        command.stderr(Stdio::from(f));
                    }
                    Err(e) => {
                        write_err(&format!("{cmd}: {e}\n"));
                        continue;
                    }
                }
            }

            if let Err(e) = command.status() {
                // spawn/exec error from your shell: respect 2> too
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

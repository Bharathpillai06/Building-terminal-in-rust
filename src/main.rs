#[allow(unused_imports)]
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use is_executable::IsExecutable;

// ---------- rustyline ----------
use rustyline::completion::{Completer, Pair};
use rustyline::error::ReadlineError;
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::history::DefaultHistory;
use rustyline::validate::Validator;
use rustyline::{Context, Editor, Helper};

#[derive(Debug, Clone)]
enum StdoutRedirect {
    Inherit,
    Truncate(String), // > or 1>
    Append(String),   // >> or 1>>
}

#[derive(Debug, Clone)]
enum StderrRedirect {
    Inherit,
    Truncate(String), // 2>
    Append(String),   // 2>>
}

#[derive(Clone, Debug)]
struct ShellHelper {
    // For the "double-tab shows list" behavior
    last_prefix: Option<String>,
    last_was_tab_no_completion: bool,
    tab_count_for_prefix: u8,
}

impl ShellHelper {
    fn new() -> Self {
        Self {
            last_prefix: None,
            last_was_tab_no_completion: false,
            tab_count_for_prefix: 0,
        }
    }
}

impl Helper for ShellHelper {}

impl Hinter for ShellHelper {
    type Hint = String;
    fn hint(&self, _line: &str, _pos: usize, _ctx: &Context<'_>) -> Option<Self::Hint> {
        None
    }
}

impl Highlighter for ShellHelper {}
impl Validator for ShellHelper {}

impl Completer for ShellHelper {
    type Candidate = Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &Context<'_>,
    ) -> rustyline::Result<(usize, Vec<Pair>)> {
        // Only complete first token (command position)
        let start = line[..pos]
            .rfind(|c: char| c.is_whitespace())
            .map(|i| i + 1)
            .unwrap_or(0);

        if start != 0 {
            return Ok((pos, vec![]));
        }

        let prefix = &line[start..pos];
        if prefix.is_empty() {
            return Ok((pos, vec![]));
        }

        let mut matches: Vec<String> = Vec::new();

        // Builtins we support
        let builtins = ["echo", "exit", "type", "pwd", "cd"];
        for b in builtins {
            if b.starts_with(prefix) {
                matches.push(b.to_string());
            }
        }

        // Executables in PATH
        for exe in executables_in_path_starting_with(prefix) {
            matches.push(exe);
        }

        matches.sort();
        matches.dedup();

        // If exactly one match, complete to it with a trailing space
        if matches.len() == 1 {
            let m = &matches[0];
            return Ok((
                start,
                vec![Pair {
                    display: m.clone(),
                    replacement: format!("{m} "),
                }],
            ));
        }

        // If multiple matches, return them as completion candidates.
        // We'll override display behavior in the readline loop (two-tab behavior).
        let pairs: Vec<Pair> = matches
            .into_iter()
            .map(|m| Pair {
                display: m.clone(),
                replacement: m,
            })
            .collect();

        Ok((start, pairs))
    }
}

// ---- helper: list matching executables in PATH (by filename) ----
fn executables_in_path_starting_with(prefix: &str) -> Vec<String> {
    let mut out = Vec::new();
    let paths = match env::var_os("PATH") {
        Some(p) => p,
        None => return out,
    };

    for dir in env::split_paths(&paths) {
        // PATH can include non-existent dirs, ignore errors
        let Ok(entries) = fs::read_dir(&dir) else { continue };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if !path.is_executable() {
                continue;
            }

            if let Some(name_os) = path.file_name() {
                let name = name_os.to_string_lossy().to_string();
                if name.starts_with(prefix) {
                    out.push(name);
                }
            }
        }
    }

    out
}

// ---- same executable finder you already use for running commands ----
fn find_executable_in_path(name: &str) -> Option<PathBuf> {
    let paths = env::var_os("PATH")?;
    for dir in env::split_paths(&paths) {
        let candidate = dir.join(name);
        if candidate.is_file() && candidate.is_executable() {
            return Some(candidate);
        }
    }
    None
}

fn main() {
    let mut rl: Editor<ShellHelper, DefaultHistory> = Editor::new().unwrap();
    rl.set_helper(Some(ShellHelper::new()));

    loop {
        // Read input line
        let line = match rl.readline("$ ") {
            Ok(l) => l,
            Err(ReadlineError::Interrupted) => continue, // Ctrl-C
            Err(ReadlineError::Eof) => break,            // Ctrl-D
            Err(e) => {
                eprintln!("readline error: {e}");
                break;
            }
        };

        // Save in history (harmless)
        let _ = rl.add_history_entry(line.as_str());

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

        if parts.is_empty() {
            continue;
        }

        let cmd = parts[0].as_str();

        // ---------- parse redirections ----------
        let mut clean_args: Vec<String> = Vec::new();
        let mut stdout_redirect = StdoutRedirect::Inherit;
        let mut stderr_redirect = StderrRedirect::Inherit;

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
                    stderr_redirect = StderrRedirect::Truncate(parts[i + 1].clone());
                    i += 2;
                }
                "2>>" => {
                    if i + 1 >= parts.len() {
                        eprintln!("{cmd}: syntax error near unexpected token `newline`");
                        syntax_error = true;
                        break;
                    }
                    stderr_redirect = StderrRedirect::Append(parts[i + 1].clone());
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

        // Create/open stderr file early if requested (even if unused)
        let mut stderr_file: Option<File> = match &stderr_redirect {
            StderrRedirect::Inherit => None,
            StderrRedirect::Truncate(path) => File::create(path).ok(),
            StderrRedirect::Append(path) => OpenOptions::new().create(true).append(true).open(path).ok(),
        };

        let mut write_err = |msg: &str| {
            if let Some(f) = stderr_file.as_mut() {
                let _ = f.write_all(msg.as_bytes());
            } else {
                eprint!("{msg}");
            }
        };

        // ---------- builtins ----------
        if cmd == "exit" {
            break;
        }

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
                StdoutRedirect::Inherit => println!("{out}"),
                StdoutRedirect::Truncate(path) => {
                    if let Err(e) = fs::write(path, format!("{out}\n")) {
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
                StdoutRedirect::Truncate(path) => match File::create(path) {
                    Ok(f) => {
                        command.stdout(Stdio::from(f));
                    }
                    Err(e) => {
                        write_err(&format!("{cmd}: {e}\n"));
                        continue;
                    }
                },
                StdoutRedirect::Append(path) => match OpenOptions::new().create(true).append(true).open(path) {
                    Ok(f) => {
                        command.stdout(Stdio::from(f));
                    }
                    Err(e) => {
                        write_err(&format!("{cmd}: {e}\n"));
                        continue;
                    }
                },
            }

            // stderr redirect
            match &stderr_redirect {
                StderrRedirect::Inherit => {}
                StderrRedirect::Truncate(path) => match File::create(path) {
                    Ok(f) => {
                        command.stderr(Stdio::from(f));
                    }
                    Err(e) => {
                        write_err(&format!("{cmd}: {e}\n"));
                        continue;
                    }
                },
                StderrRedirect::Append(path) => match OpenOptions::new().create(true).append(true).open(path) {
                    Ok(f) => {
                        command.stderr(Stdio::from(f));
                    }
                    Err(e) => {
                        write_err(&format!("{cmd}: {e}\n"));
                        continue;
                    }
                },
            }

            if let Err(e) = command.status() {
                write_err(&format!("{cmd}: {e}\n"));
            }
        } else {
            println!("{cmd}: command not found");
        }
    }
}

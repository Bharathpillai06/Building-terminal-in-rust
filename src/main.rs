#[allow(unused_imports)]
use std::cell::RefCell;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use is_executable::IsExecutable;

// ---------- rustyline ----------
use rustyline::completion::{Completer, Pair};
use rustyline::config::{CompletionType, Config};
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

// State needed for "TAB then TAB" behavior
#[derive(Debug)]
struct CompletionState {
    last_prefix: Option<String>,
    armed_for_list: bool, // after first TAB with multiple matches
}

struct ShellHelper {
    state: RefCell<CompletionState>,
}

impl ShellHelper {
    fn new() -> Self {
        Self {
            state: RefCell::new(CompletionState {
                last_prefix: None,
                armed_for_list: false,
            }),
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
            // For this stage, we only care about completing the command
            return Ok((pos, vec![]));
        }

        let prefix = &line[start..pos];
        if prefix.is_empty() {
            return Ok((pos, vec![]));
        }

        // Collect builtin + executable matches
        let mut matches: Vec<String> = Vec::new();

        let builtins = ["echo", "exit", "type", "pwd", "cd"];
        for b in builtins {
            if b.starts_with(prefix) {
                matches.push(b.to_string());
            }
        }

        for exe in executables_in_path_starting_with(prefix) {
            matches.push(exe);
        }

        matches.sort();
        matches.dedup();

        // No matches -> let rustyline ring bell (missing completion stage)
        if matches.is_empty() {
            // reset multi state
            let mut st = self.state.borrow_mut();
            st.last_prefix = None;
            st.armed_for_list = false;
            return Ok((pos, vec![]));
        }

        // Exactly one match -> complete + trailing space
        if matches.len() == 1 {
            let mut st = self.state.borrow_mut();
            st.last_prefix = None;
            st.armed_for_list = false;

            let m = &matches[0];
            return Ok((
                start,
                vec![Pair {
                    display: m.clone(),
                    replacement: format!("{m} "),
                }],
            ));
        }

        // Multiple matches -> implement:
        // 1st TAB: bell only (return no candidates)
        // 2nd TAB: show list (return candidates)
        let mut st = self.state.borrow_mut();
        let prefix_s = prefix.to_string();

        if st.last_prefix.as_deref() == Some(prefix) && st.armed_for_list {
            // SECOND TAB for same prefix -> return candidates so rustyline prints them
            st.armed_for_list = false;

            let pairs: Vec<Pair> = matches
                .into_iter()
                .map(|m| Pair {
                    display: m.clone(),
                    replacement: m, // don't change buffer; listing is what we want
                })
                .collect();

            return Ok((start, pairs));
        } else {
            // FIRST TAB for this prefix -> arm and return no candidates => bell
            st.last_prefix = Some(prefix_s);
            st.armed_for_list = true;
            return Ok((pos, vec![]));
        }
    }
}

// List matching executables in PATH (by filename)
fn executables_in_path_starting_with(prefix: &str) -> Vec<String> {
    let mut out = Vec::new();
    let paths = match env::var_os("PATH") {
        Some(p) => p,
        None => return out,
    };

    for dir in env::split_paths(&paths) {
        let Ok(entries) = fs::read_dir(&dir) else { continue };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() || !path.is_executable() {
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
    // Key part for #wh6:
    // - CompletionType::List ensures listing behavior
    // - show_all_if_ambiguous(true) makes rustyline print the list immediately
    //   when we return multiple candidates (we do that on SECOND TAB only).
    let config = Config::builder()
        .completion_type(CompletionType::List)
        .show_all_if_ambiguous(true)
        .build();

    let mut rl: Editor<ShellHelper, DefaultHistory> = Editor::with_config(config).unwrap();
    rl.set_helper(Some(ShellHelper::new()));

    loop {
        let line = match rl.readline("$ ") {
            Ok(l) => l,
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Eof) => break,
            Err(e) => {
                eprintln!("readline error: {e}");
                break;
            }
        };

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

            // In double quotes, backslash only escapes: \ " $ `
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

        // Create/open stderr file early if requested
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

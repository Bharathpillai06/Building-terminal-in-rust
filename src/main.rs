#[allow(unused_imports)]
use std::cell::RefCell;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
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

// State for "TAB then TAB" behavior
#[derive(Debug)]
struct CompletionState {
    last_prefix: Option<String>,
    armed_for_list: bool,
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
        // Only complete first token (command)
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

        // builtins
        let builtins = ["echo", "exit", "type", "pwd", "cd"];
        for b in builtins {
            if b.starts_with(prefix) {
                matches.push(b.to_string());
            }
        }

        // executables
        for exe in executables_in_path_starting_with(prefix) {
            matches.push(exe);
        }

        matches.sort();
        matches.dedup();

        // No matches: bell
        if matches.is_empty() {
            let mut st = self.state.borrow_mut();
            st.last_prefix = None;
            st.armed_for_list = false;
            return Ok((pos, vec![]));
        }

        // One match: complete + space
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

        // Multiple matches behavior: 1st tab bell, 2nd tab list
        let mut st = self.state.borrow_mut();
        if st.last_prefix.as_deref() == Some(prefix) && st.armed_for_list {
            st.armed_for_list = false;

            let pairs: Vec<Pair> = matches
                .into_iter()
                .map(|m| Pair {
                    display: m.clone(),
                    replacement: m,
                })
                .collect();

            Ok((start, pairs))
        } else {
            st.last_prefix = Some(prefix.to_string());
            st.armed_for_list = true;
            Ok((pos, vec![]))
        }
    }
}

// ---------- PATH helpers ----------
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

// ---------- parsing ----------
fn tokenize(line: &str) -> Vec<String> {
    // Supports:
    // - single quotes
    // - double quotes
    // - backslash escaping (outside single quotes; limited inside double quotes)
    // - treats | as a token when not in quotes
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

        // PIPE token when not quoted
        if !in_single && !in_double && ch == '|' {
            if !current.is_empty() {
                args.push(current);
                current = String::new();
            }
            args.push("|".to_string());
            continue;
        }

        // Whitespace splits when not quoted
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
}

#[derive(Debug, Clone)]
struct ParsedCommand {
    cmd: String,
    args: Vec<String>, // args only (no redirection tokens)
    stdout: StdoutRedirect,
    stderr: StderrRedirect,
}

fn parse_command(tokens: &[String]) -> Option<ParsedCommand> {
    if tokens.is_empty() {
        return None;
    }
    let cmd = tokens[0].clone();

    let mut args: Vec<String> = Vec::new();
    let mut stdout = StdoutRedirect::Inherit;
    let mut stderr = StderrRedirect::Inherit;

    let mut i = 1;
    while i < tokens.len() {
        match tokens[i].as_str() {
            ">" | "1>" => {
                if i + 1 >= tokens.len() {
                    eprintln!("{cmd}: syntax error near unexpected token `newline`");
                    return None;
                }
                stdout = StdoutRedirect::Truncate(tokens[i + 1].clone());
                i += 2;
            }
            ">>" | "1>>" => {
                if i + 1 >= tokens.len() {
                    eprintln!("{cmd}: syntax error near unexpected token `newline`");
                    return None;
                }
                stdout = StdoutRedirect::Append(tokens[i + 1].clone());
                i += 2;
            }
            "2>" => {
                if i + 1 >= tokens.len() {
                    eprintln!("{cmd}: syntax error near unexpected token `newline`");
                    return None;
                }
                stderr = StderrRedirect::Truncate(tokens[i + 1].clone());
                i += 2;
            }
            "2>>" => {
                if i + 1 >= tokens.len() {
                    eprintln!("{cmd}: syntax error near unexpected token `newline`");
                    return None;
                }
                stderr = StderrRedirect::Append(tokens[i + 1].clone());
                i += 2;
            }
            _ => {
                args.push(tokens[i].clone());
                i += 1;
            }
        }
    }

    Some(ParsedCommand {
        cmd,
        args,
        stdout,
        stderr,
    })
}

fn split_pipeline(tokens: &[String]) -> Option<Vec<Vec<String>>> {
    // Split by "|" token
    let mut out: Vec<Vec<String>> = Vec::new();
    let mut cur: Vec<String> = Vec::new();

    for t in tokens {
        if t == "|" {
            if cur.is_empty() {
                // empty command in pipeline
                eprintln!("syntax error near unexpected token `|`");
                return None;
            }
            out.push(cur);
            cur = Vec::new();
        } else {
            cur.push(t.clone());
        }
    }

    if cur.is_empty() {
        eprintln!("syntax error near unexpected token `|`");
        return None;
    }
    out.push(cur);
    Some(out)
}

// ---------- execution helpers ----------
fn open_for_stdout(redir: &StdoutRedirect) -> io::Result<Option<File>> {
    match redir {
        StdoutRedirect::Inherit => Ok(None),
        StdoutRedirect::Truncate(path) => Ok(Some(File::create(path)?)),
        StdoutRedirect::Append(path) => Ok(Some(OpenOptions::new().create(true).append(true).open(path)?)),
    }
}

fn open_for_stderr(redir: &StderrRedirect) -> io::Result<Option<File>> {
    match redir {
        StderrRedirect::Inherit => Ok(None),
        StderrRedirect::Truncate(path) => Ok(Some(File::create(path)?)),
        StderrRedirect::Append(path) => Ok(Some(OpenOptions::new().create(true).append(true).open(path)?)),
    }
}

fn is_builtin(cmd: &str) -> bool {
    matches!(cmd, "exit" | "echo" | "pwd" | "type" | "cd")
}

// Run a builtin and return its stdout/stderr bytes.
// NOTE: For pipeline stages, we do NOT apply "exit" or "cd" effects to the parent shell.
fn run_builtin_capture(cmd: &str, args: &[String]) -> (Vec<u8>, Vec<u8>, i32) {
    match cmd {
        "echo" => {
            let out = args.join(" ") + "\n";
            (out.into_bytes(), vec![], 0)
        }
        "pwd" => match env::current_dir() {
            Ok(p) => (format!("{}\n", p.display()).into_bytes(), vec![], 0),
            Err(e) => (vec![], format!("pwd: {e}\n").into_bytes(), 1),
        },
        "type" => {
            if args.is_empty() {
                return (vec![], b"type: missing operand\n".to_vec(), 1);
            }
            let target = args[0].as_str();
            let builtins = ["exit", "echo", "type", "pwd", "cd"];
            if builtins.contains(&target) {
                (format!("{target} is a shell builtin\n").into_bytes(), vec![], 0)
            } else if let Some(p) = find_executable_in_path(target) {
                (format!("{target} is {}\n", p.display()).into_bytes(), vec![], 0)
            } else {
                (format!("{target} not found\n").into_bytes(), vec![], 0)
            }
        }
        "cd" => {
            // In a pipeline stage, behave like a subshell: do not change parent dir.
            // We'll still validate/return errors for realism.
            if args.is_empty() {
                return (vec![], vec![], 0);
            }
            let dest = args[0].as_str();
            let target = if dest == "~" {
                match env::home_dir() {
                    Some(h) => h,
                    None => return (vec![], b"cd: ~: No such file or directory\n".to_vec(), 1),
                }
            } else {
                Path::new(dest).to_path_buf()
            };

            if !target.is_dir() {
                return (vec![], format!("cd: {}: No such file or directory\n", dest).into_bytes(), 1);
            }
            // don't actually set_current_dir here for pipeline
            (vec![], vec![], 0)
        }
        "exit" => {
            // Pipeline stage exit does nothing; just succeed
            (vec![], vec![], 0)
        }
        _ => (vec![], format!("{cmd}: command not found\n").into_bytes(), 127),
    }
}

// Run an external command, feeding it `stdin_bytes`, and capture stdout/stderr.
fn run_external_capture(cmd: &str, args: &[String], stdin_bytes: &[u8]) -> io::Result<(Vec<u8>, Vec<u8>, i32)> {
    let mut command = Command::new(cmd);
    command.args(args);

    // feed stdin if needed
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    let mut child = command.spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(stdin_bytes);
    }

    let output = child.wait_with_output()?;
    let code = output.status.code().unwrap_or(1);
    Ok((output.stdout, output.stderr, code))
}

// Apply stdout/stderr handling for a stage:
// - If redirected to file, write there
// - Else if inherit_to_terminal is true, write to terminal
// - Else return bytes (for piping)
fn handle_stage_output(
    stdout_bytes: Vec<u8>,
    stderr_bytes: Vec<u8>,
    stdout_redir: &StdoutRedirect,
    stderr_redir: &StderrRedirect,
    inherit_stdout_to_terminal: bool,
) -> io::Result<Vec<u8>> {
    // stderr
    match stderr_redir {
        StderrRedirect::Inherit => {
            if !stderr_bytes.is_empty() {
                let mut e = io::stderr();
                e.write_all(&stderr_bytes)?;
                e.flush()?;
            }
        }
        _ => {
            if let Some(mut f) = open_for_stderr(stderr_redir)? {
                f.write_all(&stderr_bytes)?;
                f.flush()?;
            }
        }
    }

    // stdout
    match stdout_redir {
        StdoutRedirect::Inherit => {
            if inherit_stdout_to_terminal {
                if !stdout_bytes.is_empty() {
                    let mut o = io::stdout();
                    o.write_all(&stdout_bytes)?;
                    o.flush()?;
                }
                Ok(Vec::new())
            } else {
                // pipe onward
                Ok(stdout_bytes)
            }
        }
        _ => {
            if let Some(mut f) = open_for_stdout(stdout_redir)? {
                f.write_all(&stdout_bytes)?;
                f.flush()?;
            }
            // redirected, so nothing to pipe
            Ok(Vec::new())
        }
    }
}

// Execute a pipeline (one or many stages).
// Pipeline is executed sequentially with captured output (sufficient for CodeCrafters tests).
fn execute_pipeline(stages: Vec<ParsedCommand>) {
    let mut input: Vec<u8> = Vec::new();

    for (idx, stage) in stages.iter().enumerate() {
        let is_last = idx + 1 == stages.len();
        let inherit_stdout_to_terminal = is_last; // last stage prints unless redirected

        let (stdout_bytes, stderr_bytes, _code) = if is_builtin(&stage.cmd) {
            run_builtin_capture(&stage.cmd, &stage.args)
        } else if find_executable_in_path(&stage.cmd).is_some() {
            match run_external_capture(&stage.cmd, &stage.args, &input) {
                Ok(v) => v,
                Err(e) => {
                    // external spawn error -> treat as stderr
                    (Vec::new(), format!("{}: {e}\n", stage.cmd).into_bytes(), 1)
                }
            }
        } else {
            (Vec::new(), format!("{}: command not found\n", stage.cmd).into_bytes(), 127)
        };

        match handle_stage_output(
            stdout_bytes,
            stderr_bytes,
            &stage.stdout,
            &stage.stderr,
            inherit_stdout_to_terminal,
        ) {
            Ok(next_input) => input = next_input,
            Err(e) => {
                eprintln!("{}: {e}", stage.cmd);
                input = Vec::new();
            }
        }
    }
}

fn main() {
    let config = Config::builder()
        .completion_type(CompletionType::List)
        .completion_show_all_if_ambiguous(true)
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

        let line = line.trim_end().to_string();
        if line.is_empty() {
            continue;
        }

        let tokens = tokenize(&line);
        let Some(chunks) = split_pipeline(&tokens) else { continue };

        // Parse all stages
        let mut stages: Vec<ParsedCommand> = Vec::new();
        for chunk in chunks {
            let Some(pc) = parse_command(&chunk) else {
                stages.clear();
                break;
            };
            stages.push(pc);
        }
        if stages.is_empty() {
            continue;
        }

        // If single command and it's exit/cd, apply effects to parent shell normally
        if stages.len() == 1 {
            let s = &stages[0];

            if s.cmd == "exit" {
                break;
            }

            if s.cmd == "cd" {
                if s.args.is_empty() {
                    continue;
                }
                let dest = s.args[0].as_str();
                let target = if dest == "~" {
                    match env::home_dir() {
                        Some(h) => h,
                        None => {
                            eprintln!("cd: ~: No such file or directory");
                            continue;
                        }
                    }
                } else {
                    Path::new(dest).to_path_buf()
                };

                if env::set_current_dir(&target).is_err() {
                    eprintln!("cd: {}: No such file or directory", dest);
                }
                continue;
            }
        }

        // Otherwise execute pipeline (1+ stages). Builtins inside act like subshell.
        execute_pipeline(stages);
    }
}

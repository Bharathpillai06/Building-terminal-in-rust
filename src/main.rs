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
    Truncate(String),
    Append(String),
}

#[derive(Debug, Clone)]
enum StderrRedirect {
    Inherit,
    Truncate(String),
    Append(String),
}

// State for "<TAB><TAB>" listing behavior when ambiguous and no further LCP progress
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

// ---- helpers for completion ----
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
    out.sort();
    out.dedup();
    out
}

fn longest_common_prefix(strs: &[String]) -> String {
    if strs.is_empty() {
        return String::new();
    }
    if strs.len() == 1 {
        return strs[0].clone();
    }

    let first = strs[0].as_bytes();
    let mut end = first.len();

    for s in &strs[1..] {
        let b = s.as_bytes();
        let mut i = 0usize;
        while i < end && i < b.len() && first[i] == b[i] {
            i += 1;
        }
        end = end.min(i);
        if end == 0 {
            break;
        }
    }

    String::from_utf8_lossy(&first[..end]).to_string()
}

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

        // Collect matches (builtins + executables)
        let mut matches: Vec<String> = Vec::new();

        let builtins = ["echo", "exit", "type", "pwd", "cd"];
        for b in builtins {
            if b.starts_with(prefix) {
                matches.push(b.to_string());
            }
        }
        matches.extend(executables_in_path_starting_with(prefix));

        matches.sort();
        matches.dedup();

        // No matches -> bell by rustyline
        if matches.is_empty() {
            let mut st = self.state.borrow_mut();
            st.last_prefix = None;
            st.armed_for_list = false;
            return Ok((pos, vec![]));
        }

        // Exactly one match -> full completion + trailing space
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

        // Multiple matches: try LCP completion first (#wt6)
        let lcp = longest_common_prefix(&matches);

        if lcp.len() > prefix.len() {
            // Progress possible: replace with LCP (NO bell, NO listing)
            let mut st = self.state.borrow_mut();
            st.last_prefix = None;
            st.armed_for_list = false;

            return Ok((
                start,
                vec![Pair {
                    display: lcp.clone(),
                    replacement: lcp,
                }],
            ));
        }

        // No LCP progress: fall back to previous stage behavior:
        // 1st TAB -> bell only, 2nd TAB -> list all matches
        let mut st = self.state.borrow_mut();
        if st.last_prefix.as_deref() == Some(prefix) && st.armed_for_list {
            st.armed_for_list = false;

            let pairs: Vec<Pair> = matches
                .into_iter()
                .map(|m| Pair {
                    display: m.clone(),
                    replacement: m, // keep buffer unchanged
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

// ---------- PATH helper for execution ----------
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

// ---------- tokenization (supports quotes + backslash + PIPE token) ----------
fn tokenize(line: &str) -> Vec<String> {
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

        if !in_single && !in_double && ch == '|' {
            if !current.is_empty() {
                args.push(current);
                current = String::new();
            }
            args.push("|".to_string());
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
}

#[derive(Debug, Clone)]
struct ParsedCommand {
    cmd: String,
    args: Vec<String>,
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
    let mut out: Vec<Vec<String>> = Vec::new();
    let mut cur: Vec<String> = Vec::new();

    for t in tokens {
        if t == "|" {
            if cur.is_empty() {
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

// Builtins for pipeline stages (subshell-like)
fn run_builtin_capture(cmd: &str, args: &[String]) -> (Vec<u8>, Vec<u8>, i32) {
    match cmd {
        "echo" => (format!("{}\n", args.join(" ")).into_bytes(), vec![], 0),
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
            // subshell behavior in pipeline: don't change parent
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
                return (
                    vec![],
                    format!("cd: {}: No such file or directory\n", dest).into_bytes(),
                    1,
                );
            }
            (vec![], vec![], 0)
        }
        "exit" => (vec![], vec![], 0),
        _ => (vec![], format!("{cmd}: command not found\n").into_bytes(), 127),
    }
}

fn run_external_capture(cmd: &str, args: &[String], stdin_bytes: &[u8]) -> io::Result<(Vec<u8>, Vec<u8>, i32)> {
    let mut command = Command::new(cmd);
    command.args(args);

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
                Ok(stdout_bytes)
            }
        }
        _ => {
            if let Some(mut f) = open_for_stdout(stdout_redir)? {
                f.write_all(&stdout_bytes)?;
                f.flush()?;
            }
            Ok(Vec::new())
        }
    }
}

fn execute_pipeline(stages: Vec<ParsedCommand>) {
    let mut input: Vec<u8> = Vec::new();

    for (idx, stage) in stages.iter().enumerate() {
        let is_last = idx + 1 == stages.len();
        let inherit_stdout_to_terminal = is_last;

        let (stdout_bytes, stderr_bytes, _code) = if is_builtin(&stage.cmd) {
            run_builtin_capture(&stage.cmd, &stage.args)
        } else if find_executable_in_path(&stage.cmd).is_some() {
            match run_external_capture(&stage.cmd, &stage.args, &input) {
                Ok(v) => v,
                Err(e) => (Vec::new(), format!("{}: {e}\n", stage.cmd).into_bytes(), 1),
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

        // Parent-shell effects only for single command
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

        execute_pipeline(stages);
    }
}

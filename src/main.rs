#[allow(unused_imports)]
use std::cell::RefCell;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

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

        // Multiple matches: try LCP completion first
        let lcp = longest_common_prefix(&matches);

        if lcp.len() > prefix.len() {
            // Progress possible: replace with LCP
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

        // No LCP progress:
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
        StdoutRedirect::Append(path) => Ok(Some(
            OpenOptions::new().create(true).append(true).open(path)?,
        )),
    }
}

fn open_for_stderr(redir: &StderrRedirect) -> io::Result<Option<File>> {
    match redir {
        StderrRedirect::Inherit => Ok(None),
        StderrRedirect::Truncate(path) => Ok(Some(File::create(path)?)),
        StderrRedirect::Append(path) => Ok(Some(
            OpenOptions::new().create(true).append(true).open(path)?,
        )),
    }
}

fn is_builtin(cmd: &str) -> bool {
    matches!(cmd, "exit" | "echo" | "pwd" | "type" | "cd")
}

// ---------- single-command execution ----------
fn run_builtin_in_parent(cmd: &str, args: &[String]) {
    match cmd {
        "echo" => {
            println!("{}", args.join(" "));
        }
        "pwd" => match env::current_dir() {
            Ok(p) => println!("{}", p.display()),
            Err(e) => eprintln!("pwd: {e}"),
        },
        "type" => {
            if args.is_empty() {
                eprintln!("type: missing operand");
                return;
            }
            let target = args[0].as_str();
            let builtins = ["exit", "echo", "type", "pwd", "cd"];
            if builtins.contains(&target) {
                println!("{target} is a shell builtin");
            } else if let Some(p) = find_executable_in_path(target) {
                println!("{target} is {}", p.display());
            } else {
                println!("{target} not found");
            }
        }
        _ => {}
    }
}

fn run_single_external(stage: &ParsedCommand) {
    // Resolve executable: CodeCrafters expects PATH lookup behavior.
    let exec_path = match find_executable_in_path(&stage.cmd) {
        Some(_) => stage.cmd.clone(),
        None => {
            eprintln!("{}: command not found", stage.cmd);
            return;
        }
    };

    let mut cmd = Command::new(exec_path);
    cmd.args(&stage.args);

    // stdout
    match &stage.stdout {
        StdoutRedirect::Inherit => {
            cmd.stdout(Stdio::inherit());
        }
        _ => match open_for_stdout(&stage.stdout) {
            Ok(Some(f)) => {
                cmd.stdout(Stdio::from(f));
            }
            Ok(None) => {
                cmd.stdout(Stdio::inherit());
            }
            Err(e) => {
                eprintln!("{}: {e}", stage.cmd);
                return;
            }
        },
    }

    // stderr
    match &stage.stderr {
        StderrRedirect::Inherit => {
            cmd.stderr(Stdio::inherit());
        }
        _ => match open_for_stderr(&stage.stderr) {
            Ok(Some(f)) => {
                cmd.stderr(Stdio::from(f));
            }
            Ok(None) => {
                cmd.stderr(Stdio::inherit());
            }
            Err(e) => {
                eprintln!("{}: {e}", stage.cmd);
                return;
            }
        },
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{}: {e}", stage.cmd);
            return;
        }
    };

    let _ = child.wait();
}

// ---------- STREAMING pipeline execution (this is the fix) ----------
fn execute_external_pipeline_streaming(stages: &[ParsedCommand]) {
    // This stage (Dual-command pipeline) wants *real* pipes, not buffering.
    // We'll support N-stage external pipelines in a streaming manner.

    // Validate: external only
    for s in stages {
        if is_builtin(&s.cmd) {
            // Not required for this stage; keep behavior simple.
            // You can extend later for "Pipelines with built-ins".
            eprintln!("{}: pipeline with built-ins not supported yet", s.cmd);
            return;
        }
        if find_executable_in_path(&s.cmd).is_none() {
            eprintln!("{}: command not found", s.cmd);
            return;
        }
    }

    let mut children: Vec<Child> = Vec::new();
    let mut prev_stdout: Option<std::process::ChildStdout> = None;

    for (i, stage) in stages.iter().enumerate() {
        let is_last = i + 1 == stages.len();

        let mut cmd = Command::new(&stage.cmd);
        cmd.args(&stage.args);

        // stdin: from previous stage if present
        if let Some(stdout) = prev_stdout.take() {
            cmd.stdin(Stdio::from(stdout));
        } else {
            cmd.stdin(Stdio::inherit());
        }

        // stdout:
        // - if not last: MUST be piped so next command can read it
        // - if last: apply redirection (inherit/truncate/append)
        if !is_last {
            cmd.stdout(Stdio::piped());
        } else {
            match &stage.stdout {
                StdoutRedirect::Inherit => cmd.stdout(Stdio::inherit()),
                _ => match open_for_stdout(&stage.stdout) {
                    Ok(Some(f)) => cmd.stdout(Stdio::from(f)),
                    Ok(None) => cmd.stdout(Stdio::inherit()),
                    Err(e) => {
                        eprintln!("{}: {e}", stage.cmd);
                        return;
                    }
                },
            };
        }

        // stderr: apply redirection always
        match &stage.stderr {
            StderrRedirect::Inherit => cmd.stderr(Stdio::inherit()),
            _ => match open_for_stderr(&stage.stderr) {
                Ok(Some(f)) => cmd.stderr(Stdio::from(f)),
                Ok(None) => cmd.stderr(Stdio::inherit()),
                Err(e) => {
                    eprintln!("{}: {e}", stage.cmd);
                    return;
                }
            },
        };

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("{}: {e}", stage.cmd);
                return;
            }
        };

        // Capture stdout to feed next stage
        if !is_last {
            prev_stdout = child.stdout.take();
        }

        children.push(child);
    }

    // IMPORTANT:
    // Waiting on the last command first ensures pipelines like:
    // tail -f file | head -n 5
    // terminate when head exits (tail may then get SIGPIPE / exit).
    if let Some(mut last) = children.pop() {
        let _ = last.wait();
    }
    // Reap the rest
    for mut c in children {
        let _ = c.wait();
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

            // Other builtins
            if is_builtin(&s.cmd) {
                run_builtin_in_parent(&s.cmd, &s.args);
                continue;
            }

            // Single external
            run_single_external(s);
            continue;
        }

        // PIPELINE (streaming) — this is what fixes Dual-command pipeline tests
        execute_external_pipeline_streaming(&stages);
    }
}

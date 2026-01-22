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

use std::os::unix::io::FromRawFd;

// ---------- libc pipe ----------
use libc;

// ---------------- Redirect enums ----------------
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

        let builtins = ["echo", "exit", "type", "pwd", "cd", "history"];
        for b in builtins {
            if b.starts_with(prefix) {
                matches.push(b.to_string());
            }
        }
        matches.extend(executables_in_path_starting_with(prefix));

        matches.sort();
        matches.dedup();

        if matches.is_empty() {
            let mut st = self.state.borrow_mut();
            st.last_prefix = None;
            st.armed_for_list = false;
            return Ok((pos, vec![]));
        }

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

        let lcp = longest_common_prefix(&matches);
        if lcp.len() > prefix.len() {
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
    matches!(cmd, "exit" | "echo" | "pwd" | "type" | "cd" | "history")
}

// ---------- builtin output routing (single command) ----------
fn write_routed_output(
    stdout_bytes: &[u8],
    stderr_bytes: &[u8],
    stdout_redir: &StdoutRedirect,
    stderr_redir: &StderrRedirect,
    cmd_name: &str,
) {
    // stderr
    match stderr_redir {
        StderrRedirect::Inherit => {
            if !stderr_bytes.is_empty() {
                let mut e = io::stderr();
                let _ = e.write_all(stderr_bytes);
                let _ = e.flush();
            }
        }
        _ => match open_for_stderr(stderr_redir) {
            Ok(Some(mut f)) => {
                let _ = f.write_all(stderr_bytes);
                let _ = f.flush();
            }
            Ok(None) => {}
            Err(e) => eprintln!("{cmd_name}: {e}"),
        },
    }

    // stdout
    match stdout_redir {
        StdoutRedirect::Inherit => {
            if !stdout_bytes.is_empty() {
                let mut o = io::stdout();
                let _ = o.write_all(stdout_bytes);
                let _ = o.flush();
            }
        }
        _ => match open_for_stdout(stdout_redir) {
            Ok(Some(mut f)) => {
                let _ = f.write_all(stdout_bytes);
                let _ = f.flush();
            }
            Ok(None) => {}
            Err(e) => eprintln!("{cmd_name}: {e}"),
        },
    }
}

// -------- history printing helper (matches tester formatting) --------
fn history_output(history: &[String], n: Option<usize>) -> Vec<u8> {
    let len = history.len();
    let start = match n {
        Some(k) if k < len => len - k,
        Some(_) => 0,
        None => 0,
    };

    let mut out = String::new();
    for (idx0, cmd) in history.iter().enumerate().skip(start) {
        // Tester format: "    1  cmd"
        out.push_str(&format!("{:>5}  {}\n", idx0 + 1, cmd));
    }
    out.into_bytes()
}

// ---------- builtin output bytes ----------
fn builtin_bytes(cmd: &str, args: &[String], history: &[String]) -> (Vec<u8>, Vec<u8>, i32) {
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
            let builtins = ["exit", "echo", "type", "pwd", "cd", "history"];
            if builtins.contains(&target) {
                (format!("{target} is a shell builtin\n").into_bytes(), vec![], 0)
            } else if let Some(p) = find_executable_in_path(target) {
                (format!("{target} is {}\n", p.display()).into_bytes(), vec![], 0)
            } else {
                (format!("{target} not found\n").into_bytes(), vec![], 0)
            }
        }
        "history" => {
            // history [n]
            let n = if args.len() == 1 {
                args[0].parse::<usize>().ok()
            } else {
                None
            };
            (history_output(history, n), vec![], 0)
        }
        "cd" => (vec![], vec![], 0),   // pipeline "cd" doesn't affect parent
        "exit" => (vec![], vec![], 0), // pipeline "exit" treated as no-op
        _ => (vec![], format!("{cmd}: command not found\n").into_bytes(), 127),
    }
}

// ---------- run single external ----------
fn run_single_external(stage: &ParsedCommand) {
    if find_executable_in_path(&stage.cmd).is_none() {
        eprintln!("{}: command not found", stage.cmd);
        return;
    }

    let mut cmd = Command::new(&stage.cmd);
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

// ---------- POSIX pipe helper ----------
fn make_pipe() -> io::Result<(File, File)> {
    let mut fds = [0i32; 2];
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    let read_end = unsafe { File::from_raw_fd(fds[0]) };
    let write_end = unsafe { File::from_raw_fd(fds[1]) };
    Ok((read_end, write_end))
}

// ---------- write builtin output either to pipe or terminal/file ----------
fn write_to_target(
    out: Vec<u8>,
    err: Vec<u8>,
    stdout_redir: &StdoutRedirect,
    stderr_redir: &StderrRedirect,
    cmd: &str,
    out_sink: Option<File>,
) {
    // stderr: pipeline stages keep stderr on terminal unless redirected
    match stderr_redir {
        StderrRedirect::Inherit => {
            if !err.is_empty() {
                let mut e = io::stderr();
                let _ = e.write_all(&err);
                let _ = e.flush();
            }
        }
        _ => {
            if let Ok(Some(mut f)) = open_for_stderr(stderr_redir) {
                let _ = f.write_all(&err);
                let _ = f.flush();
            }
        }
    }

    // stdout
    if let Some(mut pipe_writer) = out_sink {
        // If we have a pipe sink, always write stdout there (that’s pipeline semantics)
        let _ = pipe_writer.write_all(&out);
        let _ = pipe_writer.flush();
        // pipe_writer drops here => closes pipe
        return;
    }

    // No pipe sink => last stage: honor stdout redirection
    match stdout_redir {
        StdoutRedirect::Inherit => {
            if !out.is_empty() {
                let mut o = io::stdout();
                let _ = o.write_all(&out);
                let _ = o.flush();
            }
        }
        _ => {
            if let Ok(Some(mut f)) = open_for_stdout(stdout_redir) {
                let _ = f.write_all(&out);
                let _ = f.flush();
            }
        }
    }
}

// ---------- FULL pipeline execution (supports N stages, builtins + externals) ----------
fn execute_pipeline(stages: &[ParsedCommand], history_vec: &[String]) {
    if stages.is_empty() {
        return;
    }

    // Create N-1 pipes
    let mut pipes: Vec<(File, File)> = Vec::new();
    for _ in 0..(stages.len().saturating_sub(1)) {
        match make_pipe() {
            Ok(p) => pipes.push(p),
            Err(e) => {
                eprintln!("pipe: {e}");
                return;
            }
        }
    }

    let mut children: Vec<Child> = Vec::new();
    let mut builtin_threads: Vec<std::thread::JoinHandle<()>> = Vec::new();

    for (i, stage) in stages.iter().enumerate() {
        let is_first = i == 0;
        let is_last = i + 1 == stages.len();

        // stdin for stage i
        let stdin_for_stage: Option<File> = if is_first {
            None
        } else {
            Some(pipes[i - 1].0.try_clone().unwrap())
        };

        // stdout sink for stage i (pipe write end) if not last
        let stdout_pipe_write: Option<File> = if is_last {
            None
        } else {
            Some(pipes[i].1.try_clone().unwrap())
        };

        if is_builtin(&stage.cmd) {
            let cmd = stage.cmd.clone();
            let args = stage.args.clone();
            let stdout_redir = stage.stdout.clone();
            let stderr_redir = stage.stderr.clone();
            let hist_snapshot: Vec<String> = history_vec.to_vec();
            let out_sink = stdout_pipe_write;

            // Builtins for this stage do not consume stdin in these CodeCrafters stages
            drop(stdin_for_stage);

            let h = std::thread::spawn(move || {
                let (out, err, _code) = builtin_bytes(&cmd, &args, &hist_snapshot);
                write_to_target(out, err, &stdout_redir, &stderr_redir, &cmd, out_sink);
            });
            builtin_threads.push(h);
        } else {
            if find_executable_in_path(&stage.cmd).is_none() {
                eprintln!("{}: command not found", stage.cmd);
                return;
            }

            let mut cmd = Command::new(&stage.cmd);
            cmd.args(&stage.args);

            // stdin
            if let Some(f) = stdin_for_stage {
                cmd.stdin(Stdio::from(f));
            } else {
                cmd.stdin(Stdio::inherit());
            }

            // stdout
            if !is_last {
                if let Some(f) = stdout_pipe_write {
                    cmd.stdout(Stdio::from(f));
                } else {
                    cmd.stdout(Stdio::piped());
                }
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

            // stderr
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
            }

            match cmd.spawn() {
                Ok(child) => children.push(child),
                Err(e) => {
                    eprintln!("{}: {e}", stage.cmd);
                    return;
                }
            }
        }
    }

    // Very important: close all pipe fds in parent so downstream sees EOF properly
    drop(pipes);

    // Join builtins
    for h in builtin_threads {
        let _ = h.join();
    }

    // Wait children (works fine for multi-stage)
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

    // Our own history list for the "history" builtin output (must include invalid commands + history itself)
    let mut history_vec: Vec<String> = Vec::new();

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

        // Add to rustyline history so up/down arrows work
        let _ = rl.add_history_entry(line.as_str());

        // Add to our command history so "history" builtin prints what tester expects
        history_vec.push(line.clone());

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

        // SINGLE COMMAND: parent effects + builtins + externals
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

            if is_builtin(&s.cmd) {
                let (out, err, _code) = builtin_bytes(&s.cmd, &s.args, &history_vec);
                write_routed_output(&out, &err, &s.stdout, &s.stderr, &s.cmd);
                continue;
            }

            // external single
            run_single_external(s);
            continue;
        }

        // PIPELINE (supports builtins + multi-command pipelines)
        execute_pipeline(&stages, &history_vec);
    }
}

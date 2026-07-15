use clap::{Parser, Subcommand, ValueEnum};
use colored::Colorize;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// Color palette - subtle dark backgrounds with pastel hues
const COLOR_PALETTE: &[&str] = &[
    "1e2233", // soft navy
    "1e2828", // soft sage
    "2d1f2d", // dusty plum
    "1f2d2d", // seafoam
    "2b2433", // lavender
    "33261f", // warm taupe
    "1f2b33", // powder blue
    "2d2626", // dusty rose
    "262d26", // soft mint
    "332b1f", // soft peach
    "261f2d", // soft violet
    "1f332b", // soft teal
];

static TIMINGS_ENABLED: AtomicBool = AtomicBool::new(false);
static ACTIVE_WORKTREE: Mutex<Option<PathBuf>> = Mutex::new(None);
static ACTIVE_CHILD_PID: Mutex<Option<u32>> = Mutex::new(None);
static ACTIVE_AGENT: Mutex<Option<Agent>> = Mutex::new(None);

thread_local! {
    static TIMING_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

struct TimingSpan {
    label: String,
    start: Instant,
}

impl TimingSpan {
    fn new(label: &str) -> Self {
        if TIMINGS_ENABLED.load(Ordering::Relaxed) {
            let depth = TIMING_DEPTH.with(|d| d.get());
            let indent = "  ".repeat(depth);
            eprintln!("{}⏱ {} ...", indent, label.dimmed());
            TIMING_DEPTH.with(|d| d.set(depth + 1));
        }
        Self {
            label: label.to_string(),
            start: Instant::now(),
        }
    }
}

impl Drop for TimingSpan {
    fn drop(&mut self) {
        if TIMINGS_ENABLED.load(Ordering::Relaxed) {
            TIMING_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
            let depth = TIMING_DEPTH.with(|d| d.get());
            let indent = "  ".repeat(depth);
            let elapsed = self.start.elapsed();
            let ms = elapsed.as_millis();
            let time_str = if ms >= 1000 {
                format!("{:.1}s", elapsed.as_secs_f64())
            } else {
                format!("{}ms", ms)
            };
            eprintln!("{}⏱ {} {}", indent, self.label.dimmed(), time_str.yellow());
        }
    }
}

/// Convenience macro for creating a timing span in the current scope.
macro_rules! timing {
    ($label:expr) => {
        let _timing_span = TimingSpan::new($label);
    };
}

#[derive(Parser)]
#[command(name = "checkout")]
#[command(about = "Create git worktrees and open coding-agent sessions")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Print timing information for each operation
    #[arg(long, global = true)]
    timings: bool,

    /// Coding agent for new sessions and resume-last
    #[arg(long, global = true, value_enum, default_value_t = Agent::Codex)]
    agent: Agent,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, ValueEnum)]
#[serde(rename_all = "lowercase")]
enum Agent {
    #[default]
    Codex,
    Claude,
}

impl Agent {
    fn command(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "codex" => Some(Self::Codex),
            "claude" => Some(Self::Claude),
            _ => None,
        }
    }

    fn skill(self, claude: &'static str, codex: &'static str) -> &'static str {
        match self {
            Self::Codex => codex,
            Self::Claude => claude,
        }
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Check out a GitHub PR into a worktree
    Pr {
        /// PR number or GitHub PR URL (e.g., 123 or https://github.com/org/repo/pull/123)
        pr: String,

        /// Optional skill to run after checkout (e.g., /walkthrough)
        skill: Option<String>,

        /// Skip launching the coding agent after creating the worktree
        #[arg(long = "no-agent", alias = "no-claude")]
        no_agent: bool,

        /// Path to the repo (default: $CHECKOUT_REPO)
        #[arg(long)]
        repo: Option<PathBuf>,

        /// Resume the existing worktree session without prompting
        #[arg(long)]
        resume_existing: bool,
    },
    /// Open a resource in its existing iTerm session or a new checkout tab
    Open {
        #[command(subcommand)]
        target: OpenTarget,
    },
    /// Check whether a resource has a live iTerm session
    Session {
        #[command(subcommand)]
        target: SessionTarget,
    },
    /// Open a persistent worktree for a Statsig gate
    Statsig {
        /// Statsig gate name
        gate: String,

        /// Path to the repo (default: $CHECKOUT_REPO)
        #[arg(long)]
        repo: Option<PathBuf>,

        /// Resume the existing worktree session without prompting
        #[arg(long)]
        resume_existing: bool,
    },
    /// Check out a GitHub PR into a worktree and generate a walkthrough
    Walkthrough {
        /// PR number or GitHub PR URL (e.g., 123 or https://github.com/org/repo/pull/123)
        pr: String,

        /// Skip launching the coding agent after creating the worktree
        #[arg(long = "no-agent", alias = "no-claude")]
        no_agent: bool,

        /// Path to the repo (default: $CHECKOUT_REPO)
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Check out a GitHub PR into a worktree and review it
    Review {
        /// PR number or GitHub PR URL (e.g., 123 or https://github.com/org/repo/pull/123)
        pr: String,

        /// Skip launching the coding agent after creating the worktree
        #[arg(long = "no-agent", alias = "no-claude")]
        no_agent: bool,

        /// Path to the repo (default: $CHECKOUT_REPO)
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Create a new branch in a worktree
    Branch {
        /// Branch name (e.g. darren/my-feature)
        name: String,

        /// Skip launching the coding agent after creating the worktree
        #[arg(long = "no-agent", alias = "no-claude")]
        no_agent: bool,

        /// Path to a file whose contents will be used as the initial agent prompt
        #[arg(long = "prompt", alias = "claude-prompt")]
        prompt: Option<PathBuf>,

        /// Path to the repo (default: $CHECKOUT_REPO)
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Create a new worktree with a random name
    New {
        /// Skip launching the coding agent after creating the worktree
        #[arg(long = "no-agent", alias = "no-claude")]
        no_agent: bool,

        /// Path to a file whose contents will be used as the initial agent prompt
        #[arg(long = "prompt", alias = "claude-prompt")]
        prompt: Option<PathBuf>,

        /// Path to the repo (default: $CHECKOUT_REPO)
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Create a new worktree and start the workstream-begin skill
    Begin {
        /// Path to the repo (default: $CHECKOUT_REPO)
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// List all worktrees and their status
    Status {
        /// Path to the repo (default: $CHECKOUT_REPO)
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Remove worktrees that have no uncommitted changes
    Clean {
        /// Path to the repo (default: $CHECKOUT_REPO)
        #[arg(long)]
        repo: Option<PathBuf>,

        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Browse all worktree sessions and resume one with its original agent
    Resume {
        /// Path to the repo (default: $CHECKOUT_REPO)
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Resume the most recently exited session for the selected agent
    ResumeLast {
        /// Path to the repo (default: $CHECKOUT_REPO)
        #[arg(long)]
        repo: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum OpenTarget {
    /// Open a GitHub PR worktree
    Pr {
        /// PR number or GitHub PR URL
        pr: String,

        /// Path to the repo (default: $CHECKOUT_REPO)
        #[arg(long)]
        repo: Option<PathBuf>,

        /// Print a machine-readable result
        #[arg(long)]
        json: bool,
    },
    /// Open a Statsig gate worktree
    Statsig {
        /// Statsig gate name
        gate: String,

        /// Path to the repo (default: $CHECKOUT_REPO)
        #[arg(long)]
        repo: Option<PathBuf>,

        /// Print a machine-readable result
        #[arg(long)]
        json: bool,
    },
    /// Open the coding session for a local workspace
    Workspace {
        /// Path to the workspace
        #[arg(long)]
        repo: PathBuf,

        /// Print a machine-readable result
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum SessionTarget {
    /// Check for a live GitHub PR session
    Pr {
        /// PR number or GitHub PR URL
        pr: String,

        /// Known PR branch, avoiding another GitHub lookup
        #[arg(long)]
        branch: Option<String>,

        /// Path to the repo (default: $CHECKOUT_REPO)
        #[arg(long)]
        repo: Option<PathBuf>,

        /// Print a machine-readable result
        #[arg(long)]
        json: bool,
    },
    /// Check for a live Statsig gate session
    Statsig {
        /// Statsig gate name
        gate: String,

        /// Path to the repo (default: $CHECKOUT_REPO)
        #[arg(long)]
        repo: Option<PathBuf>,

        /// Print a machine-readable result
        #[arg(long)]
        json: bool,
    },
    /// Check or register the coding session for a local workspace
    Workspace {
        /// Path to the workspace
        #[arg(long)]
        repo: PathBuf,

        /// Associate the current iTerm session with this workspace
        #[arg(long)]
        register_current: bool,

        /// Print a machine-readable result
        #[arg(long)]
        json: bool,
    },
}

#[derive(Deserialize)]
struct PrDetails {
    #[serde(rename = "headRefName")]
    head_ref_name: String,
    title: String,
}

#[derive(Debug, Eq, PartialEq)]
struct ResumeTarget {
    last_modified: SystemTime,
    agent: Agent,
    resume_id: Option<String>,
}

#[derive(Debug)]
enum ExistingWorktreeAction {
    UseExisting,
    ResumeSession(ResumeTarget),
    CreateNew,
}

fn main() {
    setup_ctrlc_handler();

    if let Err(e) = run() {
        eprintln!("{} {}", "error:".red().bold(), e);
        std::process::exit(1);
    }
}

/// Set iTerm2 background color using proprietary escape sequence.
fn set_iterm_background(hex_color: &str) {
    print!("\x1b]1337;SetColors=bg={}\x07", hex_color);
    std::io::stdout().flush().ok();
}

/// Reset iTerm2 background to default
fn reset_iterm_background() {
    // OSC 111 resets background color to default (standard xterm sequence)
    print!("\x1b]111\x07");
    std::io::stdout().flush().ok();
}

fn rename_iterm_session(title: &str) -> Result<(), String> {
    let iterm_session_id = env::var("ITERM_SESSION_ID")
        .map_err(|_| "ITERM_SESSION_ID is not set".to_string())?;
    let session_uuid = iterm_session_id
        .split_once(':')
        .map_or(iterm_session_id.as_str(), |(_, uuid)| uuid);
    let response = iterm_api_request(&serde_json::json!({
        "action": "rename",
        "sessionIds": [session_uuid],
        "title": title,
    }))?;
    if response.exists == Some(true) {
        Ok(())
    } else {
        Err(format!("iTerm2 session not found: {}", session_uuid))
    }
}

/// Set iTerm2 session title
fn set_iterm_title(title: &str) {
    // Target the exact session as well as writing OSC title sequences. The
    // latter do not reach iTerm2 from every coding-agent subprocess.
    let _ = rename_iterm_session(title);
    // OSC 1 sets tab/icon title, OSC 2 sets window title
    // Set both to ensure the title shows
    print!("\x1b]1;{}\x07\x1b]2;{}\x07", title, title);
    std::io::stdout().flush().ok();
}

/// Reset iTerm2 session title
fn reset_iterm_title() {
    let _ = rename_iterm_session("");
    print!("\x1b]1;\x07\x1b]2;\x07");
    std::io::stdout().flush().ok();
}

/// Set terminal working directory via OSC 7 escape sequence
/// This tells the terminal what directory cmd-click paths should resolve from
fn set_terminal_cwd(path: &PathBuf) {
    let hostname = hostname::get()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_else(|_| "localhost".to_string());
    print!("\x1b]7;file://{}{}\x07", hostname, path.display());
    std::io::stdout().flush().ok();
}

// Track whether we've modified iTerm settings
static ITERM_MODIFIED: AtomicBool = AtomicBool::new(false);

/// RAII guard that resets iTerm settings on drop
struct ItermGuard;

impl ItermGuard {
    fn new(bg_color: &str, title: &str) -> Self {
        set_iterm_background(bg_color);
        set_iterm_title(title);
        ITERM_MODIFIED.store(true, Ordering::SeqCst);
        Self
    }
}

impl Drop for ItermGuard {
    fn drop(&mut self) {
        if ITERM_MODIFIED.load(Ordering::SeqCst) {
            reset_iterm_background();
            reset_iterm_title();
            ITERM_MODIFIED.store(false, Ordering::SeqCst);
        }
    }
}

fn setup_ctrlc_handler() {
    // With the `termination` feature, this fires on SIGINT, SIGTERM, and SIGHUP.
    // SIGHUP matters for iTerm tab-close: a child agent can otherwise survive
    // the wrapper and keep the worktree from being reused.
    ctrlc::set_handler(move || {
        if ITERM_MODIFIED.load(Ordering::SeqCst) {
            reset_iterm_background();
            reset_iterm_title();
        }
        // Kill the agent child before we exit — otherwise it can be reparented
        // to launchd and keep holding the pid/worktree.
        if let Ok(guard) = ACTIVE_CHILD_PID.lock() {
            if let Some(pid) = *guard {
                let _ = Command::new("kill")
                    .args(["-TERM", &pid.to_string()])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
        // Write session exited file so the worktree can be reused later.
        // PidFileGuard::drop is bypassed by process::exit, so we do it here.
        if let Ok(guard) = ACTIVE_WORKTREE.lock() {
            if let Some(path) = guard.as_ref() {
                remove_session_pid(path);
                let agent = ACTIVE_AGENT
                    .lock()
                    .ok()
                    .and_then(|agent| *agent)
                    .unwrap_or_default();
                write_session_exited(path, agent);
            }
        }
        std::process::exit(130);
    })
    .ok();
}

fn get_color_dir() -> PathBuf {
    let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(format!("{}/.local/share/checkout/colors", home))
}

fn worktree_color_file(worktree_path: &PathBuf) -> PathBuf {
    // Use the worktree directory name as the color file name
    let name = worktree_path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    get_color_dir().join(name)
}

fn get_worktree_color(worktree_path: &PathBuf) -> Option<String> {
    let color_file = worktree_color_file(worktree_path);
    fs::read_to_string(color_file).ok().map(|s| s.trim().to_string())
}

fn save_worktree_color(worktree_path: &PathBuf, color: &str) -> Result<(), String> {
    let color_dir = get_color_dir();
    fs::create_dir_all(&color_dir).map_err(|e| format!("Failed to create color dir: {}", e))?;

    let color_file = worktree_color_file(worktree_path);
    fs::write(&color_file, color).map_err(|e| format!("Failed to save color: {}", e))?;

    Ok(())
}

fn get_used_colors() -> HashSet<String> {
    let mut used = HashSet::new();
    let color_dir = get_color_dir();

    if let Ok(entries) = fs::read_dir(color_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Ok(color) = fs::read_to_string(&path) {
                    used.insert(color.trim().to_string());
                }
            }
        }
    }

    used
}

fn pick_available_color(current_worktree: &PathBuf) -> String {
    if let Some(existing) = get_worktree_color(current_worktree) {
        return existing;
    }

    let used = get_used_colors();

    for color in COLOR_PALETTE {
        if !used.contains(*color) {
            return color.to_string();
        }
    }

    let hash = current_worktree.to_string_lossy().bytes().fold(0usize, |acc, b| acc.wrapping_add(b as usize));
    COLOR_PALETTE[hash % COLOR_PALETTE.len()].to_string()
}

fn get_session_dir() -> PathBuf {
    let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(format!("{}/.local/share/checkout/sessions", home))
}

fn session_file_name(worktree_path: &Path) -> String {
    worktree_path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn session_pid_file(worktree_path: &Path) -> PathBuf {
    get_session_dir().join(format!("{}.pid", session_file_name(worktree_path)))
}

fn session_exited_file(worktree_path: &Path) -> PathBuf {
    get_session_dir().join(format!("{}.exited", session_file_name(worktree_path)))
}

fn session_name_file(worktree_path: &Path) -> PathBuf {
    get_session_dir().join(format!("{}.name", session_file_name(worktree_path)))
}

fn worktree_iterm_session_file(worktree_path: &Path) -> PathBuf {
    let digest = format!("{:x}", md5::compute(worktree_path.to_string_lossy().as_bytes()));
    get_session_dir().join(format!("worktree-{}.iterm", digest))
}

fn resource_iterm_session_file(resource: &str, identifier: &str, repo_root: &Path) -> PathBuf {
    let key = format!("{}:{}:{}", repo_root.display(), resource, identifier);
    let digest = format!("{:x}", md5::compute(key.as_bytes()));
    get_session_dir().join(format!("resource-{}.iterm", digest))
}

fn iterm_api_socket_file() -> PathBuf {
    let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(format!("{}/.local/share/checkout/iterm-api.sock", home))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ItermApiResponse {
    ok: bool,
    exists: Option<bool>,
    session_id: Option<String>,
    action: Option<String>,
    error: Option<String>,
}

fn iterm_api_request(request: &Value) -> Result<ItermApiResponse, String> {
    iterm_api_request_at(&iterm_api_socket_file(), request)
}

fn iterm_api_request_at(socket_path: &Path, request: &Value) -> Result<ItermApiResponse, String> {
    let mut stream = UnixStream::connect(socket_path).map_err(|error| {
        format!(
            "iTerm Python API helper is unavailable: {}. Run checkout-pr/iterm/install.sh and restart iTerm2",
            error
        )
    })?;
    let timeout = Some(Duration::from_millis(500));
    stream
        .set_read_timeout(timeout)
        .map_err(|error| format!("Failed to configure iTerm Python API helper: {}", error))?;
    stream
        .set_write_timeout(timeout)
        .map_err(|error| format!("Failed to configure iTerm Python API helper: {}", error))?;
    stream
        .write_all(request.to_string().as_bytes())
        .map_err(|error| format!("Failed to write to iTerm Python API helper: {}", error))?;
    stream
        .write_all(b"\n")
        .map_err(|error| format!("Failed to write to iTerm Python API helper: {}", error))?;
    let mut output = String::new();
    stream
        .read_to_string(&mut output)
        .map_err(|error| format!("Failed to read from iTerm Python API helper: {}", error))?;
    let response: ItermApiResponse = serde_json::from_str(output.trim())
        .map_err(|error| format!("iTerm Python API helper returned invalid JSON: {}", error))?;
    if !response.ok {
        return Err(response
            .error
            .clone()
            .unwrap_or_else(|| "iTerm Python API request failed".to_string()));
    }
    Ok(response)
}

fn normalized_iterm_session_id(value: &str) -> Option<String> {
    let id = value.rsplit_once(':').map_or(value, |(_, id)| id).trim();
    (!id.is_empty()).then(|| id.to_string())
}

fn save_iterm_session_id(path: PathBuf, session_id: &str) -> Result<(), String> {
    let session_id = normalized_iterm_session_id(session_id)
        .ok_or_else(|| "iTerm session ID is empty".to_string())?;
    let dir = get_session_dir();
    fs::create_dir_all(&dir)
        .map_err(|error| format!("Failed to create session dir {}: {}", dir.display(), error))?;
    fs::write(path, session_id)
        .map_err(|error| format!("Failed to save iTerm session ID: {}", error))
}

fn read_iterm_session_id(path: PathBuf) -> Option<String> {
    normalized_iterm_session_id(&fs::read_to_string(path).ok()?)
}

fn save_worktree_iterm_session(worktree_path: &Path, session_id: &str) -> Result<(), String> {
    save_iterm_session_id(worktree_iterm_session_file(worktree_path), session_id)
}

fn read_worktree_iterm_session(worktree_path: &Path) -> Option<String> {
    read_iterm_session_id(worktree_iterm_session_file(worktree_path))
}

fn save_resource_iterm_session(
    resource: &str,
    identifier: &str,
    repo_root: &Path,
    session_id: &str,
) -> Result<(), String> {
    save_iterm_session_id(
        resource_iterm_session_file(resource, identifier, repo_root),
        session_id,
    )
}

fn read_resource_iterm_session(resource: &str, identifier: &str, repo_root: &Path) -> Option<String> {
    read_iterm_session_id(resource_iterm_session_file(resource, identifier, repo_root))
}

fn record_current_iterm_session(worktree_path: &Path) -> Result<(), String> {
    let Ok(session_id) = env::var("ITERM_SESSION_ID") else {
        return Ok(());
    };
    save_worktree_iterm_session(worktree_path, &session_id)
}

fn save_session_name(worktree_path: &Path, name: &str) -> Result<(), String> {
    let dir = get_session_dir();
    fs::create_dir_all(&dir)
        .map_err(|e| format!("Failed to create session dir {}: {}", dir.display(), e))?;
    fs::write(session_name_file(worktree_path), name)
        .map_err(|e| format!("Failed to save session name: {}", e))
}

fn read_session_name(worktree_path: &Path) -> Option<String> {
    let name = fs::read_to_string(session_name_file(worktree_path)).ok()?;
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// Convert a git branch into the short, stable name shared by Codex and iTerm.
/// Branch namespaces such as `darren/` or `dependabot/npm_and_yarn/` are dropped.
fn session_name_from_branch(branch: &str) -> String {
    let branch = branch.strip_prefix("refs/heads/").unwrap_or(branch);
    let branch = branch.strip_prefix("origin/").unwrap_or(branch);
    let leaf = branch.rsplit('/').next().unwrap_or(branch);

    let mut name = String::new();
    let mut previous_was_separator = false;
    for character in leaf.chars() {
        if character.is_ascii_alphanumeric() {
            name.push(character.to_ascii_lowercase());
            previous_was_separator = false;
        } else if !name.is_empty() && !previous_was_separator {
            name.push('-');
            previous_was_separator = true;
        }
    }

    if name.len() > 25 {
        let boundary_is_separator = name.as_bytes().get(25) == Some(&b'-');
        name.truncate(25);
        if !boundary_is_separator {
            if let Some(separator) = name.rfind('-') {
                name.truncate(separator);
            }
        }
    }
    while name.ends_with('-') {
        name.pop();
    }
    if name.is_empty() {
        "worktree".to_string()
    } else {
        name
    }
}

fn session_name_for_resume(worktree_path: &Path, branch_hint: &str) -> String {
    read_session_name(worktree_path).unwrap_or_else(|| session_name_from_branch(branch_hint))
}

fn write_session_pid(worktree_path: &Path, pid: u32, agent: Agent) {
    let dir = get_session_dir();
    let _ = fs::create_dir_all(&dir);
    let _ = fs::write(
        session_pid_file(worktree_path),
        format!("{}\n{}", pid, agent.command()),
    );
}

fn remove_session_pid(worktree_path: &Path) {
    let _ = fs::remove_file(session_pid_file(worktree_path));
}

/// Remove the bazel output base directory for a worktree.
/// Bazel stores its cache in /private/var/tmp/_bazel_<user>/<md5(workspace_path)>/,
/// which can consume tens of GB per worktree and is never cleaned up automatically.
fn remove_bazel_output_base(worktree_path: &Path) {
    let path_str = worktree_path.to_string_lossy();
    let hash = format!("{:x}", md5::compute(path_str.as_bytes()));
    let user = env::var("USER").unwrap_or_else(|_| "unknown".to_string());
    let output_base = PathBuf::from(format!("/private/var/tmp/_bazel_{}/{}", user, hash));
    if !output_base.exists() {
        return;
    }
    // Bazel write-protects its `external/` repos, so a plain remove hits EACCES
    // partway through. chmod the tree writable first, then shell out to `rm -rf`
    // since the on-disk shape (symlinks, deep external repos) is more than
    // `fs::remove_dir_all` wants to chase down.
    let _ = Command::new("chmod")
        .args(["-R", "u+w", &output_base.to_string_lossy()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let status = Command::new("rm")
        .args(["-rf", &output_base.to_string_lossy()])
        .status();
    match status {
        Ok(s) if s.success() => println!(
            "    {} Removed bazel cache {}",
            "→".blue().bold(),
            output_base.display().to_string().dimmed()
        ),
        Ok(s) => println!(
            "    {} Failed to remove bazel cache: rm exited with {}",
            "⚠".yellow(),
            s.to_string().dimmed()
        ),
        Err(e) => println!(
            "    {} Failed to remove bazel cache: {}",
            "⚠".yellow(),
            e.to_string().dimmed()
        ),
    }
}

fn write_session_exited(worktree_path: &Path, agent: Agent) {
    let dir = get_session_dir();
    let _ = fs::create_dir_all(&dir);
    let timestamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let clean = Command::new("git")
        .args(["-C", &worktree_path.to_string_lossy(), "status", "--short"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().is_empty())
        .unwrap_or(false);
    let _ = fs::write(
        session_exited_file(worktree_path),
        format!(
            "{}\n{}\n{}\n{}",
            timestamp,
            worktree_path.display(),
            if clean { "clean" } else { "dirty" },
            agent.command()
        ),
    );
}

fn read_session_pid(worktree_path: &Path) -> Option<u32> {
    fs::read_to_string(session_pid_file(worktree_path))
        .ok()?
        .lines()
        .next()?
        .trim()
        .parse()
        .ok()
}

fn read_session_agent(worktree_path: &Path) -> Option<Agent> {
    fs::read_to_string(session_pid_file(worktree_path))
        .ok()?
        .lines()
        .nth(1)
        .and_then(Agent::parse)
}

#[derive(Debug, Eq, PartialEq)]
struct ExitedSession {
    timestamp: u64,
    worktree_path: PathBuf,
    clean: bool,
    agent: Agent,
}

fn parse_exited_session(content: &str) -> Option<ExitedSession> {
    let mut lines = content.lines();
    let timestamp = lines.next()?.trim().parse().ok()?;
    let worktree_path = PathBuf::from(lines.next()?.trim());
    let clean = lines.next()?.trim() == "clean";
    // Markers written before multi-agent support could only have come from Claude.
    let agent = lines.next().and_then(Agent::parse).unwrap_or(Agent::Claude);
    Some(ExitedSession {
        timestamp,
        worktree_path,
        clean,
        agent,
    })
}

fn is_pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// RAII guard that removes the PID file on drop (normal exit or panic).
/// Ctrl+C bypasses Drop, but stale PID files are self-healing: the next
/// `status`/`clean` run detects the dead PID and cleans up.
struct PidFileGuard {
    worktree_path: PathBuf,
    agent: Agent,
}

impl PidFileGuard {
    fn new(worktree_path: &Path, pid: u32, agent: Agent) -> Self {
        write_session_pid(worktree_path, pid, agent);
        // Clear the exited marker so this worktree isn't considered idle
        let _ = fs::remove_file(session_exited_file(worktree_path));
        // Register so the ctrlc handler can clean up and kill the child
        if let Ok(mut guard) = ACTIVE_WORKTREE.lock() {
            *guard = Some(worktree_path.to_path_buf());
        }
        if let Ok(mut guard) = ACTIVE_CHILD_PID.lock() {
            *guard = Some(pid);
        }
        if let Ok(mut guard) = ACTIVE_AGENT.lock() {
            *guard = Some(agent);
        }
        Self {
            worktree_path: worktree_path.to_path_buf(),
            agent,
        }
    }
}

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        // Clear the globals first so ctrlc handler won't double-write
        if let Ok(mut guard) = ACTIVE_WORKTREE.lock() {
            *guard = None;
        }
        if let Ok(mut guard) = ACTIVE_CHILD_PID.lock() {
            *guard = None;
        }
        if let Ok(mut guard) = ACTIVE_AGENT.lock() {
            *guard = None;
        }
        remove_session_pid(&self.worktree_path);
        write_session_exited(&self.worktree_path, self.agent);
    }
}

fn default_repo_root() -> PathBuf {
    PathBuf::from(env::var("CHECKOUT_REPO").expect("CHECKOUT_REPO env var must be set"))
}

fn default_worktree_dir() -> PathBuf {
    PathBuf::from(env::var("CHECKOUT_WORKTREE_DIR").expect("CHECKOUT_WORKTREE_DIR env var must be set"))
}

fn run() -> Result<(), String> {
    let cli = Cli::parse();
    let agent = cli.agent;

    if cli.timings {
        TIMINGS_ENABLED.store(true, Ordering::Relaxed);
    }

    match cli.command {
        Commands::Pr { pr, no_agent, repo, skill, resume_existing } => {
            let initial_skill = agent.skill("/checkout:checkout-pr", "$checkout-pr");
            let chained_skill = skill.as_deref().map(|skill| normalize_skill(agent, skill));
            run_pr(&pr, no_agent, repo, initial_skill, chained_skill.as_deref(), agent, resume_existing)
        },
        Commands::Open { target } => match target {
            OpenTarget::Pr { pr, repo, json } => run_open_pr(&pr, repo, json, agent),
            OpenTarget::Statsig { gate, repo, json } => run_open_statsig(&gate, repo, json, agent),
            OpenTarget::Workspace { repo, json } => run_open_workspace(repo, json, agent),
        },
        Commands::Session { target } => match target {
            SessionTarget::Pr { pr, branch, repo, json } => {
                run_session_pr(&pr, branch.as_deref(), repo, json)
            }
            SessionTarget::Statsig { gate, repo, json } => run_session_statsig(&gate, repo, json),
            SessionTarget::Workspace { repo, register_current, json } => {
                run_session_workspace(repo, register_current, json)
            }
        },
        Commands::Statsig { gate, repo, resume_existing } => run_statsig(&gate, repo, agent, resume_existing),
        Commands::Walkthrough { pr, no_agent, repo } => run_pr(
            &pr,
            no_agent,
            repo,
            agent.skill("/checkout:checkout-pr", "$checkout-pr"),
            Some(agent.skill("/walkthrough", "$walkthrough")),
            agent,
            false,
        ),
        Commands::Review { pr, no_agent, repo } => run_pr(
            &pr,
            no_agent,
            repo,
            agent.skill("/checkout:checkout-and-review-pr", "$checkout-and-review-pr"),
            None,
            agent,
            false,
        ),
        Commands::Branch { name, no_agent, prompt, repo } => {
            let prompt = read_prompt_file(prompt)?;
            run_branch(&name, no_agent, prompt, repo, agent, false)
        },
        Commands::New { no_agent, prompt, repo } => {
            let prompt = read_prompt_file(prompt)?;
            run_new(no_agent, prompt, repo, agent)
        },
        Commands::Begin { repo } => run_new(
            false,
            Some(agent.skill("/darren:workstream-begin sandbox", "$darren-workstream-begin sandbox").to_string()),
            repo,
            agent,
        ),
        Commands::Status { repo } => run_status(repo),
        Commands::Clean { repo, yes } => run_clean(repo, yes),
        Commands::Resume { repo } => run_resume(repo),
        Commands::ResumeLast { repo } => run_resume_last(repo, agent),
    }
}

fn normalize_skill(agent: Agent, skill: &str) -> String {
    if agent == Agent::Codex {
        if let Some(name) = skill.strip_prefix('/') {
            let name = name.strip_prefix("checkout:").unwrap_or(name);
            return format!("${}", name.replace(':', "-"));
        }
    }
    skill.to_string()
}

fn read_prompt_file(path: Option<PathBuf>) -> Result<Option<String>, String> {
    match path {
        Some(p) => {
            let content = fs::read_to_string(&p)
                .map_err(|e| format!("Failed to read prompt file {}: {}", p.display(), e))?;
            Ok(Some(content))
        }
        None => Ok(None),
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn statsig_slug(gate: &str) -> String {
    let mut slug = gate
        .chars()
        .map(|character| if character.is_ascii_alphanumeric() { character.to_ascii_lowercase() } else { '-' })
        .collect::<String>();
    slug = Regex::new(r"-+").unwrap().replace_all(&slug, "-").trim_matches('-').to_string();
    if slug.is_empty() {
        slug = "gate".to_string();
    }
    if slug.len() > 17 {
        let digest = format!("{:x}", md5::compute(gate.as_bytes()));
        slug.truncate(10);
        while slug.ends_with('-') {
            slug.pop();
        }
        slug.push('-');
        slug.push_str(&digest[..6]);
    }
    slug
}

fn statsig_branch_name(gate: &str) -> String {
    format!("darren/statsig-{}", statsig_slug(gate))
}

fn checkout_launch_command(
    resource: &str,
    identifier: &str,
    repo_root: &Path,
    agent: Agent,
) -> Result<String, String> {
    let executable = env::current_exe()
        .map_err(|error| format!("Failed to locate checkout executable: {}", error))?;
    Ok([
        shell_quote(&executable.to_string_lossy()),
        resource.to_string(),
        shell_quote(identifier),
        "--resume-existing".to_string(),
        "--repo".to_string(),
        shell_quote(&repo_root.to_string_lossy()),
        "--agent".to_string(),
        agent.command().to_string(),
    ].join(" "))
}

struct ItermOpenResult {
    action: String,
    session_id: String,
}

fn find_live_iterm_session(
    resource_session_id: Option<&str>,
    worktree_session_id: Option<&str>,
    session_name: &str,
    legacy_prefix: Option<&str>,
) -> Result<Option<String>, String> {
    let response = iterm_api_request(&serde_json::json!({
        "action": "status",
        "sessionIds": [
            resource_session_id.unwrap_or(""),
            worktree_session_id.unwrap_or(""),
        ],
        "sessionName": session_name,
        "legacyPrefix": legacy_prefix.unwrap_or(""),
    }))?;
    if response.exists != Some(true) {
        return Ok(None);
    }
    let session_id = response
        .session_id
        .and_then(|value| normalized_iterm_session_id(&value))
        .ok_or_else(|| "iTerm Python API helper returned an empty session ID".to_string())?;
    Ok(Some(session_id))
}

fn focus_or_open_iterm(
    resource_session_id: Option<&str>,
    worktree_session_id: Option<&str>,
    session_name: &str,
    legacy_prefix: Option<&str>,
    launch_command: &str,
) -> Result<ItermOpenResult, String> {
    let response = iterm_api_request(&serde_json::json!({
        "action": "open",
        "sessionIds": [
            resource_session_id.unwrap_or(""),
            worktree_session_id.unwrap_or(""),
        ],
        "sessionName": session_name,
        "legacyPrefix": legacy_prefix.unwrap_or(""),
        "launchCommand": launch_command,
    }))?;
    let action = response
        .action
        .filter(|action| action == "focused" || action == "opened")
        .ok_or_else(|| "iTerm Python API helper returned an invalid action".to_string())?;
    let session_id = response
        .session_id
        .and_then(|value| normalized_iterm_session_id(&value))
        .ok_or_else(|| "iTerm Python API helper returned an empty session ID".to_string())?;
    Ok(ItermOpenResult { action, session_id })
}

fn print_open_result(json: bool, action: &str, resource: &str, identifier: &str, session_name: &str) {
    if json {
        println!("{}", serde_json::json!({
            "action": action,
            "resourceType": resource,
            "resourceId": identifier,
            "sessionName": session_name,
        }));
    } else {
        let verb = if action == "focused" { "Focused" } else { "Opened" };
        println!("{} {} {} in iTerm", "→".blue().bold(), verb, session_name.cyan());
    }
}

fn print_session_result(json: bool, exists: bool, resource: &str, identifier: &str, session_name: &str) {
    if json {
        println!("{}", serde_json::json!({
            "exists": exists,
            "resourceType": resource,
            "resourceId": identifier,
            "sessionName": session_name,
        }));
    } else {
        let state = if exists { "Live".green() } else { "Not running".dimmed() };
        println!("{} {} {}", "→".blue().bold(), session_name.cyan(), state);
    }
}

fn workspace_session_name(repo_root: &Path) -> String {
    let name = repo_root.file_name().and_then(|value| value.to_str()).unwrap_or("workspace");
    session_name_from_branch(name)
}

fn workspace_launch_command(repo_root: &Path, agent: Agent) -> String {
    let agent_command = match agent {
        Agent::Codex => "codex resume --last || codex",
        Agent::Claude => "claude --continue || claude",
    };
    format!("cd {} && {}", shell_quote(&repo_root.to_string_lossy()), agent_command)
}

fn save_live_session(
    resource: &str,
    identifier: &str,
    repo_root: &Path,
    worktree: Option<&Path>,
    session_id: &str,
) -> Result<(), String> {
    save_resource_iterm_session(resource, identifier, repo_root, session_id)?;
    if let Some(worktree) = worktree {
        save_worktree_iterm_session(worktree, session_id)?;
    }
    Ok(())
}

fn clear_stale_session(resource: &str, identifier: &str, repo_root: &Path, worktree: Option<&Path>) {
    let _ = fs::remove_file(resource_iterm_session_file(resource, identifier, repo_root));
    if let Some(worktree) = worktree {
        let _ = fs::remove_file(worktree_iterm_session_file(worktree));
    }
}

fn session_status(
    resource: &str,
    identifier: &str,
    repo_root: &Path,
    worktree: Option<&Path>,
    session_name: &str,
    legacy_prefix: Option<&str>,
) -> Result<bool, String> {
    let resource_session_id = read_resource_iterm_session(resource, identifier, repo_root);
    let worktree_session_id = worktree.and_then(read_worktree_iterm_session);
    let session_id = find_live_iterm_session(
        resource_session_id.as_deref(),
        worktree_session_id.as_deref(),
        session_name,
        legacy_prefix,
    )?;
    if let Some(session_id) = session_id {
        save_live_session(resource, identifier, repo_root, worktree, &session_id)?;
        return Ok(true);
    }
    clear_stale_session(resource, identifier, repo_root, worktree);
    Ok(false)
}

fn find_pr_worktree(
    repo_root: &PathBuf,
    pr_number: u64,
    branch: &str,
) -> Result<Option<PathBuf>, String> {
    let branch_slug = branch.rsplit('/').next().unwrap_or(branch);
    Ok(find_existing_worktree(repo_root, &format!("branch-{}", branch_slug))?
        .or(find_existing_worktree(repo_root, &format!("pr-{}-", pr_number))?)
        .or(find_existing_worktree(repo_root, &format!("[{}]", branch))?))
}

fn find_branch_worktree(repo_root: &PathBuf, branch: &str) -> Result<Option<PathBuf>, String> {
    let slug = branch.rsplit('/').next().unwrap_or(branch);
    Ok(find_existing_worktree(repo_root, &format!("branch-{}", slug))?
        .or(find_existing_worktree(repo_root, &format!("[{}]", branch))?))
}

fn run_open_pr(pr: &str, repo: Option<PathBuf>, json: bool, agent: Agent) -> Result<(), String> {
    let pr_number = extract_pr_number(pr)?;
    let repo_root = repo.unwrap_or_else(default_repo_root);
    if !repo_root.exists() {
        return Err(format!("Repo not found at {}", repo_root.display()));
    }
    let details = fetch_pr_details(pr_number, &repo_root)?;
    let session_name = session_name_from_branch(&details.head_ref_name);
    let identifier = pr_number.to_string();
    let existing_worktree = find_pr_worktree(&repo_root, pr_number, &details.head_ref_name)?;
    let resource_session_id = read_resource_iterm_session("pr", &identifier, &repo_root);
    let worktree_session_id = existing_worktree.as_deref().and_then(read_worktree_iterm_session);
    let command = checkout_launch_command("pr", &identifier, &repo_root, agent)?;
    let legacy_prefix = format!("pr-{}-", pr_number);
    let result = focus_or_open_iterm(
        resource_session_id.as_deref(),
        worktree_session_id.as_deref(),
        &session_name,
        Some(&legacy_prefix),
        &command,
    )?;
    save_resource_iterm_session("pr", &identifier, &repo_root, &result.session_id)?;
    if let Some(worktree) = existing_worktree {
        save_worktree_iterm_session(&worktree, &result.session_id)?;
    }
    print_open_result(json, &result.action, "pr", &identifier, &session_name);
    Ok(())
}

fn run_open_statsig(gate: &str, repo: Option<PathBuf>, json: bool, agent: Agent) -> Result<(), String> {
    let gate = gate.trim();
    if gate.is_empty() {
        return Err("Statsig gate name is required".to_string());
    }
    let repo_root = repo.unwrap_or_else(default_repo_root);
    if !repo_root.exists() {
        return Err(format!("Repo not found at {}", repo_root.display()));
    }
    let branch = statsig_branch_name(gate);
    let session_name = session_name_from_branch(&branch);
    let existing_worktree = find_branch_worktree(&repo_root, &branch)?;
    let resource_session_id = read_resource_iterm_session("statsig", gate, &repo_root);
    let worktree_session_id = existing_worktree.as_deref().and_then(read_worktree_iterm_session);
    let command = checkout_launch_command("statsig", gate, &repo_root, agent)?;
    let result = focus_or_open_iterm(
        resource_session_id.as_deref(),
        worktree_session_id.as_deref(),
        &session_name,
        None,
        &command,
    )?;
    save_resource_iterm_session("statsig", gate, &repo_root, &result.session_id)?;
    if let Some(worktree) = existing_worktree {
        save_worktree_iterm_session(&worktree, &result.session_id)?;
    }
    print_open_result(json, &result.action, "statsig", gate, &session_name);
    Ok(())
}

fn run_open_workspace(repo_root: PathBuf, json: bool, agent: Agent) -> Result<(), String> {
    if !repo_root.exists() {
        return Err(format!("Workspace not found at {}", repo_root.display()));
    }
    let identifier = "development";
    let session_name = workspace_session_name(&repo_root);
    let resource_session_id = read_resource_iterm_session("workspace", identifier, &repo_root);
    let worktree_session_id = read_worktree_iterm_session(&repo_root);
    let command = workspace_launch_command(&repo_root, agent);
    let result = focus_or_open_iterm(
        resource_session_id.as_deref(),
        worktree_session_id.as_deref(),
        &session_name,
        None,
        &command,
    )?;
    save_live_session("workspace", identifier, &repo_root, Some(&repo_root), &result.session_id)?;
    print_open_result(json, &result.action, "workspace", identifier, &session_name);
    Ok(())
}

fn run_session_pr(
    pr: &str,
    branch: Option<&str>,
    repo: Option<PathBuf>,
    json: bool,
) -> Result<(), String> {
    let pr_number = extract_pr_number(pr)?;
    let repo_root = repo.unwrap_or_else(default_repo_root);
    if !repo_root.exists() {
        return Err(format!("Repo not found at {}", repo_root.display()));
    }
    let branch = match branch {
        Some(branch) => branch.to_string(),
        None => fetch_pr_details(pr_number, &repo_root)?.head_ref_name,
    };
    let session_name = session_name_from_branch(&branch);
    let worktree = find_pr_worktree(&repo_root, pr_number, &branch)?;
    let identifier = pr_number.to_string();
    let legacy_prefix = format!("pr-{}-", pr_number);
    let exists = session_status(
        "pr",
        &identifier,
        &repo_root,
        worktree.as_deref(),
        &session_name,
        Some(&legacy_prefix),
    )?;
    print_session_result(json, exists, "pr", &identifier, &session_name);
    Ok(())
}

fn run_session_statsig(gate: &str, repo: Option<PathBuf>, json: bool) -> Result<(), String> {
    let gate = gate.trim();
    if gate.is_empty() {
        return Err("Statsig gate name is required".to_string());
    }
    let repo_root = repo.unwrap_or_else(default_repo_root);
    if !repo_root.exists() {
        return Err(format!("Repo not found at {}", repo_root.display()));
    }
    let branch = statsig_branch_name(gate);
    let session_name = session_name_from_branch(&branch);
    let worktree = find_branch_worktree(&repo_root, &branch)?;
    let exists = session_status(
        "statsig",
        gate,
        &repo_root,
        worktree.as_deref(),
        &session_name,
        None,
    )?;
    print_session_result(json, exists, "statsig", gate, &session_name);
    Ok(())
}

fn run_session_workspace(repo_root: PathBuf, register_current: bool, json: bool) -> Result<(), String> {
    if !repo_root.exists() {
        return Err(format!("Workspace not found at {}", repo_root.display()));
    }
    let identifier = "development";
    let session_name = workspace_session_name(&repo_root);
    if register_current {
        let session_id = env::var("ITERM_SESSION_ID")
            .map_err(|_| "ITERM_SESSION_ID is not set".to_string())?;
        save_live_session("workspace", identifier, &repo_root, Some(&repo_root), &session_id)?;
        print_session_result(json, true, "workspace", identifier, &session_name);
        return Ok(());
    }
    let exists = session_status(
        "workspace",
        identifier,
        &repo_root,
        Some(&repo_root),
        &session_name,
        None,
    )?;
    print_session_result(json, exists, "workspace", identifier, &session_name);
    Ok(())
}

fn run_statsig(gate: &str, repo: Option<PathBuf>, agent: Agent, resume_existing: bool) -> Result<(), String> {
    let gate = gate.trim();
    if gate.is_empty() {
        return Err("Statsig gate name is required".to_string());
    }
    let prompt = format!(
        "Use $statsig-cli to inspect the Statsig gate `{}` and continue the rollout or investigation associated with it. Verify the current gate state and recent history before recommending or making changes.",
        gate,
    );
    run_branch(&statsig_branch_name(gate), false, Some(prompt), repo, agent, resume_existing)
}

fn run_pr(
    pr: &str,
    no_agent: bool,
    repo: Option<PathBuf>,
    initial_prompt: &str,
    chained_skill: Option<&str>,
    agent: Agent,
    resume_existing: bool,
) -> Result<(), String> {
    timing!("run_pr");
    let pr_number = extract_pr_number(pr)?;
    println!(
        "{} PR #{}",
        "→".blue().bold(),
        pr_number.to_string().cyan()
    );

    let repo_root = repo.unwrap_or_else(default_repo_root);

    if !repo_root.exists() {
        return Err(format!("Repo not found at {}", repo_root.display()));
    }

    print!("{} Fetching PR details... ", "→".blue().bold());
    std::io::stdout().flush().ok();
    let pr_details = fetch_pr_details(pr_number, &repo_root)?;
    println!("{}", "done".green());

    println!(
        "  {} {}",
        "title:".dimmed(),
        pr_details.title.white().bold()
    );
    println!(
        "  {} {}",
        "branch:".dimmed(),
        pr_details.head_ref_name.yellow()
    );

    let slug = create_slug(&pr_details.title);
    let worktree_dir = default_worktree_dir();
    let worktree_path = worktree_dir.join(format!("pr-{}-{}", pr_number, slug));

    let existing = find_pr_worktree(&repo_root, pr_number, &pr_details.head_ref_name)?;

    let mut resume_target = None;
    let mut is_new_worktree = false;

    let final_path = if let Some(existing_path) = existing {
        println!(
            "\n{} Worktree already exists at {}",
            "!".yellow().bold(),
            existing_path.display().to_string().cyan()
        );

        let available_resume = find_worktree_resume_target(&existing_path);
        let action = if resume_existing {
            available_resume
                .map(ExistingWorktreeAction::ResumeSession)
                .unwrap_or(ExistingWorktreeAction::UseExisting)
        } else {
            let changes_handle = {
                let path = existing_path.clone();
                thread::spawn(move || get_uncommitted_status(&path))
            };
            prompt_existing_worktree_action(changes_handle, agent, available_resume)?
        };

        match action {
            ExistingWorktreeAction::ResumeSession(target) => {
                resume_target = Some(target);
                existing_path
            }
            ExistingWorktreeAction::UseExisting => {
                if !resume_existing {
                    print!("{} Updating to latest... ", "→".blue().bold());
                    std::io::stdout().flush().ok();
                    match update_worktree(&existing_path, &pr_details.head_ref_name) {
                        Ok(()) => println!("{}", "done".green()),
                        Err(e) => println!("{}\n  {} {}", "skipped".yellow(), "⚠".yellow().bold(), e.dimmed()),
                    }
                }
                existing_path
            }
            ExistingWorktreeAction::CreateNew => {
                let new_path = find_next_worktree_path(&worktree_dir, &format!("pr-{}-{}", pr_number, slug))?;
                create_new_worktree_from_remote(&repo_root, &worktree_dir, &new_path, &pr_details.head_ref_name, pr_number)?;
                is_new_worktree = true;
                new_path
            }
        }
    } else {
        create_new_worktree_from_remote(&repo_root, &worktree_dir, &worktree_path, &pr_details.head_ref_name, pr_number)?;
        is_new_worktree = true;
        worktree_path
    };

    let bg_handle = if is_new_worktree {
        Some(start_new_worktree_setup(final_path.clone(), repo_root.clone())?)
    } else {
        None
    };

    println!();
    println!(
        "{} Worktree ready at {}",
        "✓".green().bold(),
        final_path.display().to_string().cyan().bold()
    );

    let launch_agent = resume_target.as_ref().map_or(agent, |target| target.agent);
    prepare_agent_worktree(launch_agent, &final_path, &repo_root)?;
    if no_agent {
        println!(
            "\n{} Run: {} {} {}",
            "tip:".yellow().bold(),
            "cd".dimmed(),
            final_path.display(),
            format!("&& {}", launch_agent.command()).dimmed()
        );
    } else {
        let bg_color = pick_available_color(&final_path);
        save_worktree_color(&final_path, &bg_color)?;
        record_current_iterm_session(&final_path)?;
        let session_name = session_name_from_branch(&pr_details.head_ref_name);

        // Guard ensures iTerm settings are reset even on Ctrl+C or panic
        let _iterm_guard = ItermGuard::new(&bg_color, &session_name);

        let system_prompt = build_worktree_system_prompt();

        if let Some(target) = &resume_target {
            println!();
            println!(
                "{} Resuming last {} session...",
                "→".blue().bold(),
                target.agent.display_name(),
            );
            println!();
            spawn_agent_continue(
                target.agent,
                &final_path,
                Some(&system_prompt),
                target.resume_id.as_deref(),
                &session_name,
            )?;
        } else {
            let full_prompt = match chained_skill {
                Some(skill) => format!("{} {}\n\nAfter completing the above, run: {}", initial_prompt, pr_number, skill),
                None => format!("{} {}", initial_prompt, pr_number),
            };
            println!();
            println!(
                "{} Spawning {} with {}...",
                "→".blue().bold(),
                agent.display_name(),
                full_prompt.cyan()
            );
            println!();
            spawn_agent_with_prompt(
                agent,
                &final_path,
                &full_prompt,
                Some(&system_prompt),
                &session_name,
            )?;
        }
    }

    if let Some(handle) = bg_handle {
        let _ = handle.join();
    }

    Ok(())
}

fn run_branch(
    name: &str,
    no_agent: bool,
    prompt: Option<String>,
    repo: Option<PathBuf>,
    agent: Agent,
    resume_existing: bool,
) -> Result<(), String> {
    timing!("run_branch");
    let branch_name = name.to_string();

    println!(
        "{} Branch {}",
        "→".blue().bold(),
        branch_name.cyan()
    );

    let repo_root = repo.unwrap_or_else(default_repo_root);

    if !repo_root.exists() {
        return Err(format!("Repo not found at {}", repo_root.display()));
    }

    // Create slug from branch name (strip any prefix like darren/ for the directory name)
    let slug = branch_name.rsplit('/').next().unwrap_or(&branch_name);
    let worktree_dir = default_worktree_dir();
    let worktree_path = worktree_dir.join(format!("branch-{}", slug));

    let existing = find_branch_worktree(&repo_root, &branch_name)?;

    let mut resume_target = None;
    let mut is_new_worktree = false;

    let final_path = if let Some(existing_path) = existing {
        println!(
            "\n{} Worktree already exists at {}",
            "!".yellow().bold(),
            existing_path.display().to_string().cyan()
        );

        let available_resume = find_worktree_resume_target(&existing_path);
        let action = if resume_existing {
            available_resume
                .map(ExistingWorktreeAction::ResumeSession)
                .unwrap_or(ExistingWorktreeAction::UseExisting)
        } else {
            let changes_handle = {
                let path = existing_path.clone();
                thread::spawn(move || get_uncommitted_status(&path))
            };
            prompt_existing_worktree_action(changes_handle, agent, available_resume)?
        };

        match action {
            ExistingWorktreeAction::ResumeSession(target) => {
                resume_target = Some(target);
                existing_path
            }
            ExistingWorktreeAction::UseExisting => {
                existing_path
            }
            ExistingWorktreeAction::CreateNew => {
                let new_path = find_next_worktree_path(&worktree_dir, &format!("branch-{}", slug))?;
                create_new_worktree_new_branch(&repo_root, &worktree_dir, &new_path, &branch_name)?;
                is_new_worktree = true;
                new_path
            }
        }
    } else {
        create_new_worktree_new_branch(&repo_root, &worktree_dir, &worktree_path, &branch_name)?;
        is_new_worktree = true;
        worktree_path
    };

    let bg_handle = if is_new_worktree {
        Some(start_new_worktree_setup(final_path.clone(), repo_root.clone())?)
    } else {
        None
    };

    println!();
    println!(
        "{} Worktree ready at {}",
        "✓".green().bold(),
        final_path.display().to_string().cyan().bold()
    );

    let launch_agent = resume_target.as_ref().map_or(agent, |target| target.agent);
    prepare_agent_worktree(launch_agent, &final_path, &repo_root)?;
    if no_agent {
        println!(
            "\n{} Run: {} {} {}",
            "tip:".yellow().bold(),
            "cd".dimmed(),
            final_path.display(),
            format!("&& {}", launch_agent.command()).dimmed()
        );
    } else {
        let bg_color = pick_available_color(&final_path);
        save_worktree_color(&final_path, &bg_color)?;
        record_current_iterm_session(&final_path)?;
        let session_name = session_name_from_branch(&branch_name);

        // Guard ensures iTerm settings are reset even on Ctrl+C or panic
        let _iterm_guard = ItermGuard::new(&bg_color, &session_name);

        let system_prompt = build_worktree_system_prompt();

        if let Some(target) = &resume_target {
            println!();
            println!(
                "{} Resuming last {} session...",
                "→".blue().bold(),
                target.agent.display_name(),
            );
            println!();
            spawn_agent_continue(
                target.agent,
                &final_path,
                Some(&system_prompt),
                target.resume_id.as_deref(),
                &session_name,
            )?;
        } else {
            println!();
            println!(
                "{} Spawning {}...",
                "→".blue().bold(),
                agent.display_name(),
            );
            println!();

            if let Some(prompt) = &prompt {
                spawn_agent_with_prompt(
                    agent,
                    &final_path,
                    prompt,
                    Some(&system_prompt),
                    &session_name,
                )?;
            } else {
                spawn_agent(agent, &final_path, Some(&system_prompt), &session_name)?;
            }
        }
    }

    if let Some(handle) = bg_handle {
        let _ = handle.join();
    }

    Ok(())
}

const ADJECTIVES: &[&str] = &[
    "azure", "bold", "calm", "deft", "eager", "fair", "glad", "hale",
    "keen", "lush", "mild", "neat", "open", "pure", "quick", "rare",
    "sage", "tame", "vast", "warm", "zesty", "brave", "crisp", "dense",
    "fond", "grim", "hazy", "idle", "just", "kind", "lean", "mute",
    "agile", "apt", "arid", "ashen", "avid", "blunt", "brisk", "coral",
    "coy", "damp", "dim", "dry", "dusky", "elfin", "even", "faint",
    "fell", "firm", "flat", "fleet", "flush", "frail", "fresh", "frost",
    "full", "gaunt", "glib", "gold", "grand", "gray", "green", "gruff",
    "gusty", "hardy", "hazel", "hex", "high", "hoary", "hush", "icy",
    "inky", "ivory", "jet", "jolly", "lax", "light", "lithe", "live",
    "lone", "lucid", "lunar", "meek", "misty", "mossy", "new", "numb",
    "oaken", "opal", "pale", "peak", "plaid", "plum", "polar", "proud",
    "quiet", "rapid", "raw", "regal", "rigid", "rocky", "rosy", "rough",
    "rusty", "sandy", "sharp", "sheer", "shy", "silky", "slim", "snowy",
    "solar", "solid", "spare", "stark", "steep", "still", "stony", "stout",
    "swift", "terse", "thin", "tidal", "tiny", "trim", "true", "twin",
    "vivid", "wary", "wavy", "white", "whole", "wide", "wild", "wiry",
];

const NOUNS: &[&str] = &[
    "brook", "cedar", "dune", "elm", "flint", "grove", "heron", "iris",
    "jade", "knoll", "lark", "moss", "nova", "oak", "pine", "quartz",
    "reef", "slate", "thorn", "vale", "wren", "birch", "cliff", "delta",
    "fern", "glade", "hawk", "isle", "junco", "kelp", "lynx", "marsh",
    "alder", "amber", "anvil", "aspen", "basil", "bay", "blaze", "bloom",
    "bolt", "brine", "cairn", "cave", "clam", "cloud", "coal", "colt",
    "cone", "coral", "cove", "crane", "creek", "crest", "crow", "dale",
    "dawn", "doe", "dove", "drift", "dusk", "ember", "finch", "fjord",
    "flame", "flax", "foam", "fog", "forge", "fox", "frost", "gale",
    "gem", "glen", "glow", "goose", "grain", "hare", "hazel", "heath",
    "hedge", "herb", "hive", "holly", "inlet", "ivy", "jay", "kite",
    "lake", "larch", "leaf", "ledge", "lily", "lime", "lodge", "maple",
    "mesa", "mink", "mint", "mist", "moon", "moor", "moth", "nectar",
    "nest", "oat", "olive", "orca", "otter", "owl", "pear", "plum",
    "pond", "poppy", "rain", "ridge", "robin", "root", "rose", "rush",
    "sage", "seal", "seed", "shade", "shell", "shore", "shrub", "smoke",
    "snow", "spark", "spire", "spruce", "star", "stem", "stone", "storm",
    "stork", "swift", "thyme", "tide", "trail", "trout", "tulip", "vine",
    "wave", "wheat", "willow", "wolf", "yew",
];

fn generate_workspace_name(existing: &HashSet<String>) -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as usize;

    let total = ADJECTIVES.len() * NOUNS.len();
    for offset in 0..total {
        let idx = (nanos + offset) % total;
        let adj = ADJECTIVES[idx % ADJECTIVES.len()];
        let noun = NOUNS[idx / ADJECTIVES.len()];
        let name = format!("{}-{}", adj, noun);
        if !existing.contains(&name) {
            return name;
        }
    }

    // Fallback: all names taken (very unlikely), append timestamp
    let adj = ADJECTIVES[nanos % ADJECTIVES.len()];
    let noun = NOUNS[(nanos / ADJECTIVES.len()) % NOUNS.len()];
    format!("{}-{}-{}", adj, noun, nanos)
}

/// Check if a worktree directory name matches the adjective-noun pattern from `checkout new`.
fn is_checkout_new_worktree(dir_name: &str) -> bool {
    let slug = dir_name.strip_prefix("branch-").unwrap_or(dir_name);
    let parts: Vec<&str> = slug.splitn(2, '-').collect();
    if parts.len() != 2 {
        return false;
    }
    ADJECTIVES.contains(&parts[0]) && NOUNS.contains(&parts[1])
}

/// Find local `darren/<adj>-<noun>` branches older than `max_age_days` that
/// aren't currently checked out by any worktree. Returns `(branch, age_days)`
/// sorted oldest-first.
fn find_stale_workspace_branches(
    repo_root: &Path,
    active_branches: &HashSet<String>,
    max_age_days: u64,
) -> Result<Vec<(String, u64)>, String> {
    let output = Command::new("git")
        .args([
            "-C", &repo_root.to_string_lossy(),
            "for-each-ref",
            "--format=%(refname:short) %(committerdate:unix)",
            "refs/heads/darren/",
        ])
        .output()
        .map_err(|e| format!("Failed to list branches: {}", e))?;
    if !output.status.success() {
        return Err(format!(
            "git for-each-ref failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let max_age_secs = max_age_days * 24 * 60 * 60;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut stale: Vec<(String, u64)> = Vec::new();
    for line in stdout.lines() {
        let mut parts = line.splitn(2, ' ');
        let name = parts.next().unwrap_or("").trim();
        let ts: u64 = parts.next().unwrap_or("0").trim().parse().unwrap_or(0);
        if name.is_empty() || active_branches.contains(name) {
            continue;
        }
        let slug = name.strip_prefix("darren/").unwrap_or(name);
        if !is_checkout_new_worktree(slug) {
            continue;
        }
        let age = now.saturating_sub(ts);
        if age >= max_age_secs {
            stale.push((name.to_string(), age / 86_400));
        }
    }
    stale.sort_by(|a, b| b.1.cmp(&a.1));
    Ok(stale)
}

/// Find the oldest idle worktree created by `checkout new`: no alive session
/// PID, and has an `.exited` marker that says "clean". Legacy worktrees closed
/// before the SIGHUP fix won't be reused until they're closed once with the
/// fixed binary. `git status` is deliberately not used here — it's slow enough
/// that running it on tens of worktrees noticeably delays `checkout begin`.
fn find_reusable_worktree(repo_root: &PathBuf) -> Result<Option<PathBuf>, String> {
    timing!("find_reusable_worktree");
    let entries = list_worktree_paths(repo_root)?;

    let mut candidates: Vec<(u64, PathBuf)> = Vec::new();

    for (path, _branch) in &entries {
        let dir_name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n,
            None => continue,
        };

        if !is_checkout_new_worktree(dir_name) {
            continue;
        }

        if let Some(pid) = read_session_pid(path) {
            if is_pid_alive(pid) {
                continue;
            }
        }

        // Must have an `.exited` marker that recorded "clean" at close time.
        let content = match fs::read_to_string(session_exited_file(path)) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let Some(exited) = parse_exited_session(&content) else {
            continue;
        };
        if !exited.clean {
            continue;
        }

        candidates.push((exited.timestamp, path.clone()));
    }

    // Pick the oldest (earliest exit timestamp) so freshly-closed worktrees
    // remain easy to re-enter.
    candidates.sort_by_key(|(ts, _)| *ts);
    Ok(candidates.into_iter().next().map(|(_, path)| path))
}

/// Remove stale `*.lock` files in the worktree's per-worktree git dir.
///
/// A crashed prior session can leave `index.lock` (or similar) behind, which
/// then blocks every git operation in the recycled worktree with a confusing
/// "Another git process seems to be running" error. We only call this when
/// recycling an idle worktree — no live session should be holding a real lock.
fn clear_stale_worktree_locks(worktree_path: &Path) {
    let output = match Command::new("git")
        .args(["-C", &worktree_path.to_string_lossy(), "rev-parse", "--git-dir"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return,
    };
    let git_dir = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    let entries = match fs::read_dir(&git_dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_lock = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.ends_with(".lock"))
            .unwrap_or(false);
        if !is_lock {
            continue;
        }
        // Only remove locks older than 60s as a paranoia belt — a live process
        // would still be actively touching it.
        let too_old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|d| d > Duration::from_secs(60))
            .unwrap_or(false);
        if !too_old {
            continue;
        }
        if fs::remove_file(&path).is_ok() {
            println!(
                "{} Cleared stale lock {}",
                "→".blue().bold(),
                path.display().to_string().dimmed()
            );
        }
    }
}

fn reset_worktree_to_master(worktree_path: &PathBuf) -> Result<(), String> {
    timing!("reset_worktree_to_master");
    clear_stale_worktree_locks(worktree_path);
    // Fetch latest master
    print!("{} Fetching latest master... ", "→".blue().bold());
    std::io::stdout().flush().ok();
    let output = Command::new("git")
        .args(["-C", &worktree_path.to_string_lossy(), "fetch", "origin", "master"])
        .output()
        .map_err(|e| format!("Failed to spawn git fetch: {}", e))?;
    if !output.status.success() {
        println!("{}", "error".red());
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "git fetch origin master failed in {} (exit {}):\n{}",
            worktree_path.display(),
            output.status.code().map(|c| c.to_string()).unwrap_or_else(|| "?".into()),
            stderr.trim()
        ));
    }
    println!("{}", "done".green());

    // Reset branch to origin/master
    print!("{} Resetting to latest master... ", "→".blue().bold());
    std::io::stdout().flush().ok();
    let output = Command::new("git")
        .args(["-C", &worktree_path.to_string_lossy(), "reset", "--hard", "origin/master"])
        .output()
        .map_err(|e| format!("Failed to spawn git reset: {}", e))?;
    if !output.status.success() {
        println!("{}", "error".red());
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(format!(
            "git reset --hard origin/master failed in {} (exit {}):\n{}{}{}",
            worktree_path.display(),
            output.status.code().map(|c| c.to_string()).unwrap_or_else(|| "?".into()),
            stderr.trim(),
            if !stderr.trim().is_empty() && !stdout.trim().is_empty() { "\n" } else { "" },
            stdout.trim()
        ));
    }
    println!("{}", "done".green());

    Ok(())
}

fn run_new(no_agent: bool, prompt: Option<String>, repo: Option<PathBuf>, agent: Agent) -> Result<(), String> {
    timing!("run_new");
    let repo_root = repo.clone().unwrap_or_else(default_repo_root);

    if !repo_root.exists() {
        return Err(format!("Repo not found at {}", repo_root.display()));
    }

    // Collect existing worktree names so we don't generate a collision
    let existing_names: HashSet<String> = list_worktree_paths(&repo_root)?
        .iter()
        .filter_map(|(path, _)| {
            path.file_name()
                .and_then(|f| f.to_str())
                .and_then(|s| s.strip_prefix("branch-"))
                .map(|s| s.to_string())
        })
        .collect();

    // Try to reuse an idle scratch worktree
    if let Some(reusable) = find_reusable_worktree(&repo_root)? {
        let workspace_name = generate_workspace_name(&existing_names);
        let branch_name = format!("darren/{}", workspace_name);

        let old_dir = reusable.file_name().unwrap().to_string_lossy().to_string();
        let old_name = old_dir.strip_prefix("branch-").unwrap_or(&old_dir);

        println!(
            "{} Recycling idle workspace {} {} {}",
            "→".blue().bold(),
            old_name.dimmed(),
            "→".dimmed(),
            workspace_name.cyan()
        );

        reset_worktree_to_master(&reusable)?;

        // Create a new branch for this workspace
        let output = Command::new("git")
            .args(["-C", &reusable.to_string_lossy(), "checkout", "-b", &branch_name])
            .output()
            .map_err(|e| format!("Failed to spawn git checkout: {}", e))?;
        if !output.status.success() {
            return Err(format!(
                "git checkout -b {} failed in {} (exit {}):\n{}",
                branch_name,
                reusable.display(),
                output.status.code().map(|c| c.to_string()).unwrap_or_else(|| "?".into()),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }

        // Rename the worktree directory to match the new workspace name
        let new_path = reusable.parent().unwrap().join(format!("branch-{}", workspace_name));
        let output = Command::new("git")
            .args([
                "-C", &repo_root.to_string_lossy(),
                "worktree", "move",
                &reusable.to_string_lossy(),
                &new_path.to_string_lossy(),
            ])
            .output()
            .map_err(|e| format!("Failed to spawn git worktree move: {}", e))?;
        if !output.status.success() {
            return Err(format!(
                "git worktree move {} → {} failed (exit {}):\n{}",
                reusable.display(),
                new_path.display(),
                output.status.code().map(|c| c.to_string()).unwrap_or_else(|| "?".into()),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }

        // `git worktree move` doesn't rename the metadata dir under
        // `.git/worktrees/`. Rename it to match so that the working-dir name
        // and the metadata-dir name stay in sync across recycles.
        let new_metadata_name = format!("branch-{}", workspace_name);
        rename_worktree_metadata(&new_path, &new_metadata_name)?;

        let bg_handle = start_new_worktree_setup(new_path.clone(), repo_root.clone())?;

        println!();
        println!(
            "{} Worktree ready at {}",
            "✓".green().bold(),
            new_path.display().to_string().cyan().bold()
        );

        prepare_agent_worktree(agent, &new_path, &repo_root)?;
        if no_agent {
            println!(
                "\n{} Run: {} {} {}",
                "tip:".yellow().bold(),
                "cd".dimmed(),
                new_path.display(),
                format!("&& {}", agent.command()).dimmed()
            );
        } else {
            let bg_color = pick_available_color(&new_path);
            save_worktree_color(&new_path, &bg_color)?;
            record_current_iterm_session(&new_path)?;
            let session_name = session_name_from_branch(&branch_name);

            let _iterm_guard = ItermGuard::new(&bg_color, &session_name);

            let system_prompt = build_worktree_system_prompt();

            println!();
            println!(
                "{} Spawning {}...",
                "→".blue().bold(),
                agent.display_name(),
            );
            println!();

            if let Some(prompt) = &prompt {
                spawn_agent_with_prompt(
                    agent,
                    &new_path,
                    prompt,
                    Some(&system_prompt),
                    &session_name,
                )?;
            } else {
                spawn_agent(agent, &new_path, Some(&system_prompt), &session_name)?;
            }
        }

        let _ = bg_handle.join();

        return Ok(());
    }

    // No reusable worktree, create a new one
    let workspace_name = generate_workspace_name(&existing_names);
    let branch_name = format!("darren/{}", workspace_name);

    println!(
        "{} New workspace {}",
        "→".blue().bold(),
        workspace_name.cyan()
    );

    run_branch(&branch_name, no_agent, prompt, repo, agent, false)
}

#[derive(Clone)]
struct WorktreeInfo {
    path: PathBuf,
    branch: String,
    has_changes: bool,
    has_active_session: bool,
    active_agent: Option<Agent>,
    orphaned_pids: Vec<u32>,
}

fn get_all_worktrees(repo_root: &PathBuf) -> Result<Vec<WorktreeInfo>, String> {
    let output = Command::new("git")
        .args(["-C", &repo_root.to_string_lossy(), "worktree", "list", "--porcelain"])
        .output()
        .map_err(|e| format!("Failed to list worktrees: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    // First pass: collect paths and branches
    let mut entries: Vec<(PathBuf, String)> = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_branch: Option<String> = None;

    for line in stdout.lines() {
        if let Some(path_str) = line.strip_prefix("worktree ") {
            current_path = Some(PathBuf::from(path_str));
        } else if let Some(branch_str) = line.strip_prefix("branch refs/heads/") {
            current_branch = Some(branch_str.to_string());
        } else if line.is_empty() {
            if let Some(path) = current_path.take() {
                if path != *repo_root {
                    let branch = current_branch.take().unwrap_or_else(|| "(detached)".to_string());
                    entries.push((path, branch));
                }
            }
            current_branch = None;
        }
    }

    // Check active sessions first (fast PID file reads) so we can skip
    // expensive git-status calls for worktrees we're keeping anyway.
    let session_status: Vec<bool> = entries
        .iter()
        .map(|(path, _)| {
            if let Some(pid) = read_session_pid(path) {
                if is_pid_alive(pid) {
                    true
                } else {
                    remove_session_pid(path); // stale PID file from a crash
                    false
                }
            } else {
                false
            }
        })
        .collect();

    // Only spawn git-status for inactive worktrees
    let children: Vec<_> = entries
        .iter()
        .zip(&session_status)
        .map(|((path, _), &active)| {
            if active {
                None
            } else {
                Command::new("git")
                    .args(["-C", &path.to_string_lossy(), "status", "--porcelain"])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                    .ok()
            }
        })
        .collect();

    // Collect results
    let mut worktrees: Vec<WorktreeInfo> = entries
        .into_iter()
        .zip(children)
        .zip(session_status)
        .map(|(((path, branch), child), has_active_session)| {
            let has_changes = child
                .and_then(|c| c.wait_with_output().ok())
                .map(|o| {
                    let stdout = String::from_utf8_lossy(&o.stdout);
                    stdout.lines().any(|l| !l.trim().is_empty() && !l.starts_with("??"))
                })
                .unwrap_or(false);
            let active_agent = has_active_session
                .then(|| read_session_agent(&path).unwrap_or(Agent::Claude));
            let orphaned_pids: Vec<u32> = Vec::new();
            WorktreeInfo { path, branch, has_changes, has_active_session, active_agent, orphaned_pids }
        })
        .collect();

    // Sort by path for consistent output
    worktrees.sort_by(|a, b| a.path.cmp(&b.path));

    Ok(worktrees)
}

fn run_status(repo: Option<PathBuf>) -> Result<(), String> {
    timing!("run_status");
    let repo_root = repo.unwrap_or_else(default_repo_root);

    if !repo_root.exists() {
        return Err(format!("Repo not found at {}", repo_root.display()));
    }

    let worktrees = get_all_worktrees(&repo_root)?;

    if worktrees.is_empty() {
        println!("{} No worktrees found", "→".blue().bold());
        return Ok(());
    }

    println!(
        "{} {} worktree(s) found:\n",
        "→".blue().bold(),
        worktrees.len()
    );

    for wt in &worktrees {
        let status = if wt.has_active_session {
            format!(
                "active {}",
                wt.active_agent.unwrap_or_default().command()
            )
            .blue()
            .bold()
        } else if wt.has_changes {
            "modified".yellow().bold()
        } else if !wt.orphaned_pids.is_empty() {
            "orphaned".magenta().bold()
        } else {
            "clean".green()
        };

        let dir_name = wt.path.file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| wt.path.display().to_string());

        println!(
            "  {} {} {}",
            format!("[{}]", status).to_string(),
            dir_name.cyan(),
            format!("({})", wt.branch).dimmed()
        );
    }

    Ok(())
}

fn remove_worktrees(worktrees: &[WorktreeInfo], repo_root: &PathBuf) -> Result<(), String> {
    let mut removed_count = 0;
    let mut failed: Vec<String> = Vec::new();

    for wt in worktrees {
        let dir_name = wt.path.file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| wt.path.display().to_string());

        for pid in &wt.orphaned_pids {
            print!(
                "{} Killing orphaned agent process (pid {})... ",
                "→".blue().bold(),
                pid.to_string().dimmed()
            );
            std::io::stdout().flush().ok();

            let kill_result = Command::new("kill")
                .arg(pid.to_string())
                .status();

            match kill_result {
                Ok(s) if s.success() => println!("{}", "done".green()),
                _ => println!("{}", "failed (may have already exited)".yellow()),
            }
        }

        print!("{} Removing {}... ", "→".blue().bold(), dir_name.cyan());
        std::io::stdout().flush().ok();

        let repo_str = repo_root.to_string_lossy();
        let wt_str = wt.path.to_string_lossy();
        // Always --force: the caller has already confirmed removal, and without
        // it git refuses to delete directories containing gitignored files
        // (e.g. .DS_Store, build artifacts) even though they show as "clean".
        let args = vec!["-C", &repo_str, "worktree", "remove", "--force", &wt_str];

        let output = Command::new("git")
            .args(&args)
            .output()
            .map_err(|e| format!("Failed to remove worktree: {}", e))?;

        if output.status.success() {
            let color_file = worktree_color_file(&wt.path);
            let _ = fs::remove_file(color_file);
            let _ = fs::remove_file(session_name_file(&wt.path));
            let _ = fs::remove_file(worktree_iterm_session_file(&wt.path));
            remove_session_pid(&wt.path);
            remove_bazel_output_base(&wt.path);

            println!("{}", "done".green());
            removed_count += 1;
        } else {
            // `git worktree remove` is not atomic: it drops the worktree's admin
            // registration (`.git/worktrees/<id>`) *before* recursively deleting
            // the working directory. If that delete fails (commonly "Directory not
            // empty" — a race with a process writing into a monorepo checkout, e.g.
            // watchman/bazel/an IDE), git leaves an orphaned directory that it will
            // no longer recognize ("is not a working tree"), so a retry can never
            // recover it. Fall back to deleting the directory ourselves and pruning
            // the (possibly stale) registration.
            let stderr = String::from_utf8_lossy(&output.stderr);
            let error_msg = stderr.trim();

            let recovered = if wt.path.exists() {
                match fs::remove_dir_all(&wt.path) {
                    Ok(()) => true,
                    Err(e) => {
                        println!("{}", "failed".red());
                        if !error_msg.is_empty() {
                            println!("    {} {}", "error:".red(), error_msg);
                        }
                        println!("    {} manual cleanup failed: {}", "error:".red(), e);
                        false
                    }
                }
            } else {
                // Directory already gone; git just needs to prune stale metadata.
                true
            };

            if recovered {
                // Prune the stale registration git left behind, then run the same
                // per-worktree cleanup the success path does.
                let _ = Command::new("git")
                    .args(["-C", &repo_str, "worktree", "prune"])
                    .output();
                let color_file = worktree_color_file(&wt.path);
                let _ = fs::remove_file(color_file);
                let _ = fs::remove_file(session_name_file(&wt.path));
                let _ = fs::remove_file(worktree_iterm_session_file(&wt.path));
                remove_session_pid(&wt.path);
                remove_bazel_output_base(&wt.path);

                println!("{}", "done (manual cleanup)".green());
                removed_count += 1;
            } else {
                failed.push(dir_name);
            }
        }
    }

    if removed_count > 0 {
        println!(
            "{} Removed {} worktree(s)",
            "✓".green().bold(),
            removed_count
        );
    }

    if !failed.is_empty() {
        println!(
            "{} Failed to remove {} worktree(s): {}",
            "✗".red().bold(),
            failed.len(),
            failed.join(", ")
        );
    }

    Ok(())
}

fn run_clean(repo: Option<PathBuf>, skip_confirm: bool) -> Result<(), String> {
    timing!("run_clean");
    let repo_root = repo.unwrap_or_else(default_repo_root);

    if !repo_root.exists() {
        return Err(format!("Repo not found at {}", repo_root.display()));
    }

    let worktrees = get_all_worktrees(&repo_root)?;

    if worktrees.is_empty() {
        println!("{} No worktrees found", "→".blue().bold());
        return Ok(());
    }

    // Orphaned processes are removable because remove_worktrees terminates them.
    let removable_worktrees: Vec<_> = worktrees.iter().filter(|w| !w.has_changes && !w.has_active_session).collect();
    let modified_worktrees: Vec<_> = worktrees.iter().filter(|w| w.has_changes && !w.has_active_session).collect();
    let active_worktrees: Vec<_> = worktrees.iter().filter(|w| w.has_active_session).collect();

    // Among removable worktrees, keep the N most recently exited reusable ones
    // so `checkout begin` can reuse them instead of creating new worktrees.
    const REUSABLE_POOL_SIZE: usize = 5;

    let kept_for_reuse: HashSet<PathBuf> = {
        let mut reusable: Vec<(u64, &PathBuf)> = removable_worktrees.iter()
            .filter(|w| {
                let dir_name = w.path.file_name().and_then(|s| s.to_str()).unwrap_or("");
                is_checkout_new_worktree(dir_name)
            })
            .map(|w| {
                let ts = fs::read_to_string(session_exited_file(&w.path))
                    .ok()
                    .and_then(|c| c.lines().next().and_then(|l| l.trim().parse::<u64>().ok()))
                    .unwrap_or(0);
                (ts, &w.path)
            })
            .collect();
        // Most recent exits first
        reusable.sort_by(|a, b| b.0.cmp(&a.0));
        reusable.into_iter().take(REUSABLE_POOL_SIZE).map(|(_, p)| p.clone()).collect()
    };

    let actually_removing: Vec<_> = removable_worktrees.iter().filter(|w| !kept_for_reuse.contains(&w.path)).collect();
    let kept_worktrees: Vec<_> = removable_worktrees.iter().filter(|w| kept_for_reuse.contains(&w.path)).collect();

    if !actually_removing.is_empty() {
        println!(
            "{} Removing {} worktree(s):\n",
            "→".blue().bold(),
            actually_removing.len()
        );

        for wt in &actually_removing {
            let dir_name = wt.path.file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| wt.path.display().to_string());

            let orphan_note = if !wt.orphaned_pids.is_empty() {
                format!(
                    " {} {}",
                    format!("(killing {} orphaned agent process{})", wt.orphaned_pids.len(),
                        if wt.orphaned_pids.len() == 1 { "" } else { "es" }).yellow(),
                    format!("pid {}", wt.orphaned_pids.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ")).dimmed()
                )
            } else {
                String::new()
            };

            println!(
                "  {} {} {}{}",
                format!("[{}]", "remove".red()),
                dir_name.cyan(),
                format!("({})", wt.branch).dimmed(),
                orphan_note
            );
        }
    }

    if !kept_worktrees.is_empty() {
        if !actually_removing.is_empty() {
            println!();
        }
        println!(
            "{} Keeping {} worktree(s) for reuse:\n",
            "→".blue().bold(),
            kept_worktrees.len()
        );

        for wt in &kept_worktrees {
            let dir_name = wt.path.file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| wt.path.display().to_string());

            println!(
                "  {} {} {}",
                format!("[{}]", "reuse".green().bold()),
                dir_name.cyan(),
                format!("({})", wt.branch).dimmed()
            );
        }
    }

    if !active_worktrees.is_empty() {
        if !actually_removing.is_empty() || !kept_worktrees.is_empty() {
            println!();
        }
        println!(
            "{} Keeping {} worktree(s) with active agent sessions:\n",
            "→".blue().bold(),
            active_worktrees.len()
        );

        for wt in &active_worktrees {
            let dir_name = wt.path.file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| wt.path.display().to_string());
            let active_status = format!("[{}]", "active".blue().bold());
            let branch_status = format!("({})", wt.branch).dimmed();

            println!(
                "  {} {} {} {}",
                active_status,
                dir_name.cyan(),
                branch_status,
                wt.active_agent.unwrap_or_default().display_name().dimmed()
            );
        }
    }

    if !modified_worktrees.is_empty() {
        if !actually_removing.is_empty() || !kept_worktrees.is_empty() || !active_worktrees.is_empty() {
            println!();
        }
        println!(
            "{} {} worktree(s) with uncommitted changes (will prompt individually):\n",
            "→".blue().bold(),
            modified_worktrees.len()
        );

        for wt in &modified_worktrees {
            let dir_name = wt.path.file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| wt.path.display().to_string());

            println!(
                "  {} {} {}",
                format!("[{}]", "modified".yellow().bold()),
                dir_name.cyan(),
                format!("({})", wt.branch).dimmed()
            );
        }
    }

    // Partition into owned vecs for removal, excluding those kept for reuse
    let (removable, modified): (Vec<_>, Vec<_>) = worktrees.into_iter()
        .filter(|w| !w.has_active_session && !kept_for_reuse.contains(&w.path))
        .partition(|w| !w.has_changes);

    if removable.is_empty() && modified.is_empty() {
        println!("\n{} Nothing to remove", "→".blue().bold());
        return Ok(());
    }

    // Kick off `git status --short` for each modified worktree in the background
    // so the detailed file list is ready by the time we prompt. Prompting is
    // sequential and gated on user input, so these run for free while the UI
    // (the clean-worktree batch prompt and earlier per-worktree prompts) waits.
    let mut status_handles: HashMap<PathBuf, thread::JoinHandle<Result<Option<String>, String>>> =
        HashMap::new();
    for w in &modified {
        let path = w.path.clone();
        status_handles.insert(w.path.clone(), thread::spawn(move || get_uncommitted_status(&path)));
    }
    let modified_paths = modified.iter().map(|worktree| worktree.path.clone()).collect();
    let agent_session_times = find_agent_session_times(&modified_paths);

    // Collect all confirmations upfront before any removals
    let mut all_to_remove: Vec<WorktreeInfo> = Vec::new();

    // Batch-confirm clean worktrees
    if !removable.is_empty() {
        if !skip_confirm {
            println!();
            print!(
                "{} Remove {} clean worktree(s)? [y/N]: ",
                "?".magenta().bold(),
                removable.len()
            );
            io::stdout().flush().map_err(|e| e.to_string())?;

            let mut input = String::new();
            io::stdin()
                .read_line(&mut input)
                .map_err(|e| format!("Failed to read input: {}", e))?;

            if input.trim().to_lowercase() == "y" {
                all_to_remove.extend(removable);
            }
        } else {
            all_to_remove.extend(removable);
        }
    }

    // Prompt individually for modified worktrees
    if !modified.is_empty() {
        println!();
        for wt in modified {
            let dir_name = wt.path.file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| wt.path.display().to_string());

            // Worktree creation time (birthtime on macOS; falls back to mtime).
            let created_str = fs::metadata(&wt.path)
                .and_then(|m| m.created().or_else(|_| m.modified()))
                .map(format_time_ago)
                .unwrap_or_else(|_| "unknown".to_string());

            let session_str = agent_session_times
                .get(&wt.path)
                .copied()
                .map(format_time_ago)
                .unwrap_or_else(|| "none".to_string());

            println!(
                "{} {} {}",
                "→".blue().bold(),
                dir_name.cyan(),
                "(has uncommitted changes)".yellow()
            );
            println!(
                "    {}",
                format!("created {} · last agent session {}", created_str, session_str).dimmed()
            );
            let status_result = status_handles.remove(&wt.path)
                .and_then(|h| h.join().ok())
                .unwrap_or_else(|| get_uncommitted_status(&wt.path));
            if let Ok(Some(status)) = status_result {
                let lines: Vec<&str> = status.lines().collect();
                const MAX_SHOWN: usize = 10;
                for line in lines.iter().take(MAX_SHOWN) {
                    println!("    {}", line.dimmed());
                }
                if lines.len() > MAX_SHOWN {
                    println!("    {}", format!("... and {} more", lines.len() - MAX_SHOWN).dimmed());
                }
            }

            print!(
                "{} Remove {}? [y/N]: ",
                "?".magenta().bold(),
                dir_name.cyan()
            );
            io::stdout().flush().map_err(|e| e.to_string())?;

            let mut input = String::new();
            io::stdin()
                .read_line(&mut input)
                .map_err(|e| format!("Failed to read input: {}", e))?;

            if input.trim().to_lowercase() == "y" {
                all_to_remove.push(wt);
            }
        }
    }

    // Remove everything that was confirmed
    if !all_to_remove.is_empty() {
        println!();
        remove_worktrees(&all_to_remove, &repo_root)?;
    }

    // Clean up stale workspace branches (darren/<adj>-<noun>, older than 7 days,
    // not currently checked out by any worktree). These accumulate because
    // `git worktree remove` doesn't delete the branch.
    const STALE_BRANCH_AGE_DAYS: u64 = 7;
    let active_branches: HashSet<String> = get_all_worktrees(&repo_root)?
        .into_iter()
        .map(|w| w.branch)
        .collect();
    let stale = find_stale_workspace_branches(&repo_root, &active_branches, STALE_BRANCH_AGE_DAYS)?;
    if !stale.is_empty() {
        println!();
        println!(
            "{} Found {} stale workspace branch(es) (older than {} days):\n",
            "→".blue().bold(),
            stale.len(),
            STALE_BRANCH_AGE_DAYS,
        );
        for (name, age_days) in &stale {
            println!(
                "  {} {} {}",
                format!("[{}]", "stale".red()),
                name.cyan(),
                format!("({} days old)", age_days).dimmed(),
            );
        }

        let confirmed = if skip_confirm {
            true
        } else {
            println!();
            print!(
                "{} Delete {} stale branch(es)? [y/N]: ",
                "?".magenta().bold(),
                stale.len(),
            );
            io::stdout().flush().map_err(|e| e.to_string())?;
            let mut input = String::new();
            io::stdin()
                .read_line(&mut input)
                .map_err(|e| format!("Failed to read input: {}", e))?;
            input.trim().to_lowercase() == "y"
        };

        if confirmed {
            println!();
            let mut deleted = 0usize;
            let mut failed: Vec<String> = Vec::new();
            for (name, _) in &stale {
                print!("{} Deleting {}... ", "→".blue().bold(), name.cyan());
                io::stdout().flush().ok();
                let output = Command::new("git")
                    .args(["-C", &repo_root.to_string_lossy(), "branch", "-D", name])
                    .output()
                    .map_err(|e| format!("Failed to spawn git branch -D: {}", e))?;
                if output.status.success() {
                    println!("{}", "done".green());
                    deleted += 1;
                } else {
                    println!("{}", "failed".red());
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let msg = stderr.trim();
                    if !msg.is_empty() {
                        println!("    {} {}", "error:".red(), msg);
                    }
                    failed.push(name.clone());
                }
            }
            if deleted > 0 {
                println!(
                    "{} Deleted {} stale branch(es)",
                    "✓".green().bold(),
                    deleted,
                );
            }
            if !failed.is_empty() {
                println!(
                    "{} Failed to delete {} branch(es): {}",
                    "✗".red().bold(),
                    failed.len(),
                    failed.join(", "),
                );
            }
        }
    }

    Ok(())
}

/// Trust mise configs before the agent starts, then run non-critical setup in
/// the background. Trust is path-based, so this must also run after an idle
/// worktree is moved to a new workspace path.
fn start_new_worktree_setup(
    worktree_path: PathBuf,
    repo_root: PathBuf,
) -> Result<thread::JoinHandle<()>, String> {
    if which_mise().is_some() {
        run_mise_trust(&worktree_path)?;
    }

    Ok(spawn_background_setup(worktree_path, repo_root))
}

/// Spawn non-critical worktree setup steps in the background so the agent can
/// start sooner. Errors are logged to stderr but otherwise ignored.
fn spawn_background_setup(
    worktree_path: PathBuf,
    repo_root: PathBuf,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        if let Err(e) = symlink_node_modules(&worktree_path, &repo_root) {
            eprintln!("background: symlink_node_modules failed: {}", e);
        }

        if let Err(e) = symlink_vendor_bundle(&worktree_path, &repo_root) {
            eprintln!("background: symlink_vendor_bundle failed: {}", e);
        }

        // Validate the bundle against this checkout's Gemfile.lock now that the
        // vendor/ cache is linked, so the first commit's Ruby hooks don't fail.
        if let Err(e) = run_bundle_install(&worktree_path, &repo_root) {
            eprintln!("background: run_bundle_install failed: {}", e);
        }
    })
}

fn create_new_worktree_from_remote(
    repo_root: &PathBuf,
    worktree_dir: &PathBuf,
    worktree_path: &PathBuf,
    branch: &str,
    pr_number: u64,
) -> Result<(), String> {
    timing!("create_new_worktree_from_remote");
    std::fs::create_dir_all(worktree_dir)
        .map_err(|e| format!("Failed to create worktrees dir: {}", e))?;

    print!(
        "{} Fetching branch {}... ",
        "→".blue().bold(),
        branch.yellow()
    );
    std::io::stdout().flush().ok();
    if fetch_branch(repo_root, branch).is_err() {
        // Branch may have been deleted after merge — fetch the PR head ref instead
        let pr_ref = format!("pull/{}/head", pr_number);
        fetch_branch(repo_root, &pr_ref)?;
    }
    println!("{}", "done".green());

    println!(
        "{} Creating worktree at {}",
        "→".blue().bold(),
        worktree_path.display().to_string().cyan()
    );
    create_worktree_from_ref(repo_root, worktree_path, &format!("origin/{}", branch))?;
    if let Some(count) = count_worktree_files(worktree_path) {
        println!("  {} ({} files)", "done".green(), count.to_string().yellow());
    } else {
        println!("  {}", "done".green());
    }

    Ok(())
}

fn create_new_worktree_new_branch(
    repo_root: &PathBuf,
    worktree_dir: &PathBuf,
    worktree_path: &PathBuf,
    branch: &str,
) -> Result<(), String> {
    timing!("create_new_worktree_new_branch");
    std::fs::create_dir_all(worktree_dir)
        .map_err(|e| format!("Failed to create worktrees dir: {}", e))?;

    // Fetch latest master
    print!("{} Fetching latest master... ", "→".blue().bold());
    std::io::stdout().flush().ok();
    fetch_branch(repo_root, "master")?;
    println!("{}", "done".green());

    println!(
        "{} Creating worktree with new branch {}",
        "→".blue().bold(),
        branch.yellow()
    );
    create_worktree_new_branch(repo_root, worktree_path, branch)?;
    if let Some(count) = count_worktree_files(worktree_path) {
        println!("  {} ({} files)", "done".green(), count.to_string().yellow());
    } else {
        println!("  {}", "done".green());
    }

    // Track with graphite
    print!("{} Tracking with Graphite... ", "→".blue().bold());
    std::io::stdout().flush().ok();
    run_gt_track(worktree_path)?;
    println!("{}", "done".green());

    Ok(())
}

/// Returns the short status output if there are uncommitted changes, None otherwise.
fn get_uncommitted_status(worktree_path: &PathBuf) -> Result<Option<String>, String> {
    timing!("get_uncommitted_status");
    let output = Command::new("git")
        .args(["-C", &worktree_path.to_string_lossy(), "status", "--short"])
        .output()
        .map_err(|e| format!("Failed to check git status: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        Ok(None)
    } else {
        Ok(Some(stdout))
    }
}

fn prompt_existing_worktree_action(
    changes_handle: thread::JoinHandle<Result<Option<String>, String>>,
    selected_agent: Agent,
    mut resume_target: Option<ResumeTarget>,
) -> Result<ExistingWorktreeAction, String> {
    println!();
    let (use_existing_choice, create_new_choice, valid_choices) = if let Some(target) = &resume_target {
        println!(
            "  {} {}",
            "[1]".cyan().bold(),
            resume_option_label(selected_agent, target)
        );
        println!(
            "  {} Use existing worktree",
            "[2]".cyan().bold()
        );
        println!(
            "  {} Create new worktree",
            "[3]".cyan().bold()
        );
        ("2", "3", "1/2/3")
    } else {
        println!(
            "  {} Use existing worktree {}",
            "[1]".cyan().bold(),
            "(no session found to resume)".dimmed()
        );
        println!(
            "  {} Create new worktree",
            "[2]".cyan().bold()
        );
        ("1", "2", "1/2")
    };
    println!();

    loop {
        print!(
            "{} Choose an option [{}]: ",
            "?".magenta().bold(),
            valid_choices
        );
        io::stdout().flush().map_err(|e| e.to_string())?;

        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .map_err(|e| format!("Failed to read input: {}", e))?;

        match input.trim() {
            "1" if resume_target.is_some() => {
                return Ok(ExistingWorktreeAction::ResumeSession(
                    resume_target.take().unwrap(),
                ));
            }
            choice if choice == use_existing_choice => {
                let status = changes_handle
                    .join()
                    .map_err(|_| "Failed to check git status".to_string())??;
                if let Some(changes) = status {
                    // Separate tracked changes from untracked files
                    let tracked: Vec<&str> = changes.lines().filter(|l| !l.starts_with("??")).collect();
                    let untracked: Vec<&str> = changes.lines().filter(|l| l.starts_with("??")).collect();

                    if !untracked.is_empty() {
                        println!();
                        println!(
                            "{} Worktree has untracked files {}:",
                            "!".yellow().bold(),
                            "(will be kept)".dimmed()
                        );
                        for line in &untracked {
                            println!("  {}", line.dimmed());
                        }
                    }

                    if !tracked.is_empty() {
                        println!();
                        println!(
                            "{} Worktree has uncommitted changes:",
                            "!".yellow().bold()
                        );
                        for line in &tracked {
                            println!("  {}", line.dimmed());
                        }
                        println!();
                        print!(
                            "{} Discard these changes? [y/N]: ",
                            "!".yellow().bold()
                        );
                        io::stdout().flush().map_err(|e| e.to_string())?;

                        let mut confirm = String::new();
                        io::stdin()
                            .read_line(&mut confirm)
                            .map_err(|e| format!("Failed to read input: {}", e))?;

                        if confirm.trim().to_lowercase() != "y" {
                            println!("{} Cancelled", "→".blue().bold());
                            std::process::exit(0);
                        }
                    }
                }
                return Ok(ExistingWorktreeAction::UseExisting);
            }
            choice if choice == create_new_choice => return Ok(ExistingWorktreeAction::CreateNew),
            _ => {
                println!(
                    "{} Invalid option, please enter {}",
                    "!".red().bold(),
                    valid_choices.replace('/', ", ")
                );
            }
        }
    }
}

fn resume_option_label(selected_agent: Agent, target: &ResumeTarget) -> String {
    if selected_agent == target.agent {
        format!(
            "Resume last {} session (keep changes, skip update)",
            target.agent.display_name()
        )
    } else {
        format!(
            "Resume last {} session (switch from selected {}; keep changes, skip update)",
            target.agent.display_name(),
            selected_agent.display_name()
        )
    }
}

/// Rename a worktree's metadata directory under `.git/worktrees/` to match its
/// new working-dir name. `git worktree move` only renames the working dir; the
/// metadata dir keeps its original name, so a recycled worktree ends up with
/// `branch-coy-oak/.git` pointing at `.git/worktrees/branch-rusty-marsh`.
fn rename_worktree_metadata(worktree_path: &Path, new_metadata_name: &str) -> Result<(), String> {
    let dot_git = worktree_path.join(".git");
    let pointer = fs::read_to_string(&dot_git)
        .map_err(|e| format!("Failed to read {}: {}", dot_git.display(), e))?;
    let old_metadata_path = pointer
        .lines()
        .next()
        .and_then(|l| l.strip_prefix("gitdir:"))
        .ok_or_else(|| format!("{} is not a gitdir pointer file", dot_git.display()))?
        .trim();
    let old_metadata_path = PathBuf::from(old_metadata_path);

    let metadata_parent = old_metadata_path
        .parent()
        .ok_or_else(|| format!("metadata path {} has no parent", old_metadata_path.display()))?;
    let new_metadata_path = metadata_parent.join(new_metadata_name);

    if old_metadata_path == new_metadata_path {
        return Ok(());
    }
    if new_metadata_path.exists() {
        return Err(format!(
            "Cannot rename worktree metadata: {} already exists",
            new_metadata_path.display()
        ));
    }

    fs::rename(&old_metadata_path, &new_metadata_path).map_err(|e| {
        format!(
            "Failed to rename worktree metadata {} → {}: {}",
            old_metadata_path.display(),
            new_metadata_path.display(),
            e
        )
    })?;

    let new_pointer = format!("gitdir: {}\n", new_metadata_path.display());
    fs::write(&dot_git, new_pointer)
        .map_err(|e| format!("Failed to update {}: {}", dot_git.display(), e))?;

    let output = Command::new("git")
        .args(["-C", &worktree_path.to_string_lossy(), "rev-parse", "--git-dir"])
        .output()
        .map_err(|e| format!("Failed to verify worktree after metadata rename: {}", e))?;
    if !output.status.success() {
        return Err(format!(
            "git rev-parse failed after metadata rename:\n{}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    Ok(())
}

fn find_next_worktree_path(worktree_dir: &PathBuf, base_name: &str) -> Result<PathBuf, String> {
    let mut suffix = 2;
    loop {
        let candidate = worktree_dir.join(format!("{}-{}", base_name, suffix));
        if !candidate.exists() {
            return Ok(candidate);
        }
        suffix += 1;
        if suffix > 100 {
            return Err("Too many worktrees".to_string());
        }
    }
}

fn extract_pr_number(input: &str) -> Result<u64, String> {
    if let Ok(num) = input.parse::<u64>() {
        return Ok(num);
    }

    let re = Regex::new(r"/pull/(\d+)").unwrap();
    if let Some(caps) = re.captures(input) {
        if let Some(m) = caps.get(1) {
            return m
                .as_str()
                .parse()
                .map_err(|_| "Failed to parse PR number".to_string());
        }
    }

    Err(format!(
        "Could not parse PR number from '{}'. Expected a number or GitHub PR URL.",
        input
    ))
}

fn fetch_pr_details(pr_number: u64, repo_root: &PathBuf) -> Result<PrDetails, String> {
    timing!("fetch_pr_details");
    let output = Command::new("gh")
        .args(["pr", "view", &pr_number.to_string(), "--json", "headRefName,title"])
        .current_dir(repo_root)
        .output()
        .map_err(|e| format!("Failed to run gh: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("gh pr view failed: {}", stderr.trim()));
    }

    serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("Failed to parse PR details: {}", e))
}

fn create_slug(title: &str) -> String {
    let without_prefix = if let Some(idx) = title.find(": ") {
        &title[idx + 2..]
    } else {
        title
    };

    let slug: String = without_prefix
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();

    let re = Regex::new(r"-+").unwrap();
    let cleaned = re.replace_all(&slug, "-");
    let trimmed = cleaned.trim_matches('-');

    trimmed
        .split('-')
        .filter(|s| !s.is_empty())
        .take(4)
        .collect::<Vec<_>>()
        .join("-")
}

fn find_existing_worktree(repo_root: &PathBuf, pattern: &str) -> Result<Option<PathBuf>, String> {
    timing!(&format!("find_existing_worktree({})", pattern));
    let output = Command::new("git")
        .args(["-C", &repo_root.to_string_lossy(), "worktree", "list"])
        .output()
        .map_err(|e| format!("Failed to list worktrees: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    for line in stdout.lines() {
        if line.contains(pattern) {
            if let Some(path) = line.split_whitespace().next() {
                return Ok(Some(PathBuf::from(path)));
            }
        }
    }

    Ok(None)
}

/// Count files in a directory (non-recursively counts all entries via `git ls-files`)
fn count_worktree_files(worktree_path: &PathBuf) -> Option<usize> {
    let output = Command::new("git")
        .args(["-C", &worktree_path.to_string_lossy(), "ls-files"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if output.status.success() {
        let count = output.stdout.iter().filter(|&&b| b == b'\n').count();
        Some(count)
    } else {
        None
    }
}

/// Run a git command with a spinner showing elapsed time.
/// `label` is the prefix already printed (e.g. "→ Creating worktree...").
/// Returns the command's exit status.
fn run_git_with_spinner(args: &[&str]) -> Result<std::process::ExitStatus, String> {
    let args_owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let handle = thread::spawn(move || {
        Command::new("git")
            .args(&args_owned)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
    });

    let spinner_chars = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    let start = Instant::now();
    let mut i = 0;

    while !handle.is_finished() {
        let elapsed = start.elapsed().as_secs();
        let spinner = spinner_chars[i % spinner_chars.len()];
        print!("\r\x1b[K  {} {}s", spinner, elapsed);
        std::io::stdout().flush().ok();
        i += 1;
        thread::sleep(Duration::from_millis(100));
    }

    // Clear the spinner line
    print!("\r\x1b[K");
    std::io::stdout().flush().ok();

    handle
        .join()
        .map_err(|_| "git command thread panicked".to_string())?
        .map_err(|e| format!("Failed to run git command: {}", e))
}

fn fetch_branch(repo_root: &PathBuf, branch: &str) -> Result<(), String> {
    timing!(&format!("fetch_branch({})", branch));
    let max_retries = 3;
    for attempt in 1..=max_retries {
        let status = Command::new("git")
            .args(["-C", &repo_root.to_string_lossy(), "fetch", "origin", branch])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| format!("Failed to fetch: {}", e))?;

        if status.success() {
            return Ok(());
        }

        if attempt < max_retries {
            thread::sleep(Duration::from_secs(1));
        }
    }

    Err("git fetch failed after 3 attempts".to_string())
}

fn create_worktree_from_ref(repo_root: &PathBuf, worktree_path: &PathBuf, git_ref: &str) -> Result<(), String> {
    let repo_str = repo_root.to_string_lossy().to_string();
    let wt_str = worktree_path.to_string_lossy().to_string();

    let status = run_git_with_spinner(&["-C", &repo_str, "worktree", "add", &wt_str, git_ref])?;

    if !status.success() {
        // Try with FETCH_HEAD if branch is checked out elsewhere
        let status = run_git_with_spinner(&["-C", &repo_str, "worktree", "add", &wt_str, "FETCH_HEAD"])?;

        if !status.success() {
            return Err("git worktree add failed".to_string());
        }
    }

    Ok(())
}

fn create_worktree_new_branch(repo_root: &PathBuf, worktree_path: &PathBuf, branch: &str) -> Result<(), String> {
    timing!("create_worktree_new_branch (git worktree add)");
    let repo_str = repo_root.to_string_lossy().to_string();
    let wt_str = worktree_path.to_string_lossy().to_string();

    let status = run_git_with_spinner(&["-C", &repo_str, "worktree", "add", "-b", branch, &wt_str, "origin/master"])?;

    if !status.success() {
        // Branch may already exist from a previous attempt, try checking it out directly
        let status = run_git_with_spinner(&["-C", &repo_str, "worktree", "add", &wt_str, branch])?;

        if !status.success() {
            return Err("git worktree add failed".to_string());
        }
    }

    Ok(())
}

fn update_worktree(worktree_path: &PathBuf, branch: &str) -> Result<(), String> {
    timing!("update_worktree");
    let max_retries = 3;
    let mut last_stderr = String::new();
    for attempt in 1..=max_retries {
        let output = Command::new("git")
            .args(["-C", &worktree_path.to_string_lossy(), "fetch", "origin", branch])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| format!("Failed to fetch: {}", e))?;

        if output.status.success() {
            last_stderr.clear();
            break;
        }

        last_stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if attempt < max_retries {
            thread::sleep(Duration::from_secs(1));
        }
    }

    if !last_stderr.is_empty() {
        return Err(format!("git fetch failed after {} attempts: {}", max_retries, last_stderr));
    }

    let ref_name = format!("origin/{}", branch);
    let output = Command::new("git")
        .args([
            "-C",
            &worktree_path.to_string_lossy(),
            "reset",
            "--hard",
            &ref_name,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("Failed to reset: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git reset failed: {}", stderr.trim()));
    }

    Ok(())
}

fn which_mise() -> Option<PathBuf> {
    Command::new("which")
        .arg("mise")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
}

fn run_mise_trust(worktree_path: &PathBuf) -> Result<(), String> {
    let status = Command::new("mise")
        .args(["trust", "--all"])
        .current_dir(worktree_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("Failed to run mise trust: {}", e))?;

    if !status.success() {
        return Err("mise trust failed".to_string());
    }

    Ok(())
}

/// Validate/install the gem bundle for a freshly created worktree so the
/// pre-commit Ruby hooks (which run `bundle exec`) don't blow up on a bundle
/// that doesn't match this checkout's Gemfile.lock — the failure that otherwise
/// forces `git commit --no-verify`.
///
/// Mirrors `bin/create_worktree.sh`'s optimized path: point BUNDLE_PATH at the
/// main repo's `vendor/bundle` cache (which we've already symlinked in) and run
/// `bundle install --local` in frozen mode (via `BUNDLE_FROZEN=true` in the env,
/// not the `--frozen` flag, so Bundler doesn't persist it into the tracked
/// `.bundle/config`). When the worktree's lock matches the cache — the common
/// case for a branch off master — this is a fast no-op validation (~0.5s, no
/// network). Only when the branch's Gemfile.lock needs a gem that isn't cached
/// do we fall back to a networked `bundle install`.
///
/// Runs in the background thread after `symlink_vendor_bundle`, so it never
/// blocks time-to-prompt and finishes well before the user's first commit.
fn run_bundle_install(worktree_path: &PathBuf, repo_root: &PathBuf) -> Result<(), String> {
    // Nothing to do unless this is a Bundler project with a usable gem cache.
    if !worktree_path.join("Gemfile").is_file() {
        return Ok(());
    }
    let cache_bundle = repo_root.join("vendor/bundle");
    if !cache_bundle.is_dir() {
        return Ok(());
    }

    let run = |args: &[&str]| -> Result<Option<bool>, String> {
        match Command::new("bundle")
            .args(args)
            .current_dir(worktree_path)
            .env("BUNDLE_PATH", &cache_bundle)
            // Enforce frozen mode via the environment rather than the `--frozen`
            // CLI flag: Bundler "remembers" the flag by persisting
            // `BUNDLE_FROZEN: "true"` into the tracked `.bundle/config`, which
            // leaves every worktree permanently dirty. The env var is not
            // persisted, so worktrees stay clean.
            .env("BUNDLE_FROZEN", "true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
            Ok(status) => Ok(Some(status.success())),
            // Bundler isn't installed — nothing we can or should do.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("Failed to run bundle install: {}", e)),
        }
    };

    // Fast path: validate against the cached gems with no network access.
    match run(&["install", "--local"])? {
        None => return Ok(()),       // bundle not available
        Some(true) => return Ok(()), // bundle satisfied from cache
        Some(false) => {}            // fall through to networked install
    }

    // Fallback: the branch's lock needs a gem the cache doesn't have.
    match run(&["install"])? {
        Some(false) => Err("bundle install failed".to_string()),
        _ => Ok(()),
    }
}

fn run_gt_track(worktree_path: &PathBuf) -> Result<(), String> {
    timing!("run_gt_track");
    let status = Command::new("gt")
        .args(["track", "--no-interactive", "--parent", "master"])
        .current_dir(worktree_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("Failed to run gt track: {}", e))?;

    if !status.success() {
        return Err("gt track failed".to_string());
    }

    Ok(())
}

/// Find all node_modules directories in repo_root and symlink them into
/// the worktree. Skips any nested inside other node_modules since those
/// are already contained within the parent symlink. Returns the number
/// of symlinks created.
fn symlink_node_modules(worktree_path: &PathBuf, repo_root: &PathBuf) -> Result<usize, String> {
    let output = Command::new("find")
        .args([
            repo_root.to_string_lossy().as_ref(),
            "-name", "node_modules",
            "-type", "d",
            "-not", "-path", "*/node_modules/*/node_modules",
            "-prune",
        ])
        .output()
        .map_err(|e| format!("Failed to find node_modules: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut count = 0;

    for line in stdout.lines() {
        let source = PathBuf::from(line.trim());
        if !source.is_dir() || source.is_symlink() {
            continue;
        }

        let rel = source.strip_prefix(repo_root)
            .map_err(|_| format!("Path {} not under repo root", source.display()))?;
        let dest = worktree_path.join(rel);

        if dest.exists() || dest.is_symlink() {
            continue;
        }

        // Ensure parent directory exists in the worktree
        if let Some(parent) = dest.parent() {
            if !parent.exists() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("Failed to create parent dir {}: {}", parent.display(), e))?;
            }
        }

        std::os::unix::fs::symlink(&source, &dest)
            .map_err(|e| format!("Failed to symlink {}: {}", rel.display(), e))?;
        count += 1;
    }

    Ok(count)
}

/// Symlink the main repo's `vendor/` directory into the worktree so Bundler
/// can find installed gems (e.g. `numo-narray`) without re-running
/// `bundle install` per worktree. The repo's `Gemfile`/`Gemfile.lock` are
/// already tracked in git, so they show up in the worktree on their own —
/// only the gitignored `vendor/bundle` install output needs help.
///
/// No-ops if the main repo has no `vendor/`, or if the worktree already has
/// one (real dir or existing symlink).
fn symlink_vendor_bundle(worktree_path: &PathBuf, repo_root: &PathBuf) -> Result<(), String> {
    let source = repo_root.join("vendor");
    if !source.is_dir() || source.is_symlink() {
        return Ok(());
    }

    let dest = worktree_path.join("vendor");
    if dest.exists() || dest.symlink_metadata().is_ok() {
        return Ok(());
    }

    std::os::unix::fs::symlink(&source, &dest)
        .map_err(|e| format!("Failed to symlink vendor: {}", e))?;

    Ok(())
}

fn prepare_agent_worktree(
    agent: Agent,
    worktree_path: &PathBuf,
    repo_root: &PathBuf,
) -> Result<(), String> {
    if agent != Agent::Claude {
        return Ok(());
    }

    print!("{} Copying Claude settings... ", "→".blue().bold());
    std::io::stdout().flush().ok();
    symlink_claude_settings(worktree_path, repo_root)?;
    println!("{}", "done".green());

    print!("{} Adding Claude trust... ", "→".blue().bold());
    std::io::stdout().flush().ok();
    add_claude_trust(worktree_path, repo_root)?;
    println!("{}", "done".green());

    Ok(())
}

fn symlink_claude_settings(worktree_path: &PathBuf, repo_root: &PathBuf) -> Result<(), String> {
    timing!("symlink_claude_settings");
    // Symlink the main repo's .claude/settings.local.json into the worktree
    // This contains MCP server configurations and other local settings
    let source = repo_root.join(".claude/settings.local.json");

    let resolved_source = if !source.exists() {
        // Fall back to global settings if repo-specific doesn't exist
        let home = env::var("HOME").map_err(|_| "HOME not set")?;
        let global_source = PathBuf::from(format!("{}/.claude/settings.local.json", home));

        if !global_source.exists() {
            return Ok(());
        }

        global_source
    } else {
        source
    };

    let dest_dir = worktree_path.join(".claude");
    fs::create_dir_all(&dest_dir)
        .map_err(|e| format!("Failed to create .claude dir: {}", e))?;

    let dest = dest_dir.join("settings.local.json");

    // Remove existing file/symlink if present so we can create a fresh symlink
    if dest.exists() || dest.symlink_metadata().is_ok() {
        fs::remove_file(&dest)
            .map_err(|e| format!("Failed to remove existing settings file: {}", e))?;
    }

    std::os::unix::fs::symlink(&resolved_source, &dest)
        .map_err(|e| format!("Failed to symlink claude settings: {}", e))?;

    Ok(())
}

fn add_claude_trust(worktree_path: &PathBuf, repo_root: &PathBuf) -> Result<(), String> {
    timing!("add_claude_trust");
    let home = env::var("HOME").map_err(|_| "HOME not set")?;
    let claude_json_path = PathBuf::from(format!("{}/.claude.json", home));

    // Read existing file or create empty object
    let mut data: Value = if claude_json_path.exists() {
        let content = fs::read_to_string(&claude_json_path)
            .map_err(|e| format!("Failed to read .claude.json: {}", e))?;
        serde_json::from_str(&content)
            .map_err(|e| format!("Failed to parse .claude.json: {}", e))?
    } else {
        serde_json::json!({})
    };

    // Ensure projects object exists
    if data.get("projects").is_none() {
        data["projects"] = serde_json::json!({});
    }

    let worktree_path_str = worktree_path.to_string_lossy().to_string();
    let repo_root_str = repo_root.to_string_lossy().to_string();

    // Try to copy settings from the main repo, or use defaults
    let base_settings = data
        .get("projects")
        .and_then(|p| p.get(&repo_root_str))
        .cloned()
        .unwrap_or_else(|| serde_json::json!({
            "allowedTools": [],
            "mcpContextUris": [],
            "mcpServers": {},
            "enabledMcpjsonServers": [],
            "disabledMcpjsonServers": [],
            "projectOnboardingSeenCount": 0,
            "hasClaudeMdExternalIncludesApproved": false,
            "hasClaudeMdExternalIncludesWarningShown": false,
            "reactVulnerabilityCache": {
                "detected": false,
                "package": null,
                "packageName": null,
                "version": null,
                "packageManager": null
            }
        }));

    // Create new project entry based on main repo settings
    let mut new_project = base_settings.clone();
    // Always ensure trust is accepted for new worktrees
    new_project["hasTrustDialogAccepted"] = serde_json::json!(true);
    // Reset session-specific fields
    new_project.as_object_mut().map(|obj| {
        obj.remove("lastAPIDuration");
        obj.remove("lastAPIDurationWithoutRetries");
        obj.remove("lastCost");
        obj.remove("lastDuration");
        obj.remove("lastLinesAdded");
        obj.remove("lastLinesRemoved");
        obj.remove("lastModelUsage");
        obj.remove("lastSessionId");
        obj.remove("lastToolDuration");
        obj.remove("lastTotalCacheCreationInputTokens");
        obj.remove("lastTotalCacheReadInputTokens");
        obj.remove("lastTotalInputTokens");
        obj.remove("lastTotalOutputTokens");
        obj.remove("lastTotalWebSearchRequests");
        obj.remove("exampleFiles");
        obj.remove("exampleFilesGeneratedAt");
    });

    data["projects"][&worktree_path_str] = new_project;

    // Write back to file
    let content = serde_json::to_string_pretty(&data)
        .map_err(|e| format!("Failed to serialize .claude.json: {}", e))?;
    fs::write(&claude_json_path, content)
        .map_err(|e| format!("Failed to write .claude.json: {}", e))?;

    Ok(())
}

fn build_worktree_system_prompt() -> String {
    "IMPORTANT: This is a git worktree. The node_modules directories are symlinked \
     from the main repo. NEVER run `pnpm install` in this worktree — it will corrupt \
     the shared node_modules symlinks with absolute paths pointing at this worktree, \
     breaking the main repo and all other worktrees. If you need to test dependency \
     changes, first `rm` the node_modules symlink to create an isolated copy."
        .to_string()
}

fn codex_session_snapshot(worktree_path: &Path) -> HashMap<String, SystemTime> {
    let mut files = Vec::new();
    collect_jsonl_files(&get_codex_session_dir(), &mut files);

    let mut sessions: HashMap<String, SystemTime> = HashMap::new();
    for path in files {
        let Some((cwd, session_id)) = codex_session_metadata(&path) else {
            continue;
        };
        if cwd != worktree_path {
            continue;
        }
        let Ok(modified) = fs::metadata(&path).and_then(|metadata| metadata.modified()) else {
            continue;
        };
        sessions
            .entry(session_id)
            .and_modify(|current| *current = (*current).max(modified))
            .or_insert(modified);
    }
    sessions
}

fn wait_for_codex_session_id(
    worktree_path: &Path,
    before: &HashMap<String, SystemTime>,
    timeout: Duration,
) -> Option<String> {
    let deadline = Instant::now() + timeout;
    let mut delay = Duration::from_millis(100);
    while Instant::now() < deadline {
        let changed = codex_session_snapshot(worktree_path)
            .into_iter()
            .filter(|(session_id, modified)| {
                before
                    .get(session_id)
                    .is_none_or(|previous| modified > previous)
            })
            .max_by_key(|(_, modified)| *modified)
            .map(|(session_id, _)| session_id);
        if changed.is_some() {
            return changed;
        }
        thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_secs(1));
    }
    None
}

enum CodexRenameWorkerMessage {
    Started(u32),
    Finished(Result<(), String>),
}

fn read_codex_app_server_response(
    reader: &mut impl BufRead,
    request_id: u64,
) -> Result<Value, String> {
    let mut line = String::new();
    loop {
        line.clear();
        let bytes = reader
            .read_line(&mut line)
            .map_err(|e| format!("Failed to read Codex app server response: {}", e))?;
        if bytes == 0 {
            return Err(format!(
                "Codex app server exited before answering request {}",
                request_id
            ));
        }
        let Ok(response) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if response.get("id").and_then(Value::as_u64) == Some(request_id) {
            return Ok(response);
        }
    }
}

fn run_codex_rename_worker(
    app_server_args: Vec<String>,
    thread_id: String,
    name: String,
    messages: mpsc::Sender<CodexRenameWorkerMessage>,
) -> Result<(), String> {
    let mut child = Command::new("codex")
        .args(&app_server_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to start codex {}: {}", app_server_args.join(" "), e))?;
    let _ = messages.send(CodexRenameWorkerMessage::Started(child.id()));

    let result = (|| {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| "Codex app server stdin was unavailable".to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "Codex app server stdout was unavailable".to_string())?;
        let mut reader = io::BufReader::new(stdout);

        writeln!(stdin, "{}", serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "checkout",
                    "title": "Checkout",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }
        })).map_err(|e| format!("Failed to initialize Codex app server: {}", e))?;
        stdin.flush()
            .map_err(|e| format!("Failed to flush Codex initialize request: {}", e))?;

        let initialize = read_codex_app_server_response(&mut reader, 1)?;
        if let Some(error) = initialize.get("error") {
            return Err(format!("Codex app server initialize failed: {}", error));
        }

        writeln!(stdin, "{}", serde_json::json!({
            "jsonrpc": "2.0",
            "method": "initialized"
        })).map_err(|e| format!("Failed to acknowledge Codex initialization: {}", e))?;
        writeln!(stdin, "{}", serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "thread/name/set",
            "params": {"threadId": thread_id, "name": name}
        })).map_err(|e| format!("Failed to send Codex rename request: {}", e))?;
        stdin.flush()
            .map_err(|e| format!("Failed to flush Codex rename request: {}", e))?;

        let rename = read_codex_app_server_response(&mut reader, 2)?;
        if let Some(error) = rename.get("error") {
            return Err(format!("Codex thread rename failed: {}", error));
        }
        Ok(())
    })();

    let _ = child.kill();
    let _ = child.wait();
    result
}

fn set_codex_thread_name_with_timeout(
    app_server_args: &[&str],
    thread_id: &str,
    name: &str,
) -> Result<(), String> {
    let (worker_sender, receiver) = mpsc::channel();
    let args = app_server_args.iter().map(|argument| argument.to_string()).collect();
    let thread_id = thread_id.to_string();
    let name = name.to_string();
    thread::spawn(move || {
        let result = run_codex_rename_worker(args, thread_id, name, worker_sender.clone());
        let _ = worker_sender.send(CodexRenameWorkerMessage::Finished(result));
    });

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut child_pid: Option<u32> = None;
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            if let Some(pid) = child_pid {
                let _ = Command::new("kill")
                    .args(["-TERM", &pid.to_string()])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            return Err("Codex app server rename timed out".to_string());
        };
        match receiver.recv_timeout(remaining) {
            Ok(CodexRenameWorkerMessage::Started(pid)) => child_pid = Some(pid),
            Ok(CodexRenameWorkerMessage::Finished(result)) => return result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(pid) = child_pid {
                    let _ = Command::new("kill")
                        .args(["-TERM", &pid.to_string()])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                }
                return Err("Codex app server rename timed out".to_string());
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err("Codex app server rename worker exited".to_string());
            }
        }
    }
}

fn set_codex_thread_name(thread_id: &str, name: &str) -> Result<(), String> {
    let mut errors = Vec::new();
    for args in [
        &["app-server", "--stdio"][..],
        &["app-server", "proxy"][..],
    ] {
        match set_codex_thread_name_with_timeout(args, thread_id, name) {
            Ok(()) => return Ok(()),
            Err(error) => errors.push(format!("codex {}: {}", args.join(" "), error)),
        }
    }
    Err(errors.join("; "))
}

fn spawn_codex_thread_renamer(
    worktree_path: PathBuf,
    session_name: String,
    session_id: Option<String>,
    sessions_before_launch: HashMap<String, SystemTime>,
) {
    thread::spawn(move || {
        let thread_id = session_id.or_else(|| {
            wait_for_codex_session_id(
                &worktree_path,
                &sessions_before_launch,
                Duration::from_secs(10),
            )
        });
        if let Some(thread_id) = thread_id {
            // Naming is best-effort: it must never prevent the interactive
            // coding-agent session from opening.
            let _ = set_codex_thread_name(&thread_id, &session_name);
        }
    });
}

fn spawn_agent_with_prompt(
    agent: Agent,
    worktree_path: &PathBuf,
    prompt: &str,
    developer_instructions: Option<&str>,
    session_name: &str,
) -> Result<(), String> {
    spawn_agent_process(
        agent,
        worktree_path,
        Some(prompt),
        developer_instructions,
        None,
        false,
        session_name,
    )
}

fn spawn_agent_continue(
    agent: Agent,
    worktree_path: &PathBuf,
    developer_instructions: Option<&str>,
    session_id: Option<&str>,
    session_name: &str,
) -> Result<(), String> {
    spawn_agent_process(
        agent,
        worktree_path,
        None,
        developer_instructions,
        session_id,
        true,
        session_name,
    )
}

fn spawn_agent(
    agent: Agent,
    worktree_path: &PathBuf,
    developer_instructions: Option<&str>,
    session_name: &str,
) -> Result<(), String> {
    spawn_agent_process(
        agent,
        worktree_path,
        None,
        developer_instructions,
        None,
        false,
        session_name,
    )
}

fn spawn_agent_process(
    agent: Agent,
    worktree_path: &PathBuf,
    prompt: Option<&str>,
    developer_instructions: Option<&str>,
    session_id: Option<&str>,
    resume: bool,
    session_name: &str,
) -> Result<(), String> {
    set_terminal_cwd(worktree_path);
    save_session_name(worktree_path, session_name)?;

    let sessions_before_launch = if agent == Agent::Codex && session_id.is_none() {
        codex_session_snapshot(worktree_path)
    } else {
        HashMap::new()
    };

    let mut cmd = Command::new(agent.command());
    cmd.args(build_agent_args(
        agent,
        prompt,
        developer_instructions,
        resume,
        session_id,
    )?);

    let mut child = cmd
        .current_dir(worktree_path)
        .spawn()
        .map_err(|e| format!("Failed to spawn {}: {}", agent.command(), e))?;

    if agent == Agent::Codex {
        spawn_codex_thread_renamer(
            worktree_path.clone(),
            session_name.to_string(),
            session_id.map(str::to_string),
            sessions_before_launch,
        );
    }

    let _pid_guard = PidFileGuard::new(worktree_path, child.id(), agent);
    let status = child
        .wait()
        .map_err(|e| format!("Failed to wait for {}: {}", agent.command(), e))?;

    if !status.success() {
        return Err(format!("{} exited with error", agent.command()));
    }

    Ok(())
}

fn build_agent_args(
    agent: Agent,
    prompt: Option<&str>,
    developer_instructions: Option<&str>,
    resume: bool,
    session_id: Option<&str>,
) -> Result<Vec<String>, String> {
    let mut args = Vec::new();
    match agent {
        Agent::Claude => {
            args.extend(["--permission-mode".to_string(), "acceptEdits".to_string()]);
            if resume {
                args.push("--continue".to_string());
            }
            if let Some(prompt) = prompt {
                args.push(prompt.to_string());
            }
            if let Some(instructions) = developer_instructions {
                args.extend([
                    "--append-system-prompt".to_string(),
                    instructions.to_string(),
                ]);
            }
        }
        Agent::Codex => {
            if let Some(instructions) = developer_instructions {
                let value = serde_json::to_string(instructions)
                    .map_err(|e| format!("Failed to encode Codex developer instructions: {}", e))?;
                args.extend([
                    "--config".to_string(),
                    format!("developer_instructions={}", value),
                ]);
            }
            if resume {
                args.push("resume".to_string());
                if let Some(session_id) = session_id {
                    args.push(session_id.to_string());
                } else {
                    args.push("--last".to_string());
                }
            }
            if let Some(prompt) = prompt {
                args.push(prompt.to_string());
            }
        }
    }
    Ok(args)
}

// ---------- resume subcommand ----------

struct ChatMessage {
    role: String,   // "user" or "assistant"
    content: String,
}

struct SessionInfo {
    last_modified: SystemTime,
    messages: Vec<ChatMessage>,
    agent: Agent,
    resume_id: Option<String>,
}

struct WorktreeSession {
    worktree: WorktreeInfo,
    session: SessionInfo,
}

/// Encode a worktree path into the Claude projects directory name format.
/// `/Users/dtsung/figma-worktrees/branch-foo` → `-Users-dtsung-figma-worktrees-branch-foo`
fn encode_project_path(path: &Path) -> String {
    path.to_string_lossy().replace('/', "-")
}

/// Read the last `max_bytes` of a file and return complete lines from that tail.
fn tail_lines(path: &Path, max_bytes: u64) -> Result<Vec<String>, String> {
    let meta = fs::metadata(path).map_err(|e| format!("stat {}: {}", path.display(), e))?;
    let file_len = meta.len();

    let file = fs::File::open(path).map_err(|e| format!("open {}: {}", path.display(), e))?;

    let start = if file_len > max_bytes { file_len - max_bytes } else { 0 };
    let reader = io::BufReader::new(file);
    let mut lines: Vec<String> = Vec::new();

    // Skip to the start offset by reading bytes
    use std::io::{Seek, SeekFrom};
    let mut reader = reader;
    reader.seek(SeekFrom::Start(start)).map_err(|e| e.to_string())?;

    // If we skipped into the middle of a file, discard the first (partial) line
    if start > 0 {
        let mut _discard = String::new();
        reader.read_line(&mut _discard).ok();
    }

    for line in reader.lines() {
        match line {
            Ok(l) => lines.push(l),
            Err(_) => break,
        }
    }

    Ok(lines)
}

/// Parse JSONL lines to extract the last N user/assistant chat messages.
fn parse_claude_session_messages(jsonl_path: &Path, max_messages: usize) -> Vec<ChatMessage> {
    // Read last ~256KB — plenty for recent messages
    let lines = match tail_lines(jsonl_path, 256 * 1024) {
        Ok(l) => l,
        Err(_) => return Vec::new(),
    };

    let mut messages: Vec<ChatMessage> = Vec::new();

    for line in &lines {
        let val: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let msg_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");

        match msg_type {
            "user" => {
                let content = val.get("message").and_then(|m| m.get("content"));
                if let Some(content) = content {
                    if let Some(text) = content.as_str() {
                        let trimmed = text.trim_start();
                        // Skip system/tool messages (XML tags, tool results)
                        if !trimmed.starts_with('<') {
                            messages.push(ChatMessage {
                                role: "user".to_string(),
                                content: text.to_string(),
                            });
                        }
                    }
                    // Skip tool_result arrays — those aren't human messages
                }
            }
            "assistant" => {
                let content = val.get("message").and_then(|m| m.get("content"));
                if let Some(arr) = content.and_then(|c| c.as_array()) {
                    // Find the first text block (skip thinking blocks)
                    for item in arr {
                        let block_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        if block_type == "text" {
                            if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                                messages.push(ChatMessage {
                                    role: "assistant".to_string(),
                                    content: text.to_string(),
                                });
                                break;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Return the last N messages
    let start = if messages.len() > max_messages { messages.len() - max_messages } else { 0 };
    messages.split_off(start)
}

/// Find the most recent Claude session for a worktree path.
fn find_claude_worktree_session(worktree_path: &Path) -> Option<SessionInfo> {
    let home = env::var("HOME").ok()?;
    let encoded = encode_project_path(worktree_path);
    let project_dir = PathBuf::from(format!("{}/.claude/projects/{}", home, encoded));

    if !project_dir.is_dir() {
        return None;
    }

    let mut best_path: Option<PathBuf> = None;
    let mut best_time: Option<SystemTime> = None;

    if let Ok(entries) = fs::read_dir(&project_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
                if let Ok(meta) = fs::metadata(&path) {
                    if let Ok(modified) = meta.modified() {
                        if best_time.is_none() || modified > best_time.unwrap() {
                            best_time = Some(modified);
                            best_path = Some(path);
                        }
                    }
                }
            }
        }
    }

    let jsonl_path = best_path?;
    let last_modified = best_time?;
    let messages = parse_claude_session_messages(&jsonl_path, 50);

    if messages.is_empty() {
        return None;
    }

    Some(SessionInfo {
        last_modified,
        messages,
        agent: Agent::Claude,
        resume_id: None,
    })
}

/// Most-recent Claude session activity time for a worktree, based on the
/// mtime of its session transcripts. Lighter than `find_worktree_session`
/// since it doesn't parse message contents.
fn find_claude_worktree_session_time(worktree_path: &Path) -> Option<SystemTime> {
    let home = env::var("HOME").ok()?;
    let encoded = encode_project_path(worktree_path);
    let project_dir = PathBuf::from(format!("{}/.claude/projects/{}", home, encoded));

    let mut best: Option<SystemTime> = None;
    for entry in fs::read_dir(&project_dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            if let Ok(modified) = fs::metadata(&path).and_then(|m| m.modified()) {
                if best.map_or(true, |b| modified > b) {
                    best = Some(modified);
                }
            }
        }
    }
    best
}

fn get_codex_session_dir() -> PathBuf {
    if let Ok(codex_home) = env::var("CODEX_HOME") {
        return PathBuf::from(codex_home).join("sessions");
    }
    let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".codex/sessions")
}

fn collect_jsonl_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_jsonl_files(&path, files);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            files.push(path);
        }
    }
}

fn codex_session_metadata(jsonl_path: &Path) -> Option<(PathBuf, String)> {
    let file = fs::File::open(jsonl_path).ok()?;
    let first_line = io::BufReader::new(file).lines().next()?.ok()?;
    let value: Value = serde_json::from_str(&first_line).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let payload = value.get("payload")?;
    // Checkout launches top-level CLI sessions. Excluding subagent transcripts
    // prevents a worker's partial conversation from replacing the parent session.
    if payload.get("source").and_then(Value::as_str) != Some("cli") {
        return None;
    }
    let worktree = payload.get("cwd").and_then(Value::as_str).map(PathBuf::from)?;
    let session_id = payload.get("id").and_then(Value::as_str)?.to_string();
    Some((worktree, session_id))
}

fn latest_codex_session_files(
    worktree_paths: &HashSet<PathBuf>,
) -> HashMap<PathBuf, (SystemTime, PathBuf, String)> {
    let mut files = Vec::new();
    collect_jsonl_files(&get_codex_session_dir(), &mut files);

    let mut latest: HashMap<PathBuf, (SystemTime, PathBuf, String)> = HashMap::new();
    for path in files {
        let Some((worktree_path, session_id)) = codex_session_metadata(&path) else {
            continue;
        };
        if !worktree_paths.contains(&worktree_path) {
            continue;
        }
        let Ok(modified) = fs::metadata(&path).and_then(|metadata| metadata.modified()) else {
            continue;
        };
        let replace = latest
            .get(&worktree_path)
            .is_none_or(|(current, _, _)| modified > *current);
        if replace {
            latest.insert(worktree_path, (modified, path, session_id));
        }
    }
    latest
}

fn choose_resume_target(
    codex: Option<ResumeTarget>,
    claude: Option<ResumeTarget>,
) -> Option<ResumeTarget> {
    match (codex, claude) {
        (Some(codex), Some(claude)) => {
            if codex.last_modified >= claude.last_modified {
                Some(codex)
            } else {
                Some(claude)
            }
        }
        (Some(codex), None) => Some(codex),
        (None, Some(claude)) => Some(claude),
        (None, None) => None,
    }
}

fn find_worktree_resume_target(worktree_path: &Path) -> Option<ResumeTarget> {
    let claude = find_claude_worktree_session_time(worktree_path).map(|last_modified| ResumeTarget {
        last_modified,
        agent: Agent::Claude,
        resume_id: None,
    });

    let mut worktree_paths = HashSet::new();
    worktree_paths.insert(worktree_path.to_path_buf());
    let codex = latest_codex_session_files(&worktree_paths)
        .remove(worktree_path)
        .map(|(last_modified, _, resume_id)| ResumeTarget {
            last_modified,
            agent: Agent::Codex,
            resume_id: Some(resume_id),
        });

    choose_resume_target(codex, claude)
}

fn find_codex_worktree_session_id(worktree_path: &Path) -> Option<String> {
    let mut worktree_paths = HashSet::new();
    worktree_paths.insert(worktree_path.to_path_buf());
    latest_codex_session_files(&worktree_paths)
        .remove(worktree_path)
        .map(|(_, _, session_id)| session_id)
}

fn parse_codex_session_messages(jsonl_path: &Path, max_messages: usize) -> Vec<ChatMessage> {
    let lines = match tail_lines(jsonl_path, 256 * 1024) {
        Ok(lines) => lines,
        Err(_) => return Vec::new(),
    };
    let mut messages = Vec::new();

    for line in lines {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("type").and_then(Value::as_str) != Some("response_item") {
            continue;
        }
        let Some(payload) = value.get("payload") else {
            continue;
        };
        if payload.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let Some(role) = payload.get("role").and_then(Value::as_str) else {
            continue;
        };
        if role != "user" && role != "assistant" {
            continue;
        }

        let expected_type = if role == "user" { "input_text" } else { "output_text" };
        let content = payload
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some(expected_type))
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .filter(|text| role != "user" || !text.trim_start().starts_with('<'))
            .collect::<Vec<_>>()
            .join("\n");
        if !content.trim().is_empty() {
            messages.push(ChatMessage {
                role: role.to_string(),
                content,
            });
        }
    }

    let start = messages.len().saturating_sub(max_messages);
    messages.split_off(start)
}

fn find_agent_sessions(
    agent: Agent,
    entries: Vec<(PathBuf, String)>,
) -> Vec<WorktreeSession> {
    match agent {
        Agent::Claude => entries
            .into_iter()
            .filter_map(|(path, branch)| {
                let session = find_claude_worktree_session(&path)?;
                Some(WorktreeSession {
                    worktree: WorktreeInfo {
                        path,
                        branch,
                        has_changes: false,
                        has_active_session: false,
                        active_agent: None,
                        orphaned_pids: Vec::new(),
                    },
                    session,
                })
            })
            .collect(),
        Agent::Codex => {
            let branches: HashMap<PathBuf, String> = entries.into_iter().collect();
            let worktree_paths = branches.keys().cloned().collect();
            latest_codex_session_files(&worktree_paths)
                .into_iter()
                .filter_map(|(path, (last_modified, jsonl_path, session_id))| {
                    let messages = parse_codex_session_messages(&jsonl_path, 50);
                    let branch = branches.get(&path)?.clone();
                    Some(WorktreeSession {
                        worktree: WorktreeInfo {
                            path,
                            branch,
                            has_changes: false,
                            has_active_session: false,
                            active_agent: None,
                            orphaned_pids: Vec::new(),
                        },
                        session: SessionInfo {
                            last_modified,
                            messages,
                            agent: Agent::Codex,
                            resume_id: Some(session_id),
                        },
                    })
                })
                .collect()
        }
    }
}

fn find_all_agent_sessions(entries: Vec<(PathBuf, String)>) -> Vec<WorktreeSession> {
    let mut sessions = find_agent_sessions(Agent::Codex, entries.clone());
    sessions.extend(find_agent_sessions(Agent::Claude, entries));
    sessions
}

fn find_agent_session_times(worktree_paths: &HashSet<PathBuf>) -> HashMap<PathBuf, SystemTime> {
    let mut times = HashMap::new();
    for path in worktree_paths {
        if let Some(modified) = find_claude_worktree_session_time(path) {
            times.insert(path.clone(), modified);
        }
    }
    for (path, (modified, _, _)) in latest_codex_session_files(worktree_paths) {
        times
            .entry(path)
            .and_modify(|current| *current = (*current).max(modified))
            .or_insert(modified);
    }
    times
}

/// Format a `SystemTime` as a human-readable "time ago" string.
fn format_time_ago(time: SystemTime) -> String {
    let elapsed = time.elapsed().unwrap_or_default();
    let secs = elapsed.as_secs();

    if secs < 60 {
        return "just now".to_string();
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{}m ago", mins);
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{}h ago", hours);
    }
    let days = hours / 24;
    if days < 30 {
        return format!("{}d ago", days);
    }
    let months = days / 30;
    format!("{}mo ago", months)
}

/// Lightweight worktree listing: just parses `git worktree list --porcelain`
/// without spawning any git-status or process-detection subcommands.
fn list_worktree_paths(repo_root: &PathBuf) -> Result<Vec<(PathBuf, String)>, String> {
    timing!("list_worktree_paths");
    let output = Command::new("git")
        .args(["-C", &repo_root.to_string_lossy(), "worktree", "list", "--porcelain"])
        .output()
        .map_err(|e| format!("Failed to list worktrees: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut entries: Vec<(PathBuf, String)> = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_branch: Option<String> = None;

    for line in stdout.lines() {
        if let Some(path_str) = line.strip_prefix("worktree ") {
            current_path = Some(PathBuf::from(path_str));
        } else if let Some(branch_str) = line.strip_prefix("branch refs/heads/") {
            current_branch = Some(branch_str.to_string());
        } else if line.is_empty() {
            if let Some(path) = current_path.take() {
                if path != *repo_root {
                    let branch = current_branch.take().unwrap_or_else(|| "(detached)".to_string());
                    entries.push((path, branch));
                }
            }
            current_branch = None;
        }
    }

    Ok(entries)
}

fn run_resume(repo: Option<PathBuf>) -> Result<(), String> {
    timing!("run_resume");
    let repo_root = repo.unwrap_or_else(default_repo_root);

    if !repo_root.exists() {
        return Err(format!("Repo not found at {}", repo_root.display()));
    }

    // Fast path: just list worktree paths (single git command, no status checks)
    print!("{} Listing worktrees... ", "→".blue().bold());
    io::stdout().flush().ok();
    let entries = list_worktree_paths(&repo_root)?;
    println!("{} ({} found)", "done".green(), entries.len());

    if entries.is_empty() {
        println!("{} No worktrees found", "→".blue().bold());
        return Ok(());
    }

    // Find sessions via filesystem reads only — skip worktrees without sessions
    print!("{} Finding agent sessions... ", "→".blue().bold());
    io::stdout().flush().ok();
    let mut sessions = find_all_agent_sessions(entries);
    println!("{} ({} with sessions)", "done".green(), sessions.len());

    if sessions.is_empty() {
        println!("{} No worktrees with agent sessions found", "→".blue().bold());
        return Ok(());
    }

    // Sort by most recent session first
    sessions.sort_by(|a, b| b.session.last_modified.cmp(&a.session.last_modified));

    // Run the TUI and get the selected index
    let selected = run_resume_tui(&sessions)?;

    let Some(idx) = selected else {
        return Ok(());
    };

    let ws = &sessions[idx];
    let worktree_path = &ws.worktree.path;
    let agent = ws.session.agent;

    prepare_agent_worktree(agent, worktree_path, &repo_root)?;
    let bg_color = pick_available_color(worktree_path);
    save_worktree_color(worktree_path, &bg_color)?;
    record_current_iterm_session(worktree_path)?;
    let session_name = session_name_for_resume(worktree_path, &ws.worktree.branch);

    let _iterm_guard = ItermGuard::new(&bg_color, &session_name);

    println!(
        "\n{} Resuming session in {}...\n",
        "→".blue().bold(),
        worktree_path.display().to_string().cyan()
    );

    let developer_instructions = build_worktree_system_prompt();
    spawn_agent_continue(
        agent,
        worktree_path,
        Some(&developer_instructions),
        ws.session.resume_id.as_deref(),
        &session_name,
    )?;

    Ok(())
}

fn run_resume_last(repo: Option<PathBuf>, agent: Agent) -> Result<(), String> {
    timing!("run_resume_last");
    let repo_root = repo.unwrap_or_else(default_repo_root);

    if !repo_root.exists() {
        return Err(format!("Repo not found at {}", repo_root.display()));
    }

    // Read all .exited files, find the most recent one whose worktree still exists
    // and doesn't have an active session
    let session_dir = get_session_dir();
    let entries = fs::read_dir(&session_dir)
        .map_err(|e| format!("Failed to read session dir: {}", e))?;

    let mut best: Option<(u64, PathBuf)> = None;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("exited") {
            continue;
        }

        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let Some(exited) = parse_exited_session(&content) else {
            continue;
        };
        if exited.agent != agent {
            continue;
        }
        let timestamp = exited.timestamp;
        let worktree_path = exited.worktree_path;

        // Skip if worktree no longer exists
        if !worktree_path.exists() {
            let _ = fs::remove_file(&path);
            continue;
        }

        // Skip if there's an active session
        if let Some(pid) = read_session_pid(&worktree_path) {
            if is_pid_alive(pid) {
                continue;
            }
        }

        if best.as_ref().map_or(true, |(ts, _)| timestamp > *ts) {
            best = Some((timestamp, worktree_path));
        }
    }

    let Some((_timestamp, worktree_path)) = best else {
        println!("{} No recently exited sessions found", "!".yellow().bold());
        return Ok(());
    };

    let branch = worktree_path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    println!(
        "{} Resuming last session in {}",
        "→".blue().bold(),
        worktree_path.display().to_string().cyan()
    );

    prepare_agent_worktree(agent, &worktree_path, &repo_root)?;
    let bg_color = pick_available_color(&worktree_path);
    save_worktree_color(&worktree_path, &bg_color)?;
    record_current_iterm_session(&worktree_path)?;
    let session_name = session_name_for_resume(&worktree_path, &branch);

    let _iterm_guard = ItermGuard::new(&bg_color, &session_name);

    let system_prompt = build_worktree_system_prompt();
    let resume_id = if agent == Agent::Codex {
        find_codex_worktree_session_id(&worktree_path)
    } else {
        None
    };

    println!();
    println!(
        "{} Resuming {} session...",
        "→".blue().bold(),
        agent.display_name(),
    );
    println!();

    spawn_agent_continue(
        agent,
        &worktree_path,
        Some(&system_prompt),
        resume_id.as_deref(),
        &session_name,
    )?;

    Ok(())
}

/// Parse inline markdown (`**bold**`, `*italic*`, `` `code` ``) into styled spans.
fn parse_inline_markdown(text: &str, base_style: Style) -> Vec<(String, Style)> {
    let mut segments: Vec<(String, Style)> = Vec::new();
    let mut current = String::new();
    let mut bold = false;
    let mut italic = false;
    let mut in_code = false;
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = 0;

    let code_style = base_style.fg(Color::Cyan);

    let make_style = |base: Style, b: bool, it: bool| -> Style {
        let mut s = base;
        if b { s = s.add_modifier(Modifier::BOLD); }
        if it { s = s.add_modifier(Modifier::ITALIC); }
        s
    };

    while i < len {
        if chars[i] == '`' {
            if !current.is_empty() {
                let style = if in_code { code_style } else { make_style(base_style, bold, italic) };
                segments.push((std::mem::take(&mut current), style));
            }
            in_code = !in_code;
            i += 1;
        } else if in_code {
            // Inside backticks — no markdown parsing
            current.push(chars[i]);
            i += 1;
        } else if i + 1 < len && chars[i] == '*' && chars[i + 1] == '*' {
            if !current.is_empty() {
                segments.push((std::mem::take(&mut current), make_style(base_style, bold, italic)));
            }
            bold = !bold;
            i += 2;
        } else if chars[i] == '*' {
            if !current.is_empty() {
                segments.push((std::mem::take(&mut current), make_style(base_style, bold, italic)));
            }
            italic = !italic;
            i += 1;
        } else {
            current.push(chars[i]);
            i += 1;
        }
    }

    if !current.is_empty() {
        let style = if in_code { code_style } else { make_style(base_style, bold, italic) };
        segments.push((current, style));
    }

    segments
}

/// Wrap pre-styled segments into display lines, breaking at word boundaries.
fn wrap_styled_segments<'a>(segments: &[(String, Style)], width: usize) -> Vec<Vec<Span<'a>>> {
    // Flatten into a list of (char, style) pairs
    let mut styled_chars: Vec<(char, Style)> = Vec::new();
    for (text, style) in segments {
        for ch in text.chars() {
            styled_chars.push((ch, *style));
        }
    }

    if styled_chars.is_empty() {
        return Vec::new();
    }

    let mut result: Vec<Vec<Span<'a>>> = Vec::new();
    let total = styled_chars.len();
    let mut pos = 0;

    while pos < total {
        let line_end = (pos + width).min(total);

        // If the whole remainder fits, take it all
        if line_end == total {
            let line = build_spans_from_styled_chars(&styled_chars[pos..total]);
            result.push(line);
            break;
        }

        // Look for a space to break at (scan backwards from the cut point)
        let mut break_at = line_end;
        let mut found_space = false;
        for i in (pos..line_end).rev() {
            if styled_chars[i].0 == ' ' {
                break_at = i + 1; // break after the space
                found_space = true;
                break;
            }
        }

        // If no space found, hard-break at width
        if !found_space {
            break_at = line_end;
        }

        let line = build_spans_from_styled_chars(&styled_chars[pos..break_at]);
        result.push(line);
        pos = break_at;
    }

    result
}

/// Convert a slice of (char, style) pairs into coalesced Spans.
fn build_spans_from_styled_chars<'a>(chars: &[(char, Style)]) -> Vec<Span<'a>> {
    let mut spans: Vec<Span<'a>> = Vec::new();
    if chars.is_empty() {
        return spans;
    }

    let mut current_text = String::new();
    let mut current_style = chars[0].1;

    for &(ch, style) in chars {
        if style == current_style {
            current_text.push(ch);
        } else {
            if !current_text.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut current_text), current_style));
            }
            current_style = style;
            current_text.push(ch);
        }
    }

    if !current_text.is_empty() {
        spans.push(Span::styled(current_text, current_style));
    }

    spans
}

/// Render messages for a session into display lines, fitting within a line budget.
fn render_session_messages(ws: &WorktreeSession, max_lines: usize, msg_width: usize) -> Vec<Line<'static>> {
    let prefix_width = 14; // "    ┃ <agent> " with a six-character label
    let effective_msg_width = if msg_width > prefix_width + 4 { msg_width - prefix_width } else { 40 };
    let agent_label = ws.session.agent.display_name();

    let mut all_msg_lines: Vec<Vec<Line>> = Vec::new();

    for (msg_idx, msg) in ws.session.messages.iter().enumerate() {
        let (label, label_style, text_style) = if msg.role == "user" {
            ("You   ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
             Style::default().fg(Color::White))
        } else {
            (agent_label, Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
             Style::default().fg(Color::Gray))
        };

        let mut msg_lines: Vec<Line> = Vec::new();

        if msg_idx > 0 {
            msg_lines.push(Line::from(vec![
                Span::styled("    ┃", Style::default().fg(Color::DarkGray)),
            ]));
        }

        let mut first_line = true;

        for text_line in msg.content.split('\n') {
            if text_line.is_empty() {
                if first_line {
                    msg_lines.push(Line::from(vec![
                        Span::styled("    ┃ ", Style::default().fg(Color::DarkGray)),
                        Span::styled(label, label_style),
                    ]));
                    first_line = false;
                } else {
                    msg_lines.push(Line::from(vec![
                        Span::styled("    ┃ ", Style::default().fg(Color::DarkGray)),
                    ]));
                }
                continue;
            }

            let segments = parse_inline_markdown(text_line, text_style);
            let wrapped = wrap_styled_segments(&segments, effective_msg_width);

            for display_spans in wrapped {
                let mut line_spans = vec![
                    Span::styled("    ┃ ", Style::default().fg(Color::DarkGray)),
                ];
                if first_line {
                    line_spans.push(Span::styled(label, label_style));
                    line_spans.push(Span::raw(" "));
                    first_line = false;
                } else {
                    line_spans.push(Span::raw("       "));
                }
                line_spans.extend(display_spans);
                msg_lines.push(Line::from(line_spans));
            }
        }

        all_msg_lines.push(msg_lines);
    }

    // Walk backwards from newest, fitting whole messages
    let mut budget = max_lines;
    let mut start_msg = all_msg_lines.len();

    for (i, msg_lines) in all_msg_lines.iter().enumerate().rev() {
        if msg_lines.len() <= budget {
            budget -= msg_lines.len();
            start_msg = i;
        } else {
            break;
        }
    }

    let mut lines: Vec<Line> = Vec::new();

    // If there's remaining budget and an older message that didn't fit whole,
    // show its tail to fill the panel
    if budget > 0 && start_msg > 0 {
        let partial_idx = start_msg - 1;
        let partial = &all_msg_lines[partial_idx];
        let partial_msg = &ws.session.messages[partial_idx];

        // Reserve 1 line for the "..." label
        let tail_budget = budget.saturating_sub(1);
        if tail_budget > 0 {
            let (label, label_style) = if partial_msg.role == "user" {
                ("You   ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))
            } else {
                (agent_label, Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD))
            };
            lines.push(Line::from(vec![
                Span::styled("    ┃ ", Style::default().fg(Color::DarkGray)),
                Span::styled(label, label_style),
                Span::styled(" ...", Style::default().fg(Color::DarkGray)),
            ]));
            let skip = partial.len().saturating_sub(tail_budget);
            lines.extend(partial[skip..].iter().cloned());
        }
    }

    if start_msg < all_msg_lines.len() {
        for msg_lines in &all_msg_lines[start_msg..] {
            lines.extend(msg_lines.iter().cloned());
        }
    } else if all_msg_lines.len() == 1 {
        // Only one message and it's too large — show label + tail
        let newest_lines = &all_msg_lines[0];
        let newest_msg = &ws.session.messages[0];
        let (label, label_style) = if newest_msg.role == "user" {
            ("You   ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))
        } else {
            (agent_label, Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD))
        };
        lines.push(Line::from(vec![
            Span::styled("    ┃ ", Style::default().fg(Color::DarkGray)),
            Span::styled(label, label_style),
            Span::styled(" ...", Style::default().fg(Color::DarkGray)),
        ]));
        let tail_budget = max_lines.saturating_sub(1);
        let skip = newest_lines.len().saturating_sub(tail_budget);
        lines.extend(newest_lines[skip..].iter().cloned());
    }

    lines
}

fn run_resume_tui(sessions: &[WorktreeSession]) -> Result<Option<usize>, String> {
    enable_raw_mode().map_err(|e| format!("Failed to enable raw mode: {}", e))?;
    let mut stdout = io::stdout();
    stdout.execute(EnterAlternateScreen).map_err(|e| format!("Failed to enter alternate screen: {}", e))?;

    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = io::stdout().execute(LeaveAlternateScreen);
        original_hook(info);
    }));

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(|e| format!("Failed to create terminal: {}", e))?;

    let mut selected: usize = 0;
    let mut scroll_offset: usize = 0;
    let visible_branches: usize = 10;
    let scroll_margin: usize = 2;

    let result = loop {
        terminal.draw(|frame| {
            let area = frame.area();

            // Clamp visible branches to what fits
            let branch_height = visible_branches.min(sessions.len()) as u16;

            // Layout: title | messages | separator | branches | footer
            let chunks = Layout::vertical([
                Constraint::Length(2),           // title + blank
                Constraint::Min(3),              // message panel (flexible)
                Constraint::Length(1),            // separator
                Constraint::Length(branch_height), // branch list
                Constraint::Length(1),            // footer
            ]).split(area);

            let available_width = area.width as usize;

            // -- Title --
            let title = Paragraph::new(Line::from(vec![
                Span::styled("checkout resume", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::styled(" — Select a worktree to resume", Style::default().fg(Color::DarkGray)),
            ]));
            frame.render_widget(title, chunks[0]);

            // -- Message panel --
            let msg_height = chunks[1].height as usize;
            let msg_lines = render_session_messages(&sessions[selected], msg_height, available_width);
            // Bottom-align: if fewer lines than panel height, pad from top
            let mut padded: Vec<Line> = Vec::new();
            if msg_lines.len() < msg_height {
                for _ in 0..(msg_height - msg_lines.len()) {
                    padded.push(Line::from(""));
                }
            }
            padded.extend(msg_lines);
            let msg_paragraph = Paragraph::new(padded);
            frame.render_widget(msg_paragraph, chunks[1]);

            // -- Separator --
            let sep_line = "─".repeat(available_width);
            let separator = Paragraph::new(Line::from(
                Span::styled(sep_line, Style::default().fg(Color::DarkGray))
            ));
            frame.render_widget(separator, chunks[2]);

            // -- Branch list --
            let visible_count = branch_height as usize;
            let end = (scroll_offset + visible_count).min(sessions.len());

            let mut branch_lines: Vec<Line> = Vec::new();
            for i in scroll_offset..end {
                let ws = &sessions[i];
                let is_sel = i == selected;

                let dir_name = ws.worktree.path.file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| ws.worktree.path.display().to_string());

                let time_ago = format_time_ago(ws.session.last_modified);
                let arrow = if is_sel { "▸ " } else { "  " };
                let branch_part = format!("({})", ws.worktree.branch);

                let left_len = arrow.chars().count() + dir_name.chars().count() + 1 + branch_part.chars().count();
                let time_len = time_ago.chars().count();
                let pad = if available_width > left_len + time_len {
                    available_width - left_len - time_len
                } else {
                    1
                };

                let line_style = if is_sel {
                    Style::default().bg(Color::Rgb(40, 40, 50))
                } else {
                    Style::default()
                };

                branch_lines.push(Line::from(vec![
                    Span::styled(arrow, if is_sel { Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD).bg(Color::Rgb(40, 40, 50)) } else { Style::default() }),
                    Span::styled(dir_name, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD).patch(line_style)),
                    Span::styled(" ", line_style),
                    Span::styled(branch_part, Style::default().fg(Color::DarkGray).patch(line_style)),
                    Span::styled(" ".repeat(pad), line_style),
                    Span::styled(time_ago, Style::default().fg(Color::DarkGray).patch(line_style)),
                ]));
            }
            let branch_paragraph = Paragraph::new(branch_lines);
            frame.render_widget(branch_paragraph, chunks[3]);

            // -- Footer --
            let footer = Paragraph::new(Line::from(vec![
                Span::styled("↑↓", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::styled(" Navigate  ", Style::default().fg(Color::DarkGray)),
                Span::styled("Enter", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::styled(" Resume  ", Style::default().fg(Color::DarkGray)),
                Span::styled("q", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
                Span::styled(" Quit", Style::default().fg(Color::DarkGray)),
            ]));
            frame.render_widget(footer, chunks[4]);
        }).map_err(|e| format!("Draw error: {}", e))?;

        // Handle input
        if event::poll(std::time::Duration::from_millis(100)).unwrap_or(false) {
            if let Ok(Event::Key(key)) = event::read() {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break Ok(None),
                    KeyCode::Enter => {
                        break Ok(Some(selected));
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        if selected > 0 {
                            selected -= 1;
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if selected < sessions.len() - 1 {
                            selected += 1;
                        }
                    }
                    _ => {}
                }

                // Adjust scroll offset to keep selection visible with margin
                let vis = visible_branches.min(sessions.len());
                if selected < scroll_offset + scroll_margin {
                    scroll_offset = selected.saturating_sub(scroll_margin);
                } else if selected + scroll_margin >= scroll_offset + vis {
                    scroll_offset = (selected + scroll_margin + 1).saturating_sub(vis);
                }
                scroll_offset = scroll_offset.min(sessions.len().saturating_sub(vis));
            }
        }
    };

    // Restore terminal
    disable_raw_mode().map_err(|e| format!("Failed to disable raw mode: {}", e))?;
    terminal.backend_mut().execute(LeaveAlternateScreen).map_err(|e| format!("Failed to leave alternate screen: {}", e))?;
    terminal.show_cursor().map_err(|e| format!("Failed to show cursor: {}", e))?;

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_is_the_default_agent() {
        let cli = Cli::try_parse_from(["checkout", "new", "--no-agent"]).unwrap();
        assert_eq!(cli.agent, Agent::Codex);
        assert!(matches!(cli.command, Commands::New { no_agent: true, .. }));
    }

    #[test]
    fn parses_noninteractive_resource_open_commands() {
        let cli = Cli::try_parse_from([
            "checkout",
            "open",
            "pr",
            "830562",
            "--repo",
            "/tmp/figma",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Commands::Open {
                target: OpenTarget::Pr { pr, json: true, .. }
            } if pr == "830562"
        ));

        let cli = Cli::try_parse_from([
            "checkout",
            "open",
            "statsig",
            "my_gate",
            "--repo",
            "/tmp/figma",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Commands::Open {
                target: OpenTarget::Statsig { gate, json: true, .. }
            } if gate == "my_gate"
        ));

        let cli = Cli::try_parse_from([
            "checkout",
            "open",
            "workspace",
            "--repo",
            "/tmp/work-dash",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Commands::Open {
                target: OpenTarget::Workspace { repo, json: true }
            } if repo == PathBuf::from("/tmp/work-dash")
        ));

        let cli = Cli::try_parse_from([
            "checkout",
            "session",
            "pr",
            "830562",
            "--branch",
            "darren/test",
            "--repo",
            "/tmp/figma",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Commands::Session {
                target: SessionTarget::Pr { pr, branch: Some(branch), json: true, .. }
            } if pr == "830562" && branch == "darren/test"
        ));

        let cli = Cli::try_parse_from([
            "checkout",
            "session",
            "workspace",
            "--repo",
            "/tmp/work-dash",
            "--register-current",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Commands::Session {
                target: SessionTarget::Workspace { register_current: true, json: true, .. }
            }
        ));
    }

    #[test]
    fn statsig_worktrees_are_stable_and_shell_arguments_are_quoted() {
        assert_eq!(
            statsig_branch_name("my_gate"),
            "darren/statsig-my-gate"
        );
        assert_eq!(shell_quote("gate with ' quote"), "'gate with '\"'\"' quote'");
        let long = statsig_branch_name("this_is_a_very_long_gate_name_that_needs_a_stable_hash_suffix");
        assert!(long.starts_with("darren/statsig-this-is-a-"));
        assert!(session_name_from_branch(&long).len() <= 25);
        assert!(Regex::new(r"-[0-9a-f]{6}$").unwrap().is_match(&long));
    }

    #[test]
    fn iterm_session_mappings_use_stable_exact_ids() {
        assert_eq!(
            normalized_iterm_session_id("w0t1p0:ABC-123"),
            Some("ABC-123".to_string())
        );
        assert_eq!(
            normalized_iterm_session_id("ABC-123\n"),
            Some("ABC-123".to_string())
        );
        assert_eq!(normalized_iterm_session_id(""), None);

        let first = resource_iterm_session_file("pr", "42", Path::new("/tmp/repo-a"));
        let repeated = resource_iterm_session_file("pr", "42", Path::new("/tmp/repo-a"));
        let other_repo = resource_iterm_session_file("pr", "42", Path::new("/tmp/repo-b"));
        assert_eq!(first, repeated);
        assert_ne!(first, other_repo);

        let first_worktree = worktree_iterm_session_file(Path::new("/tmp/repo-a/worktree"));
        let repeated_worktree = worktree_iterm_session_file(Path::new("/tmp/repo-a/worktree"));
        let same_name_other_repo = worktree_iterm_session_file(Path::new("/tmp/repo-b/worktree"));
        assert_eq!(first_worktree, repeated_worktree);
        assert_ne!(first_worktree, same_name_other_repo);
    }

    #[test]
    fn iterm_api_client_sends_full_resource_context() {
        let socket_path =
            env::temp_dir().join(format!("checkout-iterm-api-{}.sock", std::process::id()));
        let _ = fs::remove_file(&socket_path);
        let listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = io::BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["action"], "open");
            assert_eq!(
                request["sessionIds"],
                serde_json::json!(["ABC-123", "DEF-456"])
            );
            assert_eq!(request["sessionName"], "work-dash");
            assert_eq!(request["launchCommand"], "checkout workspace");
            let mut stream = reader.into_inner();
            stream
                .write_all(b"{\"ok\":true,\"exists\":true,\"sessionId\":\"ABC-123\",\"action\":\"focused\"}\n")
                .unwrap();
        });

        let response = iterm_api_request_at(
            &socket_path,
            &serde_json::json!({
                "action": "open",
                "sessionIds": ["ABC-123", "DEF-456"],
                "sessionName": "work-dash",
                "launchCommand": "checkout workspace",
            }),
        )
        .unwrap();
        assert_eq!(response.session_id.as_deref(), Some("ABC-123"));
        assert_eq!(response.action.as_deref(), Some("focused"));
        server.join().unwrap();
        fs::remove_file(socket_path).unwrap();
    }

    #[test]
    fn iterm_api_client_reports_missing_helper() {
        let socket_path = env::temp_dir().join(format!(
            "checkout-missing-iterm-api-{}.sock",
            std::process::id()
        ));
        let _ = fs::remove_file(&socket_path);

        let error = iterm_api_request_at(
            &socket_path,
            &serde_json::json!({ "action": "status", "sessionIds": [] }),
        )
        .unwrap_err();

        assert!(error.contains("iTerm Python API helper is unavailable"));
    }

    #[test]
    fn claude_and_legacy_flags_remain_supported() {
        let cli = Cli::try_parse_from([
            "checkout",
            "new",
            "--agent",
            "claude",
            "--no-claude",
            "--claude-prompt",
            "/tmp/prompt.md",
        ])
        .unwrap();
        assert_eq!(cli.agent, Agent::Claude);
        assert!(matches!(
            cli.command,
            Commands::New {
                no_agent: true,
                prompt: Some(_),
                ..
            }
        ));
    }

    #[test]
    fn translates_claude_skill_syntax_for_codex() {
        assert_eq!(normalize_skill(Agent::Codex, "/walkthrough"), "$walkthrough");
        assert_eq!(
            normalize_skill(Agent::Codex, "/checkout:checkout-pr"),
            "$checkout-pr"
        );
        assert_eq!(normalize_skill(Agent::Claude, "/walkthrough"), "/walkthrough");
    }

    #[test]
    fn derives_short_session_names_from_branch_leaves() {
        assert_eq!(session_name_from_branch("darren/fix_PR-title"), "fix-pr-title");
        assert_eq!(
            session_name_from_branch("dependabot/npm_and_yarn/react-19"),
            "react-19"
        );
        assert_eq!(
            session_name_from_branch("refs/heads/darren/a-very-long-branch-name-that-keeps-going"),
            "a-very-long-branch-name"
        );
        assert_eq!(session_name_from_branch("---"), "worktree");
    }

    #[test]
    fn exited_markers_are_agent_aware_and_backward_compatible() {
        let current = parse_exited_session("123\n/tmp/branch-a\nclean\ncodex").unwrap();
        assert_eq!(current.agent, Agent::Codex);
        assert!(current.clean);

        let legacy = parse_exited_session("456\n/tmp/branch-b\ndirty").unwrap();
        assert_eq!(legacy.agent, Agent::Claude);
        assert!(!legacy.clean);
    }

    #[test]
    fn parses_codex_messages_without_injected_context() {
        let path = env::temp_dir().join(format!(
            "checkout-codex-session-parser-{}.jsonl",
            std::process::id()
        ));
        let records = [
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "developer",
                    "content": [{"type": "input_text", "text": "hidden"}]
                }
            }),
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "<environment_context>hidden</environment_context>"}]
                }
            }),
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "Implement the change"}]
                }
            }),
            serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "Done"}]
                }
            }),
        ];
        fs::write(
            &path,
            records
                .into_iter()
                .map(|record| record.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();

        let messages = parse_codex_session_messages(&path, 50);
        let _ = fs::remove_file(path);

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[0].content, "Implement the change");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[1].content, "Done");
    }

    #[test]
    fn reads_codex_cli_session_identity() {
        let path = env::temp_dir().join(format!(
            "checkout-codex-session-meta-{}.jsonl",
            std::process::id()
        ));
        fs::write(
            &path,
            serde_json::json!({
                "type": "session_meta",
                "payload": {
                    "id": "019f4918-33a8-7542-96cf-cfcfb2886c75",
                    "cwd": "/tmp/branch-a",
                    "source": "cli"
                }
            })
            .to_string(),
        )
        .unwrap();

        let metadata = codex_session_metadata(&path);
        let _ = fs::remove_file(path);

        assert_eq!(
            metadata,
            Some((
                PathBuf::from("/tmp/branch-a"),
                "019f4918-33a8-7542-96cf-cfcfb2886c75".to_string()
            ))
        );
    }

    #[test]
    fn builds_codex_launch_and_resume_arguments() {
        assert_eq!(
            build_agent_args(
                Agent::Codex,
                Some("do the thing"),
                Some("protect the worktree"),
                false,
                None,
            )
            .unwrap(),
            vec![
                "--config",
                "developer_instructions=\"protect the worktree\"",
                "do the thing",
            ]
        );
        assert_eq!(
            build_agent_args(
                Agent::Codex,
                None,
                Some("protect the worktree"),
                true,
                Some("019f4918-33a8-7542-96cf-cfcfb2886c75"),
            )
            .unwrap(),
            vec![
                "--config",
                "developer_instructions=\"protect the worktree\"",
                "resume",
                "019f4918-33a8-7542-96cf-cfcfb2886c75",
            ]
        );
    }

    #[test]
    fn preserves_claude_launch_arguments() {
        assert_eq!(
            build_agent_args(
                Agent::Claude,
                None,
                Some("protect the worktree"),
                true,
                None,
            )
            .unwrap(),
            vec![
                "--permission-mode",
                "acceptEdits",
                "--continue",
                "--append-system-prompt",
                "protect the worktree",
            ]
        );
    }

    #[test]
    fn resume_target_uses_the_most_recent_sessions_agent() {
        let older_codex = ResumeTarget {
            last_modified: SystemTime::UNIX_EPOCH + Duration::from_secs(10),
            agent: Agent::Codex,
            resume_id: Some("codex-session".to_string()),
        };
        let newer_claude = ResumeTarget {
            last_modified: SystemTime::UNIX_EPOCH + Duration::from_secs(20),
            agent: Agent::Claude,
            resume_id: None,
        };

        assert_eq!(
            choose_resume_target(Some(older_codex), Some(newer_claude)),
            Some(ResumeTarget {
                last_modified: SystemTime::UNIX_EPOCH + Duration::from_secs(20),
                agent: Agent::Claude,
                resume_id: None,
            })
        );
        assert_eq!(choose_resume_target(None, None), None);
    }

    #[test]
    fn cross_agent_resume_option_requests_explicit_approval() {
        let target = ResumeTarget {
            last_modified: SystemTime::UNIX_EPOCH,
            agent: Agent::Claude,
            resume_id: None,
        };

        assert_eq!(
            resume_option_label(Agent::Codex, &target),
            "Resume last Claude session (switch from selected Codex; keep changes, skip update)"
        );
        assert_eq!(
            resume_option_label(Agent::Claude, &target),
            "Resume last Claude session (keep changes, skip update)"
        );
    }
}

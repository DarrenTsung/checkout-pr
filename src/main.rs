use clap::{Parser, Subcommand};
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
use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
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
#[command(about = "Create git worktrees for PRs or new branches")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Print timing information for each operation
    #[arg(long, global = true)]
    timings: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Check out a GitHub PR into a worktree
    Pr {
        /// PR number or GitHub PR URL (e.g., 123 or https://github.com/org/repo/pull/123)
        pr: String,

        /// Optional skill to run after checkout (e.g., /walkthrough)
        skill: Option<String>,

        /// Skip spawning claude after creating the worktree
        #[arg(long)]
        no_claude: bool,

        /// Path to the repo (default: $CHECKOUT_REPO)
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Check out a GitHub PR into a worktree and generate a walkthrough
    Walkthrough {
        /// PR number or GitHub PR URL (e.g., 123 or https://github.com/org/repo/pull/123)
        pr: String,

        /// Skip spawning claude after creating the worktree
        #[arg(long)]
        no_claude: bool,

        /// Path to the repo (default: $CHECKOUT_REPO)
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Check out a GitHub PR into a worktree and review it
    Review {
        /// PR number or GitHub PR URL (e.g., 123 or https://github.com/org/repo/pull/123)
        pr: String,

        /// Skip spawning claude after creating the worktree
        #[arg(long)]
        no_claude: bool,

        /// Path to the repo (default: $CHECKOUT_REPO)
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Create a new branch in a worktree
    Branch {
        /// Branch name (e.g. darren/my-feature)
        name: String,

        /// Skip spawning claude after creating the worktree
        #[arg(long)]
        no_claude: bool,

        /// Path to a file whose contents will be used as the initial Claude prompt
        #[arg(long)]
        claude_prompt: Option<PathBuf>,

        /// Path to the repo (default: $CHECKOUT_REPO)
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Create a new worktree with a random name
    New {
        /// Skip spawning claude after creating the worktree
        #[arg(long)]
        no_claude: bool,

        /// Path to a file whose contents will be used as the initial Claude prompt
        #[arg(long)]
        claude_prompt: Option<PathBuf>,

        /// Path to the repo (default: $CHECKOUT_REPO)
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Create a new worktree and start with /darren:workstream-begin
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
    /// Browse worktrees with Claude sessions and resume one
    Resume {
        /// Path to the repo (default: $CHECKOUT_REPO)
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Resume the most recently exited Claude session
    ResumeLast {
        /// Path to the repo (default: $CHECKOUT_REPO)
        #[arg(long)]
        repo: Option<PathBuf>,
    },
}

#[derive(Deserialize)]
struct PrDetails {
    #[serde(rename = "headRefName")]
    head_ref_name: String,
    title: String,
}

#[derive(Debug)]
enum ExistingWorktreeAction {
    UseExisting,
    ResumeSession,
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

/// Set iTerm2 session title
fn set_iterm_title(title: &str) {
    // OSC 1 sets tab/icon title, OSC 2 sets window title
    // Set both to ensure the title shows
    print!("\x1b]1;{}\x07\x1b]2;{}\x07", title, title);
    std::io::stdout().flush().ok();
}

/// Reset iTerm2 session title
fn reset_iterm_title() {
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
    ctrlc::set_handler(move || {
        if ITERM_MODIFIED.load(Ordering::SeqCst) {
            reset_iterm_background();
            reset_iterm_title();
        }
        // Write session exited file so the worktree can be reused later.
        // PidFileGuard::drop is bypassed by process::exit, so we do it here.
        if let Ok(guard) = ACTIVE_WORKTREE.lock() {
            if let Some(path) = guard.as_ref() {
                remove_session_pid(path);
                write_session_exited(path);
            }
        }
        std::process::exit(130); // Standard exit code for Ctrl+C
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

fn write_session_pid(worktree_path: &Path, pid: u32) {
    let dir = get_session_dir();
    let _ = fs::create_dir_all(&dir);
    let _ = fs::write(session_pid_file(worktree_path), pid.to_string());
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

fn write_session_exited(worktree_path: &Path) {
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
        format!("{}\n{}\n{}", timestamp, worktree_path.display(), if clean { "clean" } else { "dirty" }),
    );
}

fn read_session_pid(worktree_path: &Path) -> Option<u32> {
    fs::read_to_string(session_pid_file(worktree_path))
        .ok()?
        .trim()
        .parse()
        .ok()
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
}

impl PidFileGuard {
    fn new(worktree_path: &Path, pid: u32) -> Self {
        write_session_pid(worktree_path, pid);
        // Clear the exited marker so this worktree isn't considered idle
        let _ = fs::remove_file(session_exited_file(worktree_path));
        // Register so the ctrlc handler can clean up
        if let Ok(mut guard) = ACTIVE_WORKTREE.lock() {
            *guard = Some(worktree_path.to_path_buf());
        }
        Self {
            worktree_path: worktree_path.to_path_buf(),
        }
    }
}

impl Drop for PidFileGuard {
    fn drop(&mut self) {
        // Clear the global first so ctrlc handler won't double-write
        if let Ok(mut guard) = ACTIVE_WORKTREE.lock() {
            *guard = None;
        }
        remove_session_pid(&self.worktree_path);
        write_session_exited(&self.worktree_path);
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

    if cli.timings {
        TIMINGS_ENABLED.store(true, Ordering::Relaxed);
    }

    match cli.command {
        Commands::Pr { pr, no_claude, repo, skill } => run_pr(&pr, no_claude, repo, "/checkout:checkout-pr", skill.as_deref()),
        Commands::Walkthrough { pr, no_claude, repo } => run_pr(&pr, no_claude, repo, "/checkout:checkout-pr", Some("/walkthrough")),
        Commands::Review { pr, no_claude, repo } => run_pr(&pr, no_claude, repo, "/checkout:checkout-and-review-pr", None),
        Commands::Branch { name, no_claude, claude_prompt, repo } => {
            let prompt = read_prompt_file(claude_prompt)?;
            run_branch(&name, no_claude, prompt, repo)
        },
        Commands::New { no_claude, claude_prompt, repo } => {
            let prompt = read_prompt_file(claude_prompt)?;
            run_new(no_claude, prompt, repo)
        },
        Commands::Begin { repo } => run_new(false, Some("/darren:workstream-begin multiworkspace".to_string()), repo),
        Commands::Status { repo } => run_status(repo),
        Commands::Clean { repo, yes } => run_clean(repo, yes),
        Commands::Resume { repo } => run_resume(repo),
        Commands::ResumeLast { repo } => run_resume_last(repo),
    }
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

fn run_pr(pr: &str, no_claude: bool, repo: Option<PathBuf>, claude_prompt: &str, chained_skill: Option<&str>) -> Result<(), String> {
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

    // Check for an existing worktree by branch name first (from `checkout branch`), then PR prefix,
    // then by checked-out branch (covers `checkout new` worktrees with random names)
    let branch_slug = pr_details.head_ref_name.rsplit('/').next().unwrap_or(&pr_details.head_ref_name);
    let existing = find_existing_worktree(&repo_root, &format!("branch-{}", branch_slug))?
        .or(find_existing_worktree(&repo_root, &format!("pr-{}-", pr_number))?)
        .or(find_existing_worktree(&repo_root, &format!("[{}]", pr_details.head_ref_name))?);

    let mut resume = false;
    let mut is_new_worktree = false;

    let final_path = if let Some(existing_path) = existing {
        println!(
            "\n{} Worktree already exists at {}",
            "!".yellow().bold(),
            existing_path.display().to_string().cyan()
        );

        let changes_handle = {
            let path = existing_path.clone();
            thread::spawn(move || get_uncommitted_status(&path))
        };

        let action = prompt_existing_worktree_action(changes_handle)?;

        match action {
            ExistingWorktreeAction::ResumeSession => {
                resume = true;
                existing_path
            }
            ExistingWorktreeAction::UseExisting => {
                print!("{} Updating to latest... ", "→".blue().bold());
                std::io::stdout().flush().ok();
                match update_worktree(&existing_path, &pr_details.head_ref_name) {
                    Ok(()) => println!("{}", "done".green()),
                    Err(e) => println!("{}\n  {} {}", "skipped".yellow(), "⚠".yellow().bold(), e.dimmed()),
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
        Some(spawn_background_setup(final_path.clone(), repo_root.clone()))
    } else {
        None
    };

    println!();
    println!(
        "{} Worktree ready at {}",
        "✓".green().bold(),
        final_path.display().to_string().cyan().bold()
    );

    if no_claude {
        println!(
            "\n{} Run: {} {} {}",
            "tip:".yellow().bold(),
            "cd".dimmed(),
            final_path.display(),
            "&& claude".dimmed()
        );
    } else {
        let bg_color = pick_available_color(&final_path);
        save_worktree_color(&final_path, &bg_color)?;

        // Guard ensures iTerm settings are reset even on Ctrl+C or panic
        let _iterm_guard = ItermGuard::new(&bg_color, &format!("{} [WORKTREE]", pr_details.head_ref_name));

        let system_prompt = build_worktree_system_prompt();

        if resume {
            println!();
            println!(
                "{} Resuming last claude session...",
                "→".blue().bold(),
            );
            println!();
            spawn_claude_continue(&final_path, Some(&system_prompt))?;
        } else {
            let full_prompt = match chained_skill {
                Some(skill) => format!("{} {}\n\nAfter completing the above, run: {}", claude_prompt, pr_number, skill),
                None => format!("{} {}", claude_prompt, pr_number),
            };
            println!();
            println!(
                "{} Spawning claude with {}...",
                "→".blue().bold(),
                full_prompt.cyan()
            );
            println!();
            spawn_claude_with_prompt(&final_path, &full_prompt, Some(&system_prompt))?;
        }
    }

    if let Some(handle) = bg_handle {
        let _ = handle.join();
    }

    Ok(())
}

fn run_branch(name: &str, no_claude: bool, claude_prompt: Option<String>, repo: Option<PathBuf>) -> Result<(), String> {
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

    // Check for an existing worktree by branch directory name or by git branch ref (from `checkout pr`)
    let existing = find_existing_worktree(&repo_root, &format!("branch-{}", slug))?
        .or(find_existing_worktree(&repo_root, &format!("[{}]", branch_name))?);

    let mut resume = false;
    let mut is_new_worktree = false;

    let final_path = if let Some(existing_path) = existing {
        println!(
            "\n{} Worktree already exists at {}",
            "!".yellow().bold(),
            existing_path.display().to_string().cyan()
        );

        let changes_handle = {
            let path = existing_path.clone();
            thread::spawn(move || get_uncommitted_status(&path))
        };

        let action = prompt_existing_worktree_action(changes_handle)?;

        match action {
            ExistingWorktreeAction::ResumeSession => {
                resume = true;
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
        Some(spawn_background_setup(final_path.clone(), repo_root.clone()))
    } else {
        None
    };

    println!();
    println!(
        "{} Worktree ready at {}",
        "✓".green().bold(),
        final_path.display().to_string().cyan().bold()
    );

    if no_claude {
        println!(
            "\n{} Run: {} {} {}",
            "tip:".yellow().bold(),
            "cd".dimmed(),
            final_path.display(),
            "&& claude".dimmed()
        );
    } else {
        let bg_color = pick_available_color(&final_path);
        save_worktree_color(&final_path, &bg_color)?;

        // Guard ensures iTerm settings are reset even on Ctrl+C or panic
        let _iterm_guard = ItermGuard::new(&bg_color, &format!("{} [WORKTREE]", branch_name));

        let system_prompt = build_worktree_system_prompt();

        if resume {
            println!();
            println!(
                "{} Resuming last claude session...",
                "→".blue().bold(),
            );
            println!();
            spawn_claude_continue(&final_path, Some(&system_prompt))?;
        } else {
            println!();
            println!(
                "{} Spawning claude...",
                "→".blue().bold(),
            );
            println!();

            if let Some(prompt) = &claude_prompt {
                spawn_claude_with_prompt(&final_path, prompt, Some(&system_prompt))?;
            } else {
                spawn_claude(&final_path, Some(&system_prompt))?;
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

/// Find the oldest idle worktree created by `checkout new`:
/// no active session, has an .exited file, and no uncommitted changes.
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

        // Must have an .exited file (session ended and not resumed)
        let exited_file = session_exited_file(path);
        let content = match fs::read_to_string(&exited_file) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let mut lines = content.lines();
        let timestamp: u64 = match lines.next().and_then(|l| l.trim().parse().ok()) {
            Some(t) => t,
            None => continue,
        };
        // Skip the worktree path line
        let _ = lines.next();
        // Check the clean/dirty status recorded at exit time
        let status = lines.next().unwrap_or("dirty").trim();
        if status != "clean" {
            continue;
        }

        // Must not have an active session
        if let Some(pid) = read_session_pid(path) {
            if is_pid_alive(pid) {
                continue;
            }
        }

        candidates.push((timestamp, path.clone()));
    }

    // Pick the oldest (earliest exit timestamp)
    candidates.sort_by_key(|(ts, _)| *ts);
    Ok(candidates.into_iter().next().map(|(_, path)| path))
}

fn reset_worktree_to_master(worktree_path: &PathBuf) -> Result<(), String> {
    timing!("reset_worktree_to_master");
    // Fetch latest master
    print!("{} Fetching latest master... ", "→".blue().bold());
    std::io::stdout().flush().ok();
    let status = Command::new("git")
        .args(["-C", &worktree_path.to_string_lossy(), "fetch", "origin", "master"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("Failed to fetch: {}", e))?;
    if !status.success() {
        return Err("Failed to fetch origin/master".to_string());
    }
    println!("{}", "done".green());

    // Reset branch to origin/master
    print!("{} Resetting to latest master... ", "→".blue().bold());
    std::io::stdout().flush().ok();
    let status = Command::new("git")
        .args(["-C", &worktree_path.to_string_lossy(), "reset", "--hard", "origin/master"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("Failed to reset: {}", e))?;
    if !status.success() {
        return Err("Failed to reset to origin/master".to_string());
    }
    println!("{}", "done".green());

    Ok(())
}

fn run_new(no_claude: bool, claude_prompt: Option<String>, repo: Option<PathBuf>) -> Result<(), String> {
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
        let status = Command::new("git")
            .args(["-C", &reusable.to_string_lossy(), "checkout", "-b", &branch_name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| format!("Failed to create branch: {}", e))?;
        if !status.success() {
            return Err(format!("Failed to create branch {}", branch_name));
        }

        // Rename the worktree directory to match the new workspace name
        let new_path = reusable.parent().unwrap().join(format!("branch-{}", workspace_name));
        let status = Command::new("git")
            .args([
                "-C", &repo_root.to_string_lossy(),
                "worktree", "move",
                &reusable.to_string_lossy(),
                &new_path.to_string_lossy(),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| format!("Failed to rename worktree: {}", e))?;
        if !status.success() {
            return Err(format!(
                "Failed to rename worktree from {} to {}",
                reusable.display(),
                new_path.display()
            ));
        }

        println!();
        println!(
            "{} Worktree ready at {}",
            "✓".green().bold(),
            new_path.display().to_string().cyan().bold()
        );

        if no_claude {
            println!(
                "\n{} Run: {} {} {}",
                "tip:".yellow().bold(),
                "cd".dimmed(),
                new_path.display(),
                "&& claude".dimmed()
            );
        } else {
            let bg_color = pick_available_color(&new_path);
            save_worktree_color(&new_path, &bg_color)?;

            let _iterm_guard = ItermGuard::new(&bg_color, &format!("{} [WORKTREE]", branch_name));

            let system_prompt = build_worktree_system_prompt();

            println!();
            println!(
                "{} Spawning claude...",
                "→".blue().bold(),
            );
            println!();

            if let Some(prompt) = &claude_prompt {
                spawn_claude_with_prompt(&new_path, prompt, Some(&system_prompt))?;
            } else {
                spawn_claude(&new_path, Some(&system_prompt))?;
            }
        }

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

    run_branch(&branch_name, no_claude, claude_prompt, repo)
}

#[derive(Clone)]
struct WorktreeInfo {
    path: PathBuf,
    branch: String,
    has_changes: bool,
    has_active_session: bool,
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
            let orphaned_pids: Vec<u32> = Vec::new();
            WorktreeInfo { path, branch, has_changes, has_active_session, orphaned_pids }
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
            "active".blue().bold()
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

        // Kill orphaned claude processes before removing
        for pid in &wt.orphaned_pids {
            print!(
                "{} Killing orphaned claude process (pid {})... ",
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
            remove_session_pid(&wt.path);
            remove_bazel_output_base(&wt.path);

            println!("{}", "done".green());
            removed_count += 1;
        } else {
            println!("{}", "failed".red());
            let stderr = String::from_utf8_lossy(&output.stderr);
            let error_msg = stderr.trim();
            if !error_msg.is_empty() {
                println!("    {} {}", "error:".red(), error_msg);
            }
            failed.push(dir_name);
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

    // Removable = no uncommitted changes AND no active (terminal-attached) session.
    // Orphaned claude processes (no terminal) will be killed before removal.
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
                    format!("(killing {} orphaned claude process{})", wt.orphaned_pids.len(),
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
            "{} Keeping {} worktree(s) with active Claude sessions:\n",
            "→".blue().bold(),
            active_worktrees.len()
        );

        for wt in &active_worktrees {
            let dir_name = wt.path.file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| wt.path.display().to_string());

            println!(
                "  {} {} {}",
                format!("[{}]", "active".blue().bold()),
                dir_name.cyan(),
                format!("({})", wt.branch).dimmed()
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

            print!(
                "{} Remove {} {}? [y/N]: ",
                "?".magenta().bold(),
                dir_name.cyan(),
                "(has uncommitted changes)".yellow()
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

    Ok(())
}

/// Spawn non-critical worktree setup steps in the background so Claude can
/// start sooner. Runs `mise trust` (if available) and `symlink_node_modules`.
/// Errors are logged to stderr but otherwise ignored.
fn spawn_background_setup(
    worktree_path: PathBuf,
    repo_root: PathBuf,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        if which_mise().is_some() {
            if let Err(e) = run_mise_trust(&worktree_path) {
                eprintln!("background: mise trust failed: {}", e);
            }
        }

        if let Err(e) = symlink_node_modules(&worktree_path, &repo_root) {
            eprintln!("background: symlink_node_modules failed: {}", e);
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

    // Copy claude settings
    print!("{} Copying claude settings... ", "→".blue().bold());
    std::io::stdout().flush().ok();
    symlink_claude_settings(worktree_path, repo_root)?;
    println!("{}", "done".green());

    // Add claude trust
    print!("{} Adding claude trust... ", "→".blue().bold());
    std::io::stdout().flush().ok();
    add_claude_trust(worktree_path, repo_root)?;
    println!("{}", "done".green());

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

    // Copy claude settings
    print!("{} Copying claude settings... ", "→".blue().bold());
    std::io::stdout().flush().ok();
    symlink_claude_settings(worktree_path, repo_root)?;
    println!("{}", "done".green());

    // Add claude trust
    print!("{} Adding claude trust... ", "→".blue().bold());
    std::io::stdout().flush().ok();
    add_claude_trust(worktree_path, repo_root)?;
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
) -> Result<ExistingWorktreeAction, String> {
    println!();
    println!(
        "  {} Resume last claude session {}",
        "[1]".cyan().bold(),
        "(keep changes, skip update)".dimmed()
    );
    println!("  {} Use existing worktree", "[2]".cyan().bold());
    println!("  {} Create new worktree", "[3]".cyan().bold());
    println!();

    loop {
        print!("{} Choose an option [1/2/3]: ", "?".magenta().bold());
        io::stdout().flush().map_err(|e| e.to_string())?;

        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .map_err(|e| format!("Failed to read input: {}", e))?;

        match input.trim() {
            "1" => return Ok(ExistingWorktreeAction::ResumeSession),
            "2" => {
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
            "3" => return Ok(ExistingWorktreeAction::CreateNew),
            _ => {
                println!(
                    "{} Invalid option, please enter {}",
                    "!".red().bold(),
                    "1, 2, or 3"
                );
            }
        }
    }
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

fn spawn_claude_with_prompt(worktree_path: &PathBuf, prompt: &str, append_system_prompt: Option<&str>) -> Result<(), String> {
    set_terminal_cwd(worktree_path);

    let mut cmd = Command::new("claude");
    cmd.args(["--permission-mode", "acceptEdits"])
        .arg(prompt);
    if let Some(system_prompt) = append_system_prompt {
        cmd.args(["--append-system-prompt", system_prompt]);
    }
    let mut child = cmd
        .current_dir(worktree_path)
        .spawn()
        .map_err(|e| format!("Failed to spawn claude: {}", e))?;

    let _pid_guard = PidFileGuard::new(worktree_path, child.id());
    let status = child.wait().map_err(|e| format!("Failed to wait for claude: {}", e))?;

    if !status.success() {
        return Err("claude exited with error".to_string());
    }

    Ok(())
}

fn spawn_claude_continue(worktree_path: &PathBuf, append_system_prompt: Option<&str>) -> Result<(), String> {
    set_terminal_cwd(worktree_path);

    let mut cmd = Command::new("claude");
    cmd.args(["--permission-mode", "acceptEdits", "--continue"]);
    if let Some(system_prompt) = append_system_prompt {
        cmd.args(["--append-system-prompt", system_prompt]);
    }
    let mut child = cmd
        .current_dir(worktree_path)
        .spawn()
        .map_err(|e| format!("Failed to spawn claude: {}", e))?;

    let _pid_guard = PidFileGuard::new(worktree_path, child.id());
    let status = child.wait().map_err(|e| format!("Failed to wait for claude: {}", e))?;

    if !status.success() {
        return Err("claude exited with error".to_string());
    }

    Ok(())
}

fn spawn_claude(worktree_path: &PathBuf, append_system_prompt: Option<&str>) -> Result<(), String> {
    set_terminal_cwd(worktree_path);

    let mut cmd = Command::new("claude");
    cmd.args(["--permission-mode", "acceptEdits"]);
    if let Some(system_prompt) = append_system_prompt {
        cmd.args(["--append-system-prompt", system_prompt]);
    }
    let mut child = cmd
        .current_dir(worktree_path)
        .spawn()
        .map_err(|e| format!("Failed to spawn claude: {}", e))?;

    let _pid_guard = PidFileGuard::new(worktree_path, child.id());
    let status = child.wait().map_err(|e| format!("Failed to wait for claude: {}", e))?;

    if !status.success() {
        return Err("claude exited with error".to_string());
    }

    Ok(())
}

// ---------- resume subcommand ----------

struct ChatMessage {
    role: String,   // "user" or "assistant"
    content: String,
}

struct SessionInfo {
    last_modified: SystemTime,
    messages: Vec<ChatMessage>,
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
fn parse_session_messages(jsonl_path: &Path, max_messages: usize) -> Vec<ChatMessage> {
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
fn find_worktree_session(worktree_path: &Path) -> Option<SessionInfo> {
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
    let messages = parse_session_messages(&jsonl_path, 50);

    if messages.is_empty() {
        return None;
    }

    Some(SessionInfo { last_modified, messages })
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
    print!("{} Finding Claude sessions... ", "→".blue().bold());
    io::stdout().flush().ok();
    let mut sessions: Vec<WorktreeSession> = entries
        .into_iter()
        .filter_map(|(path, branch)| {
            let session = find_worktree_session(&path)?;
            let worktree = WorktreeInfo {
                path,
                branch,
                has_changes: false,
                has_active_session: false,
                orphaned_pids: Vec::new(),
            };
            Some(WorktreeSession { worktree, session })
        })
        .collect();
    println!("{} ({} with sessions)", "done".green(), sessions.len());

    if sessions.is_empty() {
        println!("{} No worktrees with Claude sessions found", "→".blue().bold());
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

    // Set up iTerm colors and spawn claude --continue
    let bg_color = pick_available_color(worktree_path);
    save_worktree_color(worktree_path, &bg_color)?;

    let _iterm_guard = ItermGuard::new(&bg_color, &format!("{} [WORKTREE]", ws.worktree.branch));

    println!(
        "\n{} Resuming session in {}...\n",
        "→".blue().bold(),
        worktree_path.display().to_string().cyan()
    );

    spawn_claude_continue(worktree_path, None)?;

    Ok(())
}

fn run_resume_last(repo: Option<PathBuf>) -> Result<(), String> {
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

        let mut lines = content.lines();
        let timestamp: u64 = match lines.next().and_then(|l| l.trim().parse().ok()) {
            Some(t) => t,
            None => continue,
        };
        let worktree_path = match lines.next() {
            Some(p) => PathBuf::from(p.trim()),
            None => continue,
        };

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

    let bg_color = pick_available_color(&worktree_path);
    save_worktree_color(&worktree_path, &bg_color)?;

    let _iterm_guard = ItermGuard::new(&bg_color, &format!("{} [WORKTREE]", branch));

    let system_prompt = build_worktree_system_prompt();

    println!();
    println!(
        "{} Resuming claude session...",
        "→".blue().bold(),
    );
    println!();

    spawn_claude_continue(&worktree_path, Some(&system_prompt))?;

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
    let prefix_width = 14; // "    ┃ Claude " = 14 chars
    let effective_msg_width = if msg_width > prefix_width + 4 { msg_width - prefix_width } else { 40 };

    let mut all_msg_lines: Vec<Vec<Line>> = Vec::new();

    for (msg_idx, msg) in ws.session.messages.iter().enumerate() {
        let (label, label_style, text_style) = if msg.role == "user" {
            ("You   ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
             Style::default().fg(Color::White))
        } else {
            ("Claude", Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
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
                ("Claude", Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD))
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
            ("Claude", Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD))
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

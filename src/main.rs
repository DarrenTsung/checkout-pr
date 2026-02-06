use clap::{Parser, Subcommand};
use colored::Colorize;
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

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

#[derive(Parser)]
#[command(name = "checkout")]
#[command(about = "Create git worktrees for PRs or new branches")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Check out a GitHub PR into a worktree
    Pr {
        /// PR number or GitHub PR URL (e.g., 123 or https://github.com/figma/figma/pull/123)
        pr: String,

        /// Skip spawning claude after creating the worktree
        #[arg(long)]
        no_claude: bool,

        /// Path to the main figma repo (default: ~/figma/figma)
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Create a new branch in a worktree
    Branch {
        /// Branch name (will be prefixed with darren/ if not already)
        name: String,

        /// Skip spawning claude after creating the worktree
        #[arg(long)]
        no_claude: bool,

        /// Path to the main figma repo (default: ~/figma/figma)
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// List all worktrees and their status
    Status {
        /// Path to the main figma repo (default: ~/figma/figma)
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Remove worktrees that have no uncommitted changes
    Clean {
        /// Path to the main figma repo (default: ~/figma/figma)
        #[arg(long)]
        repo: Option<PathBuf>,

        /// Skip confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,
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

fn run() -> Result<(), String> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Pr { pr, no_claude, repo } => run_pr(&pr, no_claude, repo),
        Commands::Branch { name, no_claude, repo } => run_branch(&name, no_claude, repo),
        Commands::Status { repo } => run_status(repo),
        Commands::Clean { repo, yes } => run_clean(repo, yes),
    }
}

fn run_pr(pr: &str, no_claude: bool, repo: Option<PathBuf>) -> Result<(), String> {
    let pr_number = extract_pr_number(pr)?;
    println!(
        "{} PR #{}",
        "→".blue().bold(),
        pr_number.to_string().cyan()
    );

    let home = env::var("HOME").map_err(|_| "HOME not set")?;
    let repo_root = repo.unwrap_or_else(|| PathBuf::from(format!("{}/figma/figma", home)));

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
    let worktree_dir = PathBuf::from(format!("{}/figma-worktrees", home));
    let worktree_path = worktree_dir.join(format!("pr-{}-{}", pr_number, slug));

    let existing = find_existing_worktree(&repo_root, &format!("pr-{}-", pr_number))?;

    let final_path = if let Some(existing_path) = existing {
        println!(
            "\n{} Worktree already exists at {}",
            "!".yellow().bold(),
            existing_path.display().to_string().cyan()
        );

        let has_changes = has_uncommitted_changes(&existing_path)?;
        if has_changes {
            println!(
                "  {} {}",
                "⚠".yellow().bold(),
                "Worktree has uncommitted changes!".yellow()
            );
        }

        let action = prompt_existing_worktree_action(has_changes)?;

        match action {
            ExistingWorktreeAction::UseExisting => {
                print!("{} Updating to latest... ", "→".blue().bold());
                std::io::stdout().flush().ok();
                update_worktree(&existing_path, &pr_details.head_ref_name)?;
                println!("{}", "done".green());
                existing_path
            }
            ExistingWorktreeAction::CreateNew => {
                let new_path = find_next_worktree_path(&worktree_dir, &format!("pr-{}-{}", pr_number, slug))?;
                create_new_worktree_from_remote(&repo_root, &worktree_dir, &new_path, &pr_details.head_ref_name)?;
                new_path
            }
        }
    } else {
        create_new_worktree_from_remote(&repo_root, &worktree_dir, &worktree_path, &pr_details.head_ref_name)?;
        worktree_path
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
        println!();
        println!(
            "{} Spawning claude with {}...",
            "→".blue().bold(),
            format!("/darren:checkout-pr {}", pr_number).cyan()
        );
        println!();

        let bg_color = pick_available_color(&final_path);
        save_worktree_color(&final_path, &bg_color)?;

        // Guard ensures iTerm settings are reset even on Ctrl+C or panic
        let _iterm_guard = ItermGuard::new(&bg_color, &format!("{} [WORKTREE]", pr_details.head_ref_name));

        spawn_claude_pr(&final_path, pr_number)?;
    }

    Ok(())
}

fn run_branch(name: &str, no_claude: bool, repo: Option<PathBuf>) -> Result<(), String> {
    // Ensure branch name has darren/ prefix
    let branch_name = if name.starts_with("darren/") {
        name.to_string()
    } else {
        format!("darren/{}", name)
    };

    println!(
        "{} Branch {}",
        "→".blue().bold(),
        branch_name.cyan()
    );

    let home = env::var("HOME").map_err(|_| "HOME not set")?;
    let repo_root = repo.unwrap_or_else(|| PathBuf::from(format!("{}/figma/figma", home)));

    if !repo_root.exists() {
        return Err(format!("Repo not found at {}", repo_root.display()));
    }

    // Create slug from branch name (remove darren/ prefix for the slug)
    let slug = branch_name.strip_prefix("darren/").unwrap_or(&branch_name);
    let worktree_dir = PathBuf::from(format!("{}/figma-worktrees", home));
    let worktree_path = worktree_dir.join(format!("branch-{}", slug));

    // Check if worktree already exists
    let existing = find_existing_worktree(&repo_root, &format!("branch-{}", slug))?;

    let final_path = if let Some(existing_path) = existing {
        println!(
            "\n{} Worktree already exists at {}",
            "!".yellow().bold(),
            existing_path.display().to_string().cyan()
        );

        let has_changes = has_uncommitted_changes(&existing_path)?;
        if has_changes {
            println!(
                "  {} {}",
                "⚠".yellow().bold(),
                "Worktree has uncommitted changes!".yellow()
            );
        }

        let action = prompt_existing_worktree_action(has_changes)?;

        match action {
            ExistingWorktreeAction::UseExisting => {
                existing_path
            }
            ExistingWorktreeAction::CreateNew => {
                let new_path = find_next_worktree_path(&worktree_dir, &format!("branch-{}", slug))?;
                create_new_worktree_new_branch(&repo_root, &worktree_dir, &new_path, &branch_name)?;
                new_path
            }
        }
    } else {
        create_new_worktree_new_branch(&repo_root, &worktree_dir, &worktree_path, &branch_name)?;
        worktree_path
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
        println!();
        println!(
            "{} Spawning claude...",
            "→".blue().bold(),
        );
        println!();

        let bg_color = pick_available_color(&final_path);
        save_worktree_color(&final_path, &bg_color)?;

        // Guard ensures iTerm settings are reset even on Ctrl+C or panic
        let _iterm_guard = ItermGuard::new(&bg_color, &format!("{} [WORKTREE]", branch_name));

        spawn_claude(&final_path)?;
    }

    Ok(())
}

struct WorktreeInfo {
    path: PathBuf,
    branch: String,
    has_changes: bool,
}

fn get_all_worktrees(repo_root: &PathBuf) -> Result<Vec<WorktreeInfo>, String> {
    let output = Command::new("git")
        .args(["-C", &repo_root.to_string_lossy(), "worktree", "list", "--porcelain"])
        .output()
        .map_err(|e| format!("Failed to list worktrees: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut worktrees = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_branch: Option<String> = None;

    for line in stdout.lines() {
        if let Some(path_str) = line.strip_prefix("worktree ") {
            current_path = Some(PathBuf::from(path_str));
        } else if let Some(branch_str) = line.strip_prefix("branch refs/heads/") {
            current_branch = Some(branch_str.to_string());
        } else if line.is_empty() {
            if let Some(path) = current_path.take() {
                // Skip the main repo itself
                if path != *repo_root {
                    let branch = current_branch.take().unwrap_or_else(|| "(detached)".to_string());
                    let has_changes = has_uncommitted_changes(&path).unwrap_or(false);
                    worktrees.push(WorktreeInfo {
                        path,
                        branch,
                        has_changes,
                    });
                }
            }
            current_branch = None;
        }
    }

    // Sort by path for consistent output
    worktrees.sort_by(|a, b| a.path.cmp(&b.path));

    Ok(worktrees)
}

fn run_status(repo: Option<PathBuf>) -> Result<(), String> {
    let home = env::var("HOME").map_err(|_| "HOME not set")?;
    let repo_root = repo.unwrap_or_else(|| PathBuf::from(format!("{}/figma/figma", home)));

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
        let status = if wt.has_changes {
            "modified".yellow().bold()
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

    let clean_count = worktrees.iter().filter(|w| !w.has_changes).count();
    let modified_count = worktrees.iter().filter(|w| w.has_changes).count();

    println!();
    if clean_count > 0 {
        println!(
            "{} {} clean worktree(s) can be removed with: {} {}",
            "tip:".yellow().bold(),
            clean_count,
            "checkout clean".cyan(),
            if modified_count > 0 { format!("({} with changes will be kept)", modified_count) } else { String::new() }.dimmed()
        );
    }

    Ok(())
}

fn run_clean(repo: Option<PathBuf>, skip_confirm: bool) -> Result<(), String> {
    let home = env::var("HOME").map_err(|_| "HOME not set")?;
    let repo_root = repo.unwrap_or_else(|| PathBuf::from(format!("{}/figma/figma", home)));

    if !repo_root.exists() {
        return Err(format!("Repo not found at {}", repo_root.display()));
    }

    let worktrees = get_all_worktrees(&repo_root)?;
    let clean_worktrees: Vec<_> = worktrees.into_iter().filter(|w| !w.has_changes).collect();

    if clean_worktrees.is_empty() {
        println!("{} No clean worktrees to remove", "→".blue().bold());
        return Ok(());
    }

    println!(
        "{} Found {} clean worktree(s) to remove:\n",
        "→".blue().bold(),
        clean_worktrees.len()
    );

    for wt in &clean_worktrees {
        let dir_name = wt.path.file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| wt.path.display().to_string());
        println!("  {} {}", "•".dimmed(), dir_name.cyan());
    }

    if !skip_confirm {
        println!();
        print!(
            "{} Remove these worktrees? [y/N]: ",
            "?".magenta().bold()
        );
        io::stdout().flush().map_err(|e| e.to_string())?;

        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .map_err(|e| format!("Failed to read input: {}", e))?;

        if input.trim().to_lowercase() != "y" {
            println!("{} Cancelled", "→".blue().bold());
            return Ok(());
        }
    }

    println!();

    let mut removed_count = 0;
    let mut failed: Vec<String> = Vec::new();

    for wt in &clean_worktrees {
        let dir_name = wt.path.file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| wt.path.display().to_string());

        print!("{} Removing {}... ", "→".blue().bold(), dir_name.cyan());
        std::io::stdout().flush().ok();

        // Remove worktree using git
        let output = Command::new("git")
            .args([
                "-C",
                &repo_root.to_string_lossy(),
                "worktree",
                "remove",
                &wt.path.to_string_lossy(),
            ])
            .output()
            .map_err(|e| format!("Failed to remove worktree: {}", e))?;

        if output.status.success() {
            // Also remove the color file
            let color_file = worktree_color_file(&wt.path);
            let _ = fs::remove_file(color_file);

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

    println!();
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

fn create_new_worktree_from_remote(
    repo_root: &PathBuf,
    worktree_dir: &PathBuf,
    worktree_path: &PathBuf,
    branch: &str,
) -> Result<(), String> {
    std::fs::create_dir_all(worktree_dir)
        .map_err(|e| format!("Failed to create worktrees dir: {}", e))?;

    print!(
        "{} Fetching branch {}... ",
        "→".blue().bold(),
        branch.yellow()
    );
    std::io::stdout().flush().ok();
    fetch_branch(repo_root, branch)?;
    println!("{}", "done".green());

    print!(
        "{} Creating worktree at {}... ",
        "→".blue().bold(),
        worktree_path.display().to_string().cyan()
    );
    std::io::stdout().flush().ok();
    create_worktree_from_ref(repo_root, worktree_path, &format!("origin/{}", branch))?;
    println!("{}", "done".green());

    if which_mise().is_some() {
        print!("{} Running mise trust... ", "→".blue().bold());
        std::io::stdout().flush().ok();
        run_mise_trust(worktree_path)?;
        println!("{}", "done".green());
    }

    // Copy claude settings
    print!("{} Copying claude settings... ", "→".blue().bold());
    std::io::stdout().flush().ok();
    copy_claude_settings(worktree_path, repo_root)?;
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
    std::fs::create_dir_all(worktree_dir)
        .map_err(|e| format!("Failed to create worktrees dir: {}", e))?;

    // Fetch latest master
    print!("{} Fetching latest master... ", "→".blue().bold());
    std::io::stdout().flush().ok();
    fetch_branch(repo_root, "master")?;
    println!("{}", "done".green());

    print!(
        "{} Creating worktree with new branch {}... ",
        "→".blue().bold(),
        branch.yellow()
    );
    std::io::stdout().flush().ok();
    create_worktree_new_branch(repo_root, worktree_path, branch)?;
    println!("{}", "done".green());

    if which_mise().is_some() {
        print!("{} Running mise trust... ", "→".blue().bold());
        std::io::stdout().flush().ok();
        run_mise_trust(worktree_path)?;
        println!("{}", "done".green());
    }

    // Track with graphite
    print!("{} Tracking with Graphite... ", "→".blue().bold());
    std::io::stdout().flush().ok();
    run_gt_track(worktree_path)?;
    println!("{}", "done".green());

    // Copy claude settings
    print!("{} Copying claude settings... ", "→".blue().bold());
    std::io::stdout().flush().ok();
    copy_claude_settings(worktree_path, repo_root)?;
    println!("{}", "done".green());

    // Add claude trust
    print!("{} Adding claude trust... ", "→".blue().bold());
    std::io::stdout().flush().ok();
    add_claude_trust(worktree_path, repo_root)?;
    println!("{}", "done".green());

    Ok(())
}

fn has_uncommitted_changes(worktree_path: &PathBuf) -> Result<bool, String> {
    let output = Command::new("git")
        .args(["-C", &worktree_path.to_string_lossy(), "status", "--porcelain"])
        .output()
        .map_err(|e| format!("Failed to check git status: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(!stdout.trim().is_empty())
}

fn prompt_existing_worktree_action(has_changes: bool) -> Result<ExistingWorktreeAction, String> {
    println!();
    if has_changes {
        println!(
            "  {} Use existing worktree {}",
            "[1]".cyan().bold(),
            "(will discard uncommitted changes!)".yellow()
        );
    } else {
        println!("  {} Use existing worktree", "[1]".cyan().bold());
    }
    println!("  {} Create new worktree", "[2]".cyan().bold());
    println!();

    loop {
        print!("{} Choose an option [1/2]: ", "?".magenta().bold());
        io::stdout().flush().map_err(|e| e.to_string())?;

        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .map_err(|e| format!("Failed to read input: {}", e))?;

        match input.trim() {
            "1" => {
                if has_changes {
                    print!(
                        "{} Are you sure you want to discard changes? [y/N]: ",
                        "!".yellow().bold()
                    );
                    io::stdout().flush().map_err(|e| e.to_string())?;

                    let mut confirm = String::new();
                    io::stdin()
                        .read_line(&mut confirm)
                        .map_err(|e| format!("Failed to read input: {}", e))?;

                    if confirm.trim().to_lowercase() != "y" {
                        println!("{} Cancelled", "→".blue().bold());
                        continue;
                    }
                }
                return Ok(ExistingWorktreeAction::UseExisting);
            }
            "2" => return Ok(ExistingWorktreeAction::CreateNew),
            _ => {
                println!("{} Invalid option, please enter 1 or 2", "!".red().bold());
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

fn fetch_branch(repo_root: &PathBuf, branch: &str) -> Result<(), String> {
    let status = Command::new("git")
        .args(["-C", &repo_root.to_string_lossy(), "fetch", "origin", branch])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("Failed to fetch: {}", e))?;

    if !status.success() {
        return Err("git fetch failed".to_string());
    }

    Ok(())
}

fn create_worktree_from_ref(repo_root: &PathBuf, worktree_path: &PathBuf, git_ref: &str) -> Result<(), String> {
    let status = Command::new("git")
        .args([
            "-C",
            &repo_root.to_string_lossy(),
            "worktree",
            "add",
            &worktree_path.to_string_lossy(),
            git_ref,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("Failed to create worktree: {}", e))?;

    if !status.success() {
        // Try with FETCH_HEAD if branch is checked out elsewhere
        let status = Command::new("git")
            .args([
                "-C",
                &repo_root.to_string_lossy(),
                "worktree",
                "add",
                &worktree_path.to_string_lossy(),
                "FETCH_HEAD",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| format!("Failed to create worktree with FETCH_HEAD: {}", e))?;

        if !status.success() {
            return Err("git worktree add failed".to_string());
        }
    }

    Ok(())
}

fn create_worktree_new_branch(repo_root: &PathBuf, worktree_path: &PathBuf, branch: &str) -> Result<(), String> {
    let status = Command::new("git")
        .args([
            "-C",
            &repo_root.to_string_lossy(),
            "worktree",
            "add",
            "-b",
            branch,
            &worktree_path.to_string_lossy(),
            "origin/master",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("Failed to create worktree: {}", e))?;

    if !status.success() {
        return Err("git worktree add failed".to_string());
    }

    Ok(())
}

fn update_worktree(worktree_path: &PathBuf, branch: &str) -> Result<(), String> {
    let status = Command::new("git")
        .args(["-C", &worktree_path.to_string_lossy(), "fetch", "origin", branch])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("Failed to fetch: {}", e))?;

    if !status.success() {
        return Err("git fetch failed".to_string());
    }

    let ref_name = format!("origin/{}", branch);
    let status = Command::new("git")
        .args([
            "-C",
            &worktree_path.to_string_lossy(),
            "reset",
            "--hard",
            &ref_name,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("Failed to reset: {}", e))?;

    if !status.success() {
        return Err("git reset failed".to_string());
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
        .args(["trust"])
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
    let status = Command::new("gt")
        .args(["track", "--no-interactive"])
        .current_dir(worktree_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("Failed to run gt track: {}", e))?;

    if !status.success() {
        return Err("gt track failed".to_string());
    }

    Ok(())
}

fn copy_claude_settings(worktree_path: &PathBuf, repo_root: &PathBuf) -> Result<(), String> {
    // Copy the main repo's .claude/settings.local.json to the worktree
    // This contains MCP server configurations and other local settings
    let source = repo_root.join(".claude/settings.local.json");

    if !source.exists() {
        // Fall back to global settings if repo-specific doesn't exist
        let home = env::var("HOME").map_err(|_| "HOME not set")?;
        let global_source = PathBuf::from(format!("{}/.claude/settings.local.json", home));

        if !global_source.exists() {
            return Ok(());
        }

        let dest_dir = worktree_path.join(".claude");
        fs::create_dir_all(&dest_dir)
            .map_err(|e| format!("Failed to create .claude dir: {}", e))?;

        let dest = dest_dir.join("settings.local.json");
        fs::copy(&global_source, &dest)
            .map_err(|e| format!("Failed to copy claude settings: {}", e))?;

        return Ok(());
    }

    let dest_dir = worktree_path.join(".claude");
    fs::create_dir_all(&dest_dir)
        .map_err(|e| format!("Failed to create .claude dir: {}", e))?;

    let dest = dest_dir.join("settings.local.json");
    fs::copy(&source, &dest)
        .map_err(|e| format!("Failed to copy claude settings: {}", e))?;

    Ok(())
}

fn add_claude_trust(worktree_path: &PathBuf, repo_root: &PathBuf) -> Result<(), String> {
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

fn spawn_claude_pr(worktree_path: &PathBuf, pr_number: u64) -> Result<(), String> {
    let prompt = format!("/darren:checkout-pr {}", pr_number);

    set_terminal_cwd(worktree_path);

    let status = Command::new("claude")
        .arg(&prompt)
        .current_dir(worktree_path)
        .status()
        .map_err(|e| format!("Failed to spawn claude: {}", e))?;

    if !status.success() {
        return Err("claude exited with error".to_string());
    }

    Ok(())
}

fn spawn_claude(worktree_path: &PathBuf) -> Result<(), String> {
    set_terminal_cwd(worktree_path);

    let status = Command::new("claude")
        .current_dir(worktree_path)
        .status()
        .map_err(|e| format!("Failed to spawn claude: {}", e))?;

    if !status.success() {
        return Err("claude exited with error".to_string());
    }

    Ok(())
}

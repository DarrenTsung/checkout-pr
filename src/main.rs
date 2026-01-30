use clap::{Parser, Subcommand};
use colored::Colorize;
use regex::Regex;
use serde::Deserialize;
use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

/// Color palette - subtle dark backgrounds with different hues
const COLOR_PALETTE: &[&str] = &[
    "1a1a2e", // blue-ish
    "1a2e1a", // green-ish
    "2e1a1a", // red-ish
    "2e2e1a", // yellow-ish
    "2e1a2e", // purple-ish
    "1a2e2e", // cyan-ish
    "251a2e", // magenta-ish
    "1a252e", // teal-ish
    "2e251a", // orange-ish
    "1e1a2e", // indigo-ish
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
    print!("\x1b]1337;SetColors=bg=default\x07");
    std::io::stdout().flush().ok();
}

/// Set iTerm2 tab badge (persists even when apps change the title)
fn set_iterm_badge(text: &str) {
    use std::io::Write;
    // iTerm2 proprietary: SetBadgeFormat takes base64-encoded text
    let encoded = base64_encode(text);
    print!("\x1b]1337;SetBadgeFormat={}\x07", encoded);
    std::io::stdout().flush().ok();
}

/// Clear iTerm2 tab badge
fn clear_iterm_badge() {
    print!("\x1b]1337;SetBadgeFormat=\x07");
    std::io::stdout().flush().ok();
}

// Track whether we've modified iTerm settings
static ITERM_MODIFIED: AtomicBool = AtomicBool::new(false);

/// RAII guard that resets iTerm settings on drop
struct ItermGuard;

impl ItermGuard {
    fn new(bg_color: &str, badge: &str) -> Self {
        set_iterm_background(bg_color);
        set_iterm_badge(badge);
        ITERM_MODIFIED.store(true, Ordering::SeqCst);
        Self
    }
}

impl Drop for ItermGuard {
    fn drop(&mut self) {
        if ITERM_MODIFIED.load(Ordering::SeqCst) {
            reset_iterm_background();
            clear_iterm_badge();
            ITERM_MODIFIED.store(false, Ordering::SeqCst);
        }
    }
}

fn setup_ctrlc_handler() {
    ctrlc::set_handler(move || {
        if ITERM_MODIFIED.load(Ordering::SeqCst) {
            reset_iterm_background();
            clear_iterm_badge();
        }
        std::process::exit(130); // Standard exit code for Ctrl+C
    })
    .ok();
}

fn base64_encode(input: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_bytes();
    let mut result = String::new();

    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = chunk.get(1).copied().unwrap_or(0) as usize;
        let b2 = chunk.get(2).copied().unwrap_or(0) as usize;

        result.push(ALPHABET[b0 >> 2] as char);
        result.push(ALPHABET[((b0 & 0x03) << 4) | (b1 >> 4)] as char);

        if chunk.len() > 1 {
            result.push(ALPHABET[((b1 & 0x0f) << 2) | (b2 >> 6)] as char);
        } else {
            result.push('=');
        }

        if chunk.len() > 2 {
            result.push(ALPHABET[b2 & 0x3f] as char);
        } else {
            result.push('=');
        }
    }

    result
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
    let pr_details = fetch_pr_details(pr_number)?;
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

        print!("{} Opening Cursor & Sublime Merge... ", "→".blue().bold());
        std::io::stdout().flush().ok();
        open_cursor(&final_path)?;
        open_sublime_merge(&final_path)?;
        println!("{}", "done".green());

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

        print!("{} Opening Cursor & Sublime Merge... ", "→".blue().bold());
        std::io::stdout().flush().ok();
        open_cursor(&final_path)?;
        open_sublime_merge(&final_path)?;
        println!("{}", "done".green());

        spawn_claude(&final_path)?;
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

fn fetch_pr_details(pr_number: u64) -> Result<PrDetails, String> {
    let output = Command::new("gh")
        .args(["pr", "view", &pr_number.to_string(), "--json", "headRefName,title"])
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

fn open_cursor(worktree_path: &PathBuf) -> Result<(), String> {
    Command::new("cursor")
        .arg(worktree_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to open Cursor: {}", e))?;

    Ok(())
}

fn open_sublime_merge(worktree_path: &PathBuf) -> Result<(), String> {
    Command::new("smerge")
        .arg(worktree_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("Failed to open Sublime Merge: {}", e))?;

    Ok(())
}

fn spawn_claude_pr(worktree_path: &PathBuf, pr_number: u64) -> Result<(), String> {
    let prompt = format!("/darren:checkout-pr {}", pr_number);

    let cmd = format!(
        "cd '{}' && claude '{}'",
        worktree_path.display(),
        prompt
    );
    let status = Command::new("bash")
        .args(["-c", &cmd])
        .status()
        .map_err(|e| format!("Failed to spawn claude: {}", e))?;

    if !status.success() {
        return Err("claude exited with error".to_string());
    }

    Ok(())
}

fn spawn_claude(worktree_path: &PathBuf) -> Result<(), String> {
    let cmd = format!("cd '{}' && claude", worktree_path.display());
    let status = Command::new("bash")
        .args(["-c", &cmd])
        .status()
        .map_err(|e| format!("Failed to spawn claude: {}", e))?;

    if !status.success() {
        return Err("claude exited with error".to_string());
    }

    Ok(())
}

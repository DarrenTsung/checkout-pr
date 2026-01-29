use clap::Parser;
use colored::Colorize;
use regex::Regex;
use serde::Deserialize;
use std::env;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(Parser)]
#[command(name = "checkout-pr")]
#[command(about = "Create a worktree for a GitHub PR and spawn claude to review it")]
#[command(version)]
struct Args {
    /// PR number or GitHub PR URL (e.g., 123 or https://github.com/figma/figma/pull/123)
    pr: String,

    /// Skip spawning claude after creating the worktree
    #[arg(long)]
    no_claude: bool,

    /// Path to the main figma repo (default: ~/figma/figma)
    #[arg(long)]
    repo: Option<PathBuf>,
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
    if let Err(e) = run() {
        eprintln!("{} {}", "error:".red().bold(), e);
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let args = Args::parse();

    // Extract PR number
    let pr_number = extract_pr_number(&args.pr)?;
    println!(
        "{} PR #{}",
        "→".blue().bold(),
        pr_number.to_string().cyan()
    );

    // Get repo root
    let home = env::var("HOME").map_err(|_| "HOME not set")?;
    let repo_root = args
        .repo
        .unwrap_or_else(|| PathBuf::from(format!("{}/figma/figma", home)));

    if !repo_root.exists() {
        return Err(format!("Repo not found at {}", repo_root.display()));
    }

    // Fetch PR details
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

    // Create slug from title
    let slug = create_slug(&pr_details.title);
    let worktree_dir = PathBuf::from(format!("{}/figma-worktrees", home));
    let worktree_path = worktree_dir.join(format!("pr-{}-{}", pr_number, slug));

    // Check for existing worktree
    let existing = find_existing_worktree(&repo_root, pr_number)?;

    let final_path = if let Some(existing_path) = existing {
        println!(
            "\n{} Worktree already exists at {}",
            "!".yellow().bold(),
            existing_path.display().to_string().cyan()
        );

        // Check for uncommitted changes
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
                // Find next available suffix
                let new_path = find_next_worktree_path(&worktree_dir, pr_number, &slug)?;
                create_new_worktree(&repo_root, &worktree_dir, &new_path, &pr_details.head_ref_name)?;
                new_path
            }
        }
    } else {
        create_new_worktree(&repo_root, &worktree_dir, &worktree_path, &pr_details.head_ref_name)?;
        worktree_path
    };

    println!();
    println!(
        "{} Worktree ready at {}",
        "✓".green().bold(),
        final_path.display().to_string().cyan().bold()
    );

    if args.no_claude {
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

        spawn_claude(&final_path, pr_number)?;
    }

    Ok(())
}

fn create_new_worktree(
    repo_root: &PathBuf,
    worktree_dir: &PathBuf,
    worktree_path: &PathBuf,
    branch: &str,
) -> Result<(), String> {
    // Create worktrees directory
    std::fs::create_dir_all(worktree_dir)
        .map_err(|e| format!("Failed to create worktrees dir: {}", e))?;

    // Fetch the branch
    print!(
        "{} Fetching branch {}... ",
        "→".blue().bold(),
        branch.yellow()
    );
    std::io::stdout().flush().ok();
    fetch_branch(repo_root, branch)?;
    println!("{}", "done".green());

    // Create worktree
    print!(
        "{} Creating worktree at {}... ",
        "→".blue().bold(),
        worktree_path.display().to_string().cyan()
    );
    std::io::stdout().flush().ok();
    create_worktree(repo_root, worktree_path, branch)?;
    println!("{}", "done".green());

    // Run mise trust
    if which_mise().is_some() {
        print!("{} Running mise trust... ", "→".blue().bold());
        std::io::stdout().flush().ok();
        run_mise_trust(worktree_path)?;
        println!("{}", "done".green());
    }

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

fn find_next_worktree_path(worktree_dir: &PathBuf, pr_number: u64, slug: &str) -> Result<PathBuf, String> {
    let base_name = format!("pr-{}-{}", pr_number, slug);

    // Check existing worktrees to find next available suffix
    let mut suffix = 2;
    loop {
        let candidate = worktree_dir.join(format!("{}-{}", base_name, suffix));
        if !candidate.exists() {
            return Ok(candidate);
        }
        suffix += 1;
        if suffix > 100 {
            return Err("Too many worktrees for this PR".to_string());
        }
    }
}

fn extract_pr_number(input: &str) -> Result<u64, String> {
    // Try direct number
    if let Ok(num) = input.parse::<u64>() {
        return Ok(num);
    }

    // Try URL pattern
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
    // Remove prefix like "multiplayer: " or "web: "
    let without_prefix = if let Some(idx) = title.find(": ") {
        &title[idx + 2..]
    } else {
        title
    };

    // Convert to lowercase, replace non-alphanumeric with hyphens, take first 4 words
    let slug: String = without_prefix
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect();

    // Clean up multiple hyphens and trim
    let re = Regex::new(r"-+").unwrap();
    let cleaned = re.replace_all(&slug, "-");
    let trimmed = cleaned.trim_matches('-');

    // Take first 4 segments
    trimmed
        .split('-')
        .filter(|s| !s.is_empty())
        .take(4)
        .collect::<Vec<_>>()
        .join("-")
}

fn find_existing_worktree(repo_root: &PathBuf, pr_number: u64) -> Result<Option<PathBuf>, String> {
    let output = Command::new("git")
        .args(["-C", &repo_root.to_string_lossy(), "worktree", "list"])
        .output()
        .map_err(|e| format!("Failed to list worktrees: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let pattern = format!("pr-{}-", pr_number);

    for line in stdout.lines() {
        if line.contains(&pattern) {
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

fn create_worktree(repo_root: &PathBuf, worktree_path: &PathBuf, branch: &str) -> Result<(), String> {
    let ref_name = format!("origin/{}", branch);

    let status = Command::new("git")
        .args([
            "-C",
            &repo_root.to_string_lossy(),
            "worktree",
            "add",
            &worktree_path.to_string_lossy(),
            &ref_name,
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

fn update_worktree(worktree_path: &PathBuf, branch: &str) -> Result<(), String> {
    // Fetch
    let status = Command::new("git")
        .args(["-C", &worktree_path.to_string_lossy(), "fetch", "origin", branch])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("Failed to fetch: {}", e))?;

    if !status.success() {
        return Err("git fetch failed".to_string());
    }

    // Reset
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

fn spawn_claude(worktree_path: &PathBuf, pr_number: u64) -> Result<(), String> {
    let prompt = format!("/darren:checkout-pr {}", pr_number);

    let status = Command::new("claude")
        .args(["--prompt", &prompt])
        .current_dir(worktree_path)
        .status()
        .map_err(|e| format!("Failed to spawn claude: {}", e))?;

    if !status.success() {
        return Err("claude exited with error".to_string());
    }

    Ok(())
}

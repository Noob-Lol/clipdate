use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use console::style;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use rayon::prelude::*;
use regex::Regex;
use semver::Version;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};
use supports_unicode::Stream;

// ── Config file schema ───────────────────────────────────────────────────────

/// Represents one tool entry in tools.json.
///
/// Example tools.json:
/// ```json
#[doc = include_str!("../tools.json")]
/// ```
///
/// Template variables: {VERSION} is replaced with the resolved latest semver string.
#[derive(Debug, Clone, Deserialize, Serialize)]
struct ToolDef {
    /// Display name, also used to match CLI arguments.
    name: String,

    /// The executable filename as installed (e.g. "koyeb.exe").
    exe_name: String,

    /// Arguments passed to the exe to retrieve its version string.
    version_args: Vec<String>,

    /// Regex with one capture group that extracts the semver from the version output.
    /// Example: `"(\\d+\\.\\d+\\.\\d+)"`
    version_regex: String,

    /// GitHub repo in "owner/repo" form.
    repo: String,

    /// GitHub release asset filename template, e.g. `"koyeb-cli_{VERSION}_windows_amd64.zip"`.
    asset_template: String,

    /// Path of the entry *inside* the archive that should be extracted, e.g. `"koyeb.exe"` or
    /// `"cli_{VERSION}.exe"`.
    /// If not specified, the asset itself is assumed to be a raw executable file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    archive_entry_template: Option<String>,
}

fn get_own_repo() -> &'static str {
    option_env!("CLIPDATE_REPO").unwrap_or("Noob-Lol/clipdate")
}

fn get_own_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn get_os() -> &'static str {
    if std::env::consts::OS == "macos" {
        "darwin"
    } else {
        std::env::consts::OS
    }
}

fn get_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    }
}

/// Full Rust target triple, e.g. `x86_64-pc-windows-msvc` or `aarch64-apple-darwin`.
/// Baked in at compile time via build.rs — matches what cargo-dist uses in asset filenames.
fn get_target() -> &'static str {
    env!("CLIPDATE_TARGET")
}

fn get_exe_suffix() -> &'static str {
    if cfg!(windows) { ".exe" } else { "" }
}

fn get_archive_ext() -> &'static str {
    if cfg!(windows) { "zip" } else { "tar.gz" }
}

fn expand_template_with_os_arch(template: &str, version: &str, os: &str, arch: &str) -> String {
    template
        .replace("{VERSION}", version)
        .replace("{OS}", os)
        .replace("{ARCH}", arch)
        .replace("{TARGET}", get_target())
        .replace("{EXE}", get_exe_suffix())
        .replace("{EXT}", get_archive_ext())
}

fn expand_exe_template(template: &str, version: &str, os: &str, arch: &str) -> String {
    let mut s = expand_template_with_os_arch(template, version, os, arch);
    let suffix = get_exe_suffix();
    if !suffix.is_empty() && !s.ends_with(suffix) && !s.contains("{EXE}") {
        s.push_str(suffix);
    }
    s
}

// ── GitHub API ───────────────────────────────────────────────────────────────

#[derive(Deserialize, Clone)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

fn fetch_latest_version(
    client: &reqwest::blocking::Client,
    repo: &str,
) -> Result<(String, Vec<GhAsset>)> {
    let url = format!("https://api.github.com/repos/{}/releases/latest", repo);
    let resp = client
        .get(&url)
        .send()
        .with_context(|| format!("GET {}", url))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        if status.as_u16() == 403 || status.as_u16() == 429 {
            bail!(
                "GitHub rate limit hit for {}. Pass --token / set GITHUB_TOKEN to raise limits.\n{}",
                repo,
                body
            );
        }
        bail!("GitHub API error {} for {}: {}", status, repo, body);
    }

    let release: GhRelease = resp.json().with_context(|| "parsing GitHub release JSON")?;
    let tag = release.tag_name.trim_start_matches('v').to_string();
    Ok((tag, release.assets))
}

// ── Version detection ────────────────────────────────────────────────────────

/// Run `<exe> <args>` and return the combined stdout+stderr trimmed, or `None`
/// if the executable is not found / fails to run at all.
fn run_version_cmd(exe: &str, args: &[String]) -> Option<String> {
    let out = Command::new(exe).args(args).output().ok()?;
    // Combine stdout and stderr so version strings printed to either are found.
    let mut combined = String::with_capacity(out.stdout.len() + out.stderr.len() + 1);
    combined.push_str(String::from_utf8_lossy(&out.stdout).trim());
    combined.push('\n');
    combined.push_str(String::from_utf8_lossy(&out.stderr).trim());
    Some(combined)
}

fn parse_version(output: &str, re: &Regex) -> Result<Version> {
    let cap = re.captures(output).and_then(|c| c.get(1)).ok_or_else(|| {
        anyhow!(
            "version pattern '{}' not found in output:\n{}",
            re.as_str(),
            output
        )
    })?;
    Version::parse(cap.as_str())
        .with_context(|| format!("'{}' is not a valid semver", cap.as_str()))
}

// ── Download + extract ───────────────────────────────────────────────────────

fn download_bytes(
    client: &reqwest::blocking::Client,
    url: &str,
    mp: &MultiProgress,
    glyphs: &Glyphs,
) -> Result<Vec<u8>> {
    let mut resp = client
        .get(url)
        .send()
        .with_context(|| format!("GET {}", url))?;

    if !resp.status().is_success() {
        bail!("Download failed ({}): {}", resp.status(), url);
    }

    // Use Content-Length for a rich progress bar, fall back to spinner.
    let content_len = resp
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    // Register with MultiProgress so concurrent bars don't overwrite each other.
    let pb = if let Some(len) = content_len {
        let pb = mp.add(ProgressBar::new(len));
        pb.set_style(
            ProgressStyle::with_template(
                "  {spinner:.cyan} {msg}\n  [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, {eta})",
            )
            .unwrap()
            .progress_chars(glyphs.progress_chars)
            .tick_strings(glyphs.spinner),
        );
        pb
    } else {
        let pb = mp.add(ProgressBar::new_spinner());
        pb.set_style(
            ProgressStyle::with_template("{spinner:.cyan} {msg} {bytes}")
                .unwrap()
                .tick_strings(glyphs.spinner),
        );
        pb
    };
    pb.enable_steady_tick(Duration::from_millis(80));

    // Extract just the filename from the URL for a clean display message.
    let file_name = url.rsplit('/').next().unwrap_or(url);
    pb.set_message(format!("Downloading {}", file_name));

    // Stream in 64 KB chunks — reduces syscall overhead vs the old 8 KB size.
    let capacity = content_len
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(0);
    let mut buf = Vec::with_capacity(capacity);
    let mut chunk = [0u8; 65536];
    loop {
        let n = resp
            .read(&mut chunk)
            .with_context(|| "reading download body")?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        pb.inc(n as u64);
    }

    pb.finish_and_clear();
    Ok(buf)
}

fn extract_entry(zip_bytes: &[u8], entry_name: &str, dest: &Path) -> Result<()> {
    let cursor = io::Cursor::new(zip_bytes);
    let mut archive = zip::ZipArchive::new(cursor).context("opening zip")?;

    // Collect matching indices first to avoid borrowing `archive` twice.
    // Match case-insensitively against both the full path and the basename.
    let index = (0..archive.len())
        .find(|&i| {
            // by_index_raw is cheaper: reads metadata only, no decompression.
            archive
                .by_index_raw(i)
                .map(|f| {
                    let fname = f.name().replace('\\', "/");
                    let basename = fname.split('/').next_back().unwrap_or(&fname);
                    basename.eq_ignore_ascii_case(entry_name)
                        || f.name().eq_ignore_ascii_case(entry_name)
                })
                .unwrap_or(false)
        })
        .ok_or_else(|| anyhow!("'{}' not found in zip", entry_name))?;

    // Now open for real (decompressing) using the confirmed index.
    let mut file = archive.by_index(index).context("reading zip entry")?;
    let mut out = fs::File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
    io::copy(&mut file, &mut out).context("writing extracted file")?;
    Ok(())
}

fn extract_tar_gz(tar_gz_bytes: &[u8], entry_name: &str, dest: &Path) -> Result<()> {
    let cursor = io::Cursor::new(tar_gz_bytes);
    let tar = flate2::read::GzDecoder::new(cursor);
    let mut archive = tar::Archive::new(tar);

    for file in archive.entries().context("reading tar entries")? {
        let mut file = file.context("reading tar entry")?;
        let path = file.path().context("reading tar entry path")?;

        let fname = path.to_string_lossy().replace('\\', "/");
        let basename = fname.split('/').next_back().unwrap_or(&fname);

        if basename.eq_ignore_ascii_case(entry_name) || fname.eq_ignore_ascii_case(entry_name) {
            let mut out =
                fs::File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
            io::copy(&mut file, &mut out).context("writing extracted file")?;
            return Ok(());
        }
    }
    bail!("'{}' not found in tar.gz", entry_name);
}

// ── Per-tool update flow ─────────────────────────────────────────────────────

struct UpdateCtx<'a> {
    install_dir: &'a Path,
    client: &'a reqwest::blocking::Client,
    mp: &'a MultiProgress,
    dry_run: bool,
    force: bool,
    glyphs: &'a Glyphs,
    app_config: &'a AppConfig,
}

#[derive(Debug)]
enum UpdateResult {
    UpToDate(String),
    Updated {
        from: String,
        to: String,
    },
    Installed(String),
    DryRun {
        current: Option<String>,
        latest: String,
    },
}

fn process_tool(tool: &ToolDef, ctx: &UpdateCtx<'_>) -> Result<UpdateResult> {
    let exe_name = expand_exe_template(&tool.exe_name, "", get_os(), get_arch());
    let exe_path = ctx.install_dir.join(&exe_name);

    // ── Compile version regex once per tool ──────────────────────────────────
    let version_re = Regex::new(&tool.version_regex).with_context(|| {
        format!(
            "invalid version_regex for '{}': {}",
            tool.name, tool.version_regex
        )
    })?;

    // ── Current version ──────────────────────────────────────────────────────
    let current_ver: Option<Version> =
        run_version_cmd(exe_path.to_str().unwrap_or(&exe_name), &tool.version_args)
            .and_then(|out| parse_version(&out, &version_re).ok());

    process_tool_impl(tool, exe_path, current_ver, ctx)
}

fn process_tool_impl(
    tool: &ToolDef,
    exe_path: PathBuf,
    current_ver: Option<Version>,
    ctx: &UpdateCtx<'_>,
) -> Result<UpdateResult> {
    // ── Latest version ───────────────────────────────────────────────────────
    let (latest_str, assets) = fetch_latest_version(ctx.client, &tool.repo)?;
    let latest_ver = Version::parse(&latest_str)
        .with_context(|| format!("GitHub returned non-semver tag: {}", latest_str))?;

    // ── Dry run ──────────────────────────────────────────────────────────────
    if ctx.dry_run {
        return Ok(UpdateResult::DryRun {
            current: current_ver.map(|v| v.to_string()),
            latest: latest_ver.to_string(),
        });
    }

    // ── Up-to-date check ─────────────────────────────────────────────────────
    if !ctx.force
        && let Some(ref cur) = current_ver
        && *cur >= latest_ver
    {
        return Ok(UpdateResult::UpToDate(cur.to_string()));
    }

    // ── OS/Arch fallback matching ───────────────────────────────────────────
    let os_list = get_os_list(ctx.app_config);
    let arch_list = get_arch_list(ctx.app_config);

    let (url, asset_name, matched_os, matched_arch) = if let Some(m) = os_list.iter().find_map(|os| {
        arch_list.iter().find_map(|arch| {
            let cand = expand_template_with_os_arch(&tool.asset_template, &latest_str, os, arch);
            assets.iter()
                .find(|a| a.name.eq_ignore_ascii_case(&cand))
                .map(|a| (a.browser_download_url.clone(), a.name.clone(), os.clone(), arch.clone()))
        })
    }) {
        m
    } else {
        // Fallback to default template expansion
        let default_os = get_os();
        let default_arch = get_arch();
        let default_name = expand_template_with_os_arch(&tool.asset_template, &latest_str, default_os, default_arch);
        (
            format!(
                "https://github.com/{}/releases/download/v{}/{}",
                tool.repo, latest_str, default_name
            ),
            default_name,
            default_os.to_string(),
            default_arch.to_string(),
        )
    };

    // ── Download ─────────────────────────────────────────────────────────────
    let asset_bytes = download_bytes(ctx.client, &url, ctx.mp, ctx.glyphs)?;

    // ── Checksum verification ────────────────────────────────────────────────
    let checksum_asset = assets.iter().find(|a| {
        let name = a.name.to_lowercase();
        if name.ends_with(".sig") || name.ends_with(".asc") || name.ends_with(".pem") {
            return false;
        }
        name == format!("{}.sha256", asset_name.to_lowercase())
            || name == format!("{}.sha256sum", asset_name.to_lowercase())
            || name == "checksums.txt"
            || name == "sha256sums.txt"
            || name == "sha256sums"
            || name.contains("checksums")
            || name.contains("sha256")
    });

    if let Some(checksum_asset) = checksum_asset {
        let checksum_bytes = download_bytes(
            ctx.client,
            &checksum_asset.browser_download_url,
            ctx.mp,
            ctx.glyphs,
        )
        .with_context(|| format!("downloading checksum file '{}'", checksum_asset.name))?;
        let checksum_text = String::from_utf8_lossy(&checksum_bytes);

        let mut expected_hash = None;
        for line in checksum_text.lines().filter(|l| l.contains(&asset_name)) {
            if let Some(hash) = line.split_whitespace().next() {
                expected_hash = Some(hash.to_lowercase());
                break;
            }
        }

        if let Some(expected) = expected_hash {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(&asset_bytes);
            let actual = hex::encode(hasher.finalize());
            if actual != expected {
                bail!(
                    "checksum mismatch for {}: expected {}, got {}",
                    asset_name,
                    expected,
                    actual
                );
            }
        }
    }

    // ── Extract or direct-write ───────────────────────────────────────────────
    let exe_file_name = exe_path
        .file_name()
        .ok_or_else(|| anyhow!("invalid exe path: {}", exe_path.display()))?
        .to_string_lossy();
    let tmp_path = ctx.install_dir.join(format!("{}.tmp", exe_file_name));

    let archive_entry = tool
        .archive_entry_template
        .as_ref()
        .map(|t| expand_exe_template(t, &latest_str, &matched_os, &matched_arch));

    match archive_entry {
        Some(ref archive_entry) => {
            if url.ends_with(".zip") {
                extract_entry(&asset_bytes, archive_entry, &tmp_path)
                    .with_context(|| format!("extracting '{}' from zip", archive_entry))?;
            } else if url.ends_with(".tar.gz") || url.ends_with(".tgz") {
                extract_tar_gz(&asset_bytes, archive_entry, &tmp_path)
                    .with_context(|| format!("extracting '{}' from tar.gz", archive_entry))?;
            } else {
                bail!("unsupported archive format for URL: {}", url);
            }
        }
        None => {
            // Asset is a raw executable — write it directly.
            fs::write(&tmp_path, &asset_bytes)
                .with_context(|| format!("writing {}", tmp_path.display()))?;
        }
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(mut perms) = fs::metadata(&tmp_path).map(|m| m.permissions()) {
            perms.set_mode(0o755);
            let _ = fs::set_permissions(&tmp_path, perms);
        }
    }

    // ── Atomic rename with Windows file-lock workaround ───────────────────────
    // On Windows a running executable cannot be deleted, but it *can* be renamed.
    // Move the old binary aside first so we can put the new one in its place.
    let old_path = ctx.install_dir.join(format!("{}.old", exe_file_name));
    if exe_path.exists() {
        // Remove a previous leftover .old if present, ignoring errors.
        let _ = fs::remove_file(&old_path);
        fs::rename(&exe_path, &old_path)
            .with_context(|| format!("renaming {} to .old", exe_path.display()))?;
    }
    fs::rename(&tmp_path, &exe_path)
        .with_context(|| format!("renaming tmp to {}", exe_path.display()))?;
    // Best-effort removal of the .old file; ignore if still locked.
    let _ = fs::remove_file(&old_path);

    match current_ver {
        Some(cur) => Ok(UpdateResult::Updated {
            from: cur.to_string(),
            to: latest_ver.to_string(),
        }),
        None => Ok(UpdateResult::Installed(latest_ver.to_string())),
    }
}

fn perform_self_update(ctx: &UpdateCtx<'_>) -> Result<UpdateResult> {
    let repo = get_own_repo();
    let tool = ToolDef {
        name: "clipdate".to_string(),
        exe_name: "clipdate".to_string(),
        version_args: vec![],
        version_regex: "".to_string(),
        repo: repo.to_string(),
        // our names are Go style, <binary>_<version>_<os>_<arch>.<ext>, basically {NAME}_{VERSION}_{OS}_{ARCH}.{EXT}
        asset_template: "clipdate_{VERSION}_{OS}_{ARCH}.{EXT}".to_string(),
        archive_entry_template: Some("clipdate".to_string()),
    };

    let resolved_exe_name = expand_exe_template(&tool.exe_name, "", get_os(), get_arch());
    let exe_path =
        std::env::current_exe().unwrap_or_else(|_| ctx.install_dir.join(&resolved_exe_name));
    let current_ver = Version::parse(get_own_version()).ok();

    process_tool_impl(&tool, exe_path, current_ver, ctx)
}

// ── CLI ──────────────────────────────────────────────────────────────────────

/// Update self-contained CLI tools fetched from GitHub releases.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Tool names to update. If omitted, all tools in the config are updated.
    tools: Vec<String>,

    /// Path to the tools config JSON (default: tools.json next to the binary).
    /// Can also be set via the CLIPDATE_CONFIG environment variable.
    #[arg(long, short, env = "CLIPDATE_CONFIG")]
    config: Option<PathBuf>,

    /// Directory where tools are installed (default: %ChocolateyInstall%\bin or ~/.local/bin).
    /// Can also be set via the CLIPDATE_BIN_DIR environment variable.
    #[arg(long, short, env = "CLIPDATE_BIN_DIR")]
    install_dir: Option<PathBuf>,

    /// GitHub Personal Access Token for higher API rate limits.
    /// Can also be set via the GITHUB_TOKEN environment variable.
    #[arg(long, short, env = "GITHUB_TOKEN")]
    token: Option<String>,

    /// Show what would be updated without downloading anything.
    #[arg(long, short = 'n')]
    dry_run: bool,

    /// Force update even if already up to date.
    #[arg(long, short = 'f')]
    force: bool,

    /// Update clipdate itself.
    #[arg(long)]
    self_update: bool,

    /// Use ASCII-only output (no Unicode symbols or Braille/block characters).
    /// Auto-enabled when the terminal does not support Unicode
    /// (e.g. legacy cmd.exe without chcp 65001, LANG=C containers, TERM=dumb).
    #[arg(long)]
    no_unicode: bool,

    /// Path to the settings TOML file (default: clipdate.toml next to the binary).
    /// Can also be set via the CLIPDATE_SETTINGS environment variable.
    #[arg(long, env = "CLIPDATE_SETTINGS")]
    settings: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(clap::Subcommand, Debug, Clone)]
enum Commands {
    /// Add a new tool to tools.json
    Add {
        /// GitHub repository in "owner/repo" format.
        repo: String,

        /// Optional name of the tool (defaults to the repository name)
        #[arg(long, short)]
        name: Option<String>,

        /// Optional executable name (defaults to "name")
        #[arg(long)]
        exe: Option<String>,

        /// Optional asset template (will be auto-detected if not provided)
        #[arg(long)]
        asset: Option<String>,

        /// Skip prompt confirmation
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

// ── Symbols ─────────────────────────────────────────────────────────────────

/// All terminal symbols used in output, pre-selected for Unicode or ASCII mode.
/// Construct once with [`Glyphs::new`] and pass around as a reference.
struct Glyphs {
    /// Success / up-to-date marker  (✓ / ok)
    ok: &'static str,
    /// Error marker                  (✗ / x)
    error: &'static str,
    /// "Updated from→to" prefix     (↑ / ^)
    updated: &'static str,
    /// Arrow between versions        (→ / ->)
    arrow: &'static str,
    /// Dry-run up-to-date marker     (· / .)
    dot: &'static str,
    /// Progress-bar spinner frames
    spinner: &'static [&'static str],
    /// Progress-bar fill characters
    progress_chars: &'static str,
}

impl Glyphs {
    fn new(no_unicode: bool) -> Self {
        if no_unicode {
            Self {
                ok: "ok",
                error: "x",
                updated: "^",
                arrow: "->",
                dot: ".",
                spinner: &["|", "/", "-", "\\", "|", "/", "-", "\\", " "],
                progress_chars: "=> ",
            }
        } else {
            Self {
                ok: "✓",
                error: "✗",
                updated: "↑",
                arrow: "→",
                dot: "·",
                spinner: &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"],
                progress_chars: "█▉▊▋▌▍▎▏ ",
            }
        }
    }
}

// ── Output helpers ───────────────────────────────────────────────────────────

/// Print the result of a single tool update and return `true` if it was an error.
/// Both `self_update` and the parallel tool loop use identical formatting,
/// so centralising it here eliminates the duplication.
fn print_update_result(
    label: impl std::fmt::Display,
    result: Result<UpdateResult>,
    g: &Glyphs,
) -> bool {
    match result {
        Ok(UpdateResult::UpToDate(v)) => {
            println!("{} {} up to date ({})", style(g.ok).green(), label, v);
            false
        }
        Ok(UpdateResult::Updated { from, to }) => {
            println!(
                "{} {} {} {} {}",
                style(g.updated).cyan(),
                label,
                style(from).dim(),
                g.arrow,
                style(to).green()
            );
            false
        }
        Ok(UpdateResult::Installed(v)) => {
            println!("{} {} installed ({})", style(g.ok).green(), label, v);
            false
        }
        Ok(UpdateResult::DryRun { current, latest }) => {
            let cur_str = current.as_deref().unwrap_or("not installed");
            if current.as_deref() == Some(&latest) {
                println!("{} {} up to date ({})", style(g.dot).dim(), label, latest);
            } else {
                println!(
                    "{} {} would update: {} {} {}",
                    style(g.arrow).yellow(),
                    label,
                    style(cur_str).dim(),
                    g.arrow,
                    style(&latest).green()
                );
            }
            false
        }
        Err(e) => {
            eprintln!("{} {}: {:#}", style(g.error).red(), label, e);
            true
        }
    }
}

// ── Main ─────────────────────────────────────────────────────────────────────

fn build_client(token: Option<&str>) -> Result<reqwest::blocking::Client> {
    use reqwest::header::{ACCEPT, AUTHORIZATION, HeaderMap, HeaderName, HeaderValue, USER_AGENT};

    let mut headers = HeaderMap::new();

    let repo = get_own_repo();
    let user_agent = format!(
        "clipdate/{} (https://github.com/{})",
        get_own_version(),
        repo
    );

    headers.insert(
        USER_AGENT,
        user_agent
            .parse()
            .unwrap_or(HeaderValue::from_static("clipdate")),
    );
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/vnd.github+json"),
    );
    // Pin the GitHub REST API version for a stable contract.
    // See https://docs.github.com/en/rest/about-the-rest-api/api-versions
    headers.insert(
        HeaderName::from_static("x-github-api-version"),
        HeaderValue::from_static("2022-11-28"),
    );
    if let Some(tok) = token {
        let auth = format!("Bearer {tok}");
        headers.insert(
            AUTHORIZATION,
            auth.parse().with_context(|| "invalid token header value")?,
        );
    }
    reqwest::blocking::Client::builder()
        .default_headers(headers)
        .timeout(Duration::from_secs(60))
        .build()
        .context("building HTTP client")
}

fn default_install_dir() -> PathBuf {
    if cfg!(windows) {
        if let Ok(choco) = std::env::var("ChocolateyInstall") {
            return PathBuf::from(choco).join("bin");
        }
        if let Ok(profile) = std::env::var("USERPROFILE") {
            return PathBuf::from(profile).join(".local").join("bin");
        }
    } else {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(".local").join("bin");
        }
    }
    PathBuf::from("bin")
}

/// Remove any leftover `<exe>.old` files in `install_dir` from a previous
/// interrupted update. Called once at startup.
fn clean_old_files(install_dir: &Path) {
    let Ok(entries) = fs::read_dir(install_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("old") {
            let _ = fs::remove_file(&path);
        }
    }
}

fn next_to_exe(file: &str) -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(file)))
}

fn default_config_path() -> PathBuf {
    next_to_exe("tools.json").unwrap_or_else(|| PathBuf::from("tools.json"))
}

// ── Settings file ────────────────────────────────────────────────────────────

/// Persistent settings loaded from `clipdate.toml`.
///
/// All fields are optional; absent fields leave the corresponding CLI default
/// untouched. CLI flags and environment variables always take priority.
#[derive(Debug, Default, Deserialize)]
struct AppConfig {
    /// Override the install directory (same as `--install-dir` / `CLIPDATE_BIN_DIR`).
    install_dir: Option<PathBuf>,

    /// GitHub PAT for higher API rate limits (same as `--token` / `GITHUB_TOKEN`).
    token: Option<String>,

    /// `true` = always use ASCII-only output (same as `--no-unicode`).
    /// `false` or absent = let the runtime Unicode-capability check decide.
    no_unicode: Option<bool>,

    /// Path to the tools JSON file (same as `--config` / `CLIPDATE_CONFIG`).
    tools_config: Option<PathBuf>,

    #[serde(default)]
    os_map: HashMap<String, Vec<String>>,

    #[serde(default)]
    arch_map: HashMap<String, Vec<String>>,
}

fn get_os_list(config: &AppConfig) -> Vec<String> {
    let os = std::env::consts::OS;
    if let Some(list) = config.os_map.get(os).filter(|l| !l.is_empty()) {
        return list.clone();
    }
    let defaults = match os {
        "macos" => &["darwin", "macos", "osx", "apple-darwin"][..],
        "windows" => &["windows", "win", "win64", "pc-windows-msvc"],
        "linux" => &["linux", "linux-gnu", "unknown-linux-gnu"],
        other => &[other],
    };
    defaults.iter().map(|&s| s.to_string()).collect()
}

fn get_arch_list(config: &AppConfig) -> Vec<String> {
    let arch = std::env::consts::ARCH;
    if let Some(list) = config.arch_map.get(arch).filter(|l| !l.is_empty()) {
        return list.clone();
    }
    let defaults = match arch {
        "x86_64" => &["amd64", "x86_64", "x64", "64bit"][..],
        "aarch64" => &["arm64", "aarch64"],
        "i386" => &["386", "i386", "x86", "32bit"],
        other => &[other],
    };
    defaults.iter().map(|&s| s.to_string()).collect()
}

/// Load `clipdate.toml` from `override_path` if given, otherwise look for one
/// next to the binary. Returns `AppConfig::default()` if no file is found.
/// Returns an error if a file is found but cannot be parsed.
fn load_app_config(override_path: Option<&Path>) -> Result<AppConfig> {
    let path: Option<PathBuf> = if let Some(p) = override_path {
        // Explicit --settings: the file must exist.
        if !p.exists() {
            bail!("settings file not found: {}", p.display());
        }
        Some(p.to_path_buf())
    } else {
        // Optional: next to the binary.
        next_to_exe("clipdate.toml").filter(|p| p.exists())
    };

    let Some(path) = path else {
        return Ok(AppConfig::default());
    };

    let text = fs::read_to_string(&path)
        .with_context(|| format!("reading settings: {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing settings: {}", path.display()))
}

/// Fill any `None`/`false` fields on `cli` from the settings file.
///
/// This is called once right after `Cli::parse()`. Because clap already folds
/// environment variables into `cli.*`, the effective priority is:
/// CLI flag > env var > clipdate.toml > built-in default.
fn apply_settings_to_cli(cli: &mut Cli, cfg: &AppConfig) -> Result<()> {
    if cli.install_dir.is_none() {
        cli.install_dir = cfg.install_dir.clone();
    }
    if cli.token.is_none() {
        cli.token = cfg.token.clone();
    }
    // `no_unicode` is a bool flag; the config can only force it on.
    // It cannot suppress the runtime supports_unicode check.
    if !cli.no_unicode {
        cli.no_unicode = cfg.no_unicode.unwrap_or(false);
    }
    if cli.config.is_none() {
        cli.config = cfg.tools_config.clone();
    }

    Ok(())
}

fn main() -> Result<()> {
    let mut cli = Cli::parse();
    let app_config = load_app_config(cli.settings.as_deref())?;
    apply_settings_to_cli(&mut cli, &app_config)?;

    // ── Subcommand Dispatch ──────────────────────────────────────────────────
    if let Some(ref command) = cli.command {
        match command {
            Commands::Add {
                repo,
                name,
                exe,
                asset,
                yes,
            } => {
                return handle_add_command(
                    &cli,
                    &app_config,
                    repo,
                    name.clone(),
                    exe.clone(),
                    asset.clone(),
                    *yes,
                );
            }
        }
    }

    // ── Load config ──────────────────────────────────────────────────────────
    let config_path = cli.config.clone().unwrap_or_else(default_config_path);
    let all_tools: Vec<ToolDef> = if cli.config.is_some() || config_path.exists() {
        let config_str = fs::read_to_string(&config_path)
            .with_context(|| format!("reading config: {}", config_path.display()))?;
        serde_json::from_str(&config_str).with_context(|| "parsing tools.json")?
    } else {
        Vec::new()
    };

    // ── Filter tools ─────────────────────────────────────────────────────────
    let tools: Vec<&ToolDef> = if cli.tools.is_empty() {
        all_tools.iter().collect()
    } else {
        let mut filtered = Vec::with_capacity(cli.tools.len());
        for name in &cli.tools {
            match all_tools.iter().find(|t| t.name.eq_ignore_ascii_case(name)) {
                Some(t) => filtered.push(t),
                None => {
                    let available = all_tools
                        .iter()
                        .map(|t| t.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    bail!("unknown tool '{}'. Available: {}", name, available);
                }
            }
        }
        filtered
    };

    if tools.is_empty() && !cli.self_update {
        println!("No tools configured. Use `clipdate add <repo>` to add a tool.");
        return Ok(());
    }

    let install_dir = cli.install_dir.unwrap_or_else(default_install_dir);
    fs::create_dir_all(&install_dir)
        .with_context(|| format!("creating install dir: {}", install_dir.display()))?;

    // Purge leftover .old files from any previous interrupted update.
    clean_old_files(&install_dir);

    let client = build_client(cli.token.as_deref())?;

    if cli.dry_run {
        println!(
            "{}",
            style("Dry-run mode — nothing will be downloaded.").yellow()
        );
    }

    let mp = MultiProgress::new();

    let glyphs = Glyphs::new(cli.no_unicode || !supports_unicode::on(Stream::Stdout));

    let ctx = UpdateCtx {
        install_dir: &install_dir,
        client: &client,
        mp: &mp,
        dry_run: cli.dry_run,
        force: cli.force,
        glyphs: &glyphs,
        app_config: &app_config,
    };

    if cli.self_update {
        let label = style("clipdate (self-update)").bold();
        let result = perform_self_update(&ctx);
        if print_update_result(label, result, &glyphs) {
            std::process::exit(1);
        }
        return Ok(());
    }

    // ── Process each tool (in parallel) ─────────────────────────────────────
    // Collect all results first so progress bars finish before we print summaries.
    let results: Vec<(&ToolDef, Result<UpdateResult>)> = tools
        .par_iter()
        .map(|tool| (*tool, process_tool(tool, &ctx)))
        .collect();

    let errors: usize = results
        .into_iter()
        .map(|(tool, result)| {
            print_update_result(style(&tool.name).bold(), result, &glyphs) as usize
        })
        .sum();

    if errors > 0 {
        bail!("{} tool(s) failed to update", errors);
    }
    Ok(())
}

fn detect_asset_template(
    assets: &[GhAsset],
    latest_str: &str,
    name: &str,
    os_list: &[String],
    arch_list: &[String],
) -> Option<(String, Option<String>)> {
    // First try to match both OS and arch; fall back to OS-only.
    let (asset_name, matched_os, matched_arch) = assets
        .iter()
        .find_map(|asset| {
            let lower = asset.name.to_lowercase();
            let os = os_list.iter().find(|os| lower.contains(&os.to_lowercase()))?;
            let arch = arch_list.iter().find(|a| lower.contains(&a.to_lowercase()))?;
            Some((&asset.name, Some(os.clone()), Some(arch.clone())))
        })
        .or_else(|| {
            assets.iter().find_map(|asset| {
                let lower = asset.name.to_lowercase();
                let os = os_list.iter().find(|os| lower.contains(&os.to_lowercase()))?;
                Some((&asset.name, Some(os.clone()), None::<String>))
            })
        })?;

    let mut template = asset_name.clone();

    let v_with_prefix = format!("v{}", latest_str);
    if template.contains(&v_with_prefix) {
        template = template.replace(&v_with_prefix, "v{VERSION}");
    } else if template.contains(latest_str) {
        template = template.replace(latest_str, "{VERSION}");
    }

    if let Some((os, idx)) = matched_os.as_ref().and_then(|os| {
        template.to_lowercase().find(&os.to_lowercase()).map(|i| (os, i))
    }) {
        template.replace_range(idx..idx + os.len(), "{OS}");
    }

    if let Some((arch, idx)) = matched_arch.as_ref().and_then(|arch| {
        template.to_lowercase().find(&arch.to_lowercase()).map(|i| (arch, i))
    }) {
        template.replace_range(idx..idx + arch.len(), "{ARCH}");
    }

    let mut is_archive = false;
    for ext in &[".zip", ".tar.gz", ".tgz"] {
        if template.to_lowercase().ends_with(ext) {
            is_archive = true;
            let idx = template.len() - ext.len();
            template.replace_range(idx.., ".{EXT}");
            break;
        }
    }

    let archive_entry = if is_archive {
        Some(name.to_string())
    } else {
        if template.to_lowercase().ends_with(".exe") {
            let idx = template.len() - 4;
            template.replace_range(idx.., "{EXE}");
        }
        None
    };

    Some((template, archive_entry))
}

fn handle_add_command(
    cli: &Cli,
    app_config: &AppConfig,
    repo: &str,
    name_opt: Option<String>,
    exe_opt: Option<String>,
    asset_opt: Option<String>,
    yes: bool,
) -> Result<()> {
    let name = name_opt.unwrap_or_else(|| {
        repo.split('/')
            .next_back()
            .unwrap_or(repo)
            .trim_start_matches("cli-")
            .trim_end_matches("-cli")
            .to_string()
    });

    println!("Adding tool '{}' from repo '{}'...", name, repo);

    let client = build_client(cli.token.as_deref())?;
    let (latest_str, assets) = fetch_latest_version(&client, repo)?;

    println!("Latest version found: {}", latest_str);

    let os_list = get_os_list(app_config);
    let arch_list = get_arch_list(app_config);

    let (asset_template, archive_entry_template) = if let Some(asset) = asset_opt {
        let is_archive = asset.contains("{EXT}");
        let entry = if is_archive { Some(name.clone()) } else { None };
        (asset, entry)
    } else {
        detect_asset_template(&assets, &latest_str, &name, &os_list, &arch_list)
            .ok_or_else(|| anyhow!("Could not auto-detect asset template from release assets. Please specify it manually with --asset."))?
    };

    let exe_name = exe_opt.unwrap_or_else(|| name.clone());

    let new_tool = ToolDef {
        name: name.clone(),
        exe_name,
        version_args: vec!["--version".to_string()],
        version_regex: r"(\d+\.\d+\.\d+)".to_string(),
        repo: repo.to_string(),
        asset_template,
        archive_entry_template,
    };

    println!("\nProposed tool definition:");
    println!("{}", serde_json::to_string_pretty(&new_tool)?);

    let config_path = cli.config.clone().unwrap_or_else(default_config_path);
    let mut all_tools: Vec<ToolDef> = if config_path.exists() {
        let config_str = fs::read_to_string(&config_path)?;
        serde_json::from_str(&config_str).context("parsing existing tools.json")?
    } else {
        Vec::new()
    };

    if all_tools
        .iter()
        .any(|t| t.name.eq_ignore_ascii_case(&new_tool.name))
    {
        bail!("Tool '{}' already exists in tools.json.", new_tool.name);
    }

    let mut proceed = yes;
    if !proceed && console::user_attended() {
        print!(
            "\nDo you want to add this tool to {}? [y/N]: ",
            config_path.display()
        );
        let _ = io::stdout().flush();
        let mut input = String::new();
        if io::stdin().read_line(&mut input).is_ok() {
            let input = input.trim().to_lowercase();
            if input == "y" || input == "yes" {
                proceed = true;
            }
        }
    } else if !proceed {
        proceed = true;
    }

    if proceed {
        all_tools.push(new_tool);
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json_str = serde_json::to_string_pretty(&all_tools)?;
        fs::write(&config_path, json_str)?;
        println!(
            "Successfully added '{}' to {}.",
            name,
            config_path.display()
        );
    } else {
        println!("Cancelled.");
    }

    Ok(())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_template() {
        let expanded = expand_template_with_os_arch(
            "test_{VERSION}_{OS}_{ARCH}_{EXE}_{EXT}",
            "1.2.3",
            "macos",
            "amd64",
        );
        assert!(expanded.contains("1.2.3"));
        assert!(expanded.contains("macos"));
        assert!(expanded.contains("amd64"));
        assert!(!expanded.contains("{VERSION}"));
        assert!(!expanded.contains("{OS}"));
        assert!(!expanded.contains("{ARCH}"));
        assert!(!expanded.contains("{EXE}"));
        assert!(!expanded.contains("{EXT}"));

        // If we are on windows, check specific values.
        if cfg!(windows) {
            assert!(expanded.contains("zip"));
            assert!(expanded.contains(".exe"));
        } else {
            assert!(expanded.contains("tar.gz"));
        }
    }

    #[test]
    fn test_parse_version() {
        let re = Regex::new(r"(\d+\.\d+\.\d+)").unwrap();

        // Simple case
        let v = parse_version("v1.2.3", &re).unwrap();
        assert_eq!(v, Version::parse("1.2.3").unwrap());

        // Complex CLI output
        let out = "cli version 10.5.1-beta (commit 12345)";
        let v = parse_version(out, &re).unwrap();
        assert_eq!(v, Version::parse("10.5.1").unwrap());

        // Error case
        assert!(parse_version("no version here", &re).is_err());
    }

    #[test]
    fn test_archive_entry_deserialization() {
        // Test that JSON successfully parses into ToolDef.
        let json = r#"{
            "name": "foo",
            "exe_name": "foo.exe",
            "version_args": ["-v"],
            "version_regex": "(.*)",
            "repo": "foo/bar",
            "asset_template": "foo.zip",
            "archive_entry_template": "bin/foo.exe"
        }"#;
        let tool: ToolDef = serde_json::from_str(json).unwrap();
        assert_eq!(tool.archive_entry_template.as_deref(), Some("bin/foo.exe"));

        let no_entry_json = r#"{
            "name": "foo",
            "exe_name": "foo.exe",
            "version_args": ["-v"],
            "version_regex": "(.*)",
            "repo": "foo/bar",
            "asset_template": "foo.zip"
        }"#;
        let tool2: ToolDef = serde_json::from_str(no_entry_json).unwrap();
        assert_eq!(tool2.archive_entry_template, None);
    }

    #[test]
    fn test_tools_json_is_valid() {
        let json = include_str!("../tools.json");
        let tools: Vec<ToolDef> = serde_json::from_str(json).unwrap();
        assert!(tools[0].archive_entry_template.is_some());
    }

    #[test]
    fn test_app_config_deserialisation() {
        let toml = r#"
            install_dir  = "/usr/local/bin"
            token        = "ghp_test"
            no_unicode   = true
            tools_config = "/etc/clipdate/tools.json"
        "#;
        let cfg: AppConfig = toml::from_str(toml).unwrap();
        assert_eq!(
            cfg.install_dir.as_deref(),
            Some(std::path::Path::new("/usr/local/bin"))
        );
        assert_eq!(cfg.token.as_deref(), Some("ghp_test"));
        assert_eq!(cfg.no_unicode, Some(true));
        assert_eq!(
            cfg.tools_config.as_deref(),
            Some(std::path::Path::new("/etc/clipdate/tools.json"))
        );
    }

    #[test]
    fn test_app_config_empty() {
        // Empty TOML should deserialise to all-None defaults.
        let cfg: AppConfig = toml::from_str("").unwrap();
        assert!(cfg.install_dir.is_none());
        assert!(cfg.token.is_none());
        assert!(cfg.no_unicode.is_none());
        assert!(cfg.tools_config.is_none());
    }

    #[test]
    fn test_apply_settings_cli_wins() {
        // CLI-set fields must not be overwritten by config values.
        let cfg = AppConfig {
            install_dir: Some(PathBuf::from("/from/config")),
            token: Some("config_token".to_string()),
            no_unicode: Some(true),
            tools_config: Some(PathBuf::from("/from/config/tools.json")),
            os_map: HashMap::new(),
            arch_map: HashMap::new(),
        };

        let mut cli = Cli {
            tools: vec![],
            config: Some(PathBuf::from("/from/cli/tools.json")),
            install_dir: Some(PathBuf::from("/from/cli")),
            token: Some("cli_token".to_string()),
            dry_run: false,
            force: false,
            self_update: false,
            no_unicode: false, // CLI left it false; config says true
            settings: None,
            command: None,
        };

        // Call apply_settings_to_cli directly.
        apply_settings_to_cli(&mut cli, &cfg).unwrap();

        // CLI values should be preserved.
        assert_eq!(
            cli.install_dir.as_deref(),
            Some(std::path::Path::new("/from/cli"))
        );
        assert_eq!(cli.token.as_deref(), Some("cli_token"));
        assert_eq!(
            cli.config.as_deref(),
            Some(std::path::Path::new("/from/cli/tools.json"))
        );
        // no_unicode was false on CLI, config had true → config wins here (expected).
        assert!(cli.no_unicode);
    }

    #[test]
    fn test_expand_exe_template() {
        let expanded = expand_exe_template("foo", "", "windows", "amd64");
        if cfg!(windows) {
            assert_eq!(expanded, "foo.exe");
        } else {
            assert_eq!(expanded, "foo");
        }

        let expanded_explicit = expand_exe_template("foo{EXE}", "", "windows", "amd64");
        if cfg!(windows) {
            assert_eq!(expanded_explicit, "foo.exe");
        } else {
            assert_eq!(expanded_explicit, "foo");
        }
    }

    #[test]
    fn test_detect_asset_template() {
        let assets = vec![
            GhAsset {
                name: "koyeb-cli_2.0.0_windows_amd64.zip".to_string(),
                browser_download_url: "url1".to_string(),
            },
            GhAsset {
                name: "koyeb-cli_2.0.0_darwin_amd64.tar.gz".to_string(),
                browser_download_url: "url2".to_string(),
            },
        ];
        let os_list = vec!["windows".to_string(), "win".to_string()];
        let arch_list = vec!["amd64".to_string(), "x86_64".to_string()];

        let (template, entry) =
            detect_asset_template(&assets, "2.0.0", "koyeb", &os_list, &arch_list).unwrap();
        assert_eq!(template, "koyeb-cli_{VERSION}_{OS}_{ARCH}.{EXT}");
        assert_eq!(entry, Some("koyeb".to_string()));

        let assets_direct = vec![GhAsset {
            name: "opera-proxy.windows-amd64.exe".to_string(),
            browser_download_url: "url3".to_string(),
        }];
        let (template_direct, entry_direct) =
            detect_asset_template(&assets_direct, "1.0.0", "opera-proxy", &os_list, &arch_list)
                .unwrap();
        assert_eq!(template_direct, "opera-proxy.{OS}-{ARCH}{EXE}");
        assert_eq!(entry_direct, None);
    }
}

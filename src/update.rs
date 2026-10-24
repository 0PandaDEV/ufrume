use console::{Term, style};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{debug, warn};

const REPO: &str = "0PandaDEV/ufrume";
const CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;
const CHECK_TIMEOUT: Duration = Duration::from_secs(3);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const OPT_OUT_ENV: &str = "UFRUME_NO_UPDATE_CHECK";
const CURRENT: &str = env!("CARGO_PKG_VERSION");

#[cfg(windows)]
const BIN_NAME: &str = "ufrume.exe";
#[cfg(not(windows))]
const BIN_NAME: &str = "ufrume";

#[derive(Debug, Default, Serialize, Deserialize)]
struct Cache {
    checked_at: u64,
    latest: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
}

pub struct Check {
    handle: Option<JoinHandle<Option<String>>>,
    notified: bool,
}

fn cache_path() -> PathBuf {
    let base = dirs::data_local_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("ufrume").join("update-check.toml")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn load_cache() -> Cache {
    fs::read_to_string(cache_path())
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_cache(cache: &Cache) {
    let path = cache_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(content) = toml::to_string(cache) {
        let _ = fs::write(path, content);
    }
}

fn parse_version(version: &str) -> Option<(u64, u64, u64)> {
    let core = version
        .trim()
        .trim_start_matches('v')
        .split(['-', '+'])
        .next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(c), Some(cur)) => c > cur,
        _ => false,
    }
}

fn build_client(timeout: Duration) -> Result<reqwest::blocking::Client, reqwest::Error> {
    reqwest::blocking::Client::builder()
        .user_agent(format!("ufrume/{} (https://github.com/{})", CURRENT, REPO))
        .timeout(timeout)
        .build()
}

fn fetch_latest(client: &reqwest::blocking::Client) -> Result<String, Box<dyn std::error::Error>> {
    let release: Release = client
        .get(format!(
            "https://api.github.com/repos/{}/releases/latest",
            REPO
        ))
        .header("Accept", "application/vnd.github+json")
        .send()?
        .error_for_status()?
        .json()?;
    Ok(release.tag_name)
}

fn is_package_managed(exe: &Path) -> bool {
    let path = exe.to_string_lossy();
    if path.contains("/Cellar/") || path.contains("/homebrew/") || path.contains("/linuxbrew/") {
        return true;
    }
    cfg!(unix) && path.starts_with("/usr/") && !path.starts_with("/usr/local/")
}

fn current_exe() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let exe = std::env::current_exe()?;
    Ok(fs::canonicalize(&exe).unwrap_or(exe))
}

fn print_notice(latest: &str) {
    let managed = current_exe()
        .map(|e| is_package_managed(&e))
        .unwrap_or(false);
    eprintln!(
        "{} {} -> {}",
        style("A new version of ufrume is available:")
            .yellow()
            .bold(),
        style(CURRENT).dim(),
        style(latest.trim_start_matches('v')).green().bold()
    );
    if managed {
        eprintln!("  Update it with your package manager.\n");
    } else {
        eprintln!("  Run {} to update.\n", style("ufrume update").cyan());
    }
}

pub fn start_check() -> Option<Check> {
    if std::env::var_os(OPT_OUT_ENV).is_some() || !Term::stderr().is_term() {
        return None;
    }

    let cache = load_cache();
    let mut notified = false;
    if let Some(ref latest) = cache.latest
        && is_newer(latest, CURRENT)
    {
        print_notice(latest);
        notified = true;
    }

    if now_secs().saturating_sub(cache.checked_at) < CHECK_INTERVAL_SECS {
        return Some(Check {
            handle: None,
            notified,
        });
    }

    let previous = cache.latest;
    let handle = std::thread::spawn(move || {
        let latest = build_client(CHECK_TIMEOUT)
            .map_err(Box::<dyn std::error::Error>::from)
            .and_then(|c| fetch_latest(&c));
        match latest {
            Ok(tag) => {
                save_cache(&Cache {
                    checked_at: now_secs(),
                    latest: Some(tag.clone()),
                });
                Some(tag)
            }
            Err(e) => {
                debug!("update check failed: {}", e);
                save_cache(&Cache {
                    checked_at: now_secs(),
                    latest: previous,
                });
                None
            }
        }
    });

    Some(Check {
        handle: Some(handle),
        notified,
    })
}

impl Check {
    pub fn finish(self) {
        let Some(handle) = self.handle else {
            return;
        };
        if let Ok(Some(latest)) = handle.join()
            && !self.notified
            && is_newer(&latest, CURRENT)
        {
            print_notice(&latest);
        }
    }
}

fn asset_name() -> Result<String, String> {
    let os = std::env::consts::OS;
    let arch = match (os, std::env::consts::ARCH) {
        ("windows", "x86_64") => "x64",
        (_, "x86_64") => "x86_64",
        (_, "aarch64") => "arm64",
        (_, other) => return Err(format!("unsupported architecture: {}", other)),
    };
    match os {
        "linux" | "macos" => Ok(format!("ufrume-{}-{}.tar.gz", os, arch)),
        "windows" => Ok(format!("ufrume-{}-{}.zip", os, arch)),
        other => Err(format!("unsupported operating system: {}", other)),
    }
}

#[cfg(not(windows))]
fn extract_binary(archive: &[u8]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let decoder = flate2::read::GzDecoder::new(archive);
    let mut tar = tar::Archive::new(decoder);
    let mut fallback: Option<Vec<u8>> = None;

    for entry in tar.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let is_bin = entry
            .path()?
            .file_name()
            .map(|n| n == BIN_NAME)
            .unwrap_or(false);
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf)?;
        if is_bin {
            return Ok(buf);
        }
        fallback.get_or_insert(buf);
    }

    fallback.ok_or_else(|| "archive does not contain a binary".into())
}

#[cfg(windows)]
fn extract_binary(archive: &[u8]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(archive))?;
    let mut fallback: Option<Vec<u8>> = None;

    for i in 0..zip.len() {
        let mut file = zip.by_index(i)?;
        if !file.is_file() {
            continue;
        }
        let name = file.name()?;
        let is_bin = Path::new(&*name)
            .file_name()
            .map(|n| n == BIN_NAME)
            .unwrap_or(false);
        drop(name);
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        if is_bin {
            return Ok(buf);
        }
        fallback.get_or_insert(buf);
    }

    fallback.ok_or_else(|| "archive does not contain a binary".into())
}

fn replace_self(binary: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let staged = std::env::temp_dir().join(format!("ufrume-update-{}", std::process::id()));
    fs::write(&staged, binary)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))?;
    }

    let result = self_replace::self_replace(&staged);
    let _ = fs::remove_file(&staged);
    result?;
    Ok(())
}

pub fn run_update() -> Result<(), Box<dyn std::error::Error>> {
    let exe = current_exe()?;
    if is_package_managed(&exe) {
        return Err(format!(
            "{} is managed by a package manager, update it there",
            exe.display()
        )
        .into());
    }

    let asset = asset_name()?;
    let client = build_client(DOWNLOAD_TIMEOUT)?;

    println!("  Checking for updates...");
    let tag = fetch_latest(&client)?;
    save_cache(&Cache {
        checked_at: now_secs(),
        latest: Some(tag.clone()),
    });

    if !is_newer(&tag, CURRENT) {
        println!("  ufrume {} is already up to date", style(CURRENT).green());
        return Ok(());
    }

    println!(
        "  Updating {} -> {}",
        style(CURRENT).dim(),
        style(tag.trim_start_matches('v')).green()
    );

    let url = format!(
        "https://github.com/{}/releases/download/{}/{}",
        REPO, tag, asset
    );
    let bytes = client.get(&url).send()?.error_for_status()?.bytes()?;
    let binary = extract_binary(&bytes)?;

    if let Err(e) = replace_self(&binary) {
        let permission_denied = e
            .downcast_ref::<std::io::Error>()
            .map(|io| io.kind() == std::io::ErrorKind::PermissionDenied)
            .unwrap_or(false);
        if permission_denied {
            warn!("permission denied replacing {}", exe.display());
            return Err(format!(
                "permission denied replacing {}, rerun with elevated privileges",
                exe.display()
            )
            .into());
        }
        return Err(e);
    }

    println!(
        "  Updated to {}",
        style(tag.trim_start_matches('v')).green()
    );
    Ok(())
}

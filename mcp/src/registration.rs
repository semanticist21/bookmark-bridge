use serde_json::json;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub const HOST_NAME: &str = "com.kobbokkom.bridgey";
const STORE_ID: &str = "ekokljgbbianihlpahackkfdendobngd";
const UNPACKED_ID_FILE: &str = "unpacked-extension-id";
const GIT_PATH_FILE: &str = "git-path";

fn absolute(path: PathBuf) -> Result<PathBuf, String> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Err("Bridgey needs an absolute user directory path.".into())
    }
}

fn env_path(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

pub fn root() -> Result<PathBuf, String> {
    #[cfg(target_os = "macos")]
    let path =
        env_path("HOME").map(|home| home.join("Library/Application Support/Bridgey/native-v2"));
    #[cfg(target_os = "linux")]
    let path = env_path("HOME").map(|home| home.join(".local/share/bridgey/native-v2"));
    #[cfg(windows)]
    let path = env_path("LOCALAPPDATA").map(|data| data.join("Bridgey/native-v2"));
    let path = absolute(path.ok_or("Bridgey could not find your user data directory.")?)?;
    // The Windows profile supplies the user ACL. On Unix the directory also
    // protects the endpoint credentials created by the MCP process.
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(&path)
        .map_err(|e| format!("Could not create Bridgey data directory: {e}"))?;
    let metadata = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
    if !metadata.is_dir() {
        return Err("Bridgey data directory is not a directory.".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o777 != 0o700 {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
                .map_err(|e| format!("Could not protect Bridgey data directory: {e}"))?;
        }
    }
    Ok(path)
}

fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    let mut suffix = [0; 8];
    getrandom::fill(&mut suffix).map_err(|e| e.to_string())?;
    let tmp = path.with_extension(format!("{:016x}.tmp", u64::from_ne_bytes(suffix)));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    let mut file = options
        .open(&tmp)
        .map_err(|e| format!("Could not stage {}: {e}", path.display()))?;
    let result = (|| -> io::Result<()> {
        file.write_all(bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(mode))?;
        }
        drop(file);
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.map_err(|e| format!("Could not install {}: {e}", path.display()))
}

pub fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    write_atomic(path, bytes, 0o600)
}

fn valid_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|c| (b'a'..=b'p').contains(&c))
}

fn unpacked_id(root: &Path) -> Result<Option<String>, String> {
    match fs::read_to_string(root.join(UNPACKED_ID_FILE)) {
        Ok(id) if valid_id(id.trim()) => Ok(Some(id.trim().to_owned())),
        Ok(_) => {
            Err("Bridgey's unpacked extension ID is invalid. Register the correct ID again.".into())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!(
            "Could not read Bridgey's unpacked extension ID: {e}"
        )),
    }
}

pub fn check_origin(arg: &str) -> bool {
    let Some(id) = arg.strip_prefix("chrome-extension://") else {
        return false;
    };
    let id = id.strip_suffix('/').unwrap_or(id);
    valid_id(id)
        && (id == STORE_ID
            || root()
                .ok()
                .and_then(|p| unpacked_id(&p).ok())
                .flatten()
                .as_deref()
                == Some(id))
}

fn executable(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    fs::metadata(path).is_ok_and(|metadata| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        metadata.is_file()
    })
}

fn find_git() -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|dir| dir.join(format!("git{}", env::consts::EXE_SUFFIX)))
        .find(|path| executable(path))
}

pub fn git_program() -> PathBuf {
    let stored = root()
        .ok()
        .and_then(|p| fs::read_to_string(p.join(GIT_PATH_FILE)).ok())
        .map(PathBuf::from)
        .filter(|p| executable(p));
    stored.or_else(find_git).unwrap_or_else(|| {
        #[cfg(windows)]
        // CreateProcess searches the working directory for an unqualified exe.
        return PathBuf::from(r"\\?\NUL\git.exe");
        #[cfg(not(windows))]
        PathBuf::from("git")
    })
}

#[cfg(not(windows))]
fn manifest_dir() -> Result<PathBuf, String> {
    let override_path = env_path("BOOKMARK_BRIDGE_CHROME_USER_DATA_DIR");
    #[cfg(target_os = "macos")]
    let default_path =
        || env_path("HOME").map(|home| home.join("Library/Application Support/Google/Chrome"));
    #[cfg(target_os = "linux")]
    let default_path = || {
        env_path("CHROME_USER_DATA_DIR").or_else(|| {
            env_path("CHROME_CONFIG_HOME")
                .or_else(|| env_path("XDG_CONFIG_HOME"))
                .or_else(|| env_path("HOME").map(|home| home.join(".config")))
                .map(|config| config.join("google-chrome"))
        })
    };
    Ok(absolute(
        override_path
            .or_else(default_path)
            .ok_or("Chrome's user data directory is unknown.")?,
    )?
    .join("NativeMessagingHosts"))
}

pub fn register_unpacked(id: &str) -> Result<(), String> {
    if !valid_id(id) {
        return Err("The extension ID must be 32 lowercase letters from a to p.".into());
    }
    write_private(&root()?.join(UNPACKED_ID_FILE), id.as_bytes())?;
    register()
}

pub fn register() -> Result<(), String> {
    if env::var_os("BOOKMARK_BRIDGE_SKIP_REGISTRATION").as_deref()
        == Some(std::ffi::OsStr::new("1"))
    {
        return Ok(());
    }
    let root = root()?;
    if let Some(git) = find_git().and_then(|p| p.to_str().map(str::to_owned)) {
        let path = root.join(GIT_PATH_FILE);
        if fs::read(&path).ok().as_deref() != Some(git.as_bytes()) {
            write_private(&path, git.as_bytes())?;
        }
    }
    let source =
        env::current_exe().map_err(|e| format!("Could not locate the Bridgey binary: {e}"))?;
    let target = root.join(format!(
        "bookmark-bridge-{}{}",
        env!("CARGO_PKG_VERSION"),
        env::consts::EXE_SUFFIX
    ));
    if source != target {
        let bytes =
            fs::read(&source).map_err(|e| format!("Could not read the Bridgey binary: {e}"))?;
        if fs::read(&target).ok().as_deref() != Some(bytes.as_slice()) {
            write_atomic(&target, &bytes, 0o700)?;
        }
    }

    #[cfg(windows)]
    let dir = &root;
    #[cfg(not(windows))]
    let dir = &manifest_dir()?;
    fs::create_dir_all(dir)
        .map_err(|e| format!("Could not create Chrome's host directory: {e}"))?;
    let path = dir.join(format!("{HOST_NAME}.json"));
    let mut origins = vec![format!("chrome-extension://{STORE_ID}/")];
    if let Some(id) = unpacked_id(&root)?.filter(|id| id != STORE_ID) {
        origins.push(format!("chrome-extension://{id}/"));
    }
    let manifest = json!({
        "name": HOST_NAME,
        "description": "Bridgey Bookmark MCP native host",
        "path": target.to_str().ok_or("Bridgey's host path is not valid UTF-8.")?,
        "type": "stdio",
        "allowed_origins": origins,
    });
    let contents = serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?;
    if fs::read(&path).ok().as_deref() != Some(contents.as_slice()) {
        write_private(&path, &contents)?;
    }

    #[cfg(windows)]
    {
        let key = format!(r"HKCU\Software\Google\Chrome\NativeMessagingHosts\{HOST_NAME}");
        let system = absolute(env_path("SystemRoot").ok_or("Windows SystemRoot is missing.")?)?;
        let output = std::process::Command::new(system.join("System32/reg.exe"))
            .args(["ADD", &key, "/ve", "/t", "REG_SZ", "/d"])
            .arg(&path)
            .args(["/f", "/reg:32"])
            .output()
            .map_err(|e| format!("Could not start Chrome host registration: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "Could not register the Chrome host: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_files_are_replaced_and_extension_ids_are_exact() {
        let dir = tempdir::TempDir::new("bridgey-registration").unwrap();
        let path = dir.path().join("endpoint.json");
        write_private(&path, b"first").unwrap();
        write_private(&path, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(check_origin(&format!("chrome-extension://{STORE_ID}/")));
        assert!(!check_origin(&format!("https://{STORE_ID}/")));
        assert!(!check_origin(&format!(
            "chrome-extension://{STORE_ID}/extra"
        )));
        assert!(!valid_id("pppppppppppppppppppppppppppppppq"));
    }
}

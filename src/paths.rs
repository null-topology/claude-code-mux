use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

/// The variables the directory resolvers read besides `HOME` and
/// `USERPROFILE`. They hold paths, which need not be valid Unicode, so they
/// are read with `var_os` and used as they are.
pub const PATH_ENV_VARS: [&str; 5] = [
    "CCP_CONFIG_DIR",
    "APPDATA",
    "XDG_CONFIG_HOME",
    "LOCALAPPDATA",
    "XDG_STATE_HOME",
];

#[derive(Debug, Clone)]
pub struct DirResolverEnv {
    /// A `std::env::consts::OS` value: `windows`, `macos`, `linux` and so on.
    pub platform: String,
    pub env: HashMap<String, OsString>,
    pub home: OsString,
}

impl DirResolverEnv {
    /// The resolver input for `platform`, with `var` answering for each
    /// variable as `std::env::var_os` does for the process environment.
    pub fn from_vars(platform: &str, var: impl Fn(&str) -> Option<OsString>) -> Self {
        Self {
            platform: platform.into(),
            env: PATH_ENV_VARS
                .iter()
                .filter_map(|&name| Some((name.to_string(), var(name)?)))
                .collect(),
            home: var("HOME")
                .or_else(|| var("USERPROFILE"))
                .unwrap_or_else(|| "/".into()),
        }
    }
}

impl Default for DirResolverEnv {
    fn default() -> Self {
        Self::from_vars(std::env::consts::OS, |name| std::env::var_os(name))
    }
}

pub fn resolve_config_dir(deps: &DirResolverEnv) -> PathBuf {
    if let Some(override_dir) = deps.env.get("CCP_CONFIG_DIR") {
        return PathBuf::from(override_dir);
    }

    if deps.platform == "windows" {
        return windows_dir(deps, "APPDATA", "Roaming");
    }

    // Every other platform, macOS included, follows the XDG rule.
    let base = deps
        .env
        .get("XDG_CONFIG_HOME")
        .cloned()
        .unwrap_or_else(|| join_with_sep(&deps.home, &[".config"], false).into_os_string());
    join_with_sep(&base, &["claude-code-proxy"], false)
}

pub fn resolve_state_dir(deps: &DirResolverEnv) -> PathBuf {
    if deps.platform == "windows" {
        return windows_dir(deps, "LOCALAPPDATA", "Local");
    }

    let base =
        deps.env.get("XDG_STATE_HOME").cloned().unwrap_or_else(|| {
            join_with_sep(&deps.home, &[".local", "state"], false).into_os_string()
        });
    join_with_sep(&base, &["claude-code-proxy"], false)
}

pub fn legacy_config_dir(deps: &DirResolverEnv) -> PathBuf {
    join_with_sep(&deps.home, &[".config", "claude-code-proxy"], false)
}

pub fn config_dir() -> PathBuf {
    resolve_config_dir(&DirResolverEnv::default())
}

pub fn state_dir() -> PathBuf {
    resolve_state_dir(&DirResolverEnv::default())
}

pub fn codex_auth_file(deps: &DirResolverEnv) -> PathBuf {
    resolve_config_dir(deps).join("codex").join("auth.json")
}

pub fn kimi_auth_file(deps: &DirResolverEnv) -> PathBuf {
    resolve_config_dir(deps).join("kimi").join("auth.json")
}

pub fn cursor_auth_file(deps: &DirResolverEnv) -> PathBuf {
    resolve_config_dir(deps).join("cursor").join("auth.json")
}

pub fn kimi_device_id_file(deps: &DirResolverEnv) -> PathBuf {
    resolve_config_dir(deps).join("kimi").join("device_id")
}

pub fn log_file() -> PathBuf {
    resolve_state_dir(&DirResolverEnv::default()).join("proxy.log")
}

pub fn provider_auth_file(provider: &str) -> PathBuf {
    let deps = DirResolverEnv::default();
    resolve_config_dir(&deps).join(provider).join("auth.json")
}

pub fn provider_legacy_auth_file(provider: &str) -> PathBuf {
    let deps = DirResolverEnv::default();
    legacy_config_dir(&deps).join(provider).join("auth.json")
}

/// `<name>\claude-code-proxy`, else `<home>\AppData\<fallback>\claude-code-proxy`.
fn windows_dir(deps: &DirResolverEnv, name: &str, fallback: &str) -> PathBuf {
    // An empty value counts as unset: used as given it is a relative path,
    // which would put the directory under the working directory.
    if let Some(base) = deps.env.get(name).filter(|base| !base.is_empty()) {
        return join_with_sep(base, &["claude-code-proxy"], true);
    }
    // A home that is empty or made only of separators, such as the `/` that
    // stands in for an unknown one, roots the directory at the current drive;
    // an empty one joined as is would make it relative.
    let home = if deps
        .home
        .as_encoded_bytes()
        .iter()
        .all(|&b| is_sep(b, true))
    {
        OsStr::new("\\")
    } else {
        deps.home.as_os_str()
    };
    join_with_sep(home, &["AppData", fallback, "claude-code-proxy"], true)
}

/// `/`, and on Windows `\` too.
fn is_sep(byte: u8, windows: bool) -> bool {
    byte == b'/' || (windows && byte == b'\\')
}

/// Joins `parts` onto `base`, adding no separator after one that is already
/// there. On Windows the added separator is `\`, as a verbatim (`\\?\`) path
/// does not read `/` as one.
fn join_with_sep(base: &OsStr, parts: &[&str], windows: bool) -> PathBuf {
    let sep = if windows { "\\" } else { "/" };
    let mut out = OsString::new();
    for part in std::iter::once(base).chain(parts.iter().map(OsStr::new)) {
        let ends_with_sep = out
            .as_encoded_bytes()
            .last()
            .is_some_and(|&b| is_sep(b, windows));
        if !out.is_empty() && !ends_with_sep {
            out.push(sep);
        }
        out.push(part);
    }
    PathBuf::from(out)
}

fn os_env(env: &HashMap<String, String>) -> HashMap<String, OsString> {
    env.iter()
        .map(|(name, value)| (name.clone(), OsString::from(value)))
        .collect()
}

pub fn resolve_config_dir_for_env(
    platform: &str,
    home: &str,
    env: &HashMap<String, String>,
) -> PathBuf {
    resolve_config_dir(&DirResolverEnv {
        platform: platform.to_string(),
        env: os_env(env),
        home: home.into(),
    })
}

pub fn resolve_state_dir_for_env(
    platform: &str,
    home: &str,
    env: &HashMap<String, String>,
) -> PathBuf {
    resolve_state_dir(&DirResolverEnv {
        platform: platform.to_string(),
        env: os_env(env),
        home: home.into(),
    })
}

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
    pub platform: String,
    pub env: HashMap<String, OsString>,
    pub home: OsString,
}

impl Default for DirResolverEnv {
    fn default() -> Self {
        Self {
            platform: std::env::consts::OS.into(),
            env: PATH_ENV_VARS
                .iter()
                .filter_map(|&name| Some((name.to_string(), std::env::var_os(name)?)))
                .collect(),
            home: std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .unwrap_or_else(|| "/".into()),
        }
    }
}

pub fn resolve_config_dir(deps: &DirResolverEnv) -> PathBuf {
    if let Some(override_dir) = deps.env.get("CCP_CONFIG_DIR") {
        return PathBuf::from(override_dir);
    }

    if deps.platform == "win32" {
        let appdata = deps.env.get("APPDATA").cloned().unwrap_or_else(|| {
            let mut appdata = deps.home.clone();
            appdata.push("\\AppData\\Roaming");
            appdata
        });
        return join_with_sep(&appdata, &["claude-code-proxy"], true);
    }

    if deps.platform == "darwin" {
        return join_with_sep(&deps.home, &[".config", "claude-code-proxy"], false);
    }

    let base = deps
        .env
        .get("XDG_CONFIG_HOME")
        .cloned()
        .unwrap_or_else(|| join_with_sep(&deps.home, &[".config"], false).into_os_string());
    join_with_sep(&base, &["claude-code-proxy"], false)
}

pub fn resolve_state_dir(deps: &DirResolverEnv) -> PathBuf {
    if deps.platform == "win32" {
        let local = deps.env.get("LOCALAPPDATA").cloned().unwrap_or_else(|| {
            let mut local = deps.home.clone();
            local.push("\\AppData\\Local");
            local
        });
        return join_with_sep(&local, &["claude-code-proxy"], true);
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

fn join_with_sep(base: &OsStr, parts: &[&str], win32: bool) -> PathBuf {
    let sep = "/";
    let _ = win32;
    let mut out = OsString::new();
    for part in std::iter::once(base).chain(parts.iter().map(OsStr::new)) {
        if !out.is_empty() && !out.as_encoded_bytes().ends_with(sep.as_bytes()) {
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

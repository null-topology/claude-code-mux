use assert_cmd::Command;
use predicates::str::contains;
use std::env;
use std::path::PathBuf;
use tempfile::TempDir;

#[test]
fn version_aliases_print_expected_version() -> Result<(), Box<dyn std::error::Error>> {
    let expected = format!("claude-code-mux {}", env!("CARGO_PKG_VERSION"));

    for arg in ["--version", "-v", "version"] {
        let mut cmd = Command::cargo_bin("claude-code-mux")?;
        cmd.arg(arg)
            .assert()
            .success()
            .stdout(contains(expected.clone()));
    }
    Ok(())
}

/// `models` asks each backend that holds a login, so tests point the codex
/// provider at a missing credential file: no network, and the line reports
/// why nothing was listed.
fn no_codex_auth(cmd: &mut Command, temp: &TempDir) {
    cmd.env("CCP_CODEX_AUTH_FILE", temp.path().join("missing-auth.json"));
    cmd.env("CCP_CONFIG_DIR", temp.path());
}

#[test]
fn models_prints_all_providers() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let mut cmd = Command::cargo_bin("claude-code-mux")?;
    no_codex_auth(&mut cmd, &temp);
    cmd.arg("models");
    let out = String::from_utf8(cmd.output()?.stdout)?;
    assert!(out.contains("codex: unavailable (unauthorized:"), "{out}");
    assert!(out.contains("kimi:"), "{out}");
    assert!(out.contains("[bundled list, not verified]"), "{out}");
    assert!(out.contains("cursor:"), "{out}");
    assert!(out.contains("anthropic:"), "{out}");

    let mut cmd = Command::cargo_bin("claude-code-mux")?;
    no_codex_auth(&mut cmd, &temp);
    cmd.args(["models", "--full"]);
    cmd.output()?;
    Ok(())
}

#[test]
fn help_describes_visible_commands_and_hides_demo() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("claude-code-mux")?;
    cmd.arg("--help");
    let output = cmd.output()?;
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout)?;
    for description in [
        "Print version information",
        "Start the proxy server and monitor",
        "List supported provider models",
        "Manage Codex authentication",
        "Manage Kimi authentication",
        "Manage Cursor authentication",
        "Manage Grok authentication",
    ] {
        assert!(stdout.contains(description), "missing: {description}");
    }
    assert!(!stdout.contains("demo"));
    assert!(!stdout.contains("mock data and no proxy server"));
    Ok(())
}

#[test]
fn invalid_command_exits_two() -> Result<(), Box<dyn std::error::Error>> {
    Command::cargo_bin("claude-code-mux")?
        .arg("definitely-not-a-command")
        .assert()
        .failure()
        .code(2);
    Ok(())
}

#[test]
fn unsupported_provider_auth_command_exits_two() -> Result<(), Box<dyn std::error::Error>> {
    let mut cmd = Command::cargo_bin("claude-code-mux")?;
    cmd.args(["cursor", "auth", "device"]);
    let output = cmd.output()?;
    assert_eq!(output.status.code(), Some(2));
    let out = String::from_utf8(output.stderr)?;
    assert!(out.contains("not yet implemented") || out.contains("unsupported"));
    Ok(())
}

/// Logout also deletes the legacy `$HOME/.config` copy, and Cursor falls back to
/// the macOS Keychain when `CCP_CONFIG_DIR` is unset, so both must point at temp.
fn isolated_auth_env(temp: &TempDir) -> Vec<(&'static str, PathBuf)> {
    let home = temp.path().join("home");
    vec![
        ("HOME", home.clone()),
        ("USERPROFILE", home.clone()),
        ("CCP_CONFIG_DIR", temp.path().join("config")),
        ("XDG_CONFIG_HOME", home.join(".config")),
        ("XDG_DATA_HOME", home.join(".local").join("share")),
        ("XDG_STATE_HOME", home.join(".local").join("state")),
        (
            "CCP_CODEX_AUTH_FILE",
            temp.path().join("missing-codex-auth.json"),
        ),
    ]
}

fn isolate_auth(cmd: &mut Command, env: &[(&'static str, PathBuf)]) {
    for (key, value) in env {
        cmd.env(key, value);
    }
    for key in [
        "CCP_CURSOR_AUTH_TOKEN",
        "CURSOR_AUTH_TOKEN",
        "OPENAI_API_KEY",
    ] {
        cmd.env_remove(key);
    }
}

#[test]
fn provider_logout_without_auth_is_success() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let env = isolated_auth_env(&temp);
    for (key, value) in &env {
        assert!(
            value.starts_with(temp.path()),
            "{key} must stay inside the temp tree"
        );
    }

    let mut cmd = Command::cargo_bin("claude-code-mux")?;
    cmd.args(["kimi", "auth", "logout"]);
    isolate_auth(&mut cmd, &env);
    cmd.assert().success();
    Ok(())
}

#[test]
fn cursor_logout_runs_inside_an_isolated_home() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let env = isolated_auth_env(&temp);
    let config_dir = env
        .iter()
        .find(|(key, _)| *key == "CCP_CONFIG_DIR")
        .map(|(_, value)| value.clone())
        .expect("CCP_CONFIG_DIR keeps cursor logout off the macOS Keychain");
    assert!(config_dir.starts_with(temp.path()));

    let mut cmd = Command::cargo_bin("claude-code-mux")?;
    cmd.args(["cursor", "auth", "logout"]);
    isolate_auth(&mut cmd, &env);
    cmd.assert().success();
    Ok(())
}

#[test]
fn models_output_is_stable_order() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let mut cmd = Command::cargo_bin("claude-code-mux")?;
    no_codex_auth(&mut cmd, &temp);
    cmd.args(["models", "--full"]);
    let output = cmd.output()?;
    let out = String::from_utf8(output.stdout)?;
    let codex_pos = out.find("codex:").unwrap_or(0);
    let kimi_pos = out.find("kimi:").unwrap_or(0);
    let cursor_pos = out.find("cursor:").unwrap_or(0);
    assert!(codex_pos < kimi_pos);
    assert!(kimi_pos < cursor_pos);
    Ok(())
}

#[test]
fn kimi_auth_status_reads_stored_auth() -> Result<(), Box<dyn std::error::Error>> {
    let temp = TempDir::new()?;
    let auth_dir = temp.path().join("kimi");
    std::fs::create_dir_all(&auth_dir)?;
    std::fs::write(
        auth_dir.join("auth.json"),
        r#"{"access":"a","refresh":"r","expires":4102444800000,"scope":"openid","userId":"u"}"#,
    )?;
    let mut cmd = Command::cargo_bin("claude-code-mux")?;
    cmd.args(["kimi", "auth", "status"]);
    cmd.env("CCP_CONFIG_DIR", temp.path());
    cmd.assert().success().stdout(contains("User: u"));
    Ok(())
}

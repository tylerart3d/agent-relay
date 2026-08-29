use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Copy)]
pub enum CliHarness {
    OpenCode,
    Hermes,
    Codex,
    ClaudeCode,
    Pi,
    Copilot,
}

impl CliHarness {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "opencode" => Ok(Self::OpenCode),
            "hermes" => Ok(Self::Hermes),
            "codex" => Ok(Self::Codex),
            "claude_code" => Ok(Self::ClaudeCode),
            "pi" => Ok(Self::Pi),
            "copilot" => Ok(Self::Copilot),
            _ => Err(format!("unknown CLI harness '{value}'")),
        }
    }

    pub(crate) fn command(self) -> &'static str {
        match self {
            Self::OpenCode => "opencode",
            Self::Hermes => "hermes",
            Self::Codex => "codex",
            Self::ClaudeCode => "claude",
            Self::Pi => "pi",
            Self::Copilot => "copilot",
        }
    }

    #[cfg(windows)]
    fn title(self) -> &'static str {
        match self {
            Self::OpenCode => "OpenCode · Agent Relay",
            Self::Hermes => "Hermes CLI · Agent Relay",
            Self::Codex => "Codex · Agent Relay",
            Self::ClaudeCode => "Claude Code · Agent Relay",
            Self::Pi => "Pi · Agent Relay",
            Self::Copilot => "Copilot CLI · Agent Relay",
        }
    }
}

pub fn launch(harness: CliHarness) -> Result<(), String> {
    launch_with_env(harness, &[])
}

pub fn launch_with_env(
    harness: CliHarness,
    environment: &[(String, String)],
) -> Result<(), String> {
    let executable = resolve_executable(harness)?;
    launch_resolved_with_env(harness, &executable, environment)
}

pub fn launch_resolved(harness: CliHarness, executable: &Path) -> Result<(), String> {
    launch_resolved_with_env(harness, executable, &[])
}

pub fn launch_resolved_with_env(
    harness: CliHarness,
    executable: &Path,
    environment: &[(String, String)],
) -> Result<(), String> {
    if !is_executable_file(executable) {
        return Err(format!(
            "cannot launch {} because {} is not an executable file",
            harness.command(),
            executable.display()
        ));
    }
    let home = user_home()?;
    launch_platform_terminal(harness, executable, &home, environment)
}

fn user_home() -> Result<PathBuf, String> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| "cannot locate the user home directory".to_owned())
}

pub(crate) fn resolve_executable(harness: CliHarness) -> Result<PathBuf, String> {
    let home = user_home()?;
    for candidate in platform_candidates(harness, &home) {
        if is_executable_file(&candidate) {
            return Ok(candidate);
        }
    }
    executable_from_path(harness.command()).ok_or_else(|| {
        format!(
            "{} is not installed or is not available on PATH",
            harness.command()
        )
    })
}

pub(crate) fn is_installed(harness: CliHarness) -> bool {
    resolve_executable(harness).is_ok()
}

pub(crate) fn vscode_is_installed() -> bool {
    let home = match user_home() {
        Ok(home) => home,
        Err(_) => return false,
    };
    #[cfg(windows)]
    let _ = &home;
    #[cfg(target_os = "macos")]
    let candidates = vec![
        PathBuf::from("/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code"),
        home.join("Applications")
            .join("Visual Studio Code.app")
            .join("Contents/Resources/app/bin/code"),
    ];
    #[cfg(windows)]
    let candidates = {
        let mut candidates = Vec::new();
        if let Some(local_app_data) = env::var_os("LOCALAPPDATA") {
            candidates.push(
                PathBuf::from(local_app_data).join("Programs/Microsoft VS Code/bin/code.cmd"),
            );
        }
        candidates
    };
    #[cfg(not(any(target_os = "macos", windows)))]
    let candidates: Vec<PathBuf> = Vec::new();
    candidates.iter().any(|path| is_executable_file(path)) || executable_from_path("code").is_some()
}

fn platform_candidates(harness: CliHarness, home: &Path) -> Vec<PathBuf> {
    let command = harness.command();
    let candidates = vec![
        home.join(".local").join("bin").join(command),
        home.join(".cargo").join("bin").join(command),
        home.join(".bun").join("bin").join(command),
        home.join(".npm-global").join("bin").join(command),
    ];

    #[cfg(windows)]
    let candidates = {
        let mut candidates = candidates;
        if matches!(harness, CliHarness::Hermes) {
            if let Some(local_app_data) = env::var_os("LOCALAPPDATA") {
                candidates.insert(
                    0,
                    PathBuf::from(local_app_data)
                        .join("hermes")
                        .join("hermes-agent")
                        .join("venv")
                        .join("Scripts")
                        .join("hermes.exe"),
                );
            }
        }
        if let Some(app_data) = env::var_os("APPDATA") {
            let npm = PathBuf::from(app_data).join("npm");
            candidates.push(npm.join(format!("{command}.cmd")));
            candidates.push(npm.join(format!("{command}.exe")));
        }
        candidates.extend(
            candidates
                .clone()
                .into_iter()
                .filter(|path| path.extension().is_none())
                .map(|path| path.with_extension("exe")),
        );
        candidates
    };

    candidates
}

#[cfg(windows)]
fn executable_from_path(command: &str) -> Option<PathBuf> {
    let mut lookup = Command::new("where.exe");
    lookup.arg(command).creation_flags(CREATE_NO_WINDOW);
    first_existing_path(lookup.output().ok()?.stdout)
}

#[cfg(target_os = "macos")]
fn executable_from_path(command: &str) -> Option<PathBuf> {
    let output = Command::new("/bin/zsh")
        .args(["-lic", &format!("whence -p -- {command}")])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| first_existing_path(output.stdout))?
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn executable_from_path(command: &str) -> Option<PathBuf> {
    let output = Command::new("/bin/sh")
        .args(["-lc", &format!("command -v -- {command}")])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| first_existing_path(output.stdout))?
}

fn first_existing_path(output: Vec<u8>) -> Option<PathBuf> {
    String::from_utf8_lossy(&output)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .find(|path| is_executable_file(path))
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(windows)]
    {
        // Windows has no executable permission bit. PowerShell can invoke
        // native executables and command shims such as npm's `.cmd` files.
        true
    }
}

#[cfg(windows)]
fn launch_platform_terminal(
    harness: CliHarness,
    executable: &Path,
    home: &Path,
    environment: &[(String, String)],
) -> Result<(), String> {
    let shell_command = powershell_command(executable, environment);
    let mut windows_terminal = Command::new("wt.exe");
    windows_terminal
        .args([
            "-w",
            "new",
            "new-tab",
            "--title",
            harness.title(),
            "--startingDirectory",
        ])
        .arg(home)
        .args(["powershell.exe", "-NoExit", "-Command", &shell_command]);
    if windows_terminal.spawn().is_ok() {
        return Ok(());
    }

    let mut powershell = Command::new("powershell.exe");
    powershell
        .current_dir(home)
        .args(["-NoExit", "-Command", &shell_command])
        .creation_flags(CREATE_NEW_CONSOLE);
    powershell.spawn().map(|_| ()).map_err(|error| {
        format!(
            "failed to open a terminal for {}: {error}",
            harness.command()
        )
    })
}

#[cfg(windows)]
fn powershell_command(executable: &Path, environment: &[(String, String)]) -> String {
    let mut parts = environment
        .iter()
        .map(|(key, value)| format!("$env:{key}={}", powershell_literal(value)))
        .collect::<Vec<_>>();
    parts.push(format!(
        "& {}",
        powershell_literal(executable.to_string_lossy().as_ref())
    ));
    parts.join("; ")
}

#[cfg(windows)]
fn powershell_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(target_os = "macos")]
fn launch_platform_terminal(
    _harness: CliHarness,
    executable: &Path,
    home: &Path,
    environment: &[(String, String)],
) -> Result<(), String> {
    let mut parts = environment
        .iter()
        .map(|(key, value)| format!("export {key}={}", shell_quote(value)))
        .collect::<Vec<_>>();
    parts.push(format!(
        "cd {}",
        shell_quote(home.to_string_lossy().as_ref())
    ));
    parts.push(shell_quote(executable.to_string_lossy().as_ref()));
    let shell_command = parts.join(" && ");
    let output = Command::new("osascript")
        .args([
            "-e",
            "on run argv",
            "-e",
            "tell application \"Terminal\" to do script (item 1 of argv)",
            "-e",
            "tell application \"Terminal\" to activate",
            "-e",
            "end run",
            "--",
            &shell_command,
        ])
        .output()
        .map_err(|error| format!("failed to open Terminal: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "failed to open Terminal: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

#[cfg(all(not(windows), not(target_os = "macos")))]
fn launch_platform_terminal(
    _harness: CliHarness,
    executable: &Path,
    home: &Path,
    environment: &[(String, String)],
) -> Result<(), String> {
    for terminal_command in ["x-terminal-emulator", "gnome-terminal", "konsole"] {
        let mut command = Command::new(terminal_command);
        command
            .current_dir(home)
            .envs(environment.iter().map(|(key, value)| (key, value)))
            .arg("-e")
            .arg(executable);
        if command.spawn().is_ok() {
            return Ok(());
        }
    }
    Err("no supported terminal application was found".to_owned())
}

#[cfg(target_os = "macos")]
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_supported_cli_harnesses() {
        assert_eq!(CliHarness::parse("codex").unwrap().command(), "codex");
        assert_eq!(CliHarness::parse("hermes").unwrap().command(), "hermes");
        assert_eq!(
            CliHarness::parse("claude_code").unwrap().command(),
            "claude"
        );
        assert!(CliHarness::parse("vscode").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn injects_environment_into_the_launched_powershell_session() {
        let command = powershell_command(
            Path::new("C:\\Tools\\copilot.cmd"),
            &[("COPILOT_MODEL".to_owned(), "m1-pro/it's-model".to_owned())],
        );
        assert_eq!(
            command,
            "$env:COPILOT_MODEL='m1-pro/it''s-model'; & 'C:\\Tools\\copilot.cmd'"
        );
    }

    #[test]
    fn path_lookup_ignores_nonexistent_results() {
        let existing = std::env::current_exe().expect("current executable");
        let output = format!(
            "{}\n{}\n",
            existing.with_extension("missing").display(),
            existing.display()
        );
        assert_eq!(first_existing_path(output.into_bytes()), Some(existing));
    }

    #[cfg(unix)]
    #[test]
    fn unix_preflight_rejects_files_without_an_execute_bit() {
        use std::{fs, os::unix::fs::PermissionsExt};

        let path = std::env::temp_dir().join(format!(
            "agent-relay-terminal-preflight-{}",
            std::process::id()
        ));
        fs::write(&path, "#!/bin/sh\n").expect("write test executable");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644))
            .expect("set non-executable permissions");
        assert!(!is_executable_file(&path));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
            .expect("set executable permissions");
        assert!(is_executable_file(&path));
        fs::remove_file(path).expect("remove test executable");
    }

    #[cfg(windows)]
    #[test]
    fn windows_preflight_accepts_command_shims() {
        let path = std::env::temp_dir().join(format!(
            "agent-relay-terminal-preflight-{}.cmd",
            std::process::id()
        ));
        std::fs::write(&path, "@echo off\r\n").expect("write command shim");
        assert!(is_executable_file(&path));
        std::fs::remove_file(path).expect("remove command shim");
    }
}

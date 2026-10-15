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
    fn arguments(self) -> &'static [&'static str] {
        match self {
            Self::Codex => &["-c", "model=agentrelay", "-c", "model_provider=agentrelay"],
            _ => &[],
        }
    }

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

pub fn launch_project(
    harness: CliHarness,
    directory: Option<&str>,
    environment: &[(String, String)],
) -> Result<(), String> {
    launch_in_directory(
        harness,
        &resolve_executable(harness)?,
        environment,
        directory,
    )
}

pub fn launch_in_directory(
    harness: CliHarness,
    executable: &Path,
    environment: &[(String, String)],
    directory: Option<&str>,
) -> Result<(), String> {
    if !is_executable_file(executable) {
        return Err(format!(
            "cannot launch {} because {} is not an executable file",
            harness.command(),
            executable.display()
        ));
    }
    let home = working_directory(directory)?;
    launch_platform_terminal(harness, executable, &home, environment, harness.arguments())
}

pub fn launch_codex(directory: Option<&str>, context: u32, catalog: &Path) -> Result<(), String> {
    let executable = resolve_executable(CliHarness::Codex)?;
    let home = working_directory(directory)?;
    let context_arg = format!("model_context_window={context}");
    let compact_arg = format!(
        "model_auto_compact_token_limit={}",
        u64::from(context) * 4 / 5
    );
    let mut arguments = CliHarness::Codex.arguments().to_vec();
    let catalog_arg = format!(
        "model_catalog_json={}",
        serde_json::to_string(&catalog.to_string_lossy()).map_err(|error| error.to_string())?
    );
    arguments.extend(["-c", &context_arg, "-c", &compact_arg, "-c", &catalog_arg]);
    launch_platform_terminal(CliHarness::Codex, &executable, &home, &[], &arguments)
}

pub fn working_directory(directory: Option<&str>) -> Result<PathBuf, String> {
    let path = match directory {
        Some(path) => PathBuf::from(path),
        None => user_home()?,
    };
    if !path.is_absolute() || !path.is_dir() {
        return Err(
            "Project folder is missing or unavailable. Choose another folder before launching."
                .into(),
        );
    }
    Ok(path)
}

pub fn pick_project_folder() -> Result<Option<String>, String> {
    #[cfg(windows)]
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-STA", "-Command", "Add-Type -AssemblyName System.Windows.Forms; $picker = New-Object System.Windows.Forms.FolderBrowserDialog; $picker.Description = 'Choose a project folder'; if ($picker.ShowDialog() -eq 'OK') { [Console]::OutputEncoding = [System.Text.Encoding]::UTF8; [Console]::Write($picker.SelectedPath) }; $picker.Dispose()"])
        .creation_flags(CREATE_NO_WINDOW).output();
    #[cfg(target_os = "macos")]
    let output = Command::new("osascript")
        .args([
            "-e",
            "try",
            "-e",
            "POSIX path of (choose folder with prompt \"Choose a project folder\")",
            "-e",
            "on error number -128",
            "-e",
            "return \"\"",
            "-e",
            "end try",
        ])
        .output();
    #[cfg(not(any(windows, target_os = "macos")))]
    return Err("Folder picker is supported on Windows and macOS.".into());
    #[cfg(any(windows, target_os = "macos"))]
    {
        let output = output.map_err(|error| format!("Unable to open folder picker: {error}"))?;
        if !output.status.success() {
            return Err("Unable to open folder picker.".into());
        }
        let path = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if path.is_empty() {
            return Ok(None);
        }
        working_directory(Some(&path))?;
        Ok(Some(path))
    }
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
    arguments: &[&str],
) -> Result<(), String> {
    let shell_command = powershell_command(executable, environment, arguments);
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
fn powershell_command(
    executable: &Path,
    environment: &[(String, String)],
    arguments: &[&str],
) -> String {
    let mut parts = environment
        .iter()
        .map(|(key, value)| format!("$env:{key}={}", powershell_literal(value)))
        .collect::<Vec<_>>();
    parts.push(format!(
        "& {}{}",
        powershell_literal(executable.to_string_lossy().as_ref()),
        arguments
            .iter()
            .map(|arg| format!(" {}", powershell_literal(arg)))
            .collect::<String>()
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
    arguments: &[&str],
) -> Result<(), String> {
    let mut parts = environment
        .iter()
        .map(|(key, value)| format!("export {key}={}", shell_quote(value)))
        .collect::<Vec<_>>();
    parts.push(format!(
        "cd {}",
        shell_quote(home.to_string_lossy().as_ref())
    ));
    parts.push(format!(
        "{}{}",
        shell_quote(executable.to_string_lossy().as_ref()),
        arguments
            .iter()
            .map(|arg| format!(" {}", shell_quote(arg)))
            .collect::<String>()
    ));
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
    arguments: &[&str],
) -> Result<(), String> {
    for terminal_command in ["x-terminal-emulator", "gnome-terminal", "konsole"] {
        let mut command = Command::new(terminal_command);
        command
            .current_dir(home)
            .envs(environment.iter().map(|(key, value)| (key, value)))
            .arg("-e")
            .arg(executable)
            .args(arguments);
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
    fn project_directory_must_exist_and_be_absolute() {
        assert!(working_directory(Some("relative-project")).is_err());
        let directory = std::env::temp_dir();
        assert_eq!(working_directory(directory.to_str()).unwrap(), directory);
        assert!(working_directory(directory.join("missing-agentrelay-project").to_str()).is_err());
        assert!(working_directory(std::env::current_exe().unwrap().to_str()).is_err());
    }

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
            &[],
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

    #[test]
    fn only_codex_launches_with_the_relay_overrides() {
        assert_eq!(
            CliHarness::Codex.arguments(),
            &["-c", "model=agentrelay", "-c", "model_provider=agentrelay"]
        );
        for harness in [
            CliHarness::OpenCode,
            CliHarness::Hermes,
            CliHarness::Pi,
            CliHarness::ClaudeCode,
            CliHarness::Copilot,
        ] {
            assert!(harness.arguments().is_empty());
        }
    }

    #[cfg(windows)]
    #[test]
    fn codex_launch_quotes_the_executable_and_profile_arguments() {
        assert_eq!(
            powershell_command(
                Path::new("C:\\My Tools\\codex.cmd"),
                &[],
                CliHarness::Codex.arguments()
            ),
            "& 'C:\\My Tools\\codex.cmd' '-c' 'model=agentrelay' '-c' 'model_provider=agentrelay'"
        );
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

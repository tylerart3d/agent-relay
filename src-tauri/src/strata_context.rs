//! Prepare a context override without editing the operator's Strata config.
use std::{
    fs,
    path::{Path, PathBuf},
};

use serde_json::Value;

pub(crate) struct ContextFile {
    pub path: PathBuf,
    pub contents: String,
}

pub(crate) fn prepare(
    profile_path: &Path,
    model_id: &str,
    lines: &mut [String],
    context: u32,
) -> Result<ContextFile, String> {
    let directory = profile_path
        .parent()
        .ok_or("profile has no parent directory")?
        .join("runtime-context");
    let managed_path = directory.join(format!("{}.json", urlencoding::encode(model_id)));
    // Only inspect cmd, never comments, cmdStop, or metadata descriptions.
    let start = lines
        .iter()
        .position(|line| line.starts_with("    cmd:"))
        .ok_or_else(|| format!("Strata profile {model_id} has no cmd"))?;
    let end = lines
        .iter()
        .enumerate()
        .skip(start + 1)
        .find_map(|(i, line)| {
            (!line.trim().is_empty() && line.len() - line.trim_start().len() <= 4).then_some(i)
        })
        .unwrap_or(lines.len());
    let mut config_arg = None;
    for (i, line) in lines.iter().enumerate().take(end).skip(start) {
        if line.trim_start().starts_with('#') {
            continue;
        }
        if let Some((from, to)) = config_argument(line)? {
            if config_arg.is_some() {
                return Err("Strata cmd has multiple --config arguments".into());
            }
            config_arg = Some((i, from, to));
        }
    }
    let (line, from, to) = config_arg.ok_or_else(|| {
        format!("Strata profile {model_id} needs an absolute --config JSON path to change context")
    })?;
    let configured = PathBuf::from(&lines[line][from..to]);
    if !configured.is_absolute() {
        return Err("Strata context changes require an absolute --config path".into());
    }
    let mut source = configured.clone();
    let mut config = read_config(&configured)?;
    if configured == managed_path {
        source = config
            .get("_agentrelay_context_source")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute() && path != &managed_path)
            .ok_or("Managed Strata context config is missing its original source path")?;
        config = read_config(&source)?;
    }
    let args = config
        .get_mut("args")
        .and_then(Value::as_array_mut)
        .ok_or("Strata config needs an args array containing --max-context")?;
    let positions: Vec<_> = args
        .iter()
        .enumerate()
        .filter_map(|(i, arg)| (arg.as_str() == Some("--max-context")).then_some(i))
        .collect();
    if positions.len() != 1 {
        return Err("Strata config must contain exactly one --max-context argument".into());
    }
    let value = args
        .get_mut(positions[0] + 1)
        .ok_or("Strata --max-context is missing its value")?;
    if value.as_str().and_then(|v| v.parse::<u32>().ok()).is_none() {
        return Err("Strata --max-context must be followed by a numeric string".into());
    }
    *value = Value::String(context.to_string());
    config
        .as_object_mut()
        .ok_or("Strata config must be a JSON object")?
        .insert(
            "_agentrelay_context_source".into(),
            Value::String(source.to_string_lossy().into_owned()),
        );
    let contents = format!(
        "{}\n",
        serde_json::to_string_pretty(&config).map_err(|error| error.to_string())?
    );
    // Keep the original quoting; quote an unquoted path because the app-data
    // directory may contain spaces. Forward slashes also work on Windows.
    let replacement = managed_path.to_string_lossy().replace('\\', "/");
    let quoted = from > 0 && matches!(lines[line].as_bytes()[from - 1], b'\'' | b'"');
    let replacement = if quoted {
        replacement
    } else {
        format!("\"{replacement}\"")
    };
    lines[line].replace_range(from..to, &replacement);
    Ok(ContextFile {
        path: managed_path,
        contents,
    })
}

fn read_config(path: &Path) -> Result<Value, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("Cannot read Strata config {}: {error}", path.display()))?;
    serde_json::from_str(text.trim_start_matches('\u{feff}'))
        .map_err(|error| format!("Invalid Strata config {}: {error}", path.display()))
}

fn config_argument(line: &str) -> Result<Option<(usize, usize)>, String> {
    let bytes = line.as_bytes();
    let mut found = None;
    for (start, _) in line.match_indices("--config") {
        let end = start + "--config".len();
        if start > 0 && !bytes[start - 1].is_ascii_whitespace() {
            continue;
        }
        if !bytes
            .get(end)
            .is_some_and(|c| c.is_ascii_whitespace() || *c == b'=')
        {
            continue;
        }
        if found.is_some() {
            return Err("Strata cmd has multiple --config arguments".into());
        }
        let mut from = end;
        while bytes
            .get(from)
            .is_some_and(|c| c.is_ascii_whitespace() || *c == b'=')
        {
            from += 1;
        }
        let quote = bytes
            .get(from)
            .copied()
            .filter(|c| matches!(c, b'\'' | b'"'));
        if quote.is_some() {
            from += 1;
        }
        let mut to = from;
        while bytes.get(to).is_some_and(|c| match quote {
            Some(q) => *c != q,
            None => !c.is_ascii_whitespace(),
        }) {
            to += 1;
        }
        if to == from || (quote.is_some() && bytes.get(to).is_none()) {
            return Err("Strata --config has an empty or unclosed path".into());
        }
        found = Some((from, to));
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_quoted_windows_and_posix_config_paths_without_shell_expansion() {
        for (line, expected) in [
            (
                r#"python server.py --config "C:\Models\voice config.json" --port 1234"#,
                r#"C:\Models\voice config.json"#,
            ),
            (
                "python server.py --config='/Users/test/model config.json'",
                "/Users/test/model config.json",
            ),
            (
                "python server.py --config=/tmp/strata.json --port 1234",
                "/tmp/strata.json",
            ),
        ] {
            let (start, end) = config_argument(line).unwrap().unwrap();
            assert_eq!(&line[start..end], expected);
        }
        assert_eq!(
            config_argument("python --mcp-config mcp.json").unwrap(),
            None
        );
        assert!(config_argument("python --config \"unfinished").is_err());
        assert!(config_argument("python --config one --config two").is_err());
    }
}

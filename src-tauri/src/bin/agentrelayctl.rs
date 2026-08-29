use std::{env, io::Read, process::ExitCode, time::Duration};

use reqwest::{Client, Method, StatusCode};
use serde_json::{json, Map, Value};

const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:38475";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, PartialEq)]
struct Cli {
    endpoint: String,
    pretty: bool,
    command: Command,
}

#[derive(Debug, PartialEq)]
enum Command {
    Status,
    Health,
    Models {
        host: Option<String>,
        running: bool,
    },
    Load {
        host: String,
        model: String,
        force: bool,
    },
    Unload {
        host: String,
        force: bool,
    },
    UnloadAll {
        force: bool,
    },
    Chat {
        model: String,
        prompt: PromptSource,
        max_tokens: u32,
    },
    ChannelRoutes,
    ProcessChannel {
        channel: String,
        account: String,
        conversation: String,
        sender: String,
        text: String,
    },
    Version,
    Help,
}

#[derive(Debug, PartialEq)]
enum PromptSource {
    Argument(String),
    Stdin,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = match parse_args(env::args().skip(1).collect()) {
        Ok(cli) => cli,
        Err(error) => {
            print_error("usage", &error, false);
            eprintln!("\n{}", usage());
            return ExitCode::from(2);
        }
    };

    if cli.command == Command::Help {
        println!("{}", usage());
        return ExitCode::SUCCESS;
    }
    if cli.command == Command::Version {
        print_json(
            &json!({"ok": true, "version": env!("CARGO_PKG_VERSION")}),
            cli.pretty,
        );
        return ExitCode::SUCCESS;
    }

    let client = match Client::builder().timeout(REQUEST_TIMEOUT).build() {
        Ok(client) => client,
        Err(error) => {
            print_error("client", &error.to_string(), cli.pretty);
            return ExitCode::FAILURE;
        }
    };
    match execute(&client, &cli).await {
        Ok((value, code)) => {
            print_json(&value, cli.pretty);
            ExitCode::from(code)
        }
        Err(error) => {
            print_error("connection", &error, cli.pretty);
            ExitCode::from(3)
        }
    }
}

async fn execute(client: &Client, cli: &Cli) -> Result<(Value, u8), String> {
    match &cli.command {
        Command::Status => {
            let (status, body) = request_json(
                client,
                Method::GET,
                &endpoint(&cli.endpoint, "/api/v1/status"),
                None,
            )
            .await?;
            Ok(response_envelope(status, "status", body))
        }
        Command::Health => health(client, &cli.endpoint).await,
        Command::Models { host, running } => {
            models(client, &cli.endpoint, host.as_deref(), *running).await
        }
        Command::Load { host, model, force } => {
            control(
                client,
                &cli.endpoint,
                "load",
                json!({"host_id": host, "model_id": model, "force": force}),
            )
            .await
        }
        Command::Unload { host, force } => {
            control(
                client,
                &cli.endpoint,
                "unload",
                json!({"host_id": host, "force": force}),
            )
            .await
        }
        Command::UnloadAll { force } => unload_all(client, &cli.endpoint, *force).await,
        Command::Chat {
            model,
            prompt,
            max_tokens,
        } => chat(client, &cli.endpoint, model, prompt, *max_tokens).await,
        Command::ChannelRoutes => {
            let (status, body) = request_json(
                client,
                Method::GET,
                &endpoint(&cli.endpoint, "/api/v1/channels/routes"),
                None,
            )
            .await?;
            Ok(response_envelope(status, "channels", body))
        }
        Command::ProcessChannel {
            channel,
            account,
            conversation,
            sender,
            text,
        } => {
            let (status, body) = request_json(
                client,
                Method::POST,
                &endpoint(&cli.endpoint, "/api/v1/channels/command"),
                Some(json!({
                    "channel": channel,
                    "account_id": account,
                    "conversation_id": conversation,
                    "sender_id": sender,
                    "text": text,
                })),
            )
            .await?;
            Ok(response_envelope(status, "channel", body))
        }
        Command::Version | Command::Help => unreachable!(),
    }
}

async fn health(client: &Client, base: &str) -> Result<(Value, u8), String> {
    let (status, body) =
        request_json(client, Method::GET, &endpoint(base, "/api/v1/status"), None).await?;
    if !status.is_success() {
        return Ok(response_envelope(status, "status", body));
    }
    let hosts = body
        .get("hosts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let online_hosts = hosts
        .iter()
        .filter(|host| host.get("connection").and_then(Value::as_str) != Some("offline"))
        .count();
    Ok((
        json!({
            "ok": true,
            "endpoint": base,
            "local_host_id": body.get("local_host_id"),
            "online_hosts": online_hosts,
            "host_count": hosts.len(),
            "refreshed_at_ms": body.get("refreshed_at_ms")
        }),
        0,
    ))
}

async fn models(
    client: &Client,
    base: &str,
    host_filter: Option<&str>,
    running_only: bool,
) -> Result<(Value, u8), String> {
    let (status, body) =
        request_json(client, Method::GET, &endpoint(base, "/api/v1/status"), None).await?;
    if !status.is_success() {
        return Ok(response_envelope(status, "status", body));
    }

    let mut output = Vec::new();
    for host in body
        .get("hosts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let host_id = host.get("id").and_then(Value::as_str).unwrap_or_default();
        if host_filter.is_some_and(|filter| filter != host_id) {
            continue;
        }
        let loaded = host.get("loaded_model_id").and_then(Value::as_str);
        for profile in host
            .get("models")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let profile_id = profile
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let is_loaded = loaded == Some(profile_id);
            if running_only && !is_loaded {
                continue;
            }
            let mut entry = profile.as_object().cloned().unwrap_or_else(Map::new);
            entry.insert("host_id".into(), Value::String(host_id.to_owned()));
            entry.insert(
                "host_display_name".into(),
                host.get("display_name").cloned().unwrap_or(Value::Null),
            );
            entry.insert(
                "connection".into(),
                host.get("connection").cloned().unwrap_or(Value::Null),
            );
            entry.insert("loaded".into(), Value::Bool(is_loaded));
            entry.insert(
                "active_requests".into(),
                host.get("active_requests").cloned().unwrap_or(json!(0)),
            );
            entry.insert(
                "tokens_per_second".into(),
                host.get("tokens_per_second")
                    .cloned()
                    .unwrap_or(Value::Null),
            );
            output.push(Value::Object(entry));
        }
    }

    if host_filter.is_some() && output.is_empty() {
        let known_host = body
            .get("hosts")
            .and_then(Value::as_array)
            .is_some_and(|hosts| {
                hosts
                    .iter()
                    .any(|host| host.get("id").and_then(Value::as_str) == host_filter)
            });
        if !known_host {
            return Ok((
                json!({"ok": false, "error": {"kind": "unknown_host", "message": format!("unknown fleet host: {}", host_filter.unwrap_or_default())}}),
                5,
            ));
        }
    }

    Ok((json!({"ok": true, "models": output}), 0))
}

async fn control(
    client: &Client,
    base: &str,
    action: &str,
    payload: Value,
) -> Result<(Value, u8), String> {
    let (status, body) = request_json(
        client,
        Method::POST,
        &endpoint(base, &format!("/api/v1/control/{action}")),
        Some(payload),
    )
    .await?;
    Ok(response_envelope(status, "result", body))
}

async fn unload_all(client: &Client, base: &str, force: bool) -> Result<(Value, u8), String> {
    let (status, snapshot) =
        request_json(client, Method::GET, &endpoint(base, "/api/v1/status"), None).await?;
    if !status.is_success() {
        return Ok(response_envelope(status, "status", snapshot));
    }

    let host_ids: Vec<String> = snapshot
        .get("hosts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|host| host.get("connection").and_then(Value::as_str) != Some("offline"))
        .filter(|host| {
            host.get("loaded_model_id")
                .is_some_and(|model| !model.is_null())
        })
        .filter_map(|host| host.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();

    let mut results = Vec::new();
    let mut failed = false;
    for host_id in host_ids {
        let (status, body) = request_json(
            client,
            Method::POST,
            &endpoint(base, "/api/v1/control/unload"),
            Some(json!({"host_id": host_id, "force": force})),
        )
        .await?;
        failed |= !status.is_success();
        results.push(json!({
            "host_id": host_id,
            "status": status.as_u16(),
            "ok": status.is_success(),
            "result": body
        }));
    }

    Ok((json!({"ok": !failed, "results": results}), u8::from(failed)))
}

async fn chat(
    client: &Client,
    base: &str,
    model: &str,
    prompt: &PromptSource,
    max_tokens: u32,
) -> Result<(Value, u8), String> {
    let prompt = match prompt {
        PromptSource::Argument(prompt) => prompt.clone(),
        PromptSource::Stdin => {
            let mut prompt = String::new();
            std::io::stdin()
                .read_to_string(&mut prompt)
                .map_err(|error| format!("failed to read prompt from stdin: {error}"))?;
            if prompt.trim().is_empty() {
                return Ok((
                    json!({"ok": false, "error": {"kind": "empty_prompt", "message": "stdin contained no prompt"}}),
                    2,
                ));
            }
            prompt
        }
    };
    let payload = json!({
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": max_tokens,
        "stream": false
    });
    let (status, body) = request_json(
        client,
        Method::POST,
        &endpoint(base, "/v1/chat/completions"),
        Some(payload),
    )
    .await?;
    Ok(response_envelope(status, "response", body))
}

async fn request_json(
    client: &Client,
    method: Method,
    url: &str,
    body: Option<Value>,
) -> Result<(StatusCode, Value), String> {
    let mut request = client.request(method, url);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request
        .send()
        .await
        .map_err(|error| format!("failed to contact Agent Relay at {url}: {error}"))?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| format!("failed to read Agent Relay response: {error}"))?;
    let body = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| json!({"error": String::from_utf8_lossy(&bytes).trim().to_owned()}));
    Ok((status, body))
}

fn response_envelope(status: StatusCode, key: &str, body: Value) -> (Value, u8) {
    if status.is_success() {
        let mut response = Map::new();
        response.insert("ok".into(), Value::Bool(true));
        response.insert(key.into(), body);
        (Value::Object(response), 0)
    } else {
        let message = body
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or_else(|| {
                status
                    .canonical_reason()
                    .unwrap_or("Agent Relay request failed")
            });
        (
            json!({
                "ok": false,
                "status": status.as_u16(),
                "error": {"kind": error_kind(status), "message": message},
                "response": body
            }),
            exit_code(status),
        )
    }
}

fn error_kind(status: StatusCode) -> &'static str {
    match status {
        StatusCode::CONFLICT => "conflict",
        StatusCode::NOT_FOUND => "not_found",
        StatusCode::SERVICE_UNAVAILABLE => "unavailable",
        _ if status.is_client_error() => "invalid_request",
        _ => "server_error",
    }
}

fn exit_code(status: StatusCode) -> u8 {
    match status {
        StatusCode::CONFLICT => 4,
        StatusCode::NOT_FOUND => 5,
        StatusCode::SERVICE_UNAVAILABLE => 6,
        _ => 1,
    }
}

fn endpoint(base: &str, path: &str) -> String {
    format!("{}{}", base.trim_end_matches('/'), path)
}

fn print_json(value: &Value, pretty: bool) {
    let output = if pretty {
        serde_json::to_string_pretty(value)
    } else {
        serde_json::to_string(value)
    }
    .expect("JSON output must serialize");
    println!("{output}");
}

fn print_error(kind: &str, message: &str, pretty: bool) {
    let value = json!({"ok": false, "error": {"kind": kind, "message": message}});
    let output = if pretty {
        serde_json::to_string_pretty(&value)
    } else {
        serde_json::to_string(&value)
    }
    .expect("JSON error must serialize");
    eprintln!("{output}");
}

fn parse_args(args: Vec<String>) -> Result<Cli, String> {
    let mut endpoint = env::var("AGENTRELAY_ENDPOINT").unwrap_or_else(|_| DEFAULT_ENDPOINT.into());
    let mut pretty = false;
    let mut position = 0;
    while position < args.len() {
        match args[position].as_str() {
            "--endpoint" => {
                position += 1;
                endpoint = args
                    .get(position)
                    .cloned()
                    .ok_or_else(|| "--endpoint requires a URL".to_owned())?;
                position += 1;
            }
            "--pretty" => {
                pretty = true;
                position += 1;
            }
            _ => break,
        }
    }

    let command_name = args.get(position).map(String::as_str).unwrap_or("help");
    position += usize::from(position < args.len());
    let command_args = &args[position..];
    let command = match command_name {
        "status" => no_args(command_args, Command::Status)?,
        "health" => no_args(command_args, Command::Health)?,
        "models" => parse_models(command_args)?,
        "load" => parse_load(command_args)?,
        "unload" => parse_unload(command_args)?,
        "unload-all" => Command::UnloadAll {
            force: parse_force_only(command_args)?,
        },
        "chat" => parse_chat(command_args)?,
        "channel-routes" => no_args(command_args, Command::ChannelRoutes)?,
        "channel-command" => parse_channel_command(command_args)?,
        "version" | "--version" | "-V" => no_args(command_args, Command::Version)?,
        "help" | "--help" | "-h" => Command::Help,
        other => return Err(format!("unknown command: {other}")),
    };
    Ok(Cli {
        endpoint,
        pretty,
        command,
    })
}

fn no_args(args: &[String], command: Command) -> Result<Command, String> {
    if args.is_empty() {
        Ok(command)
    } else {
        Err(format!("unexpected argument: {}", args[0]))
    }
}

fn parse_models(args: &[String]) -> Result<Command, String> {
    let mut host = None;
    let mut running = false;
    let mut position = 0;
    while position < args.len() {
        match args[position].as_str() {
            "--host" => {
                position += 1;
                host = Some(
                    args.get(position)
                        .cloned()
                        .ok_or_else(|| "--host requires a host ID".to_owned())?,
                );
            }
            "--running" => running = true,
            other => return Err(format!("unexpected models argument: {other}")),
        }
        position += 1;
    }
    Ok(Command::Models { host, running })
}

fn parse_load(args: &[String]) -> Result<Command, String> {
    let first = args
        .first()
        .ok_or_else(|| "load requires <host>/<model> or <host> <model>".to_owned())?;
    let (host, model, mut position) = if let Some((host, model)) = split_target(first) {
        (host, model, 1)
    } else {
        let model = args
            .get(1)
            .cloned()
            .ok_or_else(|| "load requires a model ID".to_owned())?;
        (first.clone(), model, 2)
    };
    let mut force = false;
    while position < args.len() {
        match args[position].as_str() {
            "--force" => force = true,
            other => return Err(format!("unexpected load argument: {other}")),
        }
        position += 1;
    }
    Ok(Command::Load { host, model, force })
}

fn parse_unload(args: &[String]) -> Result<Command, String> {
    let host = args
        .first()
        .cloned()
        .ok_or_else(|| "unload requires a host ID".to_owned())?;
    let force = parse_force_only(&args[1..])?;
    Ok(Command::Unload { host, force })
}

fn parse_force_only(args: &[String]) -> Result<bool, String> {
    let mut force = false;
    for argument in args {
        if argument == "--force" && !force {
            force = true;
        } else {
            return Err(format!("unexpected argument: {argument}"));
        }
    }
    Ok(force)
}

fn parse_chat(args: &[String]) -> Result<Command, String> {
    let model = args
        .first()
        .cloned()
        .ok_or_else(|| "chat requires <host>/<model>".to_owned())?;
    if split_target(&model).is_none() {
        return Err("chat model must use <host>/<model>".into());
    }
    let mut prompt = None;
    let mut max_tokens = 512;
    let mut position = 1;
    while position < args.len() {
        match args[position].as_str() {
            "--prompt" => {
                position += 1;
                prompt = Some(PromptSource::Argument(
                    args.get(position)
                        .cloned()
                        .ok_or_else(|| "--prompt requires text".to_owned())?,
                ));
            }
            "--stdin" => prompt = Some(PromptSource::Stdin),
            "--max-tokens" => {
                position += 1;
                max_tokens = args
                    .get(position)
                    .ok_or_else(|| "--max-tokens requires a value".to_owned())?
                    .parse::<u32>()
                    .map_err(|_| "--max-tokens must be a positive integer".to_owned())?;
                if max_tokens == 0 {
                    return Err("--max-tokens must be greater than zero".into());
                }
            }
            other => return Err(format!("unexpected chat argument: {other}")),
        }
        position += 1;
    }
    let prompt = prompt.ok_or_else(|| "chat requires --prompt <text> or --stdin".to_owned())?;
    Ok(Command::Chat {
        model,
        prompt,
        max_tokens,
    })
}

fn parse_channel_command(args: &[String]) -> Result<Command, String> {
    let channel = args
        .first()
        .cloned()
        .ok_or_else(|| "channel-command requires <channel> <conversation>".to_owned())?;
    let conversation = args
        .get(1)
        .cloned()
        .ok_or_else(|| "channel-command requires a conversation ID".to_owned())?;
    let mut account = "default".to_owned();
    let mut sender = String::new();
    let mut text = None;
    let mut position = 2;
    while position < args.len() {
        match args[position].as_str() {
            "--account" => {
                position += 1;
                account = args
                    .get(position)
                    .cloned()
                    .ok_or_else(|| "--account requires an ID".to_owned())?;
            }
            "--sender" => {
                position += 1;
                sender = args
                    .get(position)
                    .cloned()
                    .ok_or_else(|| "--sender requires an ID".to_owned())?;
            }
            "--text" => {
                position += 1;
                text = Some(
                    args.get(position)
                        .cloned()
                        .ok_or_else(|| "--text requires an Agent Relay command".to_owned())?,
                );
            }
            other => return Err(format!("unexpected channel-command argument: {other}")),
        }
        position += 1;
    }
    Ok(Command::ProcessChannel {
        channel,
        account,
        conversation,
        sender,
        text: text.ok_or_else(|| "channel-command requires --text <command>".to_owned())?,
    })
}

fn split_target(target: &str) -> Option<(String, String)> {
    target
        .split_once('/')
        .filter(|(host, model)| !host.is_empty() && !model.is_empty())
        .map(|(host, model)| (host.to_owned(), model.to_owned()))
}

fn usage() -> &'static str {
    r#"agentrelayctl [--endpoint URL] [--pretty] <command>

Commands:
  health                                     Check the local control API
  status                                     Return the complete fleet snapshot
  models [--host ID] [--running]             List model profiles
  load <host>/<model> [--force]              Load a model on one host
  unload <host> [--force]                    Unload the model on one host
  unload-all [--force]                       Unload every loaded host
  chat <host>/<model> --prompt TEXT           Send a chat request
       [--max-tokens N]
  chat <host>/<model> --stdin                 Read a prompt from stdin
       [--max-tokens N]
  channel-routes                             List sticky messaging routes
  channel-command <channel> <conversation>   Process an /ar command as a channel
       --text TEXT [--account ID] [--sender ID]
  version                                    Print the CLI version

Output is compact JSON by default. Use --pretty for indented JSON.
Exit codes: 0 success, 1 server error, 2 usage, 3 connection, 4 conflict,
5 not found, 6 unavailable. AGENTRELAY_ENDPOINT overrides the default endpoint."#
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn parses_qualified_load_with_force() {
        let cli = parse_args(strings(&[
            "--endpoint",
            "http://localhost:9000",
            "--pretty",
            "load",
            "workstation/qwen",
            "--force",
        ]))
        .expect("parse load");

        assert_eq!(cli.endpoint, "http://localhost:9000");
        assert!(cli.pretty);
        assert_eq!(
            cli.command,
            Command::Load {
                host: "workstation".into(),
                model: "qwen".into(),
                force: true
            }
        );
    }

    #[test]
    fn parses_separate_load_target() {
        let cli =
            parse_args(strings(&["load", "m1-pro", "ornith"])).expect("parse separate target");
        assert_eq!(
            cli.command,
            Command::Load {
                host: "m1-pro".into(),
                model: "ornith".into(),
                force: false
            }
        );
    }

    #[test]
    fn parses_filtered_model_list() {
        let cli = parse_args(strings(&["models", "--host", "air-m4", "--running"]))
            .expect("parse models");
        assert_eq!(
            cli.command,
            Command::Models {
                host: Some("air-m4".into()),
                running: true
            }
        );
    }

    #[test]
    fn chat_requires_an_explicit_prompt_source() {
        let error = parse_args(strings(&["chat", "workstation/qwen"]))
            .expect_err("missing prompt must fail");
        assert!(error.contains("--prompt"));
    }

    #[test]
    fn maps_control_statuses_to_stable_exit_codes() {
        assert_eq!(exit_code(StatusCode::CONFLICT), 4);
        assert_eq!(exit_code(StatusCode::NOT_FOUND), 5);
        assert_eq!(exit_code(StatusCode::SERVICE_UNAVAILABLE), 6);
        assert_eq!(exit_code(StatusCode::INTERNAL_SERVER_ERROR), 1);
    }

    #[test]
    fn parses_channel_command_with_stable_defaults() {
        let cli = parse_args(strings(&[
            "channel-command",
            "photon",
            "chat-42",
            "--sender",
            "+15551234567",
            "--text",
            "/ar status",
        ]))
        .expect("parse channel command");
        assert_eq!(
            cli.command,
            Command::ProcessChannel {
                channel: "photon".into(),
                account: "default".into(),
                conversation: "chat-42".into(),
                sender: "+15551234567".into(),
                text: "/ar status".into(),
            }
        );
    }
}

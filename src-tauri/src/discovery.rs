use std::{
    collections::{HashMap, HashSet},
    fs,
    net::Ipv4Addr,
    path::{Path, PathBuf},
    process::Command,
};

use futures_util::future::join_all;
use serde::Deserialize;

use crate::{
    config::{self, HostConfig},
    domain::PeerStatusResponse,
    peer_api::tailscale_candidates,
};

pub const PEER_PROTOCOL: &str = "agent-relay-peer-v1";
pub const DISCOVERED_HOSTS_FILE_NAME: &str = "discovered-hosts.json";

#[derive(Debug, Deserialize)]
struct TailscaleStatus {
    #[serde(rename = "Peer", default)]
    peers: HashMap<String, TailscalePeer>,
}

#[derive(Clone, Debug, Deserialize)]
struct TailscalePeer {
    #[serde(rename = "DNSName", default)]
    dns_name: String,
    #[serde(rename = "HostName", default)]
    host_name: String,
    #[serde(rename = "TailscaleIPs", default)]
    addresses: Vec<String>,
    #[serde(rename = "Online", default)]
    online: bool,
}

pub fn load(config_dir: &Path) -> Result<Vec<HostConfig>, String> {
    let path = config_dir.join(DISCOVERED_HOSTS_FILE_NAME);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let contents = fs::read_to_string(&path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    serde_json::from_str(&contents)
        .map_err(|error| format!("failed to parse {}: {error}", path.display()))
}

pub fn persist(path: &Path, hosts: &[HostConfig]) -> Result<(), String> {
    let contents = serde_json::to_string_pretty(hosts)
        .map_err(|error| format!("failed to serialize discovered hosts: {error}"))?;
    config::atomic_write_text(path, &format!("{contents}\n"))
}

pub fn path(config_dir: &Path) -> PathBuf {
    config_dir.join(DISCOVERED_HOSTS_FILE_NAME)
}

pub async fn scan(
    client: &reqwest::Client,
    peer_api_port: u16,
    local_host_id: &str,
) -> Result<Vec<HostConfig>, String> {
    let peers = tokio::task::spawn_blocking(tailscale_peers)
        .await
        .map_err(|error| format!("Tailscale discovery task failed: {error}"))??;
    let probes = peers
        .into_iter()
        .filter(|peer| peer.online)
        .filter_map(|peer| {
            let address = peer
                .addresses
                .iter()
                .find_map(|value| value.parse::<Ipv4Addr>().ok())?;
            Some(probe_peer(client.clone(), peer_api_port, address, peer))
        });
    let mut discovered = join_all(probes)
        .await
        .into_iter()
        .flatten()
        .filter(|host| host.id != local_host_id)
        .collect::<Vec<_>>();
    let mut seen = HashSet::new();
    discovered.retain(|host| seen.insert(host.id.clone()));
    discovered.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(discovered)
}

async fn probe_peer(
    client: reqwest::Client,
    peer_api_port: u16,
    address: Ipv4Addr,
    peer: TailscalePeer,
) -> Option<HostConfig> {
    let endpoint = format!("http://{address}:{peer_api_port}/api/v1/status");
    let response = client
        .get(endpoint)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?;
    let status = response.json::<PeerStatusResponse>().await.ok()?;
    if status.protocol.as_deref() != Some(PEER_PROTOCOL) || status.host_id.trim().is_empty() {
        return None;
    }
    let fallback_name = if peer.host_name.trim().is_empty() {
        peer.dns_name
            .trim_end_matches('.')
            .split('.')
            .next()
            .unwrap_or(status.host_id.as_str())
            .to_owned()
    } else {
        peer.host_name
    };
    Some(HostConfig {
        id: status.host_id,
        display_name: status
            .display_name
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(fallback_name),
        address: address.to_string(),
        hardware: status
            .hardware
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "Agent Relay peer".into()),
    })
}

fn tailscale_peers() -> Result<Vec<TailscalePeer>, String> {
    let mut failures = Vec::new();
    for executable in tailscale_candidates() {
        if executable.is_absolute() && !executable.is_file() {
            continue;
        }
        let mut command = Command::new(&executable);
        command
            .args(["status", "--json"])
            .env("TAILSCALE_BE_CLI", "1");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        match command.output() {
            Ok(output) if output.status.success() => {
                let status = serde_json::from_slice::<TailscaleStatus>(&output.stdout)
                    .map_err(|error| format!("invalid Tailscale status JSON: {error}"))?;
                return Ok(status.peers.into_values().collect());
            }
            Ok(output) => failures.push(format!(
                "{} failed: {}",
                executable.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            )),
            Err(error) => failures.push(format!("{}: {error}", executable.display())),
        }
    }
    Err(format!(
        "could not read Tailscale peers{}",
        if failures.is_empty() {
            String::new()
        } else {
            format!(": {}", failures.join("; "))
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tailscale_peer_inventory() {
        let status: TailscaleStatus = serde_json::from_str(
            r#"{
              "Peer": {
                "node-key": {
                  "DNSName": "mini.example.ts.net.",
                  "HostName": "mini",
                  "TailscaleIPs": ["100.64.0.42", "fd7a:115c:a1e0::1"],
                  "Online": true
                }
              }
            }"#,
        )
        .unwrap();
        let peer = status.peers.values().next().unwrap();
        assert!(peer.online);
        assert_eq!(peer.host_name, "mini");
        assert_eq!(peer.addresses[0], "100.64.0.42");
    }

    #[test]
    fn discovered_host_store_round_trips() {
        let directory =
            std::env::temp_dir().join(format!("agent-relay-discovery-{}", std::process::id()));
        fs::create_dir_all(&directory).unwrap();
        let hosts = vec![HostConfig {
            id: "mini".into(),
            display_name: "MINI".into(),
            address: "100.64.0.42".into(),
            hardware: "Apple M1".into(),
        }];
        persist(&path(&directory), &hosts).unwrap();
        assert_eq!(load(&directory).unwrap(), hosts);
        let _ = fs::remove_dir_all(directory);
    }
}

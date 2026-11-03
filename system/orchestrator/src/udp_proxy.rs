use anyhow::{Context, Result};
use bollard::Docker;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;

use crate::config;
use crate::service_def::{PortBind, PortProtocol, ServiceDef};
use crate::state;

/// Max UDP payload (IPv4). One datagram buffer per recv.
const BUF_SIZE: usize = 65_507;
/// Evict clients that have not sent anything for this long.
const CLIENT_IDLE_SECS: u64 = 300;
/// How often the sweeper runs.
const SWEEP_INTERVAL_SECS: u64 = 30;
/// Reply forwarders exit after this much silence from the server.
const FORWARDER_IDLE_SECS: u64 = 300;
/// Re-resolve the active container's IP at most this often.
const RESOLVE_TTL_SECS: u64 = 2;
/// Cap on concurrent client sessions per relay port.
const MAX_CLIENTS: usize = 16;
/// Rate-limit repeated error logs.
const LOG_INTERVAL_SECS: u64 = 5;

/// One UDP relay target extracted from a service definition.
///
/// Created for every `protocol: udp` NamedPort entry that has `fixed_port`.
/// The relay binds `fixed_port` on the host and forwards datagrams to the
/// active color container's IP on the container network — no Docker publish,
/// so blue/green swaps never cause host-port collisions, and the client-facing
/// port stays stable across deploys (mosh requires report == connect port).
#[derive(Debug, Clone)]
pub struct UdpTarget {
    pub service: String,
    pub network: String,
    pub fixed_port: u16,
    pub container_port: u16,
    pub bind: PortBind,
}

/// Extract all UDP relay targets from service definitions.
pub fn collect_udp_targets(defs: &[ServiceDef]) -> Vec<UdpTarget> {
    let mut targets = Vec::new();
    for def in defs {
        for port in &def.ports {
            if port.protocol == PortProtocol::Udp {
                if let Some(fixed_port) = port.fixed_port {
                    targets.push(UdpTarget {
                        service: def.service.clone(),
                        network: def.network.clone(),
                        fixed_port,
                        container_port: port.container_port,
                        bind: port.bind,
                    });
                }
            }
        }
    }
    targets
}

pub async fn serve() -> Result<()> {
    let defs = ServiceDef::load_all()?;
    let targets = collect_udp_targets(&defs);

    if targets.is_empty() {
        eprintln!("[udp-proxy] no protocol:udp fixed_port entries found in service definitions");
        std::future::pending::<()>().await;
        return Ok(());
    }

    let docker = Arc::new(
        Docker::connect_with_socket_defaults()
            .context("udp-proxy: failed to connect to Docker socket")?,
    );

    let mut set = tokio::task::JoinSet::new();
    for target in targets {
        set.spawn(run_relay(target, Arc::clone(&docker)));
    }

    // All relays run forever. If any exits (error or unexpected return),
    // propagate the result so supervisord can restart the process.
    while let Some(result) = set.join_next().await {
        result??;
    }
    Ok(())
}

/// Per-client state: a dedicated outbound socket toward the server, so server
/// replies land on this socket and stay demultiplexed between clients.
struct ClientEntry {
    outbound: Arc<UdpSocket>,
    last_seen: Instant,
}

/// The active container's resolved address plus when it was fetched.
struct Resolved {
    addr: SocketAddr,
    at: Instant,
}

async fn run_relay(target: UdpTarget, docker: Arc<Docker>) -> Result<()> {
    if target.fixed_port == 0 {
        anyhow::bail!(
            "udp-proxy: fixed_port 0 is invalid for service '{}'",
            target.service
        );
    }
    let bind_ip = target
        .bind
        .resolve()
        .with_context(|| format!("udp-proxy: resolving bind for service '{}'", target.service))?;
    let main = Arc::new(
        UdpSocket::bind((bind_ip.as_str(), target.fixed_port))
            .await
            .with_context(|| {
                format!(
                    "udp-proxy: failed to bind UDP {bind_ip}:{}",
                    target.fixed_port
                )
            })?,
    );
    println!(
        "[udp-proxy] {} UDP {bind_ip}:{} → container {}:{} (active color container IP)",
        target.service, bind_ip, target.network, target.container_port
    );

    let mut clients: HashMap<SocketAddr, ClientEntry> = HashMap::new();
    let mut resolved: Option<Resolved> = None;
    let mut last_err: Option<Instant> = None;
    let mut buf = vec![0u8; BUF_SIZE];
    let mut sweep = tokio::time::interval(Duration::from_secs(SWEEP_INTERVAL_SECS));
    sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            received = main.recv_from(&mut buf) => {
                let (n, peer) = match received {
                    Ok(x) => x,
                    Err(e) => {
                        log_throttled(&mut last_err, &format!("udp-proxy: recv error: {e}"));
                        continue;
                    }
                };
                let outbound = outbound_for(&mut clients, &main, &target.service, peer).await;
                let Some(outbound) = outbound else { continue };
                let server = server_addr(&mut resolved, &target, &docker, &mut last_err).await;
                let Some(server) = server else { continue };
                if let Err(e) = outbound.send_to(&buf[..n], server).await {
                    log_throttled(&mut last_err, &format!("udp-proxy: send to {server} failed: {e}"));
                }
            }
            _ = sweep.tick() => {
                let before = clients.len();
                clients.retain(|_, c| c.last_seen.elapsed() < Duration::from_secs(CLIENT_IDLE_SECS));
                if clients.len() != before {
                    println!(
                        "[udp-proxy] {} swept {} idle client(s) ({} active)",
                        target.service,
                        before - clients.len(),
                        clients.len()
                    );
                }
            }
        }
    }
}

/// Return this client's outbound socket, creating one (and its reply
/// forwarder) on first sight. Returns None when the client cap is hit or the
/// outbound bind fails.
async fn outbound_for(
    clients: &mut HashMap<SocketAddr, ClientEntry>,
    main: &Arc<UdpSocket>,
    service: &str,
    peer: SocketAddr,
) -> Option<Arc<UdpSocket>> {
    if let Some(entry) = clients.get_mut(&peer) {
        entry.last_seen = Instant::now();
        return Some(Arc::clone(&entry.outbound));
    }
    if clients.len() >= MAX_CLIENTS {
        eprintln!("[udp-proxy] {service}: client limit {MAX_CLIENTS} reached, dropping {peer}");
        return None;
    }
    let outbound = match UdpSocket::bind(("0.0.0.0", 0)).await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            eprintln!("[udp-proxy] {service}: outbound bind failed: {e}");
            return None;
        }
    };
    println!(
        "[udp-proxy] {service}: client {peer} ({} active)",
        clients.len() + 1
    );
    tokio::spawn(forward_replies(
        Arc::clone(&outbound),
        Arc::clone(main),
        peer,
    ));
    clients.insert(
        peer,
        ClientEntry {
            outbound: Arc::clone(&outbound),
            last_seen: Instant::now(),
        },
    );
    Some(outbound)
}

/// Forward server replies back to one client. Runs until the server is silent
/// for FORWARDER_IDLE_SECS (self-terminating; eviction drops the map entry).
async fn forward_replies(outbound: Arc<UdpSocket>, main: Arc<UdpSocket>, client: SocketAddr) {
    let mut buf = vec![0u8; BUF_SIZE];
    loop {
        match tokio::time::timeout(
            Duration::from_secs(FORWARDER_IDLE_SECS),
            outbound.recv_from(&mut buf),
        )
        .await
        {
            Ok(Ok((n, _src))) => {
                if main.send_to(&buf[..n], client).await.is_err() {
                    break;
                }
            }
            // Idle timeout or socket error — end this forwarder.
            Ok(Err(_)) | Err(_) => break,
        }
    }
}

/// Resolve the active container's address, using the cache unless stale.
/// On resolve failure, fall back to the last known address — mosh
/// retransmits, and a stale IP heals on a later packet. None = never resolved.
async fn server_addr(
    cached: &mut Option<Resolved>,
    target: &UdpTarget,
    docker: &Docker,
    last_err: &mut Option<Instant>,
) -> Option<SocketAddr> {
    let stale = cached
        .as_ref()
        .is_none_or(|r| r.at.elapsed() > Duration::from_secs(RESOLVE_TTL_SECS));
    if stale {
        match resolve_server(target, docker).await {
            Ok(addr) => {
                *cached = Some(Resolved {
                    addr,
                    at: Instant::now(),
                })
            }
            Err(e) => log_throttled(last_err, &format!("udp-proxy: resolve failed: {e:#}")),
        }
    }
    cached.as_ref().map(|r| r.addr)
}

/// Resolve the active color container's IP on the container network.
async fn resolve_server(target: &UdpTarget, docker: &Docker) -> Result<SocketAddr> {
    let color = state::read_active(&target.service)?;
    let name = config::container_name(&target.service, color);
    let info = docker
        .inspect_container(&name, None)
        .await
        .with_context(|| format!("udp-proxy: inspect {name} failed"))?;
    let ip = info
        .network_settings
        .as_ref()
        .and_then(|ns| ns.networks.as_ref())
        .and_then(|nets| nets.get(&target.network))
        .and_then(|ep| ep.ip_address.as_ref())
        .map(|s| {
            s.trim()
                .trim_end_matches('/')
                .split('/')
                .next()
                .unwrap_or("")
        })
        .filter(|s| !s.is_empty())
        .with_context(|| {
            format!(
                "udp-proxy: container {name} has no IP on network {}",
                target.network
            )
        })?;
    let ip: std::net::IpAddr = ip
        .parse()
        .with_context(|| format!("udp-proxy: unparseable container IP '{ip}'"))?;
    Ok(SocketAddr::new(ip, target.container_port))
}

fn log_throttled(last: &mut Option<Instant>, msg: &str) {
    if last.is_none_or(|t| t.elapsed() > Duration::from_secs(LOG_INTERVAL_SECS)) {
        eprintln!("{msg}");
        *last = Some(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def_with_udp_fixed(service: &str, container_port: u16, fixed_port: u16) -> ServiceDef {
        serde_yaml::from_str(&format!(
            r"
service: {service}
image: ghcr.io/test/test:{service}-{{sha}}
port_scheme:
  blue_offset: 0
  green_offset: 10000
ports:
  - name: mosh
    container_port: {container_port}
    fixed_port: {fixed_port}
    protocol: udp
    bind: tailnet
health_check:
  type: docker
  timeout_secs: 30
volumes: []
required_env: []
network: codery-net
"
        ))
        .unwrap()
    }

    fn tcp_only_def(service: &str) -> ServiceDef {
        serde_yaml::from_str(&format!(
            r"
service: {service}
image: ghcr.io/test/test:{service}-{{sha}}
port_scheme:
  blue_offset: 0
  green_offset: 10000
ports:
  - name: ssh
    container_port: 2022
    fixed_port: 3333
    bind: tailnet
health_check:
  type: docker
  timeout_secs: 30
volumes: []
required_env: []
network: codery-net
"
        ))
        .unwrap()
    }

    #[test]
    fn collect_finds_udp_fixed_ports() {
        let defs = vec![def_with_udp_fixed("apps", 61000, 61000)];
        let targets = collect_udp_targets(&defs);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].service, "apps");
        assert_eq!(targets[0].fixed_port, 61000);
        assert_eq!(targets[0].container_port, 61000);
        assert_eq!(targets[0].network, "codery-net");
    }

    #[test]
    fn collect_skips_tcp_fixed_ports() {
        let defs = vec![tcp_only_def("paseo")];
        assert!(collect_udp_targets(&defs).is_empty());
    }

    #[test]
    fn collect_skips_udp_without_fixed_port() {
        let def: ServiceDef = serde_yaml::from_str(
            r"
service: apps
image: ghcr.io/test/test:apps-{sha}
port_scheme:
  blue_offset: 0
  green_offset: 10000
ports:
  - name: mosh
    container_port: 61000
    protocol: udp
    bind: tailnet
health_check:
  type: docker
  timeout_secs: 30
volumes: []
required_env: []
network: codery-net
",
        )
        .unwrap();
        assert!(collect_udp_targets(&[def]).is_empty());
    }

    #[test]
    fn mixed_services_collected() {
        let defs = vec![
            tcp_only_def("paseo"),
            def_with_udp_fixed("apps", 61000, 61000),
        ];
        let targets = collect_udp_targets(&defs);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].service, "apps");
    }

    #[test]
    fn udp_fixed_ports_are_not_published() {
        // port_binding_specs must exclude relay-served UDP ports so the
        // deployer never creates a Docker binding for them.
        let def = def_with_udp_fixed("apps", 61000, 61000);
        let specs = def.port_binding_specs(crate::config::Color::Blue);
        assert!(specs.is_empty());
        // TCP ports keep publishing.
        let tcp_def = tcp_only_def("paseo");
        let specs = tcp_def.port_binding_specs(crate::config::Color::Blue);
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].host, 2022);
    }
}

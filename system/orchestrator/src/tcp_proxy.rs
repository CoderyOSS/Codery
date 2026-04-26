use anyhow::{Context, Result};
use tokio::net::{TcpListener, TcpStream};

use crate::service_def::{PortScheme, ServiceDef};
use crate::state;

/// One proxy target extracted from a service definition.
#[derive(Debug, Clone)]
pub struct ProxyTarget {
    pub service: String,
    pub fixed_port: u16,
    pub container_port: u16,
    pub scheme: PortScheme,
}

/// Extract all proxy targets from a slice of service definitions.
/// A target exists for every `NamedPort` entry that has `fixed_port` set.
pub fn collect_proxy_targets(defs: &[ServiceDef]) -> Vec<ProxyTarget> {
    let mut targets = Vec::new();
    for def in defs {
        for port in &def.ports {
            if let Some(fixed_port) = port.fixed_port {
                targets.push(ProxyTarget {
                    service: def.service.clone(),
                    fixed_port,
                    container_port: port.container_port,
                    scheme: def.port_scheme.clone(),
                });
            }
        }
    }
    targets
}

pub async fn serve() -> Result<()> {
    let defs = ServiceDef::load_all()?;
    let targets = collect_proxy_targets(&defs);

    if targets.is_empty() {
        eprintln!("[tcp-proxy] no fixed_port entries found in service definitions");
        std::future::pending::<()>().await;
        return Ok(());
    }

    let mut set = tokio::task::JoinSet::new();
    for target in targets {
        set.spawn(run_listener(target));
    }

    // All listeners run forever. If any exits (error or unexpected return),
    // propagate the result so supervisord can restart the process.
    while let Some(result) = set.join_next().await {
        result??;
    }
    Ok(())
}

async fn run_listener(target: ProxyTarget) -> Result<()> {
    if target.fixed_port == 0 {
        anyhow::bail!("tcp-proxy: fixed_port 0 is invalid for service '{}'", target.service);
    }
    let listener = TcpListener::bind(("0.0.0.0", target.fixed_port))
        .await
        .with_context(|| format!("tcp-proxy: failed to bind :{}", target.fixed_port))?;
    println!(
        "[tcp-proxy] {} :{} → container port {} (active color port)",
        target.service, target.fixed_port, target.container_port
    );
    loop {
        let (inbound, peer) = listener.accept().await?;
        let service = target.service.clone();
        let scheme = target.scheme.clone();
        let container_port = target.container_port;
        tokio::spawn(async move {
            if let Err(e) = proxy_connection(inbound, &service, container_port, &scheme).await {
                eprintln!("[tcp-proxy] {peer}: {e}");
            }
        });
    }
}

async fn proxy_connection(
    mut inbound: TcpStream,
    service: &str,
    container_port: u16,
    scheme: &PortScheme,
) -> Result<()> {
    let color = state::read_active(service)?;
    let target_port = scheme.host_port(&color, container_port);
    let mut outbound = TcpStream::connect(("127.0.0.1", target_port))
        .await
        .with_context(|| format!("tcp-proxy: failed to connect to 127.0.0.1:{target_port}"))?;
    // EOF and connection-reset are normal at session end; suppress those errors.
    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def_with_fixed_port(service: &str, container_port: u16, fixed_port: u16) -> ServiceDef {
        serde_yaml::from_str(&format!(
            r#"
service: {service}
image: ghcr.io/test/test:{service}-{{sha}}
port_scheme:
  blue_offset: 10000
  green_offset: 20000
ports:
  - name: ssh
    container_port: {container_port}
    fixed_port: {fixed_port}
health_check:
  type: docker
  timeout_secs: 30
volumes: []
required_env: []
network: test-net
"#
        ))
        .unwrap()
    }

    fn def_without_fixed_port(service: &str) -> ServiceDef {
        serde_yaml::from_str(&format!(
            r#"
service: {service}
image: ghcr.io/test/test:{service}-{{sha}}
port_scheme:
  blue_offset: 10000
  green_offset: 20000
ports:
  - name: web
    container_port: 3000
    subdomain: foo.example.com
health_check:
  type: docker
  timeout_secs: 30
volumes: []
required_env: []
network: test-net
"#
        ))
        .unwrap()
    }

    #[test]
    fn collect_finds_fixed_ports() {
        let defs = vec![
            def_with_fixed_port("sandbox", 22, 2222),
            def_without_fixed_port("apps"),
        ];
        let targets = collect_proxy_targets(&defs);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].service, "sandbox");
        assert_eq!(targets[0].fixed_port, 2222);
        assert_eq!(targets[0].container_port, 22);
        assert_eq!(targets[0].scheme.blue_offset, 10000);
        assert_eq!(targets[0].scheme.green_offset, 20000);
    }

    #[test]
    fn collect_empty_when_no_fixed_ports() {
        let defs = vec![def_without_fixed_port("apps")];
        let targets = collect_proxy_targets(&defs);
        assert!(targets.is_empty());
    }

    #[test]
    fn collect_multiple_services() {
        let defs = vec![
            def_with_fixed_port("sandbox", 22, 2222),
            def_with_fixed_port("other", 22, 3333),
        ];
        let targets = collect_proxy_targets(&defs);
        assert_eq!(targets.len(), 2);
        let ports: Vec<u16> = targets.iter().map(|t| t.fixed_port).collect();
        assert!(ports.contains(&2222));
        assert!(ports.contains(&3333));
    }

    #[test]
    fn collect_port_scheme_preserved() {
        let defs = vec![def_with_fixed_port("sandbox", 22, 2222)];
        let targets = collect_proxy_targets(&defs);
        let target = &targets[0];
        // Verify the scheme computes correct target ports for both colors
        assert_eq!(target.scheme.host_port("blue", target.container_port), 10022);
        assert_eq!(target.scheme.host_port("green", target.container_port), 20022);
    }
}

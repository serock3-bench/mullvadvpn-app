use std::{
    env,
    fs::{self, OpenOptions},
    io::Write as _,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    os::unix::fs::OpenOptionsExt as _,
    path::PathBuf,
    process::Stdio,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use benchy_lib::iperf;
use benchy_runner::{
    BenchmarkDefinition, Direction, IPERF_CPU, MachineLock, Recorder, THROUGHPUT, checked_output,
    default_output_path, parse_iperf_output,
};
use masque_common::{
    Config, MASQUE_CPU, MasqueProxies, Process, remote_command, remote_write_file,
    scratch_dir_name, wait_for_tunnel,
};
use tokio::{io::AsyncWriteExt, process::Command};

const ALICE_TUNNEL_ADDRESS: Ipv4Addr = Ipv4Addr::new(10, 0, 1, 1);
const BOB_TUNNEL_ADDRESS: Ipv4Addr = Ipv4Addr::new(10, 0, 1, 2);
const TUNNEL_SUBNET: &str = "10.0.1.0/24";

#[tokio::main]
pub async fn run(direction: Direction) -> Result<()> {
    masque_common::init_logging();

    let name = match direction {
        Direction::Upload => "wg-over-masque-upload",
        Direction::Download => "wg-over-masque-download",
    };
    let definition = BenchmarkDefinition {
        repository: "mullvadvpn-app",
        name,
        description: "Kernel WireGuard over MASQUE; throughput and proxy CPU on Alice",
    };
    let mut recorder = Recorder::new(definition, default_output_path(name)).await;
    let result = async {
        let config = Config::from_env()?;
        recorder.parameter("duration_seconds", config.duration);
        recorder.parameter("tunnel_mtu", config.tunnel_mtu);
        recorder.parameter("masque_mtu", config.masque_mtu);
        recorder.parameter("wireguard_port", config.wireguard_port);
        recorder.parameter("direction", direction.name());
        recorder.parameter("measured_host", "alice");
        recorder.parameter("aead_backend", "aws-lc-rs");
        let (iperf, cpu) = controller(&config, direction).await?;
        let throughput = if direction.sender() {
            iperf.end.sum_sent.bits_per_second
        } else {
            iperf.end.sum_received.bits_per_second
        };
        recorder.iperf_throughput(
            THROUGHPUT,
            throughput,
            &iperf.interval_output,
            direction.sender(),
        )?;
        recorder.measurement(IPERF_CPU, iperf.end.cpu_utilization_percent.host_total)?;
        recorder.measurement(MASQUE_CPU, cpu)?;
        Ok(())
    }
    .await;
    match result {
        Ok(()) => recorder.success().await,
        Err(error) => {
            recorder.failure(&error).await?;
            Err(error)
        }
    }
}

async fn controller(config: &Config, direction: Direction) -> Result<(iperf::Output, f64)> {
    let _machine_lock = MachineLock::acquire("/tmp/benchy.lock")?;

    // The MASQUE client on Bob forwards datagrams to Alice's WireGuard listen port on
    // the server-side loopback.
    let mut proxies = MasqueProxies::start(
        config,
        SocketAddr::new(IpAddr::from(Ipv4Addr::LOCALHOST), config.wireguard_port),
    )
    .await?;
    let result = tunnel_run(config, direction, &proxies).await;
    let cleanup = proxies.stop().await;
    benchy_runner::process::with_cleanup(result, cleanup)
}

async fn tunnel_run(
    config: &Config,
    direction: Direction,
    proxies: &MasqueProxies,
) -> Result<(iperf::Output, f64)> {
    let keys = WireguardKeys::generate().await?;
    let mut tunnel = setup_wireguard(config, &keys).await?;
    let mut processes: Vec<Process> = Vec::new();
    let result = async {
        wait_for_tunnel(&config.peer, ALICE_TUNNEL_ADDRESS).await?;
        processes.push(
            Process::command("iperf3")
                .args([
                    "--server".to_owned(),
                    "--bind".into(),
                    BOB_TUNNEL_ADDRESS.to_string(),
                    "--port".into(),
                    config.iperf_port.to_string(),
                ])
                .on_peer(&config.peer)
                .spawn()
                .await?,
        );
        processes[0]
            .wait_tcp(SocketAddr::new(
                BOB_TUNNEL_ADDRESS.into(),
                config.iperf_port,
            ))
            .await?;
        let mut args = vec![
            "--json".to_owned(),
            "--interval".into(),
            "1".into(),
            "--client".into(),
            BOB_TUNNEL_ADDRESS.to_string(),
            "--port".into(),
            config.iperf_port.to_string(),
            "--time".into(),
            config.duration.to_string(),
        ];
        if !direction.sender() {
            args.push("--reverse".into());
        }
        let cpu = proxies.cpu().await?;
        processes.push(
            Process::command("iperf3")
                .args(&args)
                .capture_stdout()
                .spawn()
                .await?,
        );
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(config.duration + 30),
            processes[1].wait_output(),
        )
        .await
        .context("TCP workload timed out")??;
        let cpu = cpu.finish().await?;
        Ok((parse_iperf_output(output).await?, cpu))
    };
    let result = tokio::select! {result=result=>result,_=tokio::signal::ctrl_c()=>Err(anyhow::anyhow!("benchmark interrupted"))};
    let cleanup = benchy_runner::process::stop_all(&mut processes).await;
    let result = benchy_runner::process::with_cleanup(result, cleanup);
    benchy_runner::process::with_cleanup(result, tunnel.stop().await)
}

struct WireguardKeys {
    alice_private: String,
    alice_public: String,
    bob_private: String,
    bob_public: String,
    preshared_key: String,
}

impl WireguardKeys {
    async fn generate() -> Result<Self> {
        let alice_private = wg_key(&["genkey"], "").await?;
        let alice_public = wg_key(&["pubkey"], &alice_private).await?;
        let bob_private = wg_key(&["genkey"], "").await?;
        let bob_public = wg_key(&["pubkey"], &bob_private).await?;
        let preshared_key = wg_key(&["genpsk"], "").await?;
        Ok(Self {
            alice_private,
            alice_public,
            bob_private,
            bob_public,
            preshared_key,
        })
    }

    fn peer_config(&self, public_key: &str) -> String {
        format!(
            "[Peer]\nPublicKey = {public_key}\nPresharedKey = {}\nAllowedIPs = {TUNNEL_SUBNET}\n",
            self.preshared_key
        )
    }

    /// Alice does not set an endpoint; she learns it from Bob's handshake.
    fn alice_config(&self, config: &Config) -> String {
        format!(
            "[Interface]\nListenPort = {}\nPrivateKey = {}\n\n{}",
            config.wireguard_port,
            self.alice_private,
            self.peer_config(&self.bob_public)
        )
    }

    fn bob_config(&self, config: &Config) -> String {
        format!(
            "[Interface]\nListenPort = {}\nPrivateKey = {}\n\n{}Endpoint = 127.0.0.1:{}\n",
            config.wireguard_port,
            self.bob_private,
            self.peer_config(&self.alice_public),
            config.masque_client_port
        )
    }
}

async fn wg_key(args: &[&str], stdin_data: &str) -> Result<String> {
    let mut child = Command::new("wg")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to run wg {}", args.join(" ")))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(stdin_data.as_bytes())
            .await
            .with_context(|| format!("failed to write to wg {}", args.join(" ")))?;
    }
    let output = child
        .wait_with_output()
        .await
        .with_context(|| format!("failed to run wg {}", args.join(" ")))?;
    if !output.status.success() {
        bail!("wg {} failed with {}", args.join(" "), output.status);
    }
    String::from_utf8(output.stdout)
        .with_context(|| format!("wg {} produced non-UTF-8 output", args.join(" ")))
        .map(|key| key.trim().to_owned())
}

struct WireguardTunnel {
    peer: String,
    interface: String,
    alice_created: bool,
    bob_created: bool,
}

impl WireguardTunnel {
    async fn stop(&mut self) -> Result<()> {
        let mut errors = Vec::new();
        if self.bob_created {
            match sudo_remote(&self.peer, &["ip", "link", "del", &self.interface]).await {
                Ok(()) => self.bob_created = false,
                Err(error) => errors.push(format!("Bob: {error:#}")),
            }
        }
        if self.alice_created {
            match sudo(&["ip", "link", "del", &self.interface]).await {
                Ok(()) => self.alice_created = false,
                Err(error) => errors.push(format!("Alice: {error:#}")),
            }
        }
        anyhow::ensure!(
            errors.is_empty(),
            "WireGuard cleanup failed: {}",
            errors.join("; ")
        );
        Ok(())
    }
}

struct PrivateConfigFile(PathBuf);

impl PrivateConfigFile {
    fn new(contents: &str) -> Result<Self> {
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let path =
            env::temp_dir().join(format!("benchy-wg-{}-{timestamp}.conf", std::process::id()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .context("failed to create private WireGuard config")?;
        let config = Self(path);
        file.write_all(contents.as_bytes())
            .context("failed to write private WireGuard config")?;
        Ok(config)
    }

    fn path(&self) -> Result<&str> {
        self.0
            .to_str()
            .context("WireGuard config path is not valid UTF-8")
    }
}

impl Drop for PrivateConfigFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

async fn setup_wireguard(config: &Config, keys: &WireguardKeys) -> Result<WireguardTunnel> {
    let mut tunnel = WireguardTunnel {
        peer: config.peer.clone(),
        interface: config.interface.clone(),
        alice_created: false,
        bob_created: false,
    };
    if let Err(error) = configure_wireguard(config, keys, &mut tunnel).await {
        return benchy_runner::process::with_cleanup(Err(error), tunnel.stop().await);
    }
    Ok(tunnel)
}

async fn configure_wireguard(
    config: &Config,
    keys: &WireguardKeys,
    tunnel: &mut WireguardTunnel,
) -> Result<()> {
    let interface = config.interface.as_str();
    sudo(&["ip", "link", "add", interface, "type", "wireguard"]).await?;
    tunnel.alice_created = true;
    sudo(&[
        "ip",
        "address",
        "replace",
        &tunnel_address(ALICE_TUNNEL_ADDRESS),
        "dev",
        interface,
    ])
    .await?;
    sudo(&[
        "ip",
        "link",
        "set",
        "dev",
        interface,
        "mtu",
        &config.tunnel_mtu.to_string(),
        "up",
    ])
    .await?;

    let alice_conf = PrivateConfigFile::new(&keys.alice_config(config))?;
    sudo(&["wg", "setconf", interface, alice_conf.path()?]).await?;
    drop(alice_conf);

    let scratch = scratch_dir_name();
    let remote_conf = format!("{scratch}/wg-bob.conf");
    remote_write_file(&config.peer, &remote_conf, &keys.bob_config(config)).await?;

    sudo_remote(
        &config.peer,
        &["ip", "link", "add", interface, "type", "wireguard"],
    )
    .await?;
    tunnel.bob_created = true;
    sudo_remote(
        &config.peer,
        &[
            "ip",
            "address",
            "replace",
            &tunnel_address(BOB_TUNNEL_ADDRESS),
            "dev",
            interface,
        ],
    )
    .await?;
    sudo_remote(
        &config.peer,
        &[
            "ip",
            "link",
            "set",
            "dev",
            interface,
            "mtu",
            &config.tunnel_mtu.to_string(),
            "up",
        ],
    )
    .await?;
    sudo_remote(&config.peer, &["wg", "setconf", interface, &remote_conf]).await?;
    Ok(())
}

fn tunnel_address(address: Ipv4Addr) -> String {
    format!("{address}/24")
}

async fn sudo(args: &[&str]) -> Result<()> {
    let mut sudo_args = vec!["-n"];
    sudo_args.extend(args);
    checked_output("sudo", sudo_args)
        .await
        .with_context(|| format!("failed to run sudo {}", args.join(" ")))?;
    Ok(())
}

async fn sudo_remote(peer: &str, args: &[&str]) -> Result<()> {
    let mut sudo_args = vec!["-n".to_owned()];
    sudo_args.extend(args.iter().map(|arg| (*arg).to_owned()));
    let command = remote_command("sudo", &sudo_args);
    checked_output("ssh", [peer, &command])
        .await
        .with_context(|| format!("failed to run sudo {} on the peer", args.join(" ")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt as _, path::PathBuf};

    use super::PrivateConfigFile;

    #[test]
    fn private_config_is_restricted_and_removed() {
        let path: PathBuf;
        {
            let config = PrivateConfigFile::new("private key").unwrap();
            path = PathBuf::from(config.path().unwrap());
            assert_eq!(fs::read_to_string(&path).unwrap(), "private key");
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(!path.exists());
    }
}

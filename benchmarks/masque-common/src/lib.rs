//! Shared controller logic for the MASQUE system benchmarks.
//!
//! The benchmarks run on the controller host (Alice) and drive a second host (Bob) over
//! SSH, mirroring the GotaTun throughput benchmark. The proxy binaries under test are the
//! `mullvad-masque-proxy` examples, built from this repository checkout and deployed to
//! Bob when needed.

use anyhow::{Context, Result, bail, ensure};
use benchy_lib::{
    Unit,
    iperf::{UdpOutput, UdpSummary},
};
pub use benchy_runner::process::{Process, remote_command, shell_quote};
use benchy_runner::{
    BenchmarkDefinition, Direction, IPERF_CPU, MachineLock, MeasurementDefinition, Recorder,
    THROUGHPUT, UDP_JITTER, UDP_LOSS, checked_output, cpu::ProcessCpu, default_output_path,
    parse_iperf_udp_output,
};
use std::process::Stdio;
use std::{
    env,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::AsyncWriteExt,
    process::Command,
    time::{sleep, timeout},
};

pub const MASQUE_CPU: MeasurementDefinition = MeasurementDefinition {
    id: "cpu.masque",
    label: "MASQUE CPU",
    unit: Unit::Percent,
};
const READY_MARKER: &str = "Listening on";
const PROBE_ATTEMPTS: usize = 10;
const PROBE_INTERVAL: Duration = Duration::from_millis(500);

/// Measure the controller's endpoint of UDP traffic through the proxy. Alice runs the
/// iperf server because the MASQUE client on Bob forwards its UDP socket to Alice.
pub async fn run_udp_benchmark(
    definition: BenchmarkDefinition,
    datagram_length: u16,
    direction: Direction,
) -> Result<()> {
    let mut recorder = Recorder::new(definition, default_output_path(definition.name)).await;
    let result = async {
        let config = Config::from_env()?;
        recorder.parameter("duration_seconds", config.duration);
        recorder.parameter("datagram_length", datagram_length);
        recorder.parameter("masque_mtu", config.masque_mtu);
        recorder.parameter("direction", direction.name());
        recorder.parameter("measured_host", "alice");
        recorder.parameter("aead_backend", "aws-lc-rs");
        let (output, cpu) = udp_controller(&config, datagram_length, direction).await?;
        let local = local_udp_summary(&output, direction)?;
        recorder.iperf_throughput(
            THROUGHPUT,
            local.bits_per_second,
            &output.interval_output,
            direction.sender(),
        )?;
        let iperf_cpu = output
            .end
            .cpu_utilization_percent
            .as_ref()
            .context("local iperf report has no CPU summary")?;
        recorder.measurement(IPERF_CPU, iperf_cpu.host_total)?;
        recorder.measurement(MASQUE_CPU, cpu)?;
        // Loss and jitter are receiver observations; do not import Bob's values for upload.
        if !direction.sender() {
            let jitter = local
                .jitter_ms
                .context("local UDP receiver report has no jitter")?;
            let loss = local
                .lost_percent
                .context("local UDP receiver report has no loss")?;
            recorder.measurement(UDP_JITTER, jitter / 1000.0)?;
            recorder.measurement(UDP_LOSS, loss)?;
        }
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

fn local_udp_summary(output: &UdpOutput, direction: Direction) -> Result<&UdpSummary> {
    if direction.sender() {
        output.end.sent()
    } else {
        output.end.received()
    }
    .context("iperf UDP output lacks the controller's direction summary")
}

async fn udp_controller(
    config: &Config,
    length: u16,
    direction: Direction,
) -> Result<(UdpOutput, f64)> {
    let _lock = MachineLock::acquire("/tmp/benchy.lock")?;
    let mut proxies = MasqueProxies::start(
        config,
        SocketAddr::new(config.alice_address, config.iperf_port),
    )
    .await?;
    let result = udp_run(config, length, direction, &proxies).await;
    let cleanup = proxies.stop().await;
    benchy_runner::process::with_cleanup(result, cleanup)
}

async fn udp_run(
    config: &Config,
    length: u16,
    direction: Direction,
    proxies: &MasqueProxies,
) -> Result<(UdpOutput, f64)> {
    let mut processes: Vec<Process> = Vec::new();
    let result = async {
        processes.push(iperf_server(config.alice_address, config.iperf_port).await?);
        processes[0]
            .wait_tcp(SocketAddr::new(config.alice_address, config.iperf_port))
            .await?;
        // Only iperf's TCP control connection bypasses MASQUE. Both UDP directions use it.
        processes.push(
            Process::command("socat")
                .args([
                    format!(
                        "TCP4-LISTEN:{},bind=127.0.0.1,fork,reuseaddr",
                        config.masque_client_port
                    ),
                    format!("TCP:{}:{}", config.alice_address, config.iperf_port),
                ])
                .on_peer(&config.peer)
                .spawn()
                .await?,
        );
        processes[1]
            .wait_tcp(SocketAddr::new(
                Ipv4Addr::LOCALHOST.into(),
                config.masque_client_port,
            ))
            .await?;
        let mut args = vec![
            "--udp".to_owned(),
            "--client".into(),
            "127.0.0.1".into(),
            "--port".into(),
            config.masque_client_port.to_string(),
            "--time".into(),
            config.duration.to_string(),
            "--length".into(),
            length.to_string(),
            "--bandwidth".into(),
            "10G".into(),
        ];
        // Bob is the client; reverse mode makes the controller/server send upload traffic.
        if direction.sender() {
            args.push("--reverse".into());
        }
        let cpu = proxies.cpu().await?;
        processes.push(
            Process::command("iperf3")
                .args(&args)
                .on_peer(&config.peer)
                .spawn()
                .await?,
        );
        let client = timeout(
            Duration::from_secs(config.duration + 30),
            processes[2].wait_output(),
        )
        .await
        .context("UDP workload timed out")??;
        ensure!(
            client.status.success(),
            "peer iperf client exited: {}",
            client.status
        );
        let cpu = cpu.finish().await?;
        let server = timeout(Duration::from_secs(10), processes[0].wait_output())
            .await
            .context("local iperf report timed out")??;
        let output = parse_iperf_udp_output(server).await?;
        Ok((output, cpu))
    };
    let result = tokio::select! { result=result=>result, _=tokio::signal::ctrl_c()=>Err(anyhow::anyhow!("benchmark interrupted")) };
    let cleanup = benchy_runner::process::stop_all(&mut processes).await;
    benchy_runner::process::with_cleanup(result, cleanup)
}

/// Environment-driven benchmark configuration.
#[derive(Debug, Clone)]
pub struct Config {
    pub peer: String,
    pub alice_address: IpAddr,
    pub bob_address: IpAddr,
    pub masque_server_port: u16,
    pub masque_client_port: u16,
    pub iperf_port: u16,
    pub wireguard_port: u16,
    pub interface: String,
    pub duration: u64,
    pub masque_mtu: u16,
    pub tunnel_mtu: u16,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let config = Self {
            peer: env_value("BENCHY_PEER", "mole@10.0.0.2"),
            alice_address: env_value("BENCHY_ALICE_ADDRESS", "10.0.0.1").parse()?,
            bob_address: env_value("BENCHY_BOB_ADDRESS", "10.0.0.2").parse()?,
            masque_server_port: env_value("BENCHY_MASQUE_SERVER_PORT", "9020").parse()?,
            masque_client_port: env_value("BENCHY_MASQUE_CLIENT_PORT", "9010").parse()?,
            iperf_port: env_value("BENCHY_IPERF_PORT", "9030").parse()?,
            wireguard_port: env_value("BENCHY_WIREGUARD_PORT", "51821").parse()?,
            interface: env_value("BENCHY_INTERFACE", "bench0"),
            duration: env_value("BENCHY_DURATION", "30").parse()?,
            masque_mtu: env_value("BENCHY_MASQUE_MTU", "1280").parse()?,
            tunnel_mtu: env_value("BENCHY_TUNNEL_MTU", "1280").parse()?,
        };
        ensure!(
            (1..=120).contains(&config.duration),
            "BENCHY_DURATION must be 1–120 seconds"
        );
        Ok(config)
    }

    pub fn masque_server_bind(&self) -> SocketAddr {
        SocketAddr::new(self.alice_address, self.masque_server_port)
    }
}

/// The MASQUE server on Alice and the MASQUE client on Bob.
pub struct MasqueProxies {
    peer: String,
    remote_dir: String,
    server: Process,
    client: Process,
}

impl MasqueProxies {
    /// Builds the proxy examples from this checkout, deploys the client to Bob, and starts
    /// both proxies. `target_addr` is the server-side forwarding destination, as resolved
    /// by the server on Alice.
    pub async fn start(config: &Config, target_addr: SocketAddr) -> Result<Self> {
        let examples = build_masque_examples().await?;
        let remote_dir = scratch_dir_name();
        if let Err(error) = deploy_client(&config.peer, &examples.client, &remote_dir).await {
            return benchy_runner::process::with_cleanup(
                Err(error),
                remove_remote_dir(&config.peer, &remote_dir).await,
            );
        }

        let mut server = match Process::command(&examples.server)
            .args([
                "--cert-path".to_owned(),
                examples.cert.to_string_lossy().into_owned(),
                "--key-path".into(),
                examples.key.to_string_lossy().into_owned(),
                "--bind-addr".into(),
                config.masque_server_bind().to_string(),
            ])
            .ready_when(READY_MARKER)
            .spawn()
            .await
        {
            Ok(server) => server,
            Err(error) => {
                return benchy_runner::process::with_cleanup(
                    Err(error),
                    remove_remote_dir(&config.peer, &remote_dir).await,
                );
            }
        };

        let remote_client = format!("{remote_dir}/masque-client");
        let client = Process::command(&remote_client)
            .args([
                "--server-addr".to_owned(),
                config.masque_server_bind().to_string(),
                "--server-hostname".into(),
                "example.org".into(),
                "--bind-addr".into(),
                format!("127.0.0.1:{}", config.masque_client_port),
                "--mtu".into(),
                config.masque_mtu.to_string(),
                "--target-addr".into(),
                target_addr.to_string(),
            ])
            .on_peer(&config.peer)
            .ready_when(READY_MARKER)
            .spawn()
            .await;
        let mut client = match client {
            Ok(client) => client,
            Err(error) => {
                let result = benchy_runner::process::with_cleanup(Err(error), server.stop().await);
                return benchy_runner::process::with_cleanup(
                    result,
                    remove_remote_dir(&config.peer, &remote_dir).await,
                );
            }
        };

        // The client connects immediately after reporting readiness; a failed connection
        // exits the process right away.
        sleep(Duration::from_millis(500)).await;
        if let Err(error) = client.check_alive() {
            let result = benchy_runner::process::with_cleanup(Err(error), client.stop().await);
            let result = benchy_runner::process::with_cleanup(result, server.stop().await);
            return benchy_runner::process::with_cleanup(
                result,
                remove_remote_dir(&config.peer, &remote_dir).await,
            );
        }

        Ok(Self {
            peer: config.peer.clone(),
            remote_dir,
            server,
            client,
        })
    }

    pub async fn cpu(&self) -> Result<ProcessCpu> {
        ProcessCpu::start(self.server.pid()).await
    }

    pub async fn stop(&mut self) -> Result<()> {
        let client = self.client.stop().await;
        let server = self.server.stop().await;
        let result = benchy_runner::process::with_cleanup(client, server);
        benchy_runner::process::with_cleanup(
            result,
            remove_remote_dir(&self.peer, &self.remote_dir).await,
        )
    }
}

struct MasqueExamples {
    server: PathBuf,
    client: PathBuf,
    cert: PathBuf,
    key: PathBuf,
}

fn app_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Builds the `mullvad-masque-proxy` example binaries from this repository checkout.
async fn build_masque_examples() -> Result<MasqueExamples> {
    let root = app_root();
    checked_output(
        "cargo",
        [
            "build".to_owned(),
            "--release".to_owned(),
            "--locked".to_owned(),
            "--manifest-path".to_owned(),
            root.join("Cargo.toml").to_string_lossy().into_owned(),
            "--package".to_owned(),
            "mullvad-masque-proxy".to_owned(),
            "--example".to_owned(),
            "masque-server".to_owned(),
            "--example".to_owned(),
            "masque-client".to_owned(),
        ],
    )
    .await
    .context("failed to build the mullvad-masque-proxy examples")?;

    let tests = root.join("mullvad-masque-proxy/tests");
    let binaries = root.join("target/release/examples");
    Ok(MasqueExamples {
        server: binaries.join("masque-server"),
        client: binaries.join("masque-client"),
        cert: tests.join("test.crt"),
        key: tests.join("test.key"),
    })
}

async fn deploy_client(peer: &str, client: &Path, remote_dir: &str) -> Result<()> {
    checked_output(
        "ssh",
        [
            peer,
            &format!(
                "mkdir -p {dir} && chmod 700 {dir}",
                dir = shell_quote(remote_dir)
            ),
        ],
    )
    .await?;
    checked_output(
        "scp",
        [
            client.as_os_str(),
            std::ffi::OsStr::new(&format!("{peer}:{remote_dir}/masque-client")),
        ],
    )
    .await?;
    let remote_executable = shell_quote(&format!("{remote_dir}/masque-client"));
    checked_output("ssh", [peer, &format!("chmod 755 {remote_executable}")]).await?;
    Ok(())
}

async fn remove_remote_dir(peer: &str, remote_dir: &str) -> Result<()> {
    checked_output(
        "ssh",
        [peer, &format!("rm -rf -- {}", shell_quote(remote_dir))],
    )
    .await?;
    Ok(())
}

/// Capture Alice's one-shot server JSON rather than requesting Bob's measurements.
pub async fn iperf_server(bind: IpAddr, port: u16) -> Result<Process> {
    Process::command("iperf3")
        .args([
            "--server".to_owned(),
            "--one-off".into(),
            "--json".into(),
            "--interval".into(),
            "1".into(),
            "--bind".into(),
            bind.to_string(),
            "--port".into(),
            port.to_string(),
        ])
        .capture_stdout()
        .spawn()
        .await
}

/// Waits until the peer can reach the given tunnel address over ICMP.
pub async fn wait_for_tunnel(peer: &str, destination: Ipv4Addr) -> Result<()> {
    let command = remote_command(
        "ping",
        &[
            "-c".to_owned(),
            "1".to_owned(),
            "-W".to_owned(),
            "1".to_owned(),
            destination.to_string(),
        ],
    );
    for _ in 0..PROBE_ATTEMPTS {
        if checked_output("ssh", [peer, &command]).await.is_ok() {
            return Ok(());
        }
        sleep(PROBE_INTERVAL).await;
    }
    bail!("the tunnel to {destination} did not become ready");
}

/// Writes a file on the peer by piping its contents over SSH.
pub async fn remote_write_file(peer: &str, path: &str, contents: &str) -> Result<()> {
    let mut child = Command::new("ssh")
        .arg(peer)
        .arg(format!("umask 077; cat > {}", shell_quote(path)))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("failed to write {path} on the peer"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(contents.as_bytes())
            .await
            .with_context(|| format!("failed to send {path} to the peer"))?;
    }
    let output = child
        .wait_with_output()
        .await
        .with_context(|| format!("failed to write {path} on the peer"))?;
    if !output.status.success() {
        bail!("failed to write {path} on the peer: {}", output.status);
    }
    Ok(())
}

/// Scratch directory shared by the proxy and WireGuard config for one invocation.
pub fn scratch_dir_name() -> String {
    format!("/tmp/benchy-{}", run_id())
}

fn run_id() -> String {
    env::var("GITHUB_RUN_ID").unwrap_or_else(|_| std::process::id().to_string())
}

fn env_value(key: &str, default: &str) -> String {
    env::var(key)
        .ok()
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_owned())
}

pub fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selects_only_the_local_direction_summary() {
        let download:UdpOutput=serde_json::from_str(r#"{"end":{"sum":{"bits_per_second":10.0,"sender":false,"jitter_ms":1.0,"lost_percent":2.0}}}"#).unwrap();
        assert!(local_udp_summary(&download, Direction::Download).is_ok());
        assert!(local_udp_summary(&download, Direction::Upload).is_err());
        let upload: UdpOutput =
            serde_json::from_str(r#"{"end":{"sum_sent":{"bits_per_second":12.0,"sender":true}}}"#)
                .unwrap();
        assert_eq!(
            local_udp_summary(&upload, Direction::Upload)
                .unwrap()
                .bits_per_second,
            12.0
        );
    }
}

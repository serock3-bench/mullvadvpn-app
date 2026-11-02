use benchy_runner::{BenchmarkDefinition, Direction};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    masque_common::init_logging();
    masque_common::run_udp_benchmark(
        BenchmarkDefinition {
            repository: "mullvadvpn-app",
            name: "masque-throughput-download",
            description: "UDP throughput and CPU measured on Alice through MASQUE",
        },
        1100,
        Direction::Download,
    )
    .await
}

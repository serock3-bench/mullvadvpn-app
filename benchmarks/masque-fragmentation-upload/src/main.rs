use benchy_runner::{BenchmarkDefinition, Direction};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    masque_common::init_logging();
    masque_common::run_udp_benchmark(
        BenchmarkDefinition {
            repository: "mullvadvpn-app",
            name: "masque-fragmentation-upload",
            description: "UDP throughput and CPU measured on Alice through MASQUE",
        },
        2200,
        Direction::Upload,
    )
    .await
}

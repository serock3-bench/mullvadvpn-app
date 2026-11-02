fn main() -> anyhow::Result<()> {
    wg_over_masque_common::run(benchy_runner::Direction::Download)
}

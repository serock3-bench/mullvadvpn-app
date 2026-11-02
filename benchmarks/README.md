# Mullvad app system benchmarks

This separate Cargo workspace defines the workloads; the sibling `benchy` checkout
provides the shared runner, result schema and GitHub/Bencher integration.

## Workloads

Each scenario has an `-upload` (Alice → Bob) and `-download` (Bob → Alice) benchmark:

| Scenario | Workload |
| --- | --- |
| `masque-throughput` | UDP iperf3 with 1100-byte datagrams, within the 1280-byte MASQUE MTU |
| `masque-fragmentation` | UDP iperf3 with 2200-byte datagrams, exercising MASQUE fragmentation |
| `wg-over-masque` | TCP iperf3 through kernel WireGuard carried over MASQUE |

All six are selected by default. For example:

```console
cargo run --release --locked --manifest-path benchmarks/Cargo.toml -p masque-throughput-upload-benchmark
```

The proxies are the checkout's `mullvad-masque-proxy` examples, built with the root
lockfile and its existing AWS-LC backend. Alice runs the MASQUE server; Bob runs the
client, forwarding datagrams to Alice. The shipped self-signed test certificate is
used on the isolated link. The example client does not verify the certificate.

For UDP, Alice runs the one-shot iperf server and captures its JSON locally. Bob
runs the client; reverse mode makes Alice send upload traffic. Only iperf's TCP
control channel bypasses MASQUE, through a socat relay on Bob. Both UDP directions
traverse the proxy. For WireGuard/TCP, Alice runs the iperf client and Bob the server;
reverse mode produces download traffic. No measurements are collected from Bob.

## Shared measures

- `throughput.network`: Alice's sender/receiver throughput, stored in bits/s and
  published as Gb/s. Local one-second intervals estimate the standard error of the
  mean, assuming negligible between-run variance.
- `cpu.iperf`: local iperf CPU from Alice's JSON report.
- `cpu.masque`: Alice's MASQUE server process user/system CPU, across its threads,
  sampled over the transfer. This excludes kernel WireGuard CPU.
- `udp.jitter` (seconds) and `udp.lost_percent`: UDP download receiver observations
  on Alice. Upload does not import Bob's receiver statistics.

CPU is a percentage of one logical CPU (200% means two CPUs) without uncertainty
bounds. Changing the old sender/receiver layout requires a new baseline; the pilot
publisher uses `alice-bob-app-v2` as its Bencher testbed.

## Actions and configuration

`Benchmarks` runs nightly on main at 01:17 UTC, on internal PRs, and manually with a
benchmark selection/iteration count. External PRs require maintainer approval on
the PR page. `Publish benchmarks` runs separately on GitHub-hosted infrastructure.
It receives the project's `BENCHER_API_KEY` Actions secret and `BENCHER_PROJECT`
repository variable (`mullvad-test` for the pilot). Alice receives no publishing
key. Raw JSON is retained in the benchmark run's summary and artifact.

| Variable | Default |
| --- | --- |
| `BENCHY_PEER` | `mole@10.0.0.2` |
| `BENCHY_ALICE_ADDRESS` | `10.0.0.1` |
| `BENCHY_BOB_ADDRESS` | `10.0.0.2` |
| `BENCHY_MASQUE_SERVER_PORT` | `9020` |
| `BENCHY_MASQUE_CLIENT_PORT` | `9010` |
| `BENCHY_IPERF_PORT` | `9030` |
| `BENCHY_WIREGUARD_PORT` | `51821` |
| `BENCHY_INTERFACE` | `bench0` |
| `BENCHY_DURATION` | `30` seconds (1–120) |
| `BENCHY_MASQUE_MTU` | `1280` |
| `BENCHY_TUNNEL_MTU` | `1280` |

Alice needs Rust, a C compiler/CMake, `iperf3`, `ssh`, `scp`, and `ss`. Bob needs
`iperf3`, `socat`, `ss` and SSH. WireGuard workloads also need `wg`, `ip`, `ping`,
kernel WireGuard support and passwordless sudo for the relevant `ip`/`wg` commands
on both machines. The shared `/tmp/benchy.lock` also excludes manual concurrent runs.

Readiness inspects the owned listener with `ss`, avoiding broken iperf control
connections. Cleanup stops only recorded process IDs and deletes interfaces created
by that invocation. WireGuard keys are ephemeral; configuration files have mode
0600 and live in a mode-0700 scratch directory on Bob. A lost controller/SSH session
can leave resources behind: inspect the failed run before manually deleting its
`bench0` interface. Normal errors and Ctrl-C attempt all cleanup steps and surface
cleanup errors rather than reporting success.

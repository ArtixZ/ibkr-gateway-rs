<div align="center">

# gatewayctl

**Independent IB Gateway supervision for macOS**

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Platform: Apple silicon macOS](https://img.shields.io/badge/platform-Apple%20silicon%20macOS-lightgrey.svg)
![Status: experimental](https://img.shields.io/badge/status-experimental-orange.svg)

Isolated paper/live sessions. Keychain credentials. Native restart recovery.

</div>

`gatewayctl` combines a small Rust supervisor with a Java agent inside each
Gateway JVM. It replaces the IBC/monitor layer without adding another JVM,
container, or API proxy. Trading applications keep using the standard TWS API.

> **Unofficial and experimental.** Not affiliated with Interactive Brokers or
> IBC. Start with paper trading. Live API clients can trade real money, and
> broker-required MFA still needs your approval.

## What you get

| Capability | How it works |
| --- | --- |
| Paper/live isolation | Separate processes, settings, ports, locks, and logs |
| Private credentials | macOS Keychain; no passwords in JVM arguments or INI files |
| Native restarts | Retains Gateway's own restart handler and adopts its replacement process |
| Verified readiness | Checks process ownership, account identity, and API responsiveness |
| Background operation | Per-user LaunchAgents with JSON status and bounded logs |

**Qualified target:** Apple silicon macOS, English-language IB Gateway **10.44**,
API server version **213**. Other versions, hardware, and locales need their
own qualification. The Mac must remain awake with a logged-in GUI session.

## Quick start

### 1. Build and install

Install [IB Gateway from IBKR](https://www.interactivebrokers.com/en/trading/ibgateway-stable.php)
separately. The build also needs Rust and JDK 17 or newer.

```sh
git clone https://github.com/ArtixZ/ibkr-gateway-rs.git
cd ibkr-gateway-rs

brew install rust openjdk@17
export JAVA_HOME="$(brew --prefix openjdk@17)/libexec/openjdk.jdk/Contents/Home"
export GATEWAY_HOME="$HOME/Applications/IB Gateway 10.44"

cargo build --release --locked
mkdir -p "$HOME/.local/bin"
install -m 755 target/release/gatewayctl "$HOME/.local/bin/gatewayctl"
export PATH="$HOME/.local/bin:$PATH"
```

If reviewing an unmerged PR, check out its branch before building. Set
`GATEWAY_HOME` to your actual installation; runtime uses its bundled JVM.

### 2. Configure credentials

```sh
gatewayctl init
gatewayctl credentials --instance paper
gatewayctl doctor
```

Before `doctor`, edit `~/.config/ibkr-gateway-rs/config.toml` so `gateway_home`
matches your installation. Both profiles start disabled. Leave an unbound
profile disabled for the enrollment step below.

**Review `api_orders`:** the example requests order access after account
verification. Set it to `false` for read-only API access. Keep credentials,
real account IDs, and runtime files out of Git.

### 3. Start paper trading first

```sh
gatewayctl service install --instance paper --enroll-if-unbound
gatewayctl start --instance paper --enroll-if-unbound
gatewayctl status --instance paper
```

Enrollment authenticates with API orders disabled, discovers and pins the
account IDs, then hands off to normal supervision. Wait for **`phase: "ready"`**
before connecting clients.

For live trading, store its credentials with `--instance live`, then repeat
the service commands for `live`. Approve any **IB Key** notification; a separate
mobile trading login can compete with Gateway's session.

## Connect and manage

Clients connect locally through the **TWS socket API**, not HTTP:

| Profile | Default endpoint | Funds |
| --- | --- | --- |
| Paper | `127.0.0.1:4002` | Simulated |
| Live | `127.0.0.1:4001` | Real money |

Use a unique, non-master client ID for each application. Gateway already holds
the login, so API clients do not need the username or password.

| Command | Purpose |
| --- | --- |
| `gatewayctl status` | Show all profiles as JSON |
| `gatewayctl restart --instance paper` | Request a session-preserving native restart |
| `gatewayctl stop --instance live` | Stop only the live Gateway |
| `gatewayctl resume --instance live` | Resume after resolving an intervention |
| `gatewayctl diagnose --instance live` | Write redacted UI diagnostics to its private log |

Daily native restarts can reuse a valid session. Fresh authentication,
expired tokens, and broker security checks may still require MFA. Unknown
dialogs and session conflicts pause automation rather than force a takeover.

**[Operations guide](docs/operations.md)** covers Keychain updates, enrollment,
service removal, logs, troubleshooting, and migration from IBC.

## Development

With the build environment above configured:

```sh
cargo fmt --check
cargo test --locked
cargo clippy --all-targets -- -D warnings
```

GUI fixtures require an Aqua session. See the
[validation guide](docs/operations.md#validation); automated checks must never
submit real orders.

## License

[MIT](LICENSE) for this project's original code. [IBC](https://github.com/IbcAlpha/IBC)
was a reference during development and remains separately GPL-licensed.
IBKR software is **not bundled**; its terms and dependency licenses remain
separate. See [publication boundaries](docs/operations.md#publication-boundaries).

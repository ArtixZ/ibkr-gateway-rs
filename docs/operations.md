# Operations guide

[Back to the overview](../README.md)

## Supported scope

The qualified environment is Apple silicon macOS, the English-language
IB Gateway 10.44 UI, and API server version 213. The health client requires
server version 213 or newer, but a newer API version alone does not establish
launcher or UI compatibility.

Gateway requires a GUI session. Keep macOS powered, awake, and logged in.
A background or SSH shell may lack
WindowServer access even when a GUI session exists; LaunchAgents run in the
user's Aqua domain. The installer does not change power settings, disable
authentication, or install a root daemon.

The Rust controller places no orders. Its checks are not a trading-safety
boundary: connected clients can submit real orders when live API access is
enabled. Pause dependent clients during initial setup and account changes
until identity and readiness have been verified.

## Configuration

`gatewayctl init` creates `~/.config/ibkr-gateway-rs/config.toml` without
overwriting an existing file. See [config.example.toml](../config.example.toml)
for the schema.

| Setting | Purpose |
| --- | --- |
| `gateway_home` | Path to the installed offline Gateway |
| `state_dir` | Private root for instance settings, ownership, and logs |
| `enabled` | Allow an explicitly configured profile to run |
| `mode` | `paper` or `live`; keep it consistent with the intended login |
| `api_port` | Unique local API port for this instance |
| `monitor_client_id` | Reserved, nonzero, non-master API client ID |
| `expected_accounts` | Exact broker account allowlist; never guess IDs |
| `api_orders` | Permit order-enabled API configuration after verification |
| `auto_restart_time` | Daily restart time in Gateway's local timezone |

Unbound profiles can use explicit read-only enrollment. If configuring account
IDs manually, replace the placeholders before setting `enabled = true`.
Piped profile JSON can be validated and saved with
`gatewayctl configure --instance paper --stdin`. Running services do not
silently adopt arbitrary configuration changes.

Each instance has its own directory under
`~/.local/share/ibkr-gateway-rs/<instance>/`. Runtime directories must be owned
by the current user with mode `0700`; controller state, sockets, and logs are
private. Never share settings directories or copy restart tokens between
accounts.

## Credentials and Keychain

Install the executable at a stable path before provisioning credentials:

```sh
gatewayctl credentials --instance paper
gatewayctl credentials --instance live
```

The prompts are hidden. Keychain service `dev.ibkr.gatewayctl` stores one
credential pair per instance. Unattended reads cannot display Keychain
permission prompts; a locked Keychain or missing access produces an explicit
intervention state.

To load credentials from a secret provider, pipe a JSON object with `username`
and `password` fields into `gatewayctl credentials --instance paper --stdin`.
Do not put secrets in command arguments, shell history, or source files.

Rebuilding an unsigned executable can change its Keychain code identity.
Re-import with `--replace` in the user's GUI session to recreate only this
application's own item, then verify unattended access. Do not grant access to
all applications. Bridge packaging uses fixed timestamps to avoid needless
identity changes when only fixtures are rebuilt.

## Enrollment and services

For a disabled, unbound profile:

```sh
gatewayctl service install --instance live --enroll-if-unbound
gatewayctl start --instance live --enroll-if-unbound
```

This explicit opt-in authenticates with API orders disabled, discovers the
broker account IDs, pins them in private configuration, and continues normal
supervision. Existing real bindings are not replaced. The handoff retains the
authenticated Gateway instead of requiring another login just to activate
the service. Future starts use the pinned account.

Alternatively, `gatewayctl enroll --instance live` performs enrollment in the
foreground. After successful enrollment, install and start the normal service.
Stopping standalone enrollment reports that it did not complete account binding.

For an already configured, enabled profile:

```sh
gatewayctl service install --instance paper
gatewayctl start --instance paper
```

Installation writes, but does not immediately load, its LaunchAgent.
The labels are `dev.ibkr.gatewayctl.paper` and `dev.ibkr.gatewayctl.live`.
Installed services are configured to run after GUI login; an explicit stop
remains stopped across supervisor restarts.

```sh
gatewayctl stop --instance live
gatewayctl service uninstall --instance live
```

Only the selected instance is affected. The uninstaller also handles a crashed
or never-loaded service, while refusing to signal an unverifiable process.

## Authentication and recovery

Daily native restart reuses Gateway's supported session state when it remains
valid. A full login, including after weekly token invalidation, can submit the
stored credentials automatically. That does not bypass a broker-required
IBKR Mobile or passkey approval.

Approve the **IB Key notification for Gateway**, rather than starting a separate
mobile trading session. A competing mobile login can disconnect Gateway if
you accept its takeover prompt. See
[IBKR's session policy](https://ibkrcampus.com/docs/web-api/authentication/multiple-sessions.md).

| Status or situation | Action |
| --- | --- |
| `ready` | Account and API readiness checks passed |
| `awaiting_mfa` | Approve the current broker challenge |
| Cancelled or expired MFA | Resolve the challenge and explicitly resume; no automatic push loop |
| Read-only login selected | Stop/start for a fresh login; `resume` cannot turn it into trading authority |
| Keychain access failure | Re-provision access from the GUI session |
| Unknown dialog or session conflict | Inspect and resolve it; no automatic takeover |
| Supervisor unreachable | Inspect service errors; stale state is not reported as healthy |

`gatewayctl restart --instance paper` requests a native restart, not an
immediate kill. Gateway's controls operate at minute granularity. The shutdown
allowance starts from the reported scheduled time, while session-resume grace
and UI-stall detection have separate limits.

A failed native restart pauses for inspection instead of discarding a
potentially valid session. Broker disconnects and ambiguous API failures do not
justify repeatedly killing a responsive JVM. Forced recovery of a stalled UI
requires corroborating API failures.

## Logs and notifications

Private, rotating controller logs live under each instance's `logs/` directory.
Use `gatewayctl diagnose --instance live` to record redacted UI labels and
component metadata. Unstructured vendor output is counted in summaries rather
than copied line-for-line. Inspect Gateway's own native logs locally when
needed; they remain sensitive.

Fatal errors before instance logging is available go to macOS system logging
under `gatewayctl`.

Notifications are optional. `tradebus_events` enables the tradebus
integration by pointing at its event directory; tradebus is not required to
run the controller and is not installed by it. Without a notification
integration, monitor status and logs yourself.

Session incidents and liveness signals use separate paper/live identities.
Routine restart progress does not clear an outage; verified readiness does.
Failed deliveries remain visible and pending transitions are retried.

## Validation

With `JAVA_HOME` and `GATEWAY_HOME` configured:

```sh
cargo fmt --check
cargo test --locked
cargo clippy --all-targets -- -D warnings
```

Run synthetic GUI scenarios only in an authorized Aqua session:

```sh
cargo test --test bridge synthetic_gui -- --ignored --test-threads=1
```

Coverage includes ownership/PID reuse, bounded IPC, API framing and account
mismatch, enrollment, MFA cancellation/expiry, read-only-session safeguards,
modal dialogs, restart timing, and native launcher preparation. The synthetic
Gateway is not packaged in the production bridge.

Real deployment still needs qualification against the installed Gateway and
actual client workloads. Do not submit real orders as automated tests. Compare
controller overhead separately from Gateway using identical versions, account
counts, and workloads; each Gateway retains its own JVM memory overhead.

## Publication boundaries

This project is [MIT-licensed](../LICENSE). Third-party dependencies and IBKR
software retain their own terms.

Obtain Gateway and its JVM from IBKR's authorized distribution. Do not publish
vendor installers, JARs, native launchers, copied per-instance installations,
or the separately licensed [official TWS API client source](https://interactivebrokers.github.io/).
The controller uses the independently maintained Rust `ibapi` dependency.

Do not publish credentials, account IDs, Keychain databases, real configuration,
runtime data, or unredacted logs/screenshots. Ignore rules are safeguards, not
sanitization or removal of information already committed.

This repository is source-first, not a signed/notarized binary distribution.
Compiled artifacts need a separate review for local paths, dependency notices,
vendor content, and signing. A source license does not establish compliance
with IBKR platform terms or grant vendor endorsement.

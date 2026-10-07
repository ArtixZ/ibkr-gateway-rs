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

After updating the executable, existing credentials can be authorized without
re-entering or exporting them:

```sh
gatewayctl credentials --instance paper --authorize
gatewayctl credentials --instance live --authorize
```

Run this in the user's GUI session and approve macOS Keychain access if asked.
This verifies unattended access without displaying or replacing the stored
pair, including a fresh-process check. Use persistent access for this
executable and re-check the service afterward; a one-time permission may not
apply to a future daemon run. It is a macOS permission prompt, not IBKR MFA.

To load credentials from a secret provider, pipe a JSON object with `username`
and `password` fields into `gatewayctl credentials --instance paper --stdin`.
Do not put secrets in command arguments, shell history, or source files.

Rebuilding an unsigned executable can change its Keychain code identity.
Re-import with `--replace` in the user's GUI session to recreate only this
application's own item, then verify unattended access. Do not grant access to
all applications. Bridge packaging uses fixed timestamps to avoid needless
identity changes when only fixtures are rebuilt.

### Updating a running instance

A successful build does not update a running supervisor or its Gateway bridge.
Check the executable in that instance's LaunchAgent `ProgramArguments`; it may
still point to an older installed build.

Stage the rebuilt executable at a new, stable path. From the user's GUI session,
run its `credentials --instance paper --authorize` and then
`credentials --instance paper --check-access` commands before replacing a working
Paper service. If macOS approval is required, leave the current service in place
until unattended access succeeds. Do not replace credentials or grant access to
all applications to avoid that approval.

Once authorized, stop only Paper, uninstall only its service, then run
`service install --instance paper` and `start --instance paper` with the staged
executable. The new LaunchAgent uses that executable's path. Verify `ready` and a
read-only client request afterward. Keep Live on its existing executable and
service unless its upgrade was also requested; do not overwrite a shared,
in-use executable or restart Live to validate a Paper update.

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
| Server-disconnected information notice | Acknowledge the specific notice and continue checking broker recovery |
| Connection-loss dialog requiring a new login | Use a bounded fresh-login attempt; do not click a session-reclaim button |
| Future version-retirement advisory | Acknowledge the known OK-only notice without interrupting the session; plan a Gateway software upgrade |
| Already unsupported version or an unfamiliar upgrade prompt | Inspect and upgrade the installed software; restarting the same version is not a fix |
| Supervisor unreachable | Inspect service errors; stale state is not reported as healthy |

`gatewayctl restart --instance paper` requests a native restart, not an
immediate kill. Gateway's controls operate at minute granularity. The shutdown
allowance starts from the reported scheduled time, while session-resume grace
and UI-stall detection have separate limits.

A failed native restart pauses for inspection instead of discarding a
potentially valid session. Broker disconnects and ambiguous API failures do not
justify repeatedly killing a responsive JVM. Forced recovery of a stalled UI
requires corroborating API failures.

Known connection-loss dialogs must not latch the generic unknown-dialog
intervention state and block the next native restart. A re-login-required
prompt takes the ordinary fresh-login path, where session conflicts and MFA
remain explicit. A disconnect while MFA is active pauses instead of submitting
another login request. Consecutive connection-driven fresh logins stop after
three attempts (or the configured process-restart limit if lower), until a
stable healthy interval or an explicit operator resume. Other messages are
still left for operator review.

### Version retirement warnings

IBKR can display an advance notice saying the installed version "will be
desupported" on a future date, including during a broker reconnect or login.
This is not a failed upgrade or proof that the current session is unusable.
Treating that advisory as an unknown modal pauses supervision and can prevent
adoption of the next scheduled native restart.

The bridge acknowledges only the recognized English-language, OK-only future
retirement notice. It never follows the download link, runs an installer, or
dismisses an already-unsupported-version prompt. Session conflicts, MFA, and
unrecognized dialogs retain their normal safeguards. Account and API checks
still determine readiness; acknowledgement alone does not make a session ready.

`gatewayctl status --instance paper` reports `upgrade_recommended: true` after
the advisory, independently of `phase`. The private controller log records an
`upgrade_notice`; if tradebus is configured, a separate human notification is
sent without opening or clearing a session-outage incident. Failed notification
delivery is persisted and retried. Repeated notices and bridge reconnections do
not resend a delivered alert within the same managed launch. A fresh managed
launch clears the status flag unless IBKR repeats the advisory.

Plan an upgrade from IBKR's authorized distribution before the stated cutoff,
or enable the [separate updater](#automatic-vendor-upgrades). Daily restarts
cannot extend support for an obsolete version. Qualify the new installation
with Paper first, then update `gateway_home` during a controlled stop of the
affected instances. Do not replace shared vendor files beneath a running
Gateway. A newer version needs launcher/UI qualification, not just an API
version check.

The macOS installer can create a new versioned directory alongside the old
one. Installation alone does not update `gateway_home`, and a running
supervisor retains the path it loaded at startup. After stopping the affected
instances, unload their LaunchAgents with
`launchctl bootout "gui/$(id -u)/dev.ibkr.gatewayctl.paper"` (replace `paper`
only for another affected instance), edit `gateway_home`, and use
`gatewayctl start --instance paper` to reload the existing service. Keep
intentionally stopped instances stopped. A native Gateway restart or `resume`
does not reload the supervisor's installation path.

When compatible, keep using the existing authorized supervisor executable
for a vendor-only upgrade. Installing new Gateway files does not itself
require rebuilding that executable or changing its Keychain code identity.
Fresh broker authentication can still require MFA.

When updating from a bridge that already latched this advisory, `resume` alone
will encounter it again. Follow [Updating a running instance](#updating-a-running-instance)
to deploy the fixed supervisor and bridge together; building the source alone
does not update the running JVM. The separate updater also recognizes this
specific retirement incident in older supervisors, including when its native
restart was inhibited, and can install a newer release without replacing the
authorized supervisor.

### Automatic vendor upgrades

The updater is opt-in and currently supports **Apple silicon Paper profiles**.
It does not place orders, enroll accounts, change API permissions, or start
stopped instances. Because `gateway_home` is shared, **all other instances
must be verifiably stopped**, including Live. Active siblings defer the upgrade
rather than being restarted or silently switched.

Install the new executable at a separate stable path if the running supervisor
has an older Keychain-authorized code identity. Use that executable for the
commands below; leave the supervisor's LaunchAgent and executable unchanged.
The updater never reads credentials itself: the existing supervisor performs
its own unattended-access check, compatibility inspection, and login.

```sh
gatewayctl upgrade check --instance paper --channel latest
gatewayctl upgrade run --instance paper --channel latest
gatewayctl upgrade install --instance paper --channel latest --at 05:00
gatewayctl upgrade status --instance paper
```

`install` registers and loads `dev.ibkr.gatewayctl.upgrade.paper`. Routine
checks run once successfully per local day in the one-hour window starting at
`--at` (default `05:00`). The job also wakes every 15 minutes to detect an
explicit retirement advisory or observe an upgrade waiting for approval.
A recognized retirement incident can trigger an immediate upgrade outside
the maintenance window. This avoids leaving an older bridge paused until its
next native restart. A sleeping or logged-out Mac cannot provide an uptime
guarantee; a logged-in Aqua session is still required.

Both `latest` and `stable` use fixed official IBKR HTTPS download URLs.
Downloads are bounded and cached using HTTP ETags and Last-Modified timestamps.
Before executing anything,
the updater verifies the installer's code signature against IBKR's Apple
Team ID and requires Gatekeeper acceptance. It does not disable Gatekeeper,
request administrator privileges, accept authentication prompts, or execute
scripts downloaded from other sources.

The verified installer runs with `-q`, an explicit new installation directory,
and a **typed install4j response file** disabling automatic launch, all-user
installation, and administrator privileges. Plain `-V...=false` arguments
are strings and are not suitable for these Boolean variables. This silent
installation path has been exercised with the IBKR 10.51 ARM installer.
The installer's `user.home` is isolated inside the staged build, so vendor
bookkeeping and Desktop shortcuts do not touch the user's ordinary Desktop
or settings. The managed Gateway still uses its existing private instance
settings after launch.
Build identity comes from `fullVersion`, not the app's sometimes-uninformative
`CFBundleShortVersionString`. The updater never downgrades a newer installation.

New installations go under `~/Applications/gatewayctl-upgrades/`. After
compatibility and credential-access preflight, the updater stops only Paper,
snapshots its private settings, updates `gateway_home`, and reloads the
existing supervisor. Success requires sustained supervisor account/API
readiness, verified process ownership, and the correct listening port.
The old installation is kept. A known compatibility failure gets one rollback
to the previous installation and settings; that failed version is then
suppressed until a newer build appears or an operator explicitly supplies
`upgrade run --instance paper --retry-failed`. `--reinstall` permits a deliberate
same-version repair or qualification run; scheduled jobs never use either flag.

MFA, session conflicts, unknown dialogs, and an operator stop **do not cause
automatic login retries or rollback logins**. The candidate is held for
inspection and an alert is recorded. Once an approved candidate becomes ready,
the next updater check can finish the transaction. An interrupted transaction
that cannot be reconciled safely also stops for inspection. Concurrent external
configuration edits are never overwritten during rollback.

State and bounded logs live under `state_dir/.upgrades/<instance>/`; retained
failed-candidate settings are private. The separate upgrade notification
identity is `ibkr-gateway-<instance>-upgrade`, not the trading-session incident.
Delivery failures remain visible in `upgrade status` and are retried on the
next invocation. Without tradebus, monitor the local upgrade status and logs.
The updater does not rebuild itself or the supervisor to bypass Keychain
approval, and broker-required MFA remains a human step.

```sh
gatewayctl upgrade uninstall --instance paper
```

This removes only the updater job, not the running Gateway or its supervisor.
Resolve any pending upgrade transaction first. Old managed installations and
failed-candidate diagnostics are retained rather than automatically deleting
potential rollback evidence.

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
Version-retirement fixtures cover startup, repeated notices, native restart,
MFA preservation, and refusal to dismiss an already-required upgrade.

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

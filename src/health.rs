use crate::config::Instance;
use anyhow::{ensure, Context, Result};
use ibapi::Client;
use std::{fmt, time::Duration};
use tokio::time::timeout;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Issue {
    AccountMismatch,
    UpstreamDisconnected,
    ClientConflict,
    HandshakeUnavailable,
    ApiNotListening,
    Deadline,
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::AccountMismatch => "account_identity_mismatch",
            Self::UpstreamDisconnected => "broker_connectivity_lost",
            Self::ClientConflict => "monitor_client_id_conflict",
            Self::HandshakeUnavailable => "api_handshake_unavailable",
            Self::ApiNotListening => "owned_gateway_api_not_listening",
            Self::Deadline => "api_health_deadline_exceeded",
        })
    }
}
impl std::error::Error for Issue {}

fn notice_issue(code: i32) -> Option<Issue> {
    match code {
        326 => Some(Issue::ClientConflict),
        1100 | 2110 | ibapi::NOTICE_STREAM_LAG_CODE => Some(Issue::UpstreamDisconnected),
        _ => None,
    }
}

pub async fn probe(instance: &Instance) -> Result<()> {
    probe_with_timeout(instance, Duration::from_secs(8)).await
}

async fn probe_with_timeout(instance: &Instance, deadline: Duration) -> Result<()> {
    inspect_accounts(instance, deadline, true).await.map(|_| ())
}

pub async fn discover(instance: &Instance) -> Result<Vec<String>> {
    ensure!(
        !instance.api_orders,
        "account discovery requires a read-only API profile"
    );
    inspect_accounts(instance, Duration::from_secs(8), false).await
}

async fn inspect_accounts(
    instance: &Instance,
    deadline: Duration,
    enforce_binding: bool,
) -> Result<Vec<String>> {
    let connection = timeout(
        deadline,
        Client::builder()
            .address(format!("127.0.0.1:{}", instance.api_port))
            .client_id(instance.monitor_client_id)
            .max_reconnect_attempts(0)
            .channel_capacity(32)
            .connect_with_notice_stream(),
    )
    .await
    .map_err(|_| Issue::HandshakeUnavailable)?
    .map_err(|error| anyhow::Error::new(error).context(Issue::HandshakeUnavailable))?;
    let (client, mut notices) = connection;
    let result = timeout(deadline, async {
        let requests = async {
            let accounts = client.managed_accounts().await?;
            ensure!(
                !accounts.is_empty()
                    && accounts.len() <= 64
                    && accounts.iter().all(|account| !account.is_empty()
                        && account.len() < 64
                        && account.bytes().all(|byte| byte.is_ascii_alphanumeric())),
                "invalid broker account identities"
            );
            ensure!(
                !enforce_binding
                    || (accounts
                        .iter()
                        .all(|a| instance.expected_accounts.contains(a))
                        && instance
                            .expected_accounts
                            .iter()
                            .all(|a| accounts.contains(a))),
                Issue::AccountMismatch
            );
            if !enforce_binding && instance.mode == crate::config::Mode::Live {
                ensure!(
                    !accounts.iter().any(|account| account.starts_with('D')),
                    "a paper account was returned during live enrollment"
                );
            }
            client.server_time().await?;
            Ok::<Vec<String>, anyhow::Error>(accounts)
        };
        tokio::pin!(requests);
        loop {
            tokio::select! {
                biased;
                notice = notices.next() => {
                    let notice = notice.context("API notice channel closed")?;
                    if let Some(issue) = notice_issue(notice.code) {
                        return Err(issue.into());
                    }
                }
                result = &mut requests => return result,
            }
        }
    })
    .await
    .map_err(|_| anyhow::Error::from(Issue::Deadline));
    timeout(Duration::from_secs(2), client.disconnect())
        .await
        .context("API client shutdown timed out")?;
    result?
}

pub async fn verify_port_owner(port: u16, pid: i32) -> Result<()> {
    let output = timeout(
        Duration::from_secs(3),
        tokio::process::Command::new("/usr/sbin/lsof")
            .args([
                "-nP",
                "-a",
                "-p",
                &pid.to_string(),
                &format!("-iTCP:{port}"),
                "-sTCP:LISTEN",
                "-t",
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("socket ownership lookup timed out")??;
    ensure!(
        output.status.success()
            && String::from_utf8(output.stdout)?
                .lines()
                .any(|s| s.trim() == pid.to_string()),
        Issue::ApiNotListening
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
    };

    #[tokio::test]
    async fn enrollment_discovers_authenticated_accounts_only_with_readonly_permissions() {
        let (mut instance, server) = simulated_gateway("UOBSERVED", false).await;
        instance.mode = crate::config::Mode::Live;
        instance.api_orders = false;
        assert_eq!(discover(&instance).await.unwrap(), ["UOBSERVED"]);
        timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn enrollment_rejects_paper_identity_for_live_mode() {
        let (mut instance, server) = simulated_gateway("DUFAKE", false).await;
        instance.mode = crate::config::Mode::Live;
        instance.api_orders = false;
        assert!(discover(&instance)
            .await
            .unwrap_err()
            .to_string()
            .contains("paper account"));
        timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }

    async fn frame(stream: &mut TcpStream, body: &[u8]) {
        stream.write_u32(body.len() as u32).await.unwrap();
        stream.write_all(body).await.unwrap();
    }

    async fn proto(stream: &mut TcpStream, message: i32, payload: &[u8]) {
        let mut body = (message + 200).to_be_bytes().to_vec();
        body.extend_from_slice(payload);
        frame(stream, &body).await;
    }

    fn integer(tag: u64, value: u64) -> Vec<u8> {
        let mut body = Vec::new();
        prost::encoding::encode_varint(tag << 3, &mut body);
        prost::encoding::encode_varint(value, &mut body);
        body
    }

    fn string(tag: u64, value: &str) -> Vec<u8> {
        let mut body = Vec::new();
        prost::encoding::encode_varint((tag << 3) | 2, &mut body);
        prost::encoding::encode_varint(value.len() as u64, &mut body);
        body.extend_from_slice(value.as_bytes());
        body
    }

    async fn read_frame(stream: &mut TcpStream) -> Option<Vec<u8>> {
        let size = stream.read_u32().await.ok()?;
        assert!(size < 65536);
        let mut body = vec![0; size as usize];
        stream.read_exact(&mut body).await.ok()?;
        Some(body)
    }

    async fn simulated_gateway(
        account: &'static str,
        disconnected: bool,
    ) -> (Instance, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut magic = [0; 4];
            stream.read_exact(&mut magic).await.unwrap();
            assert_eq!(&magic, b"API\0");
            read_frame(&mut stream).await.unwrap();
            frame(
                &mut stream,
                b"213\0"
                    .iter()
                    .chain(b"20260924 12:00:00 UTC\0")
                    .copied()
                    .collect::<Vec<_>>()
                    .as_slice(),
            )
            .await;
            let start = read_frame(&mut stream).await.unwrap();
            assert_eq!(i32::from_be_bytes(start[..4].try_into().unwrap()), 271);
            proto(&mut stream, 9, &integer(1, 123)).await;
            proto(&mut stream, 15, &string(1, account)).await;
            while let Some(request) = read_frame(&mut stream).await {
                match i32::from_be_bytes(request[..4].try_into().unwrap()) - 200 {
                    17 => {
                        if disconnected {
                            proto(&mut stream, 4, &integer(3, 1100)).await;
                        }
                        proto(&mut stream, 15, &string(1, account)).await;
                    }
                    49 => proto(&mut stream, 49, &integer(1, 1790265600)).await,
                    other => panic!(
                        "health probe made unexpected request {other}; no orders are allowed"
                    ),
                }
            }
        });
        let config: crate::config::Config =
            toml::from_str(include_str!("../config.example.toml")).unwrap();
        let mut instance = config.instances["paper"].clone();
        instance.api_port = port;
        instance.expected_accounts = vec!["DUFAKE".into()];
        (instance, server)
    }

    #[tokio::test]
    async fn validates_account_and_heartbeat_without_trading_requests() {
        let (instance, server) = simulated_gateway("DUFAKE", false).await;
        probe_with_timeout(&instance, Duration::from_secs(2))
            .await
            .unwrap();
        timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn rejects_wrong_account() {
        let (instance, server) = simulated_gateway("UOTHER", false).await;
        let error = probe_with_timeout(&instance, Duration::from_secs(2))
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<Issue>(),
            Some(&Issue::AccountMismatch),
            "{error:#}"
        );
        timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn broker_loss_is_not_a_healthy_local_socket() {
        let (instance, server) = simulated_gateway("DUFAKE", true).await;
        let error = probe_with_timeout(&instance, Duration::from_secs(2))
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<Issue>(),
            Some(&Issue::UpstreamDisconnected),
            "{error:#}"
        );
        timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
    }
}

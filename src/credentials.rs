use anyhow::{ensure, Context, Result};
use std::io::{IsTerminal, Read};
use zeroize::Zeroizing;

const SERVICE: &str = "dev.ibkr.gatewayctl";

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    pub username: Zeroizing<String>,
    pub password: Zeroizing<String>,
}

impl Credentials {
    pub fn redact(&self, value: &str, accounts: &[String]) -> Zeroizing<String> {
        let lower = Zeroizing::new(self.username.to_lowercase());
        let upper = Zeroizing::new(self.username.to_uppercase());
        let mut secrets: Vec<&str> = vec![&self.password, &self.username, &lower, &upper];
        secrets.extend(accounts.iter().map(String::as_str));
        secrets.sort_unstable_by_key(|secret| std::cmp::Reverse(secret.len()));
        let mut result = Zeroizing::new(value.to_owned());
        for secret in secrets.into_iter().filter(|s| !s.is_empty()) {
            *result = result.replace(secret, "[redacted]");
        }
        result
    }
}

#[cfg(target_os = "macos")]
pub fn load(instance: &str) -> Result<Credentials> {
    use security_framework::passwords::get_generic_password;
    let _interaction =
        security_framework::os::macos::keychain::SecKeychain::disable_user_interaction()
            .context("disable Keychain prompts for unattended operation")?;
    let bytes = Zeroizing::new(
        get_generic_password(SERVICE, instance)
            .context("Keychain item unavailable; run credentials in an interactive session")?,
    );
    let credentials: Credentials = serde_json::from_slice(&bytes).map_err(|_| {
        anyhow::anyhow!("invalid Keychain credential format; set credentials again")
    })?;
    ensure!(
        !credentials.username.is_empty() && !credentials.password.is_empty(),
        "empty Keychain credentials"
    );
    Ok(credentials)
}

#[cfg(target_os = "macos")]
pub fn set(instance: &str, replace: bool) -> Result<()> {
    let username = Zeroizing::new(rpassword::prompt_password("IBKR username (hidden): ")?);
    let password = Zeroizing::new(rpassword::prompt_password("IBKR password (hidden): ")?);
    store(instance, Credentials { username, password }, replace)
}

pub fn read_stdin() -> Result<Zeroizing<Vec<u8>>> {
    ensure!(
        !std::io::stdin().is_terminal(),
        "pipe JSON into --stdin; do not type credentials into an echoing terminal"
    );
    let mut bytes = Zeroizing::new(Vec::new());
    std::io::stdin().take(65537).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 65536,
        "stdin configuration exceeds the size limit"
    );
    Ok(bytes)
}

pub fn import_stdin(instance: &str, replace: bool) -> Result<()> {
    let bytes = read_stdin()?;
    let credentials = serde_json::from_slice(&bytes).map_err(|_| {
        anyhow::anyhow!("expected credential JSON with username and password strings")
    })?;
    store(instance, credentials, replace)
}

#[cfg(target_os = "macos")]
fn store(instance: &str, credentials: Credentials, replace: bool) -> Result<()> {
    use security_framework::passwords::{delete_generic_password, set_generic_password};
    ensure!(
        !credentials.username.is_empty() && !credentials.password.is_empty(),
        "credentials cannot be empty"
    );
    let encoded = Zeroizing::new(serde_json::to_vec(&credentials)?);
    if replace {
        match delete_generic_password(SERVICE, instance) {
            Ok(()) => {}
            Err(error) if error.code() == -25300 => {} // errSecItemNotFound
            Err(error) => return Err(error).context("replace this application's Keychain item"),
        }
    }
    set_generic_password(SERVICE, instance, &encoded).context("store Keychain credential pair")?;
    let retrieved = load(instance).context(
        "verify unattended Keychain access after import; after replacing the executable, re-import with --replace to register its new code identity",
    )?;
    ensure!(
        retrieved.username == credentials.username && retrieved.password == credentials.password,
        "Keychain credential verification failed"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diagnostics_redact_overlapping_credentials_before_shorter_prefixes() {
        let credentials = Credentials {
            username: Zeroizing::new("fixtureuser".into()),
            password: Zeroizing::new("fixtureuser-longer".into()),
        };
        let result =
            credentials.redact("fixtureuser-longer FIXTUREUSER U12345", &["U12345".into()]);
        assert_eq!(result.as_str(), "[redacted] [redacted] [redacted]");
    }
}

//! A [`DeviceConnector`] that routes connections through a jumphost when configured.
//!
//! When jumphost settings are present in [`DeviceCredentials`], the connector:
//! 1. SSHs to the jumphost with jumphost credentials
//! 2. Runs the command template on the jumphost shell (substituting device credentials)
//! 3. Returns the resulting session to the target device
//!
//! When no jumphost is configured, falls back to direct SSH.

use std::time::Duration;

use async_trait::async_trait;
use tracing::info;

use crate::state::DeviceCredentials;

/// A [`aycfgapply::connector::DeviceConnector`] that supports optional jumphost routing.
pub struct JumphostConnector {
    pub jumphost_address: String,
    pub jumphost_username: String,
    pub jumphost_password: String,
    pub jumphost_command: String,
}

impl JumphostConnector {
    /// Create from device credentials. Returns a jumphost-aware connector
    /// if jumphost is configured, or a direct connector if not.
    pub fn from_credentials(creds: &DeviceCredentials) -> Self {
        Self {
            jumphost_address: creds.jumphost_address.clone(),
            jumphost_username: creds.jumphost_username.clone(),
            jumphost_password: creds.jumphost_password.clone(),
            jumphost_command: creds.jumphost_command.clone(),
        }
    }

    fn has_jumphost(&self) -> bool {
        !self.jumphost_address.is_empty() && !self.jumphost_command.is_empty()
    }

    fn build_ssh_command(&self, target_ip: &str, username: &str) -> String {
        self.jumphost_command
            .replace("{username}", username)
            .replace("{target_ip}", target_ip)
    }
}

#[async_trait]
impl aycfgapply::connector::DeviceConnector for JumphostConnector {
    async fn connect(
        &self,
        target: &str,
        conntype: aycfgapply::cli::ConnectionType,
        username: &str,
        password: &str,
        connect_timeout: Duration,
        cmd_timeout: Duration,
    ) -> aycfgapply::connector::Result<Box<dyn aycfgapply::connector::DeviceSession>> {
        let conn = if self.has_jumphost() {
            // Extract the IP from the target (may be "ip:port" or "[ipv6]:port")
            let ip = target
                .strip_prefix('[')
                .and_then(|s| s.split(']').next())
                .unwrap_or_else(|| target.split(':').next().unwrap_or(target));

            let ssh_command = self.build_ssh_command(ip, username);
            let jump_target = crate::state::ssh_target(&self.jumphost_address, 22);
            let jump_addr: std::net::SocketAddr = jump_target.parse()
                .map_err(|e| -> aycfgapply::connector::ConnectorError {
                    format!("invalid jumphost address '{}': {}", jump_target, e).into()
                })?;

            info!(
                jumphost = %self.jumphost_address,
                command = %ssh_command,
                target = %target,
                "Connecting via jumphost (aycfgapply connector)"
            );

            let jumphost_template = format!(
                r#"Value Preset DevicePassword ()

Start
  ^.*[\$#>]\s* -> Send "{ssh_command}" WaitPassword

WaitPassword
  ^[Pp]assword:\s* -> Send ${{DevicePassword}} WaitPrompt
  ^.*# -> Send "terminal length 0" TermLen
  ^.*> -> Send "terminal length 0" TermLen

WaitPrompt
  ^.*# -> Send "terminal length 0" TermLen
  ^.*> -> Send "terminal length 0" TermLen
  ^.*refused.* -> Error "connection refused"
  ^.*denied.* -> Error "permission denied"

TermLen
  ^.*# -> Done
  ^.*> -> Done
"#,
                ssh_command = ssh_command,
            );

            let hops = vec![
                ayclic::Hop::Transport(ayclic::TransportSpec::Ssh {
                    target: jump_addr,
                    auth: ayclic::SshAuth::Password {
                        username: self.jumphost_username.clone(),
                        password: self.jumphost_password.clone(),
                    },
                    source: None,
                }),
                ayclic::Hop::Interactive(
                    aytextfsmplus::TextFSMPlus::from_str(&jumphost_template)
                        .with_preset("DevicePassword", password),
                ),
            ];

            let path = ayclic::ConnectionPath::new(hops).with_timeout(connect_timeout);
            ayclic::CiscoIosConn::from_path(
                path,
                target,
                &aytextfsmplus::NoVars,
                &aytextfsmplus::NoFuncs,
            )
            .await
            .map_err(|e| -> aycfgapply::connector::ConnectorError { Box::new(e) })?
        } else {
            let ayclic_conntype = aycfgapply::cisco_connector::map_connection_type(&conntype);
            ayclic::CiscoIosConn::with_timeouts(
                target,
                ayclic_conntype,
                username,
                password,
                connect_timeout,
                cmd_timeout,
            )
            .await
            .map_err(|e| -> aycfgapply::connector::ConnectorError { Box::new(e) })?
        };

        Ok(Box::new(JumphostSession { conn }))
    }
}

/// A [`DeviceSession`] wrapping a live `ayclic::CiscoIosConn`.
struct JumphostSession {
    conn: ayclic::CiscoIosConn,
}

#[async_trait]
impl aycfgapply::connector::DeviceSession for JumphostSession {
    async fn run_cmd(&mut self, cmd: &str) -> aycfgapply::connector::Result<String> {
        self.conn
            .run_cmd(cmd)
            .await
            .map_err(|e| -> aycfgapply::connector::ConnectorError { Box::new(e) })
    }

    async fn config_atomic(
        &mut self,
        config: &str,
        safety: aycfgapply::connector::ChangeSafety,
    ) -> aycfgapply::connector::Result<String> {
        let ayclic_safety = aycfgapply::cisco_connector::map_change_safety(&safety);
        self.conn
            .config_atomic(config, ayclic_safety)
            .await
            .map_err(|e| -> aycfgapply::connector::ConnectorError { Box::new(e) })
    }

    async fn disconnect(&mut self) -> aycfgapply::connector::Result<()> {
        self.conn
            .disconnect()
            .await
            .map_err(|e| -> aycfgapply::connector::ConnectorError { Box::new(e) })
    }
}

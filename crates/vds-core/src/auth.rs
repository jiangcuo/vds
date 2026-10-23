//! Credentials, expiry and one-time passwords (QMP `set_password` / `expire_password`).

use std::time::SystemTime;

use crate::error::{Error, Result};
use crate::secret::Secret;

/// How clients authenticate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AuthMode {
    /// TLS without any password check (`-rdp auth=none`, also used by `add_client skipauth`).
    None,
    /// TLS; the password is taken from the Client Info PDU.
    Tls,
    /// TLS + CredSSP/NTLM (NLA). Default.
    #[default]
    Nla,
}

impl AuthMode {
    /// Parses `none`, `tls` or `nla`.
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "none" => Ok(Self::None),
            "tls" => Ok(Self::Tls),
            "nla" => Ok(Self::Nla),
            other => Err(Error::InvalidConfig(format!("unknown auth mode `{other}`"))),
        }
    }

    /// Name used in logs and QMP.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Tls => "tls",
            Self::Nla => "nla",
        }
    }
}

/// What to do with connected clients when the password changes (QMP `connected`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnectedAction {
    /// Keep connected clients.
    #[default]
    Keep,
    /// Refuse the password change if a client is connected.
    Fail,
    /// Disconnect connected clients.
    Disconnect,
}

/// A username/password pair.
#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    /// User name (may be logged).
    pub username: String,
    /// Password (never logged).
    pub password: Secret<String>,
    /// Optional domain.
    pub domain: Option<String>,
}

impl core::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Credentials")
            .field("username", &self.username)
            .field("domain", &self.domain)
            .finish_non_exhaustive()
    }
}

/// Outcome of [`CredentialStore::decision`] for a new connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessDecision {
    /// No authentication required.
    Open,
    /// The client must present these credentials.
    Check(Credentials),
    /// Refuse the connection.
    Denied(&'static str),
}

/// The current credentials and their validity.
#[derive(Debug)]
pub struct CredentialStore {
    mode: AuthMode,
    creds: Option<Credentials>,
    expires_at: Option<SystemTime>,
    one_time: bool,
    consumed: bool,
}

impl CredentialStore {
    /// Creates a store without credentials.
    pub fn new(mode: AuthMode) -> Self {
        Self {
            mode,
            creds: None,
            expires_at: None,
            one_time: false,
            consumed: false,
        }
    }

    /// Authentication mode.
    pub fn mode(&self) -> AuthMode {
        self.mode
    }

    /// Replaces the credentials. Resets the expiry to "never" and the one-time state;
    /// call [`set_expiry`](Self::set_expiry) afterwards to limit validity.
    pub fn set(&mut self, creds: Option<Credentials>, one_time: bool) {
        self.creds = creds;
        self.one_time = one_time;
        self.consumed = false;
        self.expires_at = None;
    }

    /// Sets the expiry time; `None` means never.
    pub fn set_expiry(&mut self, expires_at: Option<SystemTime>) {
        self.expires_at = expires_at;
    }

    /// Returns `true` if a password is configured.
    pub fn has_password(&self) -> bool {
        self.creds.is_some()
    }

    /// Decides how a new connection is authenticated.
    pub fn decision(&self, now: SystemTime) -> AccessDecision {
        if self.mode == AuthMode::None {
            return AccessDecision::Open;
        }
        let Some(creds) = &self.creds else {
            return AccessDecision::Denied("no password set");
        };
        if self.expires_at.is_some_and(|t| t <= now) {
            return AccessDecision::Denied("password expired");
        }
        if self.one_time && self.consumed {
            return AccessDecision::Denied("one-time password already used");
        }
        AccessDecision::Check(creds.clone())
    }

    /// Records a successful authentication; invalidates one-time credentials.
    pub fn consume(&mut self) {
        if self.one_time {
            self.consumed = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::{AccessDecision, AuthMode, CredentialStore, Credentials};
    use crate::secret::Secret;

    fn creds() -> Credentials {
        Credentials {
            username: String::from("u"),
            password: Secret::new(String::from("p")),
            domain: None,
        }
    }

    #[test]
    fn none_mode_is_open() {
        let s = CredentialStore::new(AuthMode::None);
        assert_eq!(s.decision(SystemTime::now()), AccessDecision::Open);
    }

    #[test]
    fn no_password_denies() {
        let s = CredentialStore::new(AuthMode::Nla);
        assert!(matches!(s.decision(SystemTime::now()), AccessDecision::Denied(_)));
    }

    #[test]
    fn expiry_is_enforced() {
        let now = SystemTime::now();
        let mut s = CredentialStore::new(AuthMode::Nla);
        s.set(Some(creds()), false);
        s.set_expiry(Some(now + Duration::from_secs(60)));
        assert!(matches!(s.decision(now), AccessDecision::Check(_)));
        assert!(matches!(
            s.decision(now + Duration::from_secs(61)),
            AccessDecision::Denied("password expired")
        ));
    }

    #[test]
    fn setting_password_resets_expiry() {
        let now = SystemTime::now();
        let mut s = CredentialStore::new(AuthMode::Tls);
        s.set(Some(creds()), false);
        s.set_expiry(Some(now));
        s.set(Some(creds()), false);
        assert!(matches!(s.decision(now), AccessDecision::Check(_)));
    }

    #[test]
    fn one_time_is_consumed() {
        let mut s = CredentialStore::new(AuthMode::Nla);
        s.set(Some(creds()), true);
        assert!(matches!(s.decision(SystemTime::now()), AccessDecision::Check(_)));
        s.consume();
        assert!(matches!(s.decision(SystemTime::now()), AccessDecision::Denied(_)));
        s.set(Some(creds()), true);
        assert!(matches!(s.decision(SystemTime::now()), AccessDecision::Check(_)));
    }

    #[test]
    fn credentials_debug_hides_password() {
        let dbg = format!("{:?}", creds());
        assert!(!dbg.contains("\"p\""));
    }
}

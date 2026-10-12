//! The app token's store (r4.s3.w2, G5): the ONE port the client reads, writes
//! and deletes the app token through, the in-memory fake the suites drive, and
//! the audit sink each operation's single log line goes to.
//!
//! The token is the Keychain item `PulseTrader`/`server_app_token` (G1 — one
//! item for the one server). **Absence and failure are different outcomes**:
//! [`TokenStore::read`] answers `Ok(None)` for "no item" and `Err(reason)` for
//! "the store refused or errored", because start-up treats them differently —
//! an absent item means the trader connects again (`Down`), a store error
//! starts `refused` with the named reason.
//!
//! No method here returns or logs a VALUE: a reason names the item and the
//! underlying error, never the token (the risk gate's redaction rule). The
//! production adapter is [`crate::adapters::secrets`]'s `KeychainTokenStore`;
//! tests inject [`InMemoryTokenStore`] (ADR-0015: a port, an adapter, a fake).

use std::sync::{Arc, Mutex};

/// The app token's store: read, write, delete — nothing else.
pub trait TokenStore: Send + Sync {
    /// The stored token.
    ///
    /// `Ok(None)` is "the item does not exist" — the ordinary case for a fresh
    /// install. `Err(reason)` is a store that refused or errored; the reason
    /// names the item, never the value.
    ///
    /// # Errors
    ///
    /// The platform store could not be reached or refused the read.
    fn read(&self) -> Result<Option<String>, String>;

    /// Store (or overwrite) the token.
    ///
    /// # Errors
    ///
    /// The platform store could not be reached or refused the write. The
    /// reason never contains the value.
    fn write(&self, token: &str) -> Result<(), String>;

    /// Remove the item. An absent item is already the goal — `Ok(())`.
    ///
    /// # Errors
    ///
    /// The platform store could not be reached or refused the delete.
    fn delete(&self) -> Result<(), String>;
}

/// Where each token-store operation's ONE audit line goes — the risk gate's
/// audit-trail control ("each Keychain write / migration / delete logs one line
/// naming `PulseTrader`/`server_app_token` and the outcome, never the value").
pub trait TokenAudit: Send + Sync {
    /// Emit one already-formatted line.
    fn line(&self, line: &str);
}

/// The production sink: one line on stderr, the app's log channel.
#[derive(Debug, Default)]
pub struct StderrAudit;

impl TokenAudit for StderrAudit {
    fn line(&self, line: &str) {
        eprintln!("{line}");
    }
}

/// The client's injectable dependencies: the token store, and the audit sink
/// its operations log through.
#[derive(Clone)]
pub struct ClientWiring {
    /// The token store the client reaches the app token through.
    pub store: Arc<dyn TokenStore>,
    /// The sink for the one audit line per write / migration / delete.
    pub audit: Arc<dyn TokenAudit>,
}

impl ClientWiring {
    /// The app's own wiring: the Keychain adapter and stderr.
    #[must_use]
    pub fn production() -> Self {
        Self {
            store: Arc::new(crate::adapters::secrets::KeychainTokenStore),
            audit: Arc::new(StderrAudit),
        }
    }

    /// A wiring over `store`, with the production audit sink — the seam for a
    /// caller that only needs to swap the STORE (the integration suites' fake).
    #[must_use]
    pub fn with_store(store: Arc<dyn TokenStore>) -> Self {
        Self {
            store,
            audit: Arc::new(StderrAudit),
        }
    }
}

/// The in-memory fake the suites drive (ADR-0015). `read` answers the stored
/// item — or, when [`InMemoryTokenStore::answer_reads_with`] is set, a value of
/// the test's choosing (the mismatched-read-back case) — and each operation can
/// be made to fail by name, so every branch of the start-up rule is reachable
/// without a Keychain.
#[derive(Debug, Default)]
pub struct InMemoryTokenStore {
    inner: Mutex<MockItem>,
}

#[derive(Debug, Default)]
struct MockItem {
    item: Option<String>,
    read_value: Option<String>,
    fail_read: Option<String>,
    fail_write: Option<String>,
    fail_delete: Option<String>,
}

impl InMemoryTokenStore {
    /// A store already holding `token` — a relaunch after a stored connect.
    #[must_use]
    pub fn with_token(token: &str) -> Self {
        let store = Self::default();
        store.lock().item = Some(token.to_owned());
        store
    }

    /// What the store holds, for the test's assertions.
    #[must_use]
    pub fn token(&self) -> Option<String> {
        self.lock().item.clone()
    }

    /// Make reads answer `value` instead of the stored item (a store whose
    /// read-back does not match what was written).
    pub fn answer_reads_with(&self, value: &str) {
        self.lock().read_value = Some(value.to_owned());
    }

    /// Make every read fail with `reason`.
    pub fn fail_reads(&self, reason: &str) {
        self.lock().fail_read = Some(reason.to_owned());
    }

    /// Make every write fail with `reason`.
    pub fn fail_writes(&self, reason: &str) {
        self.lock().fail_write = Some(reason.to_owned());
    }

    /// Make every delete fail with `reason`.
    pub fn fail_deletes(&self, reason: &str) {
        self.lock().fail_delete = Some(reason.to_owned());
    }

    /// The guard, adopting a poisoned one — the lib convention: a panic in a
    /// holder must not turn into a permanently poisoned store.
    fn lock(&self) -> std::sync::MutexGuard<'_, MockItem> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl TokenStore for InMemoryTokenStore {
    fn read(&self) -> Result<Option<String>, String> {
        let item = self.lock();
        if let Some(reason) = &item.fail_read {
            return Err(reason.clone());
        }
        Ok(item.read_value.clone().or_else(|| item.item.clone()))
    }

    fn write(&self, token: &str) -> Result<(), String> {
        let mut item = self.lock();
        if let Some(reason) = &item.fail_write {
            return Err(reason.clone());
        }
        item.item = Some(token.to_owned());
        Ok(())
    }

    fn delete(&self) -> Result<(), String> {
        let mut item = self.lock();
        if let Some(reason) = &item.fail_delete {
            return Err(reason.clone());
        }
        item.item = None;
        Ok(())
    }
}

use std::sync::{Arc, LazyLock};

use cove_cspp::CsppStore as _;
use cove_util::ResultExt as _;
use flume::{Receiver, Sender};
use parking_lot::RwLock;
use rand::RngExt as _;
use std::str::FromStr as _;
use strum::IntoEnumIterator as _;
use tracing::{error, info, warn};
use zeroize::Zeroizing;

use cove_cspp::backup_data::{
    BackupManifest, DescriptorPair, MANIFEST_RECORD_ID, MASTER_KEY_RECORD_ID, WalletEntry,
    WalletMode, WalletSecret, wallet_record_id,
};
use cove_cspp::master_key_crypto;
use cove_cspp::wallet_crypto;
use cove_device::cloud_storage::{CloudStorage, CloudStorageError};
use cove_device::keychain::Keychain;
use cove_device::passkey::PasskeyAccess;
use cove_types::network::Network;

use crate::database::Database;
use crate::database::global_config::CloudBackup;
use crate::wallet::metadata::{WalletMetadata, WalletMode as LocalWalletMode, WalletType};

const RP_ID: &str = "covebitcoinwallet.com";
const CREDENTIAL_ID_KEY: &str = "cspp::v1::credential_id";
const PRF_SALT_KEY: &str = "cspp::v1::prf_salt";

type Message = CloudBackupReconcileMessage;

pub static CLOUD_BACKUP_MANAGER: LazyLock<Arc<RustCloudBackupManager>> =
    LazyLock::new(RustCloudBackupManager::init);

#[derive(Debug, Clone, Hash, Eq, PartialEq, uniffi::Enum)]
pub enum CloudBackupState {
    Disabled,
    Enabling,
    Restoring,
    Enabled,
    Error(String),
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum CloudBackupReconcileMessage {
    StateChanged(CloudBackupState),
    ProgressUpdated { completed: u32, total: u32 },
    EnableComplete,
    RestoreComplete(CloudBackupRestoreReport),
    SyncFailed(String),
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct CloudBackupRestoreReport {
    pub wallets_restored: u32,
    pub wallets_failed: u32,
    pub failed_wallet_errors: Vec<String>,
}

#[derive(Debug, Clone, Hash, Eq, PartialEq, uniffi::Enum)]
pub enum CloudBackupWalletStatus {
    BackedUp,
    NotBackedUp,
    DeletedFromDevice,
}

#[derive(Debug, Clone, Hash, Eq, PartialEq, uniffi::Record)]
pub struct CloudBackupWalletItem {
    pub name: String,
    pub network: Network,
    pub wallet_mode: LocalWalletMode,
    pub wallet_type: WalletType,
    pub fingerprint: Option<String>,
    pub status: CloudBackupWalletStatus,
    /// Cloud record ID, only set for cloud-only wallets
    pub record_id: Option<String>,
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum CloudBackupDetailResult {
    Success(CloudBackupDetail),
    AccessError(String),
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct CloudBackupDetail {
    pub last_sync: Option<u64>,
    pub backed_up: Vec<CloudBackupWalletItem>,
    pub not_backed_up: Vec<CloudBackupWalletItem>,
    /// Number of wallets in the cloud that aren't on this device
    pub cloud_only_count: u32,
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum DeepVerificationResult {
    Verified(DeepVerificationReport),
    UserCancelled(Option<CloudBackupDetail>),
    NotEnabled,
    Failed(DeepVerificationFailure),
}

#[derive(Debug, Clone, uniffi::Record)]
pub struct DeepVerificationReport {
    /// Cloud master key PRF wrapping was repaired
    pub master_key_wrapper_repaired: bool,
    /// Local keychain was repaired from verified cloud master key
    pub local_master_key_repaired: bool,
    /// credential_id was recovered via discoverable auth
    pub credential_recovered: bool,
    pub wallets_verified: u32,
    pub wallets_failed: u32,
    /// Wallet backups with unsupported version (newer format, skipped)
    pub wallets_unsupported: u32,
    /// May be None if manifest was missing but master key verified
    pub detail: Option<CloudBackupDetail>,
}

#[derive(Debug, Clone, uniffi::Enum)]
pub enum DeepVerificationFailure {
    /// Transient iCloud/network/passkey error — safe to retry
    Retry { message: String, detail: Option<CloudBackupDetail> },
    /// Manifest missing, master key verified intact — recreate from local wallets
    RecreateManifest { message: String, detail: Option<CloudBackupDetail>, warning: String },
    /// No verified cloud or local master key available — full re-enable needed
    ReinitializeBackup { message: String, detail: Option<CloudBackupDetail>, warning: String },
    /// Backup uses a newer format — do not overwrite
    UnsupportedVersion { message: String, detail: Option<CloudBackupDetail> },
}

#[derive(Debug, thiserror::Error)]
enum CloudBackupError {
    #[error("not supported: {0}")]
    NotSupported(String),

    #[error("passkey error: {0}")]
    Passkey(String),

    #[error("crypto error: {0}")]
    Crypto(String),

    #[error("cloud storage error: {0}")]
    Cloud(String),

    #[error("internal error: {0}")]
    Internal(String),
}

#[uniffi::export(callback_interface)]
pub trait CloudBackupManagerReconciler: Send + Sync + std::fmt::Debug + 'static {
    fn reconcile(&self, message: CloudBackupReconcileMessage);
}

#[derive(Clone, Debug, uniffi::Object)]
pub struct RustCloudBackupManager {
    #[allow(dead_code)]
    pub state: Arc<RwLock<CloudBackupState>>,
    pub reconciler: Sender<Message>,
    pub reconcile_receiver: Arc<Receiver<Message>>,
}

impl RustCloudBackupManager {
    fn init() -> Arc<Self> {
        let (sender, receiver) = flume::bounded(1000);

        Self {
            state: Arc::new(RwLock::new(CloudBackupState::Disabled)),
            reconciler: sender,
            reconcile_receiver: Arc::new(receiver),
        }
        .into()
    }

    fn send(&self, message: Message) {
        if let Message::StateChanged(state) = &message {
            *self.state.write() = state.clone();
        }

        if let Err(e) = self.reconciler.send(message) {
            error!("unable to send cloud backup message: {e:?}");
        }
    }
}

#[uniffi::export]
impl RustCloudBackupManager {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        CLOUD_BACKUP_MANAGER.clone()
    }

    pub fn listen_for_updates(&self, reconciler: Box<dyn CloudBackupManagerReconciler>) {
        let reconcile_receiver = self.reconcile_receiver.clone();

        std::thread::spawn(move || {
            while let Ok(field) = reconcile_receiver.recv() {
                reconciler.reconcile(field);
            }
        });
    }

    pub fn current_state(&self) -> CloudBackupState {
        self.state.read().clone()
    }

    /// Number of wallets in the cloud backup
    pub fn backup_wallet_count(&self) -> Option<u32> {
        let db = Database::global();
        match db.global_config.cloud_backup() {
            CloudBackup::Enabled { wallet_count: Some(count), .. }
            | CloudBackup::Unverified { wallet_count: Some(count), .. } => Some(count),
            CloudBackup::Enabled { wallet_count: None, last_sync }
            | CloudBackup::Unverified { wallet_count: None, last_sync } => {
                // backfill for DBs that predate the wallet_count field
                let count = count_all_wallets(&db);
                let _ = db.global_config.set_cloud_backup(&CloudBackup::Enabled {
                    last_sync,
                    wallet_count: Some(count),
                });
                Some(count)
            }
            CloudBackup::Disabled => None,
        }
    }

    /// Read persisted cloud backup state from DB and update in-memory state
    ///
    /// Called after bootstrap completes so the UI reflects the correct state
    /// even before the reconciler has delivered its first message
    pub fn sync_persisted_state(&self) {
        let db_state = Database::global().global_config.cloud_backup();
        let mut state = self.state.write();

        if matches!(*state, CloudBackupState::Disabled) {
            let new_state = match db_state {
                CloudBackup::Enabled { .. } | CloudBackup::Unverified { .. } => {
                    CloudBackupState::Enabled
                }
                CloudBackup::Disabled => CloudBackupState::Disabled,
            };

            if *state != new_state {
                *state = new_state.clone();
                drop(state);
                self.send(Message::StateChanged(new_state));
            }
        }
    }

    /// Cloud backup detail for the detail screen
    ///
    /// Returns None if cloud backup is disabled. Compares cached wallet list
    /// against current local wallets to determine backup status
    /// Check if cloud backup is enabled, used as nav guard
    pub fn is_cloud_backup_enabled(&self) -> bool {
        let db = Database::global();
        matches!(
            db.global_config.cloud_backup(),
            CloudBackup::Enabled { .. } | CloudBackup::Unverified { .. }
        )
    }

    /// Download the cloud manifest and build detail from it as source of truth
    ///
    /// Returns None if disabled. On NotFound, re-uploads all wallets automatically.
    /// On other errors, returns AccessError so the UI can offer a re-upload button
    pub fn refresh_cloud_backup_detail(&self) -> Option<CloudBackupDetailResult> {
        if !matches!(*self.state.read(), CloudBackupState::Enabled) {
            return None;
        }

        let cloud = CloudStorage::global();
        let manifest_json = match cloud.download_manifest() {
            Ok(json) => json,
            Err(CloudStorageError::NotFound(_)) => {
                info!("Manifest not found, re-uploading all wallets");
                if let Err(e) = self.do_reupload_all_wallets() {
                    return Some(CloudBackupDetailResult::AccessError(format!(
                        "Failed to re-upload wallets: {e}"
                    )));
                }
                match cloud.download_manifest() {
                    Ok(json) => json,
                    Err(e) => return Some(CloudBackupDetailResult::AccessError(e.to_string())),
                }
            }
            Err(e) => return Some(CloudBackupDetailResult::AccessError(e.to_string())),
        };

        let manifest: BackupManifest = match serde_json::from_slice(&manifest_json) {
            Ok(m) => m,
            Err(e) => {
                return Some(CloudBackupDetailResult::AccessError(format!(
                    "Failed to parse manifest: {e}"
                )));
            }
        };

        Some(CloudBackupDetailResult::Success(build_detail_from_manifest(&manifest)))
    }

    /// Download and decrypt wallets that are in the cloud but not on this device
    pub fn fetch_cloud_only_wallets(&self) -> Vec<CloudBackupWalletItem> {
        let result = self.do_fetch_cloud_only_wallets();
        match result {
            Ok(items) => items,
            Err(e) => {
                error!("Failed to fetch cloud-only wallets: {e}");
                Vec::new()
            }
        }
    }

    /// Restore a single cloud-only wallet to this device
    ///
    /// Returns None on success, Some(error) on failure
    pub fn restore_cloud_wallet(&self, record_id: String) -> Option<String> {
        self.do_restore_cloud_wallet(&record_id).err().map(|e| e.to_string())
    }

    /// Delete a single wallet backup from iCloud and remove it from the manifest
    ///
    /// Returns None on success, Some(error) on failure
    pub fn delete_cloud_wallet(&self, record_id: String) -> Option<String> {
        self.do_delete_cloud_wallet(&record_id).err().map(|e| e.to_string())
    }

    /// Sync any local wallets that aren't in the cloud backup yet
    ///
    /// Called from the iOS sync button on the detail screen
    pub fn sync_unsynced_wallets(&self) {
        if !matches!(*self.state.read(), CloudBackupState::Enabled) {
            return;
        }

        self.send(Message::StateChanged(CloudBackupState::Enabling));

        let this = CLOUD_BACKUP_MANAGER.clone();
        cove_tokio::task::spawn_blocking(move || {
            if let Err(e) = this.do_sync_unsynced_wallets() {
                error!("Cloud backup sync failed: {e}");
                this.send(Message::SyncFailed(e.to_string()));
            }
            this.send(Message::StateChanged(CloudBackupState::Enabled));
        });
    }

    /// Deep verification of cloud backup integrity
    ///
    /// Proves the passkey PRF can unwrap the cloud master key, proves the
    /// master key can decrypt wallet backups, and auto-repairs recoverable
    /// issues (PRF wrapper, local keychain)
    pub fn deep_verify_cloud_backup(&self) -> DeepVerificationResult {
        if !matches!(*self.state.read(), CloudBackupState::Enabled) {
            return DeepVerificationResult::NotEnabled;
        }

        let result = match self.do_deep_verify_cloud_backup() {
            Ok(result) => result,
            Err(e) => {
                error!("Deep verification unexpected error: {e}");
                DeepVerificationResult::Failed(DeepVerificationFailure::Retry {
                    message: e.to_string(),
                    detail: None,
                })
            }
        };

        self.persist_verification_result(&result);
        result
    }

    /// Whether the persisted cloud backup state is unverified
    pub fn is_cloud_backup_unverified(&self) -> bool {
        matches!(Database::global().global_config.cloud_backup(), CloudBackup::Unverified { .. })
    }

    /// Create a new passkey and re-wrap the local master key for cloud storage
    ///
    /// Recovery action for when the original passkey has been deleted. Proves the
    /// local master key can decrypt at least one cloud wallet, then creates a new
    /// passkey, re-encrypts the wrapper, and uploads it. Returns None on success,
    /// Some(error) on failure
    pub fn repair_passkey_wrapper(&self) -> Option<String> {
        self.do_repair_passkey_wrapper().err().map(|e| e.to_string())
    }

    /// Re-upload all local wallets and create a fresh manifest
    ///
    /// Called from the UI when the manifest can't be downloaded (e.g. container mismatch).
    /// Returns None on success, Some(error) on failure
    pub fn reupload_all_wallets(&self) -> Option<String> {
        self.do_reupload_all_wallets().err().map(|e| e.to_string())
    }

    /// Background startup health check for cloud backup integrity
    ///
    /// Verifies the master key is in the keychain and backup files exist in iCloud.
    /// Returns None if everything is OK, Some(warning) if there's a problem
    pub fn verify_backup_integrity(&self) -> Option<String> {
        if !matches!(*self.state.read(), CloudBackupState::Enabled) {
            return None;
        }

        let mut issues: Vec<&str> = Vec::new();

        let keychain = Keychain::global();
        let cspp = cove_cspp::Cspp::new(keychain.clone());
        if !cspp.has_master_key() {
            issues.push("master key not found in keychain");
        }

        if keychain.get(CREDENTIAL_ID_KEY.into()).is_none() {
            issues
                .push("passkey credential not found — open Cloud Backup in Settings to re-verify");
        }
        if keychain.get(PRF_SALT_KEY.into()).is_none() {
            issues.push("passkey salt not found — open Cloud Backup in Settings to re-verify");
        }

        let cloud = CloudStorage::global();
        match cloud.has_cloud_backup() {
            Ok(true) => {}
            Ok(false) => issues.push("backup files not found in iCloud"),
            Err(e) => {
                error!("Backup integrity: cloud check failed: {e}");
                issues.push("could not verify iCloud files");
            }
        }

        match cloud.download_master_key_backup() {
            Ok(_) => {}
            Err(CloudStorageError::NotFound(_)) => {
                issues.push("master key backup not found in iCloud");
            }
            Err(e) => {
                error!("Backup integrity: master key check failed: {e}");
                issues.push("could not verify master key backup in iCloud");
            }
        }

        // check for unsynced wallets and auto-sync if needed
        if issues.is_empty() {
            match cloud.download_manifest() {
                Ok(json) => {
                    if let Ok(manifest) = serde_json::from_slice::<BackupManifest>(&json) {
                        let db = Database::global();
                        let local_count = count_all_wallets(&db);
                        let cloud_count = manifest.wallet_record_ids.len() as u32;

                        if local_count > cloud_count {
                            info!(
                                "Backup integrity: {local_count} local wallets vs {cloud_count} in cloud, auto-syncing"
                            );
                            if let Err(e) = self.do_sync_unsynced_wallets() {
                                error!("Backup integrity: auto-sync failed: {e}");
                                issues.push("some wallets are not backed up");
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!("Backup integrity: manifest check failed: {e}");
                }
            }
        }

        if issues.is_empty() {
            info!("Backup integrity check passed");
            None
        } else {
            let msg = issues.join("; ");
            error!("Backup integrity issues: {msg}");
            Some(msg)
        }
    }

    /// Enable cloud backup — idempotent, safe to retry
    ///
    /// Creates passkey (or reuses existing), encrypts master key + all wallets,
    /// uploads to CloudKit, marks enabled only after manifest upload succeeds
    pub fn enable_cloud_backup(&self) {
        {
            let state = self.state.read();
            if matches!(*state, CloudBackupState::Enabling | CloudBackupState::Restoring) {
                warn!("enable_cloud_backup called while already {state:?}, ignoring");
                return;
            }
        }

        let this = CLOUD_BACKUP_MANAGER.clone();
        cove_tokio::task::spawn_blocking(move || {
            if let Err(e) = this.do_enable_cloud_backup() {
                error!("Cloud backup enable failed: {e}");
                this.send(Message::StateChanged(CloudBackupState::Error(e.to_string())));
            }
        });
    }

    /// Restore from cloud backup — called after device restore
    ///
    /// Uses discoverable credential assertion (no local keychain state required)
    pub fn restore_from_cloud_backup(&self) {
        {
            let state = self.state.read();
            if matches!(*state, CloudBackupState::Enabling | CloudBackupState::Restoring) {
                warn!("restore_from_cloud_backup called while already {state:?}, ignoring");
                return;
            }
        }

        let this = CLOUD_BACKUP_MANAGER.clone();
        cove_tokio::task::spawn_blocking(move || {
            if let Err(e) = this.do_restore_from_cloud_backup() {
                error!("Cloud backup restore failed: {e}");
                this.send(Message::StateChanged(CloudBackupState::Error(e.to_string())));
            }
        });
    }
}

impl RustCloudBackupManager {
    /// Back up a newly created wallet, fire-and-forget
    ///
    /// Returns immediately if cloud backup isn't enabled (e.g. during restore)
    pub fn backup_new_wallet(&self, metadata: crate::wallet::metadata::WalletMetadata) {
        if !matches!(*self.state.read(), CloudBackupState::Enabled) {
            return;
        }

        let this = CLOUD_BACKUP_MANAGER.clone();
        cove_tokio::task::spawn_blocking(move || {
            if let Err(e) = this.do_backup_wallets(&[metadata]) {
                warn!("Failed to backup new wallet, retrying full sync: {e}");
                if let Err(e) = this.do_sync_unsynced_wallets() {
                    error!("Retry sync also failed: {e}");
                    this.send(Message::SyncFailed(e.to_string()));
                }
            }
        });
    }

    fn persist_verification_result(&self, result: &DeepVerificationResult) {
        let db = Database::global();
        let current = db.global_config.cloud_backup();

        let (last_sync, wallet_count) = match &current {
            CloudBackup::Enabled { last_sync, wallet_count }
            | CloudBackup::Unverified { last_sync, wallet_count } => (*last_sync, *wallet_count),
            CloudBackup::Disabled => return,
        };

        let new_state = match result {
            DeepVerificationResult::Verified(_) => CloudBackup::Enabled { last_sync, wallet_count },
            DeepVerificationResult::UserCancelled(_) | DeepVerificationResult::Failed(_) => {
                CloudBackup::Unverified { last_sync, wallet_count }
            }
            DeepVerificationResult::NotEnabled => return,
        };

        if current != new_state
            && let Err(e) = db.global_config.set_cloud_backup(&new_state)
        {
            error!("Failed to persist verification state: {e}");
        }
    }

    fn do_repair_passkey_wrapper(&self) -> Result<(), CloudBackupError> {
        let keychain = Keychain::global();
        let cspp = cove_cspp::Cspp::new(keychain.clone());
        let cloud = CloudStorage::global();
        let passkey = PasskeyAccess::global();

        // step 1: load local master key (bypass cache)
        let local_mk = cspp
            .load_master_key_from_store()
            .map_err_prefix("load local master key", CloudBackupError::Internal)?
            .ok_or_else(|| CloudBackupError::Internal("no local master key".into()))?;

        // step 2: download manifest and prove local key can decrypt a wallet
        let manifest = match cloud.download_manifest() {
            Ok(json) => {
                let m: BackupManifest =
                    serde_json::from_slice(&json).map_err_str(CloudBackupError::Internal)?;
                Some(m)
            }
            Err(CloudStorageError::NotFound(_)) => None,
            Err(e) => return Err(CloudBackupError::Cloud(format!("download manifest: {e}"))),
        };

        if let Some(ref m) = manifest
            && !m.wallet_record_ids.is_empty()
        {
            let critical_key = Zeroizing::new(local_mk.critical_data_key());
            let mut proved = false;
            let mut had_wrong_key = false;

            for rid in &m.wallet_record_ids {
                match cloud.download_wallet_backup(rid.clone()) {
                    Ok(json) => {
                        let encrypted: cove_cspp::backup_data::EncryptedWalletBackup =
                            match serde_json::from_slice(&json) {
                                Ok(e) => e,
                                Err(_) => continue,
                            };
                        if encrypted.version != 1 {
                            continue;
                        }
                        match wallet_crypto::decrypt_wallet_backup(&encrypted, &critical_key) {
                            Ok(_) => {
                                proved = true;
                                break;
                            }
                            Err(cove_cspp::CsppError::WrongKey) => {
                                had_wrong_key = true;
                            }
                            Err(_) => continue,
                        }
                    }
                    Err(_) => continue,
                }
            }

            if !proved && had_wrong_key {
                return Err(CloudBackupError::Crypto(
                    "local master key cannot decrypt existing cloud wallet backups".into(),
                ));
            }

            if !proved {
                return Err(CloudBackupError::Cloud(
                    "could not download any wallet to verify local key".into(),
                ));
            }
        }

        // step 3: create new passkey and re-wrap
        let new_prf = create_prf_key_without_persisting(passkey)?;

        let encrypted_backup =
            master_key_crypto::encrypt_master_key(&local_mk, &new_prf.prf_key, &new_prf.prf_salt)
                .map_err_str(CloudBackupError::Crypto)?;

        let backup_json =
            serde_json::to_vec(&encrypted_backup).map_err_str(CloudBackupError::Internal)?;

        // step 4: upload, then persist credentials
        cloud.upload_master_key_backup(backup_json).map_err_str(CloudBackupError::Cloud)?;

        keychain
            .save(CREDENTIAL_ID_KEY.into(), hex::encode(&new_prf.credential_id))
            .map_err_prefix("save credential", CloudBackupError::Internal)?;
        keychain
            .save(PRF_SALT_KEY.into(), hex::encode(new_prf.prf_salt))
            .map_err_prefix("save prf_salt", CloudBackupError::Internal)?;

        info!("Repaired cloud master key wrapper with new passkey");
        Ok(())
    }

    fn do_deep_verify_cloud_backup(&self) -> Result<DeepVerificationResult, CloudBackupError> {
        let keychain = Keychain::global();
        let cspp = cove_cspp::Cspp::new(keychain.clone());
        let cloud = CloudStorage::global();
        let passkey = PasskeyAccess::global();

        let mut report = DeepVerificationReport {
            master_key_wrapper_repaired: false,
            local_master_key_repaired: false,
            credential_recovered: false,
            wallets_verified: 0,
            wallets_failed: 0,
            wallets_unsupported: 0,
            detail: None,
        };

        // step 2: load local master key from store (bypasses cache)
        let local_master_key = cspp
            .load_master_key_from_store()
            .map_err_prefix("load local master key", CloudBackupError::Internal)?;

        // step 3: download manifest
        let mut manifest_missing = false;
        let manifest = match cloud.download_manifest() {
            Ok(json) => {
                let m: BackupManifest =
                    serde_json::from_slice(&json).map_err_str(CloudBackupError::Internal)?;
                if m.version != 1 {
                    return Ok(DeepVerificationResult::Failed(
                        DeepVerificationFailure::UnsupportedVersion {
                            message: format!("manifest version {} is not supported", m.version),
                            detail: None,
                        },
                    ));
                }
                let detail = build_detail_from_manifest(&m);
                report.detail = Some(detail);
                Some(m)
            }
            Err(CloudStorageError::NotFound(_)) => {
                manifest_missing = true;
                None
            }
            Err(e) => {
                return Ok(DeepVerificationResult::Failed(DeepVerificationFailure::Retry {
                    message: format!("failed to download manifest: {e}"),
                    detail: None,
                }));
            }
        };

        // step 4: download encrypted master key backup
        let encrypted_master = match cloud.download_master_key_backup() {
            Ok(json) => {
                let em: cove_cspp::backup_data::EncryptedMasterKeyBackup =
                    serde_json::from_slice(&json).map_err_str(CloudBackupError::Internal)?;
                if em.version != 1 {
                    return Ok(DeepVerificationResult::Failed(
                        DeepVerificationFailure::UnsupportedVersion {
                            message: format!(
                                "master key backup version {} is not supported",
                                em.version
                            ),
                            detail: report.detail.clone(),
                        },
                    ));
                }
                Some(em)
            }
            Err(CloudStorageError::NotFound(_)) => {
                if local_master_key.is_some() {
                    None // will enter wrapper-repair path
                } else {
                    return Ok(DeepVerificationResult::Failed(
                        DeepVerificationFailure::ReinitializeBackup {
                            message: "master key backup not found in iCloud and no local key"
                                .into(),
                            detail: report.detail.clone(),
                            warning: "This will replace your entire cloud backup set. Wallets that only exist in the cloud backup will be lost".into(),
                        },
                    ));
                }
            }
            Err(e) => {
                return Ok(DeepVerificationResult::Failed(DeepVerificationFailure::Retry {
                    message: format!("failed to download master key backup: {e}"),
                    detail: report.detail.clone(),
                }));
            }
        };

        // step 5 + 6: passkey auth and master key decryption
        let (verified_master_key, needs_wrapper_repair) = if let Some(ref em) = encrypted_master {
            let prf_salt = em.prf_salt;

            // try authenticate with cascading fallback
            let prf_result = authenticate_with_fallback(keychain, passkey, &prf_salt);

            match prf_result {
                Ok((prf_key, credential_id, recovered)) => {
                    report.credential_recovered = recovered;

                    // step 6: decrypt cloud master key
                    match master_key_crypto::decrypt_master_key(em, &prf_key) {
                        Ok(mk) => {
                            // persist credential_id + prf_salt after successful decrypt
                            keychain
                                .save(CREDENTIAL_ID_KEY.into(), hex::encode(&credential_id))
                                .map_err_prefix("save credential_id", CloudBackupError::Internal)?;
                            keychain
                                .save(PRF_SALT_KEY.into(), hex::encode(prf_salt))
                                .map_err_prefix("save prf_salt", CloudBackupError::Internal)?;
                            (Some(mk), false)
                        }
                        Err(_) => {
                            if local_master_key.is_some() {
                                // decrypt failed but we have local key — wrapper repair
                                (None, true)
                            } else {
                                return Ok(DeepVerificationResult::Failed(
                                    DeepVerificationFailure::ReinitializeBackup {
                                        message: "could not decrypt cloud master key and no local key available".into(),
                                        detail: report.detail.clone(),
                                        warning: "This will replace your entire cloud backup set. Wallets that only exist in the cloud backup will be lost".into(),
                                    },
                                ));
                            }
                        }
                    }
                }
                Err(CloudBackupError::Passkey(msg)) if msg == "user cancelled" => {
                    return Ok(DeepVerificationResult::UserCancelled(report.detail));
                }
                Err(CloudBackupError::Passkey(msg))
                    if msg.contains("no credential found") || msg.contains("NoCredentialFound") =>
                {
                    if local_master_key.is_some() {
                        (None, true) // wrapper repair with new passkey
                    } else {
                        return Ok(DeepVerificationResult::Failed(
                            DeepVerificationFailure::ReinitializeBackup {
                                message: "no passkey found and no local master key".into(),
                                detail: report.detail.clone(),
                                warning: "This will replace your entire cloud backup set. Wallets that only exist in the cloud backup will be lost".into(),
                            },
                        ));
                    }
                }
                Err(e) => {
                    return Ok(DeepVerificationResult::Failed(DeepVerificationFailure::Retry {
                        message: format!("passkey authentication failed: {e}"),
                        detail: report.detail.clone(),
                    }));
                }
            }
        } else {
            // no encrypted master key in cloud — will wrapper-repair
            (None, true)
        };

        // determine which master key to use for verification
        let master_key = if let Some(mk) = verified_master_key {
            // step 7: reconcile cloud master key with local state
            match &local_master_key {
                None => {
                    // local missing — repair from cloud
                    cspp.save_master_key(&mk)
                        .map_err_prefix("repair local master key", CloudBackupError::Internal)?;
                    cove_cspp::reset_master_key_cache();
                    report.local_master_key_repaired = true;
                    info!("Repaired local master key from cloud");
                }
                Some(local_mk) if local_mk.as_bytes() != mk.as_bytes() => {
                    // local differs — cloud is authoritative
                    cspp.save_master_key(&mk)
                        .map_err_prefix("repair local master key", CloudBackupError::Internal)?;
                    cove_cspp::reset_master_key_cache();
                    report.local_master_key_repaired = true;
                    info!("Repaired local master key to match cloud");
                }
                Some(_) => {
                    // match — all good
                }
            }

            // step 8: missing manifest after master key verification
            if manifest_missing {
                return Ok(DeepVerificationResult::Failed(
                    DeepVerificationFailure::RecreateManifest {
                        message: "backup index not found in iCloud".into(),
                        detail: report.detail.clone(),
                        warning: "Recreating the backup index from this device will remove references to wallets that only exist in the cloud backup".into(),
                    },
                ));
            }

            mk
        } else if needs_wrapper_repair {
            // auto-repair path: wrapper repair using local master key
            let local_mk = local_master_key.as_ref().expect("checked earlier");

            // proof step: verify local key can decrypt at least one wallet
            if let Some(ref m) = manifest
                && !m.wallet_record_ids.is_empty()
            {
                let critical_key = Zeroizing::new(local_mk.critical_data_key());
                let mut proved = false;
                let mut had_wrong_key = false;

                for rid in &m.wallet_record_ids {
                    match cloud.download_wallet_backup(rid.clone()) {
                        Ok(json) => {
                            let encrypted: cove_cspp::backup_data::EncryptedWalletBackup =
                                match serde_json::from_slice(&json) {
                                    Ok(e) => e,
                                    Err(_) => continue,
                                };
                            if encrypted.version != 1 {
                                continue;
                            }
                            match wallet_crypto::decrypt_wallet_backup(&encrypted, &critical_key) {
                                Ok(_) => {
                                    proved = true;
                                    break;
                                }
                                Err(cove_cspp::CsppError::WrongKey) => {
                                    had_wrong_key = true;
                                }
                                Err(_) => continue,
                            }
                        }
                        Err(_) => continue,
                    }
                }

                if !proved && had_wrong_key {
                    return Ok(DeepVerificationResult::Failed(
                        DeepVerificationFailure::ReinitializeBackup {
                            message: "local master key cannot decrypt existing cloud wallet backups".into(),
                            detail: report.detail.clone(),
                            warning: "This will replace your entire cloud backup set. Wallets that only exist in the cloud backup will be lost".into(),
                        },
                    ));
                }

                if !proved {
                    return Ok(DeepVerificationResult::Failed(DeepVerificationFailure::Retry {
                        message: "could not download any wallet to verify local key".into(),
                        detail: report.detail.clone(),
                    }));
                }
            }

            // create new passkey and re-wrap, but don't persist until upload succeeds
            let new_prf = create_prf_key_without_persisting(passkey)?;

            let encrypted_backup = master_key_crypto::encrypt_master_key(
                local_mk,
                &new_prf.prf_key,
                &new_prf.prf_salt,
            )
            .map_err_str(CloudBackupError::Crypto)?;

            let backup_json =
                serde_json::to_vec(&encrypted_backup).map_err_str(CloudBackupError::Internal)?;

            cloud.upload_master_key_backup(backup_json).map_err_str(CloudBackupError::Cloud)?;

            // only persist after successful upload
            keychain
                .save(CREDENTIAL_ID_KEY.into(), hex::encode(&new_prf.credential_id))
                .map_err_prefix("save credential", CloudBackupError::Internal)?;
            keychain
                .save(PRF_SALT_KEY.into(), hex::encode(new_prf.prf_salt))
                .map_err_prefix("save prf_salt", CloudBackupError::Internal)?;

            report.master_key_wrapper_repaired = true;
            info!("Repaired cloud master key wrapper with new passkey");

            if manifest_missing {
                return Ok(DeepVerificationResult::Failed(
                    DeepVerificationFailure::RecreateManifest {
                        message: "backup index not found in iCloud".into(),
                        detail: report.detail.clone(),
                        warning: "Recreating the backup index from this device will remove references to wallets that only exist in the cloud backup".into(),
                    },
                ));
            }

            cove_cspp::master_key::MasterKey::from_bytes(*local_mk.as_bytes())
        } else {
            unreachable!("either verified_master_key or needs_wrapper_repair must be set");
        };

        // step 9: verify wallet backups
        if let Some(ref m) = manifest {
            let critical_key = Zeroizing::new(master_key.critical_data_key());
            let (verified, failed, unsupported) = verify_wallet_backups(cloud, m, &critical_key);
            report.wallets_verified = verified;
            report.wallets_failed = failed;
            report.wallets_unsupported = unsupported;

            // step 10: check for local wallets not in the manifest and auto-sync
            let db = Database::global();
            let cloud_record_ids: std::collections::HashSet<_> =
                m.wallet_record_ids.iter().collect();
            let unsynced: Vec<_> = all_local_wallets(&db)
                .into_iter()
                .filter(|w| !cloud_record_ids.contains(&wallet_record_id(w.id.as_ref())))
                .collect();

            if !unsynced.is_empty() {
                let count = unsynced.len() as u32;
                info!("Deep verify: {count} local wallet(s) not in cloud, auto-syncing");
                match self.do_backup_wallets(&unsynced) {
                    Ok(()) => {
                        // rebuild detail with updated manifest
                        if let Ok(json) = cloud.download_manifest()
                            && let Ok(updated) = serde_json::from_slice::<BackupManifest>(&json)
                        {
                            report.detail = Some(build_detail_from_manifest(&updated));
                        }
                    }
                    Err(e) => {
                        warn!("Deep verify: auto-sync failed: {e}");
                    }
                }
            }
        }

        Ok(DeepVerificationResult::Verified(report))
    }

    /// Upload wallets to cloud and update manifest + local cache
    fn do_backup_wallets(
        &self,
        wallets: &[crate::wallet::metadata::WalletMetadata],
    ) -> Result<(), CloudBackupError> {
        if wallets.is_empty() {
            return Ok(());
        }

        let cspp = cove_cspp::Cspp::new(Keychain::global().clone());
        let master_key = cspp
            .get_or_create_master_key()
            .map_err_prefix("master key", CloudBackupError::Internal)?;

        let critical_key = Zeroizing::new(master_key.critical_data_key());
        let cloud = CloudStorage::global();

        let manifest_json = cloud.download_manifest().map_err_str(CloudBackupError::Cloud)?;
        let mut manifest: BackupManifest =
            serde_json::from_slice(&manifest_json).map_err_str(CloudBackupError::Internal)?;

        for metadata in wallets {
            let entry = build_wallet_entry(metadata, metadata.wallet_mode)?;
            let encrypted = wallet_crypto::encrypt_wallet_entry(&entry, &critical_key)
                .map_err_str(CloudBackupError::Crypto)?;

            let record_id = wallet_record_id(metadata.id.as_ref());
            let wallet_json =
                serde_json::to_vec(&encrypted).map_err_str(CloudBackupError::Internal)?;

            cloud
                .upload_wallet_backup(record_id.clone(), wallet_json)
                .map_err_str(CloudBackupError::Cloud)?;

            if !manifest.wallet_record_ids.contains(&record_id) {
                manifest.wallet_record_ids.push(record_id);
            }
        }

        let manifest_json =
            serde_json::to_vec(&manifest).map_err_str(CloudBackupError::Internal)?;
        cloud.upload_manifest(manifest_json).map_err_str(CloudBackupError::Cloud)?;

        let wallet_count = manifest.wallet_record_ids.len() as u32;
        let now = jiff::Timestamp::now().as_second().try_into().unwrap_or(0);
        let db = Database::global();
        db.global_config
            .set_cloud_backup(&CloudBackup::Enabled {
                last_sync: Some(now),
                wallet_count: Some(wallet_count),
            })
            .map_err_prefix("persist cloud backup state", CloudBackupError::Internal)?;

        info!("Backed up {} wallet(s) to cloud", wallets.len());
        Ok(())
    }

    fn do_sync_unsynced_wallets(&self) -> Result<(), CloudBackupError> {
        let cloud = CloudStorage::global();
        let manifest_json = cloud.download_manifest().map_err_str(CloudBackupError::Cloud)?;
        let manifest: BackupManifest =
            serde_json::from_slice(&manifest_json).map_err_str(CloudBackupError::Internal)?;

        let cloud_record_ids: std::collections::HashSet<_> =
            manifest.wallet_record_ids.iter().collect();

        let db = Database::global();
        let unsynced: Vec<_> = all_local_wallets(&db)
            .into_iter()
            .filter(|w| !cloud_record_ids.contains(&wallet_record_id(w.id.as_ref())))
            .collect();

        if unsynced.is_empty() {
            return Ok(());
        }

        self.do_backup_wallets(&unsynced)
    }

    fn do_fetch_cloud_only_wallets(&self) -> Result<Vec<CloudBackupWalletItem>, CloudBackupError> {
        let cloud = CloudStorage::global();
        let manifest_json = cloud.download_manifest().map_err_str(CloudBackupError::Cloud)?;
        let manifest: BackupManifest =
            serde_json::from_slice(&manifest_json).map_err_str(CloudBackupError::Internal)?;

        let db = Database::global();
        let local_record_ids: std::collections::HashSet<_> =
            all_local_wallets(&db).iter().map(|w| wallet_record_id(w.id.as_ref())).collect();

        let orphan_ids: Vec<_> = manifest
            .wallet_record_ids
            .iter()
            .filter(|rid| !local_record_ids.contains(*rid))
            .collect();

        if orphan_ids.is_empty() {
            return Ok(Vec::new());
        }

        let cspp = cove_cspp::Cspp::new(Keychain::global().clone());
        let master_key = cspp
            .get_or_create_master_key()
            .map_err_prefix("master key", CloudBackupError::Internal)?;

        let critical_key = Zeroizing::new(master_key.critical_data_key());
        let mut items = Vec::new();

        for record_id in orphan_ids {
            let wallet_json = match cloud.download_wallet_backup(record_id.clone()) {
                Ok(json) => json,
                Err(e) => {
                    warn!("Failed to download cloud-only wallet {record_id}: {e}");
                    continue;
                }
            };

            let encrypted: cove_cspp::backup_data::EncryptedWalletBackup =
                match serde_json::from_slice(&wallet_json) {
                    Ok(e) => e,
                    Err(e) => {
                        warn!("Failed to deserialize cloud-only wallet {record_id}: {e}");
                        continue;
                    }
                };

            let entry = match wallet_crypto::decrypt_wallet_backup(&encrypted, &critical_key) {
                Ok(e) => e,
                Err(e) => {
                    warn!("Failed to decrypt cloud-only wallet {record_id}: {e}");
                    continue;
                }
            };

            let metadata: WalletMetadata = match serde_json::from_value(entry.metadata.clone()) {
                Ok(m) => m,
                Err(e) => {
                    warn!("Failed to parse cloud-only wallet metadata {record_id}: {e}");
                    continue;
                }
            };

            items.push(CloudBackupWalletItem {
                name: metadata.name,
                network: metadata.network,
                wallet_mode: metadata.wallet_mode,
                wallet_type: metadata.wallet_type,
                fingerprint: metadata.master_fingerprint.as_ref().map(|fp| fp.as_uppercase()),
                status: CloudBackupWalletStatus::DeletedFromDevice,
                record_id: Some(record_id.clone()),
            });
        }

        Ok(items)
    }

    fn do_restore_cloud_wallet(&self, record_id: &str) -> Result<(), CloudBackupError> {
        let cloud = CloudStorage::global();
        let cspp = cove_cspp::Cspp::new(Keychain::global().clone());
        let master_key = cspp
            .get_or_create_master_key()
            .map_err_prefix("master key", CloudBackupError::Internal)?;
        let critical_key = Zeroizing::new(master_key.critical_data_key());

        let db = Database::global();
        let mut existing_fingerprints: Vec<_> = all_local_wallets(&db)
            .iter()
            .filter_map(|w| {
                w.master_fingerprint.as_ref().map(|fp| (**fp, w.network, w.wallet_mode))
            })
            .collect();

        restore_single_wallet(cloud, record_id, &critical_key, &mut existing_fingerprints)?;
        info!("Restored cloud wallet {record_id}");
        Ok(())
    }

    fn do_delete_cloud_wallet(&self, record_id: &str) -> Result<(), CloudBackupError> {
        let cloud = CloudStorage::global();

        // delete the wallet file
        cloud.delete_wallet_backup(record_id.to_string()).map_err_str(CloudBackupError::Cloud)?;

        // remove from manifest
        let manifest_json = cloud.download_manifest().map_err_str(CloudBackupError::Cloud)?;
        let mut manifest: BackupManifest =
            serde_json::from_slice(&manifest_json).map_err_str(CloudBackupError::Internal)?;

        manifest.wallet_record_ids.retain(|rid| rid != record_id);

        let updated_json = serde_json::to_vec(&manifest).map_err_str(CloudBackupError::Internal)?;
        cloud.upload_manifest(updated_json).map_err_str(CloudBackupError::Cloud)?;

        // update persisted wallet count
        let wallet_count = manifest.wallet_record_ids.len() as u32;
        let db = Database::global();
        let last_sync = match db.global_config.cloud_backup() {
            CloudBackup::Enabled { last_sync, .. } | CloudBackup::Unverified { last_sync, .. } => {
                last_sync
            }
            CloudBackup::Disabled => None,
        };
        let _ = db.global_config.set_cloud_backup(&CloudBackup::Enabled {
            last_sync,
            wallet_count: Some(wallet_count),
        });

        info!("Deleted cloud wallet {record_id}");
        Ok(())
    }

    /// Re-upload all local wallets to cloud when the manifest is missing
    ///
    /// Reuses the master key from keychain (no passkey interaction needed)
    fn do_reupload_all_wallets(&self) -> Result<(), CloudBackupError> {
        info!("Re-uploading all wallets to cloud");

        let cspp = cove_cspp::Cspp::new(Keychain::global().clone());
        let master_key = cspp
            .get_or_create_master_key()
            .map_err_prefix("master key", CloudBackupError::Internal)?;

        let critical_key = Zeroizing::new(master_key.critical_data_key());
        let cloud = CloudStorage::global();
        let db = Database::global();

        upload_all_wallets_and_manifest(cloud, &critical_key, &db)
    }

    fn do_enable_cloud_backup(&self) -> Result<(), CloudBackupError> {
        self.send(Message::StateChanged(CloudBackupState::Enabling));

        let passkey = PasskeyAccess::global();
        if !passkey.is_prf_supported() {
            return Err(CloudBackupError::NotSupported(
                "PRF extension not supported on this device".into(),
            ));
        }

        // get or create local master key
        let cspp = cove_cspp::Cspp::new(Keychain::global().clone());
        let master_key = cspp
            .get_or_create_master_key()
            .map_err_prefix("master key", CloudBackupError::Internal)?;

        let keychain = Keychain::global();
        let (prf_key, prf_salt) = obtain_prf_key(keychain, passkey)?;

        // encrypt and upload master key
        let encrypted_master =
            master_key_crypto::encrypt_master_key(&master_key, &prf_key, &prf_salt)
                .map_err_str(CloudBackupError::Crypto)?;

        let master_json =
            serde_json::to_vec(&encrypted_master).map_err_str(CloudBackupError::Internal)?;

        let cloud = CloudStorage::global();
        cloud.upload_master_key_backup(master_json).map_err_str(CloudBackupError::Cloud)?;

        let critical_key = Zeroizing::new(master_key.critical_data_key());
        let db = Database::global();
        upload_all_wallets_and_manifest(cloud, &critical_key, &db)?;

        self.send(Message::EnableComplete);
        self.send(Message::StateChanged(CloudBackupState::Enabled));

        info!("Cloud backup enabled successfully");
        Ok(())
    }

    fn do_restore_from_cloud_backup(&self) -> Result<(), CloudBackupError> {
        self.send(Message::StateChanged(CloudBackupState::Restoring));

        let cloud = CloudStorage::global();
        let passkey = PasskeyAccess::global();

        // download encrypted master key to get prf_salt
        let master_json =
            cloud.download_master_key_backup().map_err_str(CloudBackupError::Cloud)?;

        let encrypted_master: cove_cspp::backup_data::EncryptedMasterKeyBackup =
            serde_json::from_slice(&master_json).map_err_str(CloudBackupError::Internal)?;

        if encrypted_master.version != 1 {
            let version = encrypted_master.version;
            return Err(CloudBackupError::Internal(format!(
                "unsupported master key backup version: {version}",
            )));
        }

        let prf_salt = encrypted_master.prf_salt;

        // discoverable credential assertion — no credential_id needed
        let challenge: Vec<u8> = rand::rng().random::<[u8; 32]>().to_vec();
        let discovered = passkey
            .discover_and_authenticate_with_prf(RP_ID.to_string(), prf_salt.to_vec(), challenge)
            .map_err_str(CloudBackupError::Passkey)?;

        let prf_key: [u8; 32] = discovered
            .prf_output
            .try_into()
            .map_err(|_| CloudBackupError::Internal("PRF output is not 32 bytes".into()))?;

        // decrypt master key
        let master_key = master_key_crypto::decrypt_master_key(&encrypted_master, &prf_key)
            .map_err_prefix("master key decrypt", CloudBackupError::Crypto)?;

        // persist discovered credential to local keychain
        let keychain = Keychain::global();
        keychain
            .save(CREDENTIAL_ID_KEY.to_string(), hex::encode(&discovered.credential_id))
            .map_err_prefix("save credential_id", CloudBackupError::Internal)?;

        keychain
            .save(PRF_SALT_KEY.to_string(), hex::encode(prf_salt))
            .map_err_prefix("save prf_salt", CloudBackupError::Internal)?;

        // save master key to keychain for cloud backup decryption
        let cspp = cove_cspp::Cspp::new(keychain.clone());
        cspp.save_master_key(&master_key)
            .map_err_prefix("save master key", CloudBackupError::Internal)?;

        // local encryption key unchanged, DB already open — just import cloud wallets
        cove_cspp::reset_master_key_cache();

        // download manifest
        let manifest_json = cloud.download_manifest().map_err_str(CloudBackupError::Cloud)?;
        let manifest: BackupManifest =
            serde_json::from_slice(&manifest_json).map_err_str(CloudBackupError::Internal)?;

        if manifest.version != 1 {
            let version = manifest.version;
            return Err(CloudBackupError::Internal(format!(
                "unsupported manifest version: {version}",
            )));
        }

        let total = manifest.wallet_record_ids.len() as u32;
        let critical_key = Zeroizing::new(master_key.critical_data_key());
        let mut report = CloudBackupRestoreReport {
            wallets_restored: 0,
            wallets_failed: 0,
            failed_wallet_errors: Vec::new(),
        };

        let mut existing_fingerprints = crate::backup::import::collect_existing_fingerprints()
            .map_err_prefix("collect fingerprints", CloudBackupError::Internal)?;

        // download and restore each wallet
        for (i, record_id) in manifest.wallet_record_ids.iter().enumerate() {
            match restore_single_wallet(cloud, record_id, &critical_key, &mut existing_fingerprints)
            {
                Ok(()) => report.wallets_restored += 1,
                Err(e) => {
                    warn!("Failed to restore wallet {record_id}: {e}");
                    report.wallets_failed += 1;
                    report.failed_wallet_errors.push(e.to_string());
                }
            }

            self.send(Message::ProgressUpdated { completed: (i + 1) as u32, total });
        }

        // don't mark enabled if every wallet failed
        if report.wallets_restored == 0 && report.wallets_failed > 0 {
            self.send(Message::RestoreComplete(report));
            return Err(CloudBackupError::Internal("all wallets failed to restore".into()));
        }

        // mark enabled
        let wallet_count = report.wallets_restored;
        let now = jiff::Timestamp::now().as_second().try_into().unwrap_or(0);
        let db = Database::global();
        db.global_config
            .set_cloud_backup(&CloudBackup::Enabled {
                last_sync: Some(now),
                wallet_count: Some(wallet_count),
            })
            .map_err_prefix("persist cloud backup state", CloudBackupError::Internal)?;

        self.send(Message::RestoreComplete(report));
        self.send(Message::StateChanged(CloudBackupState::Enabled));

        info!("Cloud backup restore complete");
        Ok(())
    }
}

/// Build a CloudBackupDetail from a manifest by comparing against local wallets
fn build_detail_from_manifest(manifest: &BackupManifest) -> CloudBackupDetail {
    let db = Database::global();
    let last_sync = match db.global_config.cloud_backup() {
        CloudBackup::Enabled { last_sync, .. } | CloudBackup::Unverified { last_sync, .. } => {
            last_sync
        }
        CloudBackup::Disabled => None,
    };

    let cloud_record_ids: std::collections::HashSet<_> =
        manifest.wallet_record_ids.iter().cloned().collect();

    let local_wallets = all_local_wallets(&db);
    let local_record_ids: std::collections::HashSet<_> =
        local_wallets.iter().map(|w| wallet_record_id(w.id.as_ref())).collect();

    let mut backed_up = Vec::new();
    let mut not_backed_up = Vec::new();

    for w in &local_wallets {
        let record_id = wallet_record_id(w.id.as_ref());
        let status = if cloud_record_ids.contains(&record_id) {
            CloudBackupWalletStatus::BackedUp
        } else {
            CloudBackupWalletStatus::NotBackedUp
        };

        let item = CloudBackupWalletItem {
            name: w.name.clone(),
            network: w.network,
            wallet_mode: w.wallet_mode,
            wallet_type: w.wallet_type,
            fingerprint: w.master_fingerprint.as_ref().map(|fp| fp.as_uppercase()),
            status,
            record_id: None,
        };

        match item.status {
            CloudBackupWalletStatus::BackedUp => backed_up.push(item),
            _ => not_backed_up.push(item),
        }
    }

    let cloud_only_count =
        cloud_record_ids.iter().filter(|rid| !local_record_ids.contains(*rid)).count() as u32;

    CloudBackupDetail { last_sync, backed_up, not_backed_up, cloud_only_count }
}

/// Verify wallet backups by downloading and decrypting each one
///
/// Returns (verified, failed, unsupported) counts
fn verify_wallet_backups(
    cloud: &CloudStorage,
    manifest: &BackupManifest,
    critical_key: &[u8; 32],
) -> (u32, u32, u32) {
    let mut verified = 0u32;
    let mut failed = 0u32;
    let mut unsupported = 0u32;

    for record_id in &manifest.wallet_record_ids {
        let wallet_json = match cloud.download_wallet_backup(record_id.clone()) {
            Ok(json) => json,
            Err(e) => {
                warn!("Verify: failed to download wallet {record_id}: {e}");
                failed += 1;
                continue;
            }
        };

        let encrypted: cove_cspp::backup_data::EncryptedWalletBackup =
            match serde_json::from_slice(&wallet_json) {
                Ok(e) => e,
                Err(e) => {
                    warn!("Verify: failed to deserialize wallet {record_id}: {e}");
                    failed += 1;
                    continue;
                }
            };

        if encrypted.version != 1 {
            unsupported += 1;
            continue;
        }

        match wallet_crypto::decrypt_wallet_backup(&encrypted, critical_key) {
            Ok(_) => verified += 1,
            Err(e) => {
                warn!("Verify: failed to decrypt wallet {record_id}: {e}");
                failed += 1;
            }
        }
    }

    (verified, failed, unsupported)
}

/// Authenticate with passkey using cascading fallback
///
/// Returns (prf_key, credential_id, was_recovered_via_discovery)
fn authenticate_with_fallback(
    keychain: &Keychain,
    passkey: &PasskeyAccess,
    prf_salt: &[u8; 32],
) -> Result<([u8; 32], Vec<u8>, bool), CloudBackupError> {
    let stored_credential_id = keychain.get(CREDENTIAL_ID_KEY.into()).and_then(|hex_str| {
        hex::decode(hex_str)
            .inspect_err(|e| warn!("Failed to decode stored credential_id: {e}"))
            .ok()
    });

    // try stored credential first
    if let Some(ref cred_id) = stored_credential_id {
        let challenge: Vec<u8> = rand::rng().random::<[u8; 32]>().to_vec();
        match passkey.authenticate_with_prf(
            RP_ID.to_string(),
            cred_id.clone(),
            prf_salt.to_vec(),
            challenge,
        ) {
            Ok(prf_output) => {
                let prf_key: [u8; 32] = prf_output
                    .try_into()
                    .map_err(|_| CloudBackupError::Internal("PRF output is not 32 bytes".into()))?;
                return Ok((prf_key, cred_id.clone(), false));
            }
            Err(cove_device::passkey::PasskeyError::UserCancelled) => {
                return Err(CloudBackupError::Passkey("user cancelled".into()));
            }
            Err(e) => {
                info!("Stored credential auth failed ({e}), trying discovery");
            }
        }
    }

    // fallback to discoverable assertion
    let challenge: Vec<u8> = rand::rng().random::<[u8; 32]>().to_vec();
    let discovered = passkey
        .discover_and_authenticate_with_prf(RP_ID.to_string(), prf_salt.to_vec(), challenge)
        .map_err(|e| match e {
            cove_device::passkey::PasskeyError::UserCancelled => {
                CloudBackupError::Passkey("user cancelled".into())
            }
            cove_device::passkey::PasskeyError::NoCredentialFound => {
                CloudBackupError::Passkey("no credential found".into())
            }
            other => CloudBackupError::Passkey(other.to_string()),
        })?;

    let prf_key: [u8; 32] = discovered
        .prf_output
        .try_into()
        .map_err(|_| CloudBackupError::Internal("PRF output is not 32 bytes".into()))?;

    Ok((prf_key, discovered.credential_id, true))
}

struct UnpersistedPrfKey {
    prf_key: [u8; 32],
    prf_salt: [u8; 32],
    credential_id: Vec<u8>,
}

/// Create a passkey and authenticate with PRF without persisting to keychain
///
/// Used by the wrapper-repair path where we need to defer persistence until
/// after the cloud upload succeeds
fn create_prf_key_without_persisting(
    passkey: &PasskeyAccess,
) -> Result<UnpersistedPrfKey, CloudBackupError> {
    info!("Creating new passkey for wrapper repair");
    let prf_salt: [u8; 32] = rand::rng().random();
    let challenge: Vec<u8> = rand::rng().random::<[u8; 32]>().to_vec();
    let user_id = rand::rng().random::<[u8; 16]>().to_vec();

    let credential_id = passkey
        .create_passkey(RP_ID.to_string(), user_id, challenge)
        .map_err_str(CloudBackupError::Passkey)?;

    let challenge: Vec<u8> = rand::rng().random::<[u8; 32]>().to_vec();
    let prf_output = passkey
        .authenticate_with_prf(
            RP_ID.to_string(),
            credential_id.clone(),
            prf_salt.to_vec(),
            challenge,
        )
        .map_err_str(CloudBackupError::Passkey)?;

    let prf_key: [u8; 32] = prf_output
        .try_into()
        .map_err(|_| CloudBackupError::Internal("PRF output is not 32 bytes".into()))?;

    Ok(UnpersistedPrfKey { prf_key, prf_salt, credential_id })
}

/// Encrypt and upload all local wallets, create a fresh manifest, and persist enabled state
fn upload_all_wallets_and_manifest(
    cloud: &CloudStorage,
    critical_key: &[u8; 32],
    db: &Database,
) -> Result<(), CloudBackupError> {
    let mut wallet_record_ids = Vec::new();

    for metadata in all_local_wallets(db) {
        let entry = build_wallet_entry(&metadata, metadata.wallet_mode)?;
        let encrypted = wallet_crypto::encrypt_wallet_entry(&entry, critical_key)
            .map_err_str(CloudBackupError::Crypto)?;

        let record_id = wallet_record_id(metadata.id.as_ref());
        let wallet_json = serde_json::to_vec(&encrypted).map_err_str(CloudBackupError::Internal)?;

        cloud
            .upload_wallet_backup(record_id.clone(), wallet_json)
            .map_err_str(CloudBackupError::Cloud)?;

        wallet_record_ids.push(record_id);
    }

    let manifest = BackupManifest {
        version: 1,
        created_at: jiff::Timestamp::now().as_second().try_into().unwrap_or(0),
        wallet_record_ids,
    };

    let manifest_json = serde_json::to_vec(&manifest).map_err_str(CloudBackupError::Internal)?;

    cloud.upload_manifest(manifest_json).map_err_str(CloudBackupError::Cloud)?;

    let wallet_count = manifest.wallet_record_ids.len() as u32;
    let now = jiff::Timestamp::now().as_second().try_into().unwrap_or(0);
    db.global_config
        .set_cloud_backup(&CloudBackup::Enabled {
            last_sync: Some(now),
            wallet_count: Some(wallet_count),
        })
        .map_err_prefix("persist cloud backup state", CloudBackupError::Internal)?;

    Ok(())
}

/// All local wallets across every network and mode
fn all_local_wallets(db: &Database) -> Vec<WalletMetadata> {
    Network::iter()
        .flat_map(|network| {
            LocalWalletMode::iter()
                .flat_map(move |mode| db.wallets.get_all(network, mode).unwrap_or_default())
        })
        .collect()
}

fn count_all_wallets(db: &Database) -> u32 {
    all_local_wallets(db).len() as u32
}

fn restore_single_wallet(
    cloud: &CloudStorage,
    record_id: &str,
    critical_key: &[u8; 32],
    existing_fingerprints: &mut Vec<(
        crate::wallet::fingerprint::Fingerprint,
        Network,
        LocalWalletMode,
    )>,
) -> Result<(), CloudBackupError> {
    let wallet_json = cloud
        .download_wallet_backup(record_id.to_string())
        .map_err(|e| CloudBackupError::Cloud(format!("download {record_id}: {e}")))?;

    let encrypted: cove_cspp::backup_data::EncryptedWalletBackup =
        serde_json::from_slice(&wallet_json)
            .map_err_prefix("deserialize wallet", CloudBackupError::Internal)?;

    if encrypted.version != 1 {
        let version = encrypted.version;
        return Err(CloudBackupError::Internal(format!(
            "unsupported wallet backup version: {version}",
        )));
    }

    let entry = wallet_crypto::decrypt_wallet_backup(&encrypted, critical_key)
        .map_err_prefix("decrypt wallet", CloudBackupError::Crypto)?;

    // convert WalletEntry to WalletMetadata + restore
    let metadata: crate::wallet::metadata::WalletMetadata =
        serde_json::from_value(entry.metadata.clone())
            .map_err_prefix("parse wallet metadata", CloudBackupError::Internal)?;

    // duplicate detection
    if crate::backup::import::is_wallet_duplicate(&metadata, existing_fingerprints)
        .inspect_err(|e| warn!("is_wallet_duplicate check failed for {}: {e}", metadata.name))
        .unwrap_or(false)
    {
        info!("Skipping duplicate wallet {}", metadata.name);
        return Ok(());
    }

    // build a WalletBackup-like structure for reuse of import helpers
    let backup_model = crate::backup::model::WalletBackup {
        metadata: entry.metadata.clone(),
        secret: convert_cloud_secret(&entry.secret),
        descriptors: entry.descriptors.as_ref().map(|d| crate::backup::model::DescriptorPair {
            external: d.external.clone(),
            internal: d.internal.clone(),
        }),
        xpub: entry.xpub.clone(),
        labels_jsonl: None,
    };

    match &backup_model.secret {
        crate::backup::model::WalletSecret::Mnemonic(words) => {
            let mnemonic = bip39::Mnemonic::from_str(words)
                .map_err_prefix("invalid mnemonic", CloudBackupError::Internal)?;

            crate::backup::import::restore_mnemonic_wallet(&metadata, mnemonic).map_err(
                |(e, _)| CloudBackupError::Internal(format!("restore mnemonic wallet: {e}")),
            )?;
        }
        _ => {
            crate::backup::import::restore_descriptor_wallet(&metadata, &backup_model).map_err(
                |(e, _)| CloudBackupError::Internal(format!("restore descriptor wallet: {e}")),
            )?;
        }
    }

    // track fingerprint for duplicate detection of subsequent wallets
    if let Some(fp) = &metadata.master_fingerprint {
        existing_fingerprints.push((**fp, metadata.network, metadata.wallet_mode));
    }

    Ok(())
}

/// Create a fresh passkey and authenticate with PRF to get the wrapping key
///
/// Always creates a new passkey — the enable flow re-encrypts everything,
/// so there's no benefit to reusing stale cached credentials (which may
/// reference a passkey deleted from the user's password manager)
fn obtain_prf_key(
    keychain: &Keychain,
    passkey: &PasskeyAccess,
) -> Result<([u8; 32], [u8; 32]), CloudBackupError> {
    // clear any stale credentials from a previous attempt or install
    keychain.delete(CREDENTIAL_ID_KEY.to_string());
    keychain.delete(PRF_SALT_KEY.to_string());

    info!("Creating new passkey");
    let prf_salt: [u8; 32] = rand::rng().random();
    let challenge: Vec<u8> = rand::rng().random::<[u8; 32]>().to_vec();
    let user_id = rand::rng().random::<[u8; 16]>().to_vec();

    let credential_id = passkey
        .create_passkey(RP_ID.to_string(), user_id, challenge)
        .map_err_str(CloudBackupError::Passkey)?;

    // authenticate with PRF to derive wrapping key
    let challenge: Vec<u8> = rand::rng().random::<[u8; 32]>().to_vec();
    let prf_output = passkey
        .authenticate_with_prf(
            RP_ID.to_string(),
            credential_id.clone(),
            prf_salt.to_vec(),
            challenge,
        )
        .map_err_str(CloudBackupError::Passkey)?;

    let prf_key: [u8; 32] = prf_output
        .try_into()
        .map_err(|_| CloudBackupError::Internal("PRF output is not 32 bytes".into()))?;

    // persist only after successful PRF auth
    keychain
        .save(CREDENTIAL_ID_KEY.to_string(), hex::encode(&credential_id))
        .map_err_prefix("save credential", CloudBackupError::Internal)?;

    keychain
        .save(PRF_SALT_KEY.to_string(), hex::encode(prf_salt))
        .map_err_prefix("save prf_salt", CloudBackupError::Internal)?;

    Ok((prf_key, prf_salt))
}

fn convert_cloud_secret(
    secret: &cove_cspp::backup_data::WalletSecret,
) -> crate::backup::model::WalletSecret {
    match secret {
        WalletSecret::Mnemonic(m) => crate::backup::model::WalletSecret::Mnemonic(m.clone()),
        WalletSecret::TapSignerBackup(b) => {
            crate::backup::model::WalletSecret::TapSignerBackup(b.clone())
        }
        WalletSecret::Descriptor(_) | WalletSecret::WatchOnly => {
            crate::backup::model::WalletSecret::None
        }
    }
}

fn build_wallet_entry(
    metadata: &crate::wallet::metadata::WalletMetadata,
    mode: LocalWalletMode,
) -> Result<WalletEntry, CloudBackupError> {
    let keychain = Keychain::global();
    let id = &metadata.id;
    let name = &metadata.name;

    let secret = match metadata.wallet_type {
        WalletType::Hot => match keychain.get_wallet_key(id) {
            Ok(Some(mnemonic)) => WalletSecret::Mnemonic(mnemonic.to_string()),
            Ok(None) => {
                return Err(CloudBackupError::Internal(format!(
                    "hot wallet '{name}' has no mnemonic"
                )));
            }
            Err(e) => {
                return Err(CloudBackupError::Internal(format!(
                    "failed to get mnemonic for '{name}': {e}"
                )));
            }
        },
        WalletType::Cold => {
            let is_tap_signer =
                metadata.hardware_metadata.as_ref().is_some_and(|hw| hw.is_tap_signer());

            if is_tap_signer {
                match keychain.get_tap_signer_backup(id) {
                    Ok(Some(backup)) => WalletSecret::TapSignerBackup(backup),
                    Ok(None) => {
                        warn!("Tap signer wallet '{name}' has no backup, exporting without it");
                        WalletSecret::WatchOnly
                    }
                    Err(e) => {
                        return Err(CloudBackupError::Internal(format!(
                            "failed to read tap signer backup for '{name}': {e}"
                        )));
                    }
                }
            } else {
                WalletSecret::WatchOnly
            }
        }
        WalletType::XpubOnly | WalletType::WatchOnly => WalletSecret::WatchOnly,
    };

    let xpub = match keychain.get_wallet_xpub(id) {
        Ok(Some(x)) => Some(x.to_string()),
        Ok(None) => None,
        Err(e) => {
            return Err(CloudBackupError::Internal(format!(
                "failed to read xpub for '{name}': {e}"
            )));
        }
    };

    let descriptors = match keychain.get_public_descriptor(id) {
        Ok(Some((ext, int))) => {
            Some(DescriptorPair { external: ext.to_string(), internal: int.to_string() })
        }
        Ok(None) => None,
        Err(e) => {
            return Err(CloudBackupError::Internal(format!(
                "failed to read descriptors for '{name}': {e}"
            )));
        }
    };

    let metadata_value = serde_json::to_value(metadata)
        .map_err_prefix("serialize metadata", CloudBackupError::Internal)?;

    let wallet_mode = match mode {
        LocalWalletMode::Main => WalletMode::Main,
        LocalWalletMode::Decoy => WalletMode::Decoy,
    };

    Ok(WalletEntry {
        wallet_id: id.to_string(),
        secret,
        metadata: metadata_value,
        descriptors,
        xpub,
        wallet_mode,
    })
}

/// Wipe all local encrypted databases (main db + per-wallet databases)
///
/// Used during "Start Fresh" flows. Shared across platforms via FFI.
/// Removes both current encrypted filenames and legacy plaintext filenames
#[uniffi::export]
pub fn wipe_local_data() {
    use crate::database::migration::log_remove_file;

    // best-effort: delete keychain secrets before removing DB files
    delete_all_wallet_keychain_items();

    let root = &*cove_common::consts::ROOT_DATA_DIR;

    // current encrypted DB
    log_remove_file(&root.join("cove.encrypted.db"));
    // legacy plaintext (if leftover)
    log_remove_file(&root.join("cove.db"));

    // BDK store files in the root data dir
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().starts_with("bdk_wallet") {
                log_remove_file(&entry.path());
            }
        }
    }

    // per-wallet databases
    let wallet_dir = &*cove_common::consts::WALLET_DATA_DIR;
    if wallet_dir.exists()
        && let Err(e) = std::fs::remove_dir_all(wallet_dir)
    {
        error!("Failed to remove wallet data dir: {e}");
    }
}

/// Re-open the database after wipe+re-bootstrap so `Database::global()`
/// returns a handle to the fresh file instead of the deleted one
#[uniffi::export]
pub fn reinit_database() {
    crate::database::wallet_data::DATABASE_CONNECTIONS.write().clear();
    Database::reinit();
}

#[uniffi::export]
pub fn cspp_master_key_record_id() -> String {
    MASTER_KEY_RECORD_ID.to_string()
}

#[uniffi::export]
pub fn cspp_manifest_record_id() -> String {
    MANIFEST_RECORD_ID.to_string()
}

/// Delete keychain items for all wallets across all networks and modes
///
/// Best-effort: if the database isn't initialized (e.g. key mismatch), skip
fn delete_all_wallet_keychain_items() {
    let Some(db_swap) = crate::database::DATABASE.get() else {
        warn!("Database not initialized, skipping keychain cleanup during wipe");
        return;
    };

    let db = db_swap.load();
    let keychain = Keychain::global();

    for wallet in all_local_wallets(&db) {
        keychain.delete_wallet_items(&wallet.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cove_cspp::backup_data::WalletSecret;

    #[test]
    fn convert_cloud_secret_mnemonic() {
        let secret = WalletSecret::Mnemonic("abandon".into());
        let result = convert_cloud_secret(&secret);
        assert!(
            matches!(result, crate::backup::model::WalletSecret::Mnemonic(ref m) if m == "abandon")
        );
    }

    #[test]
    fn convert_cloud_secret_tap_signer() {
        let secret = WalletSecret::TapSignerBackup(vec![1, 2, 3]);
        let result = convert_cloud_secret(&secret);
        assert!(
            matches!(result, crate::backup::model::WalletSecret::TapSignerBackup(ref b) if b == &[1, 2, 3])
        );
    }

    #[test]
    fn convert_cloud_secret_descriptor_to_none() {
        let secret = WalletSecret::Descriptor("wpkh(...)".into());
        let result = convert_cloud_secret(&secret);
        assert!(matches!(result, crate::backup::model::WalletSecret::None));
    }

    #[test]
    fn convert_cloud_secret_watch_only_to_none() {
        let result = convert_cloud_secret(&WalletSecret::WatchOnly);
        assert!(matches!(result, crate::backup::model::WalletSecret::None));
    }
}

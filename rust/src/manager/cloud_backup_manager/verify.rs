use cove_cspp::CsppStore as _;
use cove_cspp::master_key_crypto;
use cove_cspp::wallet_crypto;
use cove_device::cloud_storage::{CloudStorage, CloudStorageError};
use cove_device::keychain::Keychain;
use cove_device::passkey::PasskeyAccess;
use cove_util::ResultExt as _;
use rand::RngExt as _;
use tracing::{error, info, warn};
use zeroize::Zeroizing;

use super::pending::{build_detail_from_wallet_ids, cleanup_confirmed_pending_blobs};
use super::wallets::{all_local_wallets, count_all_wallets, create_prf_key_without_persisting};
use super::{
    CREDENTIAL_ID_KEY, CloudBackupError, CloudBackupState, DeepVerificationFailure,
    DeepVerificationReport, DeepVerificationResult, PRF_SALT_KEY, RP_ID, RustCloudBackupManager,
    cspp_master_key_record_id,
};
use crate::database::Database;
use crate::database::global_config::CloudBackup;

impl RustCloudBackupManager {
    /// Background startup health check for cloud backup integrity
    ///
    /// Verifies the master key is in the keychain and backup files exist in iCloud.
    /// Returns None if everything is OK, Some(warning) if there's a problem
    pub(super) fn verify_backup_integrity_impl(&self) -> Option<String> {
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

        let namespace = match self.current_namespace_id() {
            Ok(ns) => ns,
            Err(_) => {
                issues.push("namespace_id not found in keychain");
                return Some(issues.join("; "));
            }
        };

        let cloud = CloudStorage::global();
        if issues.is_empty() {
            match cloud.list_wallet_backups(namespace) {
                Ok(wallet_record_ids) => {
                    let db = Database::global();
                    let local_count = count_all_wallets(&db);
                    let cloud_count = wallet_record_ids.len() as u32;

                    if local_count > cloud_count {
                        info!(
                            "Backup integrity: {local_count} local wallets vs {cloud_count} in cloud, auto-syncing"
                        );
                        if let Err(error) = self.do_sync_unsynced_wallets() {
                            error!("Backup integrity: auto-sync failed: {error}");
                            issues.push("some wallets are not backed up");
                        }
                    }
                }
                Err(error) => {
                    warn!("Backup integrity: wallet list check failed: {error}");
                }
            }
        }

        if issues.is_empty() {
            info!("Backup integrity check passed");
            None
        } else {
            let message = issues.join("; ");
            error!("Backup integrity issues: {message}");
            Some(message)
        }
    }

    /// Deep verification of cloud backup integrity
    ///
    /// Checks state, runs do_deep_verify, wraps errors, persists result
    pub(crate) fn deep_verify_cloud_backup(&self) -> DeepVerificationResult {
        if !matches!(*self.state.read(), CloudBackupState::Enabled) {
            return DeepVerificationResult::NotEnabled;
        }

        let result = match self.do_deep_verify_cloud_backup() {
            Ok(result) => result,
            Err(error) => {
                error!("Deep verification unexpected error: {error}");
                DeepVerificationResult::Failed(DeepVerificationFailure::Retry {
                    message: error.to_string(),
                    detail: None,
                })
            }
        };

        self.persist_verification_result(&result);
        result
    }

    pub(crate) fn persist_verification_result(&self, result: &DeepVerificationResult) {
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
            && let Err(error) = db.global_config.set_cloud_backup(&new_state)
        {
            error!("Failed to persist verification state: {error}");
        }
    }

    pub(crate) fn do_repair_passkey_wrapper(&self) -> Result<(), CloudBackupError> {
        let keychain = Keychain::global();
        let cspp = cove_cspp::Cspp::new(keychain.clone());
        let cloud = CloudStorage::global();
        let passkey = PasskeyAccess::global();
        let namespace = self.current_namespace_id()?;

        let local_master_key = cspp
            .load_master_key_from_store()
            .map_err_prefix("load local master key", CloudBackupError::Internal)?
            .ok_or_else(|| CloudBackupError::Internal("no local master key".into()))?;

        let wallet_record_ids = match cloud.list_wallet_backups(namespace.clone()) {
            Ok(ids) => ids,
            Err(CloudStorageError::NotFound(_)) => Vec::new(),
            Err(error) => {
                return Err(CloudBackupError::Cloud(format!("list wallet backups: {error}")));
            }
        };

        if !wallet_record_ids.is_empty() {
            let critical_key = Zeroizing::new(local_master_key.critical_data_key());
            let mut proved = false;
            let mut had_wrong_key = false;

            for record_id in &wallet_record_ids {
                match cloud.download_wallet_backup(namespace.clone(), record_id.clone()) {
                    Ok(json) => {
                        let encrypted: cove_cspp::backup_data::EncryptedWalletBackup =
                            match serde_json::from_slice(&json) {
                                Ok(encrypted) => encrypted,
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

        let new_prf = create_prf_key_without_persisting(passkey)?;
        let encrypted_backup = master_key_crypto::encrypt_master_key(
            &local_master_key,
            &new_prf.prf_key,
            &new_prf.prf_salt,
        )
        .map_err_str(CloudBackupError::Crypto)?;

        let backup_json =
            serde_json::to_vec(&encrypted_backup).map_err_str(CloudBackupError::Internal)?;

        cloud
            .upload_master_key_backup(namespace.clone(), backup_json)
            .map_err_str(CloudBackupError::Cloud)?;

        keychain
            .save(CREDENTIAL_ID_KEY.into(), hex::encode(&new_prf.credential_id))
            .map_err_prefix("save credential", CloudBackupError::Internal)?;
        keychain
            .save(PRF_SALT_KEY.into(), hex::encode(new_prf.prf_salt))
            .map_err_prefix("save prf_salt", CloudBackupError::Internal)?;
        self.enqueue_pending_uploads(&namespace, std::iter::once(cspp_master_key_record_id()))?;

        info!("Repaired cloud master key wrapper with new passkey");
        Ok(())
    }

    pub(crate) fn do_deep_verify_cloud_backup(
        &self,
    ) -> Result<DeepVerificationResult, CloudBackupError> {
        let keychain = Keychain::global();
        let cspp = cove_cspp::Cspp::new(keychain.clone());
        let cloud = CloudStorage::global();
        let passkey = PasskeyAccess::global();
        let namespace = self.current_namespace_id()?;

        let mut report = DeepVerificationReport {
            master_key_wrapper_repaired: false,
            local_master_key_repaired: false,
            credential_recovered: false,
            wallets_verified: 0,
            wallets_failed: 0,
            wallets_unsupported: 0,
            detail: None,
        };

        let local_master_key = cspp
            .load_master_key_from_store()
            .map_err_prefix("load local master key", CloudBackupError::Internal)?;

        let mut wallets_missing = false;
        let wallet_record_ids = match cloud.list_wallet_backups(namespace.clone()) {
            Ok(ids) => {
                let listed: std::collections::HashSet<_> = ids.iter().cloned().collect();
                cleanup_confirmed_pending_blobs(&listed);

                report.detail = Some(build_detail_from_wallet_ids(&ids));
                Some(ids)
            }
            Err(CloudStorageError::NotFound(_)) => {
                wallets_missing = true;
                None
            }
            Err(error) => {
                return Ok(DeepVerificationResult::Failed(DeepVerificationFailure::Retry {
                    message: format!("failed to list wallet backups: {error}"),
                    detail: None,
                }));
            }
        };

        let encrypted_master = match cloud.download_master_key_backup(namespace.clone()) {
            Ok(json) => {
                let encrypted: cove_cspp::backup_data::EncryptedMasterKeyBackup =
                    serde_json::from_slice(&json).map_err_str(CloudBackupError::Internal)?;
                if encrypted.version != 1 {
                    return Ok(DeepVerificationResult::Failed(
                        DeepVerificationFailure::UnsupportedVersion {
                            message: format!(
                                "master key backup version {} is not supported",
                                encrypted.version
                            ),
                            detail: report.detail.clone(),
                        },
                    ));
                }
                Some(encrypted)
            }
            Err(CloudStorageError::NotFound(_)) => {
                if local_master_key.is_some() {
                    None
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
            Err(error) => {
                return Ok(DeepVerificationResult::Failed(DeepVerificationFailure::Retry {
                    message: format!("failed to download master key backup: {error}"),
                    detail: report.detail.clone(),
                }));
            }
        };

        let (verified_master_key, needs_wrapper_repair) = if let Some(ref encrypted_master) =
            encrypted_master
        {
            let prf_salt = encrypted_master.prf_salt;
            match authenticate_with_fallback(keychain, passkey, &prf_salt) {
                Ok((prf_key, credential_id, recovered)) => {
                    report.credential_recovered = recovered;
                    match master_key_crypto::decrypt_master_key(encrypted_master, &prf_key) {
                        Ok(master_key) => {
                            keychain
                                .save(CREDENTIAL_ID_KEY.into(), hex::encode(&credential_id))
                                .map_err_prefix("save credential_id", CloudBackupError::Internal)?;
                            keychain
                                .save(PRF_SALT_KEY.into(), hex::encode(prf_salt))
                                .map_err_prefix("save prf_salt", CloudBackupError::Internal)?;
                            (Some(master_key), false)
                        }
                        Err(_) => {
                            if local_master_key.is_some() {
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
                Err(CloudBackupError::Passkey(message)) if message == "user cancelled" => {
                    return Ok(DeepVerificationResult::UserCancelled(report.detail));
                }
                Err(CloudBackupError::Passkey(message))
                    if message.contains("no credential found")
                        || message.contains("NoCredentialFound") =>
                {
                    if local_master_key.is_some() {
                        (None, true)
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
                Err(error) => {
                    return Ok(DeepVerificationResult::Failed(DeepVerificationFailure::Retry {
                        message: format!("passkey authentication failed: {error}"),
                        detail: report.detail.clone(),
                    }));
                }
            }
        } else {
            (None, true)
        };

        let master_key = if let Some(master_key) = verified_master_key {
            match &local_master_key {
                None => {
                    cspp.save_master_key(&master_key)
                        .map_err_prefix("repair local master key", CloudBackupError::Internal)?;
                    cove_cspp::reset_master_key_cache();
                    report.local_master_key_repaired = true;
                    info!("Repaired local master key from cloud");
                }
                Some(local_key) if local_key.as_bytes() != master_key.as_bytes() => {
                    cspp.save_master_key(&master_key)
                        .map_err_prefix("repair local master key", CloudBackupError::Internal)?;
                    cove_cspp::reset_master_key_cache();
                    report.local_master_key_repaired = true;
                    info!("Repaired local master key to match cloud");
                }
                Some(_) => {}
            }

            if wallets_missing {
                return Ok(DeepVerificationResult::Failed(
                    DeepVerificationFailure::RecreateManifest {
                        message: "wallet backups not found in iCloud namespace".into(),
                        detail: report.detail.clone(),
                        warning: "Recreating from this device will remove references to wallets that only exist in the cloud backup".into(),
                    },
                ));
            }

            master_key
        } else if needs_wrapper_repair {
            let local_master_key = local_master_key.as_ref().expect("checked earlier");

            if let Some(ref ids) = wallet_record_ids
                && !ids.is_empty()
            {
                let critical_key = Zeroizing::new(local_master_key.critical_data_key());
                let mut proved = false;
                let mut had_wrong_key = false;

                for record_id in ids {
                    match cloud.download_wallet_backup(namespace.clone(), record_id.clone()) {
                        Ok(json) => {
                            let encrypted: cove_cspp::backup_data::EncryptedWalletBackup =
                                match serde_json::from_slice(&json) {
                                    Ok(encrypted) => encrypted,
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

            let new_prf = create_prf_key_without_persisting(passkey)?;
            let encrypted_backup = master_key_crypto::encrypt_master_key(
                local_master_key,
                &new_prf.prf_key,
                &new_prf.prf_salt,
            )
            .map_err_str(CloudBackupError::Crypto)?;

            let backup_json =
                serde_json::to_vec(&encrypted_backup).map_err_str(CloudBackupError::Internal)?;

            cloud
                .upload_master_key_backup(namespace.clone(), backup_json)
                .map_err_str(CloudBackupError::Cloud)?;

            keychain
                .save(CREDENTIAL_ID_KEY.into(), hex::encode(&new_prf.credential_id))
                .map_err_prefix("save credential", CloudBackupError::Internal)?;
            keychain
                .save(PRF_SALT_KEY.into(), hex::encode(new_prf.prf_salt))
                .map_err_prefix("save prf_salt", CloudBackupError::Internal)?;
            self.enqueue_pending_uploads(&namespace, std::iter::once(cspp_master_key_record_id()))?;

            report.master_key_wrapper_repaired = true;
            info!("Repaired cloud master key wrapper with new passkey");

            if wallets_missing {
                return Ok(DeepVerificationResult::Failed(
                    DeepVerificationFailure::RecreateManifest {
                        message: "wallet backups not found in iCloud namespace".into(),
                        detail: report.detail.clone(),
                        warning: "Recreating from this device will remove references to wallets that only exist in the cloud backup".into(),
                    },
                ));
            }

            cove_cspp::master_key::MasterKey::from_bytes(*local_master_key.as_bytes())
        } else {
            unreachable!("either verified_master_key or needs_wrapper_repair must be set");
        };

        if let Some(ref ids) = wallet_record_ids {
            let critical_key = Zeroizing::new(master_key.critical_data_key());
            let (verified, failed, unsupported) =
                verify_wallet_backups(cloud, &namespace, ids, &critical_key);
            report.wallets_verified = verified;
            report.wallets_failed = failed;
            report.wallets_unsupported = unsupported;

            let db = Database::global();
            let mut cloud_ids: std::collections::HashSet<String> = ids.iter().cloned().collect();

            if let Ok(Some(pending)) = db.cloud_backup_upload_verification.get() {
                let master_key_id = cspp_master_key_record_id();
                for blob in &pending.blobs {
                    if blob.record_id != master_key_id {
                        cloud_ids.insert(blob.record_id.clone());
                    }
                }
            }

            let unsynced: Vec<_> = all_local_wallets(&db)
                .into_iter()
                .filter(|wallet| {
                    !cloud_ids
                        .contains(&cove_cspp::backup_data::wallet_record_id(wallet.id.as_ref()))
                })
                .collect();

            if !unsynced.is_empty() {
                let count = unsynced.len() as u32;
                info!("Deep verify: {count} local wallet(s) not in cloud, auto-syncing");
                match self.do_backup_wallets(&unsynced) {
                    Ok(()) => {
                        if let Ok(updated_ids) = cloud.list_wallet_backups(namespace.clone()) {
                            report.detail = Some(build_detail_from_wallet_ids(&updated_ids));
                        }
                    }
                    Err(error) => {
                        warn!("Deep verify: auto-sync failed: {error}");
                    }
                }
            }
        }

        Ok(DeepVerificationResult::Verified(report))
    }
}

/// Verify wallet backups by downloading and decrypting each one
///
/// Returns (verified, failed, unsupported) counts
fn verify_wallet_backups(
    cloud: &CloudStorage,
    namespace: &str,
    wallet_record_ids: &[String],
    critical_key: &[u8; 32],
) -> (u32, u32, u32) {
    let mut verified = 0u32;
    let mut failed = 0u32;
    let mut unsupported = 0u32;

    for record_id in wallet_record_ids {
        let wallet_json =
            match cloud.download_wallet_backup(namespace.to_string(), record_id.clone()) {
                Ok(json) => json,
                Err(error) => {
                    warn!("Verify: failed to download wallet {record_id}: {error}");
                    failed += 1;
                    continue;
                }
            };

        let encrypted: cove_cspp::backup_data::EncryptedWalletBackup =
            match serde_json::from_slice(&wallet_json) {
                Ok(encrypted) => encrypted,
                Err(error) => {
                    warn!("Verify: failed to deserialize wallet {record_id}: {error}");
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
            Err(error) => {
                warn!("Verify: failed to decrypt wallet {record_id}: {error}");
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
    let stored_credential_id: Option<Vec<u8>> =
        keychain.get(CREDENTIAL_ID_KEY.into()).and_then(|hex_str| {
            hex::decode(hex_str)
                .inspect_err(|error| warn!("Failed to decode stored credential_id: {error}"))
                .ok()
        });

    if let Some(ref credential_id) = stored_credential_id {
        let challenge: Vec<u8> = rand::rng().random::<[u8; 32]>().to_vec();
        match passkey.authenticate_with_prf(
            RP_ID.to_string(),
            credential_id.clone(),
            prf_salt.to_vec(),
            challenge,
        ) {
            Ok(prf_output) => {
                let prf_key: [u8; 32] = prf_output
                    .try_into()
                    .map_err(|_| CloudBackupError::Internal("PRF output is not 32 bytes".into()))?;
                return Ok((prf_key, credential_id.clone(), false));
            }
            Err(cove_device::passkey::PasskeyError::UserCancelled) => {
                return Err(CloudBackupError::Passkey("user cancelled".into()));
            }
            Err(error) => {
                info!("Stored credential auth failed ({error}), trying discovery");
            }
        }
    }

    let challenge: Vec<u8> = rand::rng().random::<[u8; 32]>().to_vec();
    let discovered = passkey
        .discover_and_authenticate_with_prf(RP_ID.to_string(), prf_salt.to_vec(), challenge)
        .map_err(|error| match error {
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

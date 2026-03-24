use cove_cspp::CsppStore as _;
use cove_cspp::master_key_crypto;
use cove_device::cloud_storage::CloudStorage;
use cove_device::keychain::Keychain;
use cove_device::passkey::PasskeyAccess;
use cove_util::ResultExt as _;
use rand::RngExt as _;
use tracing::{info, warn};
use zeroize::Zeroizing;

use super::wallets::{
    all_local_wallets, obtain_prf_key, persist_enabled_cloud_backup_state, restore_single_wallet,
    upload_all_wallets,
};
use super::{
    CREDENTIAL_ID_KEY, CloudBackupError, CloudBackupReconcileMessage as Message,
    CloudBackupRestoreReport, CloudBackupState, CloudBackupWalletItem, CloudBackupWalletStatus,
    NAMESPACE_ID_KEY, PRF_SALT_KEY, RustCloudBackupManager,
};
use crate::database::Database;
use crate::database::global_config::CloudBackup;
use crate::wallet::metadata::WalletMetadata;

impl RustCloudBackupManager {
    pub(crate) fn do_sync_unsynced_wallets(&self) -> Result<(), CloudBackupError> {
        let namespace = self.current_namespace_id()?;
        info!("Sync: listing cloud wallet backups for namespace {namespace}");
        let cloud = CloudStorage::global();
        let cloud_record_ids: std::collections::HashSet<_> = cloud
            .list_wallet_backups(namespace)
            .map_err_str(CloudBackupError::Cloud)?
            .into_iter()
            .collect();

        let db = Database::global();
        let mut cloud_record_ids = cloud_record_ids;
        if let Ok(Some(pending)) = db.cloud_backup_upload_verification.get() {
            let master_key_id = super::cspp_master_key_record_id();
            for blob in &pending.blobs {
                if blob.record_id != master_key_id {
                    cloud_record_ids.insert(blob.record_id.clone());
                }
            }
        }

        info!("Sync: found {} wallet(s) in cloud (including pending)", cloud_record_ids.len());
        let unsynced: Vec<_> = all_local_wallets(&db)
            .into_iter()
            .filter(|wallet| {
                !cloud_record_ids
                    .contains(&cove_cspp::backup_data::wallet_record_id(wallet.id.as_ref()))
            })
            .collect();

        if unsynced.is_empty() {
            info!("Sync: all wallets already synced");
            return Ok(());
        }

        info!("Sync: {} wallet(s) need backup", unsynced.len());
        self.do_backup_wallets(&unsynced)
    }

    pub(crate) fn do_fetch_cloud_only_wallets(
        &self,
    ) -> Result<Vec<CloudBackupWalletItem>, CloudBackupError> {
        let namespace = self.current_namespace_id()?;
        let cloud = CloudStorage::global();
        let wallet_record_ids =
            cloud.list_wallet_backups(namespace.clone()).map_err_str(CloudBackupError::Cloud)?;

        let db = Database::global();
        let local_record_ids: std::collections::HashSet<_> = all_local_wallets(&db)
            .iter()
            .map(|wallet| cove_cspp::backup_data::wallet_record_id(wallet.id.as_ref()))
            .collect();

        let orphan_ids: Vec<_> = wallet_record_ids
            .iter()
            .filter(|record_id| !local_record_ids.contains(*record_id))
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
            let wallet_json =
                match cloud.download_wallet_backup(namespace.clone(), record_id.clone()) {
                    Ok(json) => json,
                    Err(error) => {
                        warn!("Failed to download cloud-only wallet {record_id}: {error}");
                        continue;
                    }
                };

            let encrypted: cove_cspp::backup_data::EncryptedWalletBackup =
                match serde_json::from_slice(&wallet_json) {
                    Ok(encrypted) => encrypted,
                    Err(error) => {
                        warn!("Failed to deserialize cloud-only wallet {record_id}: {error}");
                        continue;
                    }
                };

            let entry =
                match cove_cspp::wallet_crypto::decrypt_wallet_backup(&encrypted, &critical_key) {
                    Ok(entry) => entry,
                    Err(error) => {
                        warn!("Failed to decrypt cloud-only wallet {record_id}: {error}");
                        continue;
                    }
                };

            let metadata: WalletMetadata = match serde_json::from_value(entry.metadata.clone()) {
                Ok(metadata) => metadata,
                Err(error) => {
                    warn!("Failed to parse cloud-only wallet metadata {record_id}: {error}");
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

    pub(crate) fn do_restore_cloud_wallet(&self, record_id: &str) -> Result<(), CloudBackupError> {
        let namespace = self.current_namespace_id()?;
        let cloud = CloudStorage::global();
        let cspp = cove_cspp::Cspp::new(Keychain::global().clone());
        let master_key = cspp
            .get_or_create_master_key()
            .map_err_prefix("master key", CloudBackupError::Internal)?;
        let critical_key = Zeroizing::new(master_key.critical_data_key());

        let db = Database::global();
        let mut existing_fingerprints: Vec<_> = all_local_wallets(&db)
            .iter()
            .filter_map(|wallet| {
                wallet
                    .master_fingerprint
                    .as_ref()
                    .map(|fp| (**fp, wallet.network, wallet.wallet_mode))
            })
            .collect();

        restore_single_wallet(
            cloud,
            &namespace,
            record_id,
            &critical_key,
            &mut existing_fingerprints,
        )?;
        info!("Restored cloud wallet {record_id}");
        Ok(())
    }

    pub(crate) fn do_delete_cloud_wallet(&self, record_id: &str) -> Result<(), CloudBackupError> {
        let namespace = self.current_namespace_id()?;
        let cloud = CloudStorage::global();

        cloud
            .delete_wallet_backup(namespace.clone(), record_id.to_string())
            .map_err_str(CloudBackupError::Cloud)?;
        self.remove_pending_uploads(&namespace, std::iter::once(record_id.to_string()))?;

        let wallet_record_ids =
            cloud.list_wallet_backups(namespace).map_err_str(CloudBackupError::Cloud)?;
        let wallet_count = wallet_record_ids.len() as u32;
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

    /// Re-upload all local wallets to cloud
    ///
    /// Reuses the master key from keychain (no passkey interaction needed)
    pub(crate) fn do_reupload_all_wallets(&self) -> Result<(), CloudBackupError> {
        info!("Re-uploading all wallets to cloud");

        let namespace = self.current_namespace_id()?;
        let cspp = cove_cspp::Cspp::new(Keychain::global().clone());
        let master_key = cspp
            .get_or_create_master_key()
            .map_err_prefix("master key", CloudBackupError::Internal)?;

        let critical_key = Zeroizing::new(master_key.critical_data_key());
        let cloud = CloudStorage::global();
        let db = Database::global();

        let uploaded_record_ids = upload_all_wallets(cloud, &namespace, &critical_key, &db)?;
        persist_enabled_cloud_backup_state(&db, uploaded_record_ids.len() as u32)?;
        self.enqueue_pending_uploads(&namespace, uploaded_record_ids)?;

        Ok(())
    }

    pub(crate) fn do_enable_cloud_backup(&self) -> Result<(), CloudBackupError> {
        self.send(Message::StateChanged(CloudBackupState::Enabling));

        let passkey = PasskeyAccess::global();
        if !passkey.is_prf_supported() {
            return Err(CloudBackupError::NotSupported(
                "PRF extension not supported on this device".into(),
            ));
        }

        info!("Enable: getting master key");
        let cspp = cove_cspp::Cspp::new(Keychain::global().clone());
        let master_key = cspp
            .get_or_create_master_key()
            .map_err_prefix("master key", CloudBackupError::Internal)?;

        let namespace_id = master_key.namespace_id();
        info!("Enable: namespace_id={namespace_id}, creating passkey");
        let keychain = Keychain::global();
        let (prf_key, prf_salt) = obtain_prf_key(keychain, passkey)?;

        info!("Enable: passkey created, encrypting master key");
        let encrypted_master =
            master_key_crypto::encrypt_master_key(&master_key, &prf_key, &prf_salt)
                .map_err_str(CloudBackupError::Crypto)?;

        let master_json =
            serde_json::to_vec(&encrypted_master).map_err_str(CloudBackupError::Internal)?;

        info!("Enable: uploading master key");
        let cloud = CloudStorage::global();
        cloud
            .upload_master_key_backup(namespace_id.clone(), master_json)
            .map_err_str(CloudBackupError::Cloud)?;

        info!("Enable: master key uploaded, uploading wallets");
        let critical_key = Zeroizing::new(master_key.critical_data_key());
        let db = Database::global();
        let uploaded_wallet_record_ids =
            upload_all_wallets(cloud, &namespace_id, &critical_key, &db)?;

        info!("Enable: wallets uploaded, persisting state");
        keychain
            .save(NAMESPACE_ID_KEY.into(), namespace_id.clone())
            .map_err_prefix("save namespace_id", CloudBackupError::Internal)?;
        persist_enabled_cloud_backup_state(&db, uploaded_wallet_record_ids.len() as u32)?;
        self.enqueue_pending_uploads(
            &namespace_id,
            std::iter::once(super::cspp_master_key_record_id()).chain(uploaded_wallet_record_ids),
        )?;

        self.send(Message::EnableComplete);
        self.send(Message::StateChanged(CloudBackupState::Enabled));

        info!("Cloud backup enabled successfully");
        Ok(())
    }

    pub(super) fn do_restore_from_cloud_backup(&self) -> Result<(), CloudBackupError> {
        self.send(Message::StateChanged(CloudBackupState::Restoring));
        info!("Restore: listing namespaces");

        let cloud = CloudStorage::global();
        let passkey = PasskeyAccess::global();
        let keychain = Keychain::global();
        let cspp = cove_cspp::Cspp::new(keychain.clone());

        let namespaces = cloud.list_namespaces().map_err_str(CloudBackupError::Cloud)?;
        if namespaces.is_empty() {
            return Err(CloudBackupError::Internal("no cloud backup namespaces found".into()));
        }

        info!("Restore: authenticating with passkey");
        let first_namespace = &namespaces[0];
        let first_master_json = cloud
            .download_master_key_backup(first_namespace.clone())
            .map_err_str(CloudBackupError::Cloud)?;
        let first_encrypted: cove_cspp::backup_data::EncryptedMasterKeyBackup =
            serde_json::from_slice(&first_master_json).map_err_str(CloudBackupError::Internal)?;

        if first_encrypted.version != 1 {
            return Err(CloudBackupError::Internal(format!(
                "unsupported master key backup version: {}",
                first_encrypted.version
            )));
        }

        let prf_salt = first_encrypted.prf_salt;
        let challenge: Vec<u8> = rand::rng().random::<[u8; 32]>().to_vec();
        let discovered = passkey
            .discover_and_authenticate_with_prf(
                super::RP_ID.to_string(),
                prf_salt.to_vec(),
                challenge,
            )
            .map_err_str(CloudBackupError::Passkey)?;

        let prf_key: [u8; 32] = discovered
            .prf_output
            .try_into()
            .map_err(|_| CloudBackupError::Internal("PRF output is not 32 bytes".into()))?;

        let mut matched_namespace: Option<String> = None;
        let mut master_key: Option<cove_cspp::master_key::MasterKey> = None;

        for namespace in &namespaces {
            let master_json = if namespace == first_namespace {
                first_master_json.clone()
            } else {
                match cloud.download_master_key_backup(namespace.clone()) {
                    Ok(json) => json,
                    Err(error) => {
                        warn!("Failed to download master key for namespace {namespace}: {error}");
                        continue;
                    }
                }
            };

            let encrypted: cove_cspp::backup_data::EncryptedMasterKeyBackup =
                match serde_json::from_slice(&master_json) {
                    Ok(encrypted) => encrypted,
                    Err(error) => {
                        warn!(
                            "Failed to deserialize master key for namespace {namespace}: {error}"
                        );
                        continue;
                    }
                };

            if encrypted.version != 1 {
                continue;
            }

            match cove_cspp::master_key_crypto::decrypt_master_key(&encrypted, &prf_key) {
                Ok(candidate) => {
                    info!("Restore: found matching namespace {namespace}");
                    matched_namespace = Some(namespace.clone());
                    master_key = Some(candidate);
                    break;
                }
                Err(_) => continue,
            }
        }

        let matched_namespace = matched_namespace.ok_or(CloudBackupError::PasskeyMismatch)?;
        let master_key = master_key.expect("matched namespace should set master key");

        let local_master_key = cspp
            .load_master_key_from_store()
            .map_err_prefix("load local master key", CloudBackupError::Internal)?;

        let is_fresh_device = local_master_key.is_none();
        if is_fresh_device {
            cspp.save_master_key(&master_key)
                .map_err_prefix("save master key", CloudBackupError::Internal)?;
            cove_cspp::reset_master_key_cache();
        }

        let wallet_record_ids = cloud
            .list_wallet_backups(matched_namespace.clone())
            .map_err_str(CloudBackupError::Cloud)?;

        let total = wallet_record_ids.len() as u32;
        let critical_key = Zeroizing::new(master_key.critical_data_key());
        let mut report = CloudBackupRestoreReport {
            wallets_restored: 0,
            wallets_failed: 0,
            failed_wallet_errors: Vec::new(),
        };

        let mut existing_fingerprints = crate::backup::import::collect_existing_fingerprints()
            .map_err_prefix("collect fingerprints", CloudBackupError::Internal)?;

        for (index, record_id) in wallet_record_ids.iter().enumerate() {
            match restore_single_wallet(
                cloud,
                &matched_namespace,
                record_id,
                &critical_key,
                &mut existing_fingerprints,
            ) {
                Ok(()) => report.wallets_restored += 1,
                Err(error) => {
                    warn!("Failed to restore wallet {record_id}: {error}");
                    report.wallets_failed += 1;
                    report.failed_wallet_errors.push(error.to_string());
                }
            }

            self.send(Message::ProgressUpdated { completed: (index + 1) as u32, total });
        }

        if report.wallets_restored == 0 && report.wallets_failed > 0 {
            self.send(Message::RestoreComplete(report));
            return Err(CloudBackupError::Internal("all wallets failed to restore".into()));
        }

        if is_fresh_device {
            keychain
                .save(NAMESPACE_ID_KEY.to_string(), matched_namespace)
                .map_err_prefix("save namespace_id", CloudBackupError::Internal)?;
            keychain
                .save(CREDENTIAL_ID_KEY.to_string(), hex::encode(&discovered.credential_id))
                .map_err_prefix("save credential_id", CloudBackupError::Internal)?;
            keychain
                .save(PRF_SALT_KEY.to_string(), hex::encode(prf_salt))
                .map_err_prefix("save prf_salt", CloudBackupError::Internal)?;
        }

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

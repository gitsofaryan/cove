use std::collections::HashSet;
use std::sync::atomic::Ordering;

use cove_cspp::backup_data::wallet_record_id;
use cove_device::cloud_storage::{CloudStorage, CloudStorageError};
use cove_util::ResultExt as _;
use tracing::{error, info, warn};

use super::{
    CLOUD_BACKUP_MANAGER, CloudBackupDetail, CloudBackupDetailResult, CloudBackupError,
    CloudBackupReconcileMessage, CloudBackupState, CloudBackupWalletItem, CloudBackupWalletStatus,
    Message, RustCloudBackupManager, UPLOAD_VERIFICATION_INTERVAL, cspp_master_key_record_id,
};
use crate::database::Database;
use crate::database::cloud_backup_upload_verification::{
    PendingCloudUploadBlob, PendingCloudUploadVerification,
};
use crate::database::global_config::CloudBackup;
use crate::manager::cloud_backup_manager::wallets::all_local_wallets;

impl RustCloudBackupManager {
    /// List wallet backups in the current namespace and build detail
    ///
    /// Returns None if disabled. On NotFound, re-uploads all wallets automatically.
    /// On other errors, returns AccessError so the UI can offer a re-upload button
    pub(crate) fn refresh_cloud_backup_detail(&self) -> Option<CloudBackupDetailResult> {
        let state = self.state.read().clone();
        if !matches!(state, CloudBackupState::Enabled) {
            info!("refresh_cloud_backup_detail: skipping, state={state:?}");
            return None;
        }

        let namespace = match self.current_namespace_id() {
            Ok(ns) => ns,
            Err(e) => return Some(CloudBackupDetailResult::AccessError(e.to_string())),
        };

        info!("refresh_cloud_backup_detail: listing wallets for namespace {namespace}");
        let cloud = CloudStorage::global();
        let wallet_record_ids = match cloud.list_wallet_backups(namespace) {
            Ok(ids) => ids,
            Err(CloudStorageError::NotFound(_)) => {
                info!("No wallet backups found in namespace, re-uploading all wallets");
                if let Err(e) = self.do_reupload_all_wallets() {
                    return Some(CloudBackupDetailResult::AccessError(format!(
                        "Failed to re-upload wallets: {e}"
                    )));
                }

                match cloud.list_wallet_backups(self.current_namespace_id().unwrap_or_default()) {
                    Ok(ids) => ids,
                    Err(e) => return Some(CloudBackupDetailResult::AccessError(e.to_string())),
                }
            }
            Err(e) => return Some(CloudBackupDetailResult::AccessError(e.to_string())),
        };

        info!(
            "refresh_cloud_backup_detail: found {} wallet record(s) in cloud",
            wallet_record_ids.len()
        );

        let listed: HashSet<_> = wallet_record_ids.iter().cloned().collect();
        cleanup_confirmed_pending_blobs(&listed);

        Some(CloudBackupDetailResult::Success(build_detail_from_wallet_ids(&wallet_record_ids)))
    }

    pub(super) fn enqueue_pending_uploads<I>(
        &self,
        namespace_id: &str,
        record_ids: I,
    ) -> Result<(), CloudBackupError>
    where
        I: IntoIterator<Item = String>,
    {
        let db = Database::global();
        let table = &db.cloud_backup_upload_verification;
        let now = jiff::Timestamp::now().as_second().try_into().unwrap_or(0);

        let mut pending = match table
            .get()
            .map_err_prefix("read pending cloud upload verification", CloudBackupError::Internal)?
        {
            Some(existing) if existing.namespace_id == namespace_id => existing,
            _ => PendingCloudUploadVerification {
                namespace_id: namespace_id.to_string(),
                blobs: Vec::new(),
            },
        };

        let mut known_record_ids: HashSet<String> =
            pending.blobs.iter().map(|blob| blob.record_id.clone()).collect();

        for record_id in record_ids {
            if known_record_ids.insert(record_id.clone()) {
                pending.blobs.push(PendingCloudUploadBlob {
                    record_id,
                    enqueued_at: now,
                    last_checked_at: None,
                    attempt_count: 0,
                    confirmed_at: None,
                });
            }
        }

        if pending.blobs.is_empty() {
            return Ok(());
        }

        table.set(&pending).map_err_prefix(
            "persist pending cloud upload verification",
            CloudBackupError::Internal,
        )?;

        self.send(Message::PendingUploadVerificationChanged { pending: true });
        self.start_pending_upload_verification_loop();

        Ok(())
    }

    pub(super) fn remove_pending_uploads<I>(
        &self,
        namespace_id: &str,
        record_ids: I,
    ) -> Result<(), CloudBackupError>
    where
        I: IntoIterator<Item = String>,
    {
        let db = Database::global();
        let table = &db.cloud_backup_upload_verification;
        let Some(mut pending) = table
            .get()
            .map_err_prefix("read pending cloud upload verification", CloudBackupError::Internal)?
        else {
            return Ok(());
        };

        if pending.namespace_id != namespace_id {
            return Ok(());
        }

        let record_ids: HashSet<String> = record_ids.into_iter().collect();
        pending.blobs.retain(|blob| !record_ids.contains(&blob.record_id));

        if pending.blobs.is_empty() {
            table.delete().map_err_prefix(
                "clear pending cloud upload verification",
                CloudBackupError::Internal,
            )?;
            self.send(Message::PendingUploadVerificationChanged { pending: false });
            return Ok(());
        }

        table.set(&pending).map_err_prefix(
            "persist pending cloud upload verification",
            CloudBackupError::Internal,
        )?;
        self.send(Message::PendingUploadVerificationChanged { pending: true });

        Ok(())
    }

    pub(super) fn start_pending_upload_verification_loop(&self) {
        if self
            .pending_upload_verifier_running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }

        let this = CLOUD_BACKUP_MANAGER.clone();
        cove_tokio::task::spawn(async move {
            info!("Pending upload verification: started");

            loop {
                let this_for_pass = this.clone();
                let has_pending = cove_tokio::task::spawn_blocking(move || {
                    this_for_pass.verify_pending_uploads_once()
                })
                .await
                .unwrap_or_else(|error| {
                    error!("Pending upload verification task failed: {error}");
                    true
                });

                if !has_pending {
                    break;
                }

                tokio::time::sleep(UPLOAD_VERIFICATION_INTERVAL).await;
            }

            this.pending_upload_verifier_running.store(false, Ordering::SeqCst);

            if this.has_pending_cloud_upload_verification() {
                this.start_pending_upload_verification_loop();
                return;
            }

            info!("Pending upload verification: idle");
        });
    }

    fn verify_pending_uploads_once(&self) -> bool {
        let db = Database::global();
        let table = &db.cloud_backup_upload_verification;
        let pending = match table.get() {
            Ok(pending) => pending,
            Err(error) => {
                error!("Pending upload verification: failed to read queue: {error}");
                return true;
            }
        };
        let Some(mut pending) = pending else {
            self.send(Message::PendingUploadVerificationChanged { pending: false });
            return false;
        };

        if pending.blobs.is_empty() {
            if let Err(error) = table.delete() {
                error!("Pending upload verification: failed to delete empty queue: {error}");
                return true;
            }
            self.send(Message::PendingUploadVerificationChanged { pending: false });
            return false;
        }

        if !pending.has_unconfirmed() {
            self.send(Message::PendingUploadVerificationChanged { pending: false });
            return false;
        }

        let cloud = CloudStorage::global();
        let checked_at: u64 = jiff::Timestamp::now().as_second().try_into().unwrap_or(0);
        let namespace_id = pending.namespace_id.clone();
        for blob in &mut pending.blobs {
            if blob.confirmed_at.is_some() {
                continue;
            }

            match cloud.is_backup_uploaded(namespace_id.clone(), blob.record_id.clone()) {
                Ok(true) => {
                    let elapsed_secs = checked_at.saturating_sub(blob.enqueued_at);
                    info!(
                        "Pending upload verification: confirmed record_id={} elapsed={elapsed_secs}s attempts={}",
                        blob.record_id, blob.attempt_count
                    );
                    blob.confirmed_at = Some(checked_at);
                }
                Ok(false) => {
                    blob.last_checked_at = Some(checked_at);
                    blob.attempt_count += 1;
                    info!(
                        "Pending upload verification: not yet uploaded record_id={} attempts={}",
                        blob.record_id, blob.attempt_count
                    );
                }
                Err(error) => {
                    blob.last_checked_at = Some(checked_at);
                    blob.attempt_count += 1;
                    warn!(
                        "Pending upload verification: check failed record_id={} error={error} attempts={}",
                        blob.record_id, blob.attempt_count
                    );
                }
            }
        }

        if let Err(error) = table.set(&pending) {
            error!("Pending upload verification: failed to persist queue: {error}");
            return true;
        }

        let has_unconfirmed = pending.has_unconfirmed();
        if has_unconfirmed {
            let unconfirmed = pending.blobs.iter().filter(|b| b.confirmed_at.is_none()).count();
            self.send(Message::PendingUploadVerificationChanged { pending: true });
            info!("Pending upload verification: still pending count={unconfirmed}");
        } else {
            self.send(Message::PendingUploadVerificationChanged { pending: false });
            info!("Pending upload verification: all blobs confirmed");
        }

        has_unconfirmed
    }
}

/// Remove confirmed pending blobs that now appear in the cloud listing
pub(super) fn cleanup_confirmed_pending_blobs(listed_ids: &HashSet<String>) {
    let db = Database::global();
    let table = &db.cloud_backup_upload_verification;
    let mut pending = match table.get() {
        Ok(Some(p)) => p,
        _ => return,
    };

    let before = pending.blobs.len();
    pending.cleanup_listed(listed_ids);

    if pending.blobs.len() < before {
        if pending.blobs.is_empty() {
            let _ = table.delete();
            CLOUD_BACKUP_MANAGER.send(
                CloudBackupReconcileMessage::PendingUploadVerificationChanged { pending: false },
            );
        } else {
            let _ = table.set(&pending);
        }
    }
}

/// Build a CloudBackupDetail from wallet record IDs by comparing against local wallets
pub(super) fn build_detail_from_wallet_ids(wallet_record_ids: &[String]) -> CloudBackupDetail {
    let db = Database::global();
    let last_sync = match db.global_config.cloud_backup() {
        CloudBackup::Enabled { last_sync, .. } | CloudBackup::Unverified { last_sync, .. } => {
            last_sync
        }
        CloudBackup::Disabled => None,
    };

    let mut cloud_record_ids: HashSet<_> = wallet_record_ids.iter().cloned().collect();

    if let Ok(Some(pending)) = db.cloud_backup_upload_verification.get() {
        let master_key_id = cspp_master_key_record_id();
        for blob in &pending.blobs {
            if blob.record_id != master_key_id {
                cloud_record_ids.insert(blob.record_id.clone());
            }
        }
    }

    let local_wallets = all_local_wallets(&db);
    let local_record_ids: HashSet<_> =
        local_wallets.iter().map(|w| wallet_record_id(w.id.as_ref())).collect();

    let mut backed_up = Vec::new();
    let mut not_backed_up = Vec::new();

    for wallet in &local_wallets {
        let record_id = wallet_record_id(wallet.id.as_ref());
        let status = if cloud_record_ids.contains(&record_id) {
            CloudBackupWalletStatus::BackedUp
        } else {
            CloudBackupWalletStatus::NotBackedUp
        };

        let item = CloudBackupWalletItem {
            name: wallet.name.clone(),
            network: wallet.network,
            wallet_mode: wallet.wallet_mode,
            wallet_type: wallet.wallet_type,
            fingerprint: wallet.master_fingerprint.as_ref().map(|fp| fp.as_uppercase()),
            status,
            record_id: None,
        };

        match item.status {
            CloudBackupWalletStatus::BackedUp => backed_up.push(item),
            CloudBackupWalletStatus::NotBackedUp => not_backed_up.push(item),
            CloudBackupWalletStatus::DeletedFromDevice => {}
        }
    }

    let cloud_only_count =
        cloud_record_ids.iter().filter(|rid| !local_record_ids.contains(*rid)).count() as u32;

    CloudBackupDetail { last_sync, backed_up, not_backed_up, cloud_only_count }
}

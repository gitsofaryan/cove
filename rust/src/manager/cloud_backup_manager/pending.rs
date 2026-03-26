mod detail;
mod queue_processor;

use std::collections::HashSet;
use std::sync::atomic::Ordering;

use cove_util::ResultExt as _;
use tracing::{error, info};

use self::queue_processor::PendingUploadVerifier;
use super::{
    CLOUD_BACKUP_MANAGER, CloudBackupError, Message, RustCloudBackupManager,
    UPLOAD_VERIFICATION_INTERVAL,
};
use crate::database::Database;
use crate::database::cloud_backup_upload_verification::{
    PendingCloudUploadBlob, PendingCloudUploadVerification,
};

pub(crate) use detail::{build_detail_from_wallet_ids, cleanup_confirmed_pending_blobs};

impl RustCloudBackupManager {
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
        PendingUploadVerifier(self).run_once()
    }
}

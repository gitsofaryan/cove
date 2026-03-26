use cove_device::cloud_storage::CloudStorage;
use tracing::{error, info, warn};

use super::super::{Message, RustCloudBackupManager};
use crate::database::Database;
use crate::database::cloud_backup_upload_verification::{
    CloudBackupUploadVerificationTable, PendingCloudUploadBlob, PendingCloudUploadVerification,
};

enum BlobCheckResult {
    Confirmed,
    NotYetUploaded,
    Failed(String),
}

pub(super) struct PendingUploadVerifier<'a>(pub(super) &'a RustCloudBackupManager);

impl PendingUploadVerifier<'_> {
    pub(super) fn run_once(&self) -> bool {
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
            self.send_pending_state(false);
            return false;
        };

        if let Some(should_continue) = self.handle_terminal_state(table, &pending) {
            return should_continue;
        }

        self.verify_blobs(&mut pending);

        if let Err(error) = table.set(&pending) {
            error!("Pending upload verification: failed to persist queue: {error}");
            return true;
        }

        self.finish_pass(&pending)
    }

    fn handle_terminal_state(
        &self,
        table: &CloudBackupUploadVerificationTable,
        pending: &PendingCloudUploadVerification,
    ) -> Option<bool> {
        if pending.blobs.is_empty() {
            if let Err(error) = table.delete() {
                error!("Pending upload verification: failed to delete empty queue: {error}");
                return Some(true);
            }

            self.send_pending_state(false);
            return Some(false);
        }

        if !pending.has_unconfirmed() {
            self.send_pending_state(false);
            return Some(false);
        }

        None
    }

    fn verify_blobs(&self, pending: &mut PendingCloudUploadVerification) {
        let cloud = CloudStorage::global();
        let checked_at: u64 = jiff::Timestamp::now().as_second().try_into().unwrap_or(0);
        let namespace_id = pending.namespace_id.clone();

        for blob in &mut pending.blobs {
            if blob.confirmed_at.is_some() {
                continue;
            }

            let result = self.check_blob(cloud, &namespace_id, &blob.record_id);
            Self::apply_blob_result(blob, checked_at, &result);
            self.log_blob_result(blob, checked_at, &result);
        }
    }

    fn check_blob(
        &self,
        cloud: &CloudStorage,
        namespace_id: &str,
        record_id: &str,
    ) -> BlobCheckResult {
        match cloud.is_backup_uploaded(namespace_id.to_string(), record_id.to_string()) {
            Ok(true) => BlobCheckResult::Confirmed,
            Ok(false) => BlobCheckResult::NotYetUploaded,
            Err(error) => BlobCheckResult::Failed(error.to_string()),
        }
    }

    fn apply_blob_result(
        blob: &mut PendingCloudUploadBlob,
        checked_at: u64,
        result: &BlobCheckResult,
    ) {
        match result {
            BlobCheckResult::Confirmed => {
                blob.confirmed_at = Some(checked_at);
            }
            BlobCheckResult::NotYetUploaded | BlobCheckResult::Failed(_) => {
                blob.last_checked_at = Some(checked_at);
                blob.attempt_count += 1;
            }
        }
    }

    fn log_blob_result(
        &self,
        blob: &PendingCloudUploadBlob,
        checked_at: u64,
        result: &BlobCheckResult,
    ) {
        match result {
            BlobCheckResult::Confirmed => {
                let elapsed_secs = checked_at.saturating_sub(blob.enqueued_at);
                info!(
                    "Pending upload verification: confirmed record_id={} elapsed={elapsed_secs}s attempts={}",
                    blob.record_id, blob.attempt_count
                );
            }
            BlobCheckResult::NotYetUploaded => {
                info!(
                    "Pending upload verification: not yet uploaded record_id={} attempts={}",
                    blob.record_id, blob.attempt_count
                );
            }
            BlobCheckResult::Failed(error) => {
                warn!(
                    "Pending upload verification: check failed record_id={} error={error} attempts={}",
                    blob.record_id, blob.attempt_count
                );
            }
        }
    }

    fn finish_pass(&self, pending: &PendingCloudUploadVerification) -> bool {
        let has_unconfirmed = pending.has_unconfirmed();
        if has_unconfirmed {
            let unconfirmed =
                pending.blobs.iter().filter(|blob| blob.confirmed_at.is_none()).count();
            self.send_pending_state(true);
            info!("Pending upload verification: still pending count={unconfirmed}");
        } else {
            self.send_pending_state(false);
            info!("Pending upload verification: all blobs confirmed");
        }

        has_unconfirmed
    }

    fn send_pending_state(&self, pending: bool) {
        self.0.send(Message::PendingUploadVerificationChanged { pending });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_blob_result_confirms_blob() {
        let mut blob = PendingCloudUploadBlob {
            record_id: "wallet-a".into(),
            enqueued_at: 10,
            last_checked_at: None,
            attempt_count: 0,
            confirmed_at: None,
        };

        PendingUploadVerifier::apply_blob_result(&mut blob, 20, &BlobCheckResult::Confirmed);

        assert_eq!(blob.confirmed_at, Some(20));
        assert_eq!(blob.last_checked_at, None);
        assert_eq!(blob.attempt_count, 0);
    }

    #[test]
    fn apply_blob_result_tracks_pending_blob() {
        let mut blob = PendingCloudUploadBlob {
            record_id: "wallet-a".into(),
            enqueued_at: 10,
            last_checked_at: None,
            attempt_count: 0,
            confirmed_at: None,
        };

        PendingUploadVerifier::apply_blob_result(&mut blob, 20, &BlobCheckResult::NotYetUploaded);

        assert_eq!(blob.confirmed_at, None);
        assert_eq!(blob.last_checked_at, Some(20));
        assert_eq!(blob.attempt_count, 1);
    }

    #[test]
    fn apply_blob_result_tracks_failed_blob() {
        let mut blob = PendingCloudUploadBlob {
            record_id: "wallet-a".into(),
            enqueued_at: 10,
            last_checked_at: None,
            attempt_count: 0,
            confirmed_at: None,
        };

        PendingUploadVerifier::apply_blob_result(
            &mut blob,
            20,
            &BlobCheckResult::Failed("boom".into()),
        );

        assert_eq!(blob.confirmed_at, None);
        assert_eq!(blob.last_checked_at, Some(20));
        assert_eq!(blob.attempt_count, 1);
    }
}

mod detail;
mod queue_processor;

use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::time::Duration;

use backon::{BackoffBuilder as _, FibonacciBackoff, FibonacciBuilder};
use cove_util::ResultExt as _;
use tracing::{error, info};

use self::queue_processor::PendingUploadVerifier;
use super::{CLOUD_BACKUP_MANAGER, CloudBackupError, RustCloudBackupManager};
use crate::database::Database;
use crate::database::cloud_backup::{
    CloudUploadKind, CloudUploadVerificationState, PendingCloudUploadItem,
};

pub(crate) use detail::cleanup_confirmed_pending_blobs;

const MAX_PENDING_UPLOAD_VERIFICATION_DELAY: Duration = Duration::from_secs(10);

struct PendingUploadRetryBackoff(FibonacciBackoff);

impl PendingUploadRetryBackoff {
    fn new() -> Self {
        Self(build_pending_upload_backoff())
    }

    fn next_delay(&mut self) -> Duration {
        self.0
            .next()
            .map(|delay| delay.min(MAX_PENDING_UPLOAD_VERIFICATION_DELAY))
            .unwrap_or(MAX_PENDING_UPLOAD_VERIFICATION_DELAY)
    }

    fn reset(&mut self) {
        self.0 = build_pending_upload_backoff();
    }
}

fn build_pending_upload_backoff() -> FibonacciBackoff {
    FibonacciBuilder::default()
        .with_max_delay(MAX_PENDING_UPLOAD_VERIFICATION_DELAY)
        .without_max_times()
        .build()
}

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
        let table = &db.cloud_upload_queue;
        let now = jiff::Timestamp::now().as_second().try_into().unwrap_or(0);

        let mut pending = table
            .get()
            .map_err_prefix("read pending cloud upload queue", CloudBackupError::Internal)?
            .unwrap_or_default();

        let mut known_record_ids: HashSet<String> = pending
            .items
            .iter()
            .filter(|item| {
                item.kind == CloudUploadKind::BackupBlob
                    && item.namespace_id == namespace_id
                    && !item.is_confirmed()
            })
            .map(|item| item.record_id.clone())
            .collect();

        for record_id in record_ids {
            if let Some(existing_item) = pending.items.iter_mut().find(|item| {
                item.kind == CloudUploadKind::BackupBlob
                    && item.namespace_id == namespace_id
                    && item.record_id == record_id
                    && item.is_confirmed()
            }) {
                existing_item.enqueued_at = now;
                existing_item.verification = CloudUploadVerificationState::Pending {
                    attempt_count: 0,
                    last_checked_at: None,
                };
                known_record_ids.insert(record_id);
                continue;
            }

            if known_record_ids.insert(record_id.clone()) {
                pending.items.push(PendingCloudUploadItem {
                    kind: CloudUploadKind::BackupBlob,
                    namespace_id: namespace_id.to_string(),
                    record_id,
                    enqueued_at: now,
                    verification: CloudUploadVerificationState::Pending {
                        attempt_count: 0,
                        last_checked_at: None,
                    },
                });
            }
        }

        if pending.items.is_empty() {
            return Ok(());
        }

        table
            .set(&pending)
            .map_err_prefix("persist pending cloud upload queue", CloudBackupError::Internal)?;

        self.set_pending_upload_verification(true);
        self.wake_pending_upload_verifier();
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
        let table = &db.cloud_upload_queue;
        let Some(mut pending) = table
            .get()
            .map_err_prefix("read pending cloud upload queue", CloudBackupError::Internal)?
        else {
            return Ok(());
        };

        let record_ids: HashSet<String> = record_ids.into_iter().collect();
        pending.items.retain(|item| {
            !(item.kind == CloudUploadKind::BackupBlob
                && item.namespace_id == namespace_id
                && record_ids.contains(&item.record_id))
        });

        if pending.items.is_empty() {
            table
                .delete()
                .map_err_prefix("clear pending cloud upload queue", CloudBackupError::Internal)?;
            self.set_pending_upload_verification(false);
            self.wake_pending_upload_verifier();
            return Ok(());
        }

        table
            .set(&pending)
            .map_err_prefix("persist pending cloud upload queue", CloudBackupError::Internal)?;
        self.set_pending_upload_verification(true);
        self.wake_pending_upload_verifier();

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
        let wakeup = this.pending_upload_verifier_wakeup.clone();
        cove_tokio::task::spawn(async move {
            info!("Pending upload verification: started");
            let mut backoff = PendingUploadRetryBackoff::new();

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

                let delay = backoff.next_delay();
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = wakeup.notified() => {
                        backoff.reset();
                    }
                }
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

    #[cfg(test)]
    pub(super) fn verify_pending_uploads_once_for_test(&self) -> bool {
        self.verify_pending_uploads_once()
    }

    fn wake_pending_upload_verifier(&self) {
        if self.pending_upload_verifier_running.load(Ordering::SeqCst) {
            self.pending_upload_verifier_wakeup.notify_one();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex as StdMutex, OnceLock};

    #[test]
    fn pending_upload_retry_backoff_resets_to_short_delay() {
        let mut backoff = PendingUploadRetryBackoff::new();
        let initial_delay = backoff.next_delay();

        let _ = backoff.next_delay();
        let _ = backoff.next_delay();

        backoff.reset();

        assert_eq!(backoff.next_delay(), initial_delay);
    }

    #[test]
    fn pending_upload_retry_backoff_caps_at_max_delay() {
        let mut backoff = PendingUploadRetryBackoff::new();

        for _ in 0..10 {
            assert!(backoff.next_delay() <= MAX_PENDING_UPLOAD_VERIFICATION_DELAY);
        }
    }

    fn test_lock() -> &'static StdMutex<()> {
        static LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| StdMutex::new(()))
    }

    #[test]
    fn enqueue_pending_uploads_reactivates_confirmed_item() {
        let _guard = test_lock().lock().unwrap();
        let manager = RustCloudBackupManager::init();
        let table = &Database::global().cloud_upload_queue;

        table.delete().unwrap();
        manager.pending_upload_verifier_running.store(true, Ordering::SeqCst);

        table
            .set(&crate::database::cloud_backup::PendingCloudUploadQueue {
                items: vec![PendingCloudUploadItem {
                    kind: CloudUploadKind::BackupBlob,
                    namespace_id: "namespace-1".into(),
                    record_id: "record-1".into(),
                    enqueued_at: 1,
                    verification: CloudUploadVerificationState::Confirmed(2),
                }],
            })
            .unwrap();

        manager.enqueue_pending_uploads("namespace-1", ["record-1".to_string()]).unwrap();

        let queue = table.get().unwrap().unwrap();
        assert_eq!(queue.items.len(), 1);
        assert!(matches!(
            queue.items[0].verification,
            CloudUploadVerificationState::Pending { attempt_count: 0, last_checked_at: None }
        ));
        assert!(queue.items[0].enqueued_at >= 2);

        manager.pending_upload_verifier_running.store(false, Ordering::SeqCst);
        table.delete().unwrap();
    }
}

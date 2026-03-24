use std::sync::Arc;

use redb::TableDefinition;
use serde::{Deserialize, Serialize};

use cove_types::redb::Json;
use cove_util::result_ext::ResultExt as _;

use super::Error;

pub const TABLE: TableDefinition<&'static str, Json<PendingCloudUploadVerification>> =
    TableDefinition::new("cloud_backup_upload_verification");

const CURRENT_KEY: &str = "current";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingCloudUploadVerification {
    pub namespace_id: String,
    pub blobs: Vec<PendingCloudUploadBlob>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingCloudUploadBlob {
    pub record_id: String,
    pub enqueued_at: u64,
    pub last_checked_at: Option<u64>,
    pub attempt_count: u32,

    /// Set when isBackupUploaded confirms the blob, kept until the listing catches up
    #[serde(default)]
    pub confirmed_at: Option<u64>,
}

impl PendingCloudUploadVerification {
    /// Returns true if there are blobs still awaiting confirmation
    pub fn has_unconfirmed(&self) -> bool {
        self.blobs.iter().any(|b| b.confirmed_at.is_none())
    }

    /// Remove confirmed blobs whose record_ids appear in the listing
    pub fn cleanup_listed(&mut self, listed_ids: &std::collections::HashSet<String>) {
        self.blobs.retain(|b| b.confirmed_at.is_none() || !listed_ids.contains(&b.record_id));
    }
}

#[derive(Debug, Clone)]
pub struct CloudBackupUploadVerificationTable {
    db: Arc<redb::Database>,
}

impl CloudBackupUploadVerificationTable {
    pub fn new(db: Arc<redb::Database>, write_txn: &redb::WriteTransaction) -> Self {
        write_txn.open_table(TABLE).expect("failed to create table");

        Self { db }
    }

    pub fn get(&self) -> Result<Option<PendingCloudUploadVerification>, Error> {
        let read_txn = self.db.begin_read().map_err_str(Error::DatabaseAccess)?;
        let table = read_txn.open_table(TABLE).map_err_str(Error::TableAccess)?;

        let value =
            table.get(CURRENT_KEY).map_err_str(Error::TableAccess)?.map(|value| value.value());

        Ok(value)
    }

    pub fn set(&self, value: &PendingCloudUploadVerification) -> Result<(), Error> {
        let write_txn = self.db.begin_write().map_err_str(Error::DatabaseAccess)?;

        {
            let mut table = write_txn.open_table(TABLE).map_err_str(Error::TableAccess)?;
            table.insert(CURRENT_KEY, value).map_err_str(Error::TableAccess)?;
        }

        write_txn.commit().map_err_str(Error::DatabaseAccess)?;

        Ok(())
    }

    pub fn delete(&self) -> Result<(), Error> {
        let write_txn = self.db.begin_write().map_err_str(Error::DatabaseAccess)?;

        {
            let mut table = write_txn.open_table(TABLE).map_err_str(Error::TableAccess)?;
            table.remove(CURRENT_KEY).map_err_str(Error::TableAccess)?;
        }

        write_txn.commit().map_err_str(Error::DatabaseAccess)?;

        Ok(())
    }
}

use std::str::FromStr as _;

use cove_cspp::CsppStore as _;
use cove_cspp::backup_data::{
    DescriptorPair, WalletEntry, WalletMode, WalletSecret as CloudWalletSecret, wallet_record_id,
};
use cove_cspp::wallet_crypto;
use cove_device::cloud_storage::CloudStorage;
use cove_device::keychain::Keychain;
use cove_device::passkey::PasskeyAccess;
use cove_types::network::Network;
use cove_util::ResultExt as _;
use rand::RngExt as _;
use strum::IntoEnumIterator as _;
use tracing::{info, warn};
use zeroize::Zeroizing;

use super::{
    CREDENTIAL_ID_KEY, CloudBackupError, LocalDescriptorPair, LocalWalletMode, LocalWalletSecret,
    PRF_SALT_KEY, RP_ID, RustCloudBackupManager,
};
use crate::database::Database;
use crate::database::global_config::CloudBackup;
use crate::wallet::metadata::{WalletMetadata, WalletType};

pub(super) struct UnpersistedPrfKey {
    pub(super) prf_key: [u8; 32],
    pub(super) prf_salt: [u8; 32],
    pub(super) credential_id: Vec<u8>,
}

impl RustCloudBackupManager {
    /// Upload wallets to cloud and update local cache
    pub(super) fn do_backup_wallets(
        &self,
        wallets: &[crate::wallet::metadata::WalletMetadata],
    ) -> Result<(), CloudBackupError> {
        if wallets.is_empty() {
            return Ok(());
        }

        let namespace = self.current_namespace_id()?;
        let cspp = cove_cspp::Cspp::new(Keychain::global().clone());
        let master_key = cspp
            .get_or_create_master_key()
            .map_err_prefix("master key", CloudBackupError::Internal)?;

        let critical_key = Zeroizing::new(master_key.critical_data_key());
        let cloud = CloudStorage::global();
        let mut uploaded_record_ids = Vec::with_capacity(wallets.len());

        for (index, metadata) in wallets.iter().enumerate() {
            info!("Backup: uploading wallet {}/{} '{}'", index + 1, wallets.len(), metadata.name);
            let entry = build_wallet_entry(metadata, metadata.wallet_mode)?;
            let encrypted = wallet_crypto::encrypt_wallet_entry(&entry, &critical_key)
                .map_err_str(CloudBackupError::Crypto)?;

            let record_id = wallet_record_id(metadata.id.as_ref());
            let wallet_json =
                serde_json::to_vec(&encrypted).map_err_str(CloudBackupError::Internal)?;

            cloud
                .upload_wallet_backup(namespace.clone(), record_id.clone(), wallet_json)
                .map_err_str(CloudBackupError::Cloud)?;
            uploaded_record_ids.push(record_id);
            info!("Backup: wallet {}/{} uploaded", index + 1, wallets.len());
        }

        let db = Database::global();
        self.enqueue_pending_uploads(&namespace, uploaded_record_ids)?;

        let previous_count = match db.global_config.cloud_backup() {
            CloudBackup::Enabled { wallet_count: Some(count), .. }
            | CloudBackup::Unverified { wallet_count: Some(count), .. } => count,
            _ => 0,
        };
        let wallet_count = previous_count + wallets.len() as u32;
        persist_enabled_cloud_backup_state(&db, wallet_count)?;

        info!("Backed up {} wallet(s) to cloud", wallets.len());
        Ok(())
    }
}

/// Create a passkey and authenticate with PRF without persisting to keychain
///
/// Used by the wrapper-repair path where we need to defer persistence until
/// after the cloud upload succeeds
pub(super) fn create_prf_key_without_persisting(
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

/// Encrypt and hand off all local wallets to the given namespace
pub(super) fn upload_all_wallets(
    cloud: &CloudStorage,
    namespace: &str,
    critical_key: &[u8; 32],
    db: &Database,
) -> Result<Vec<String>, CloudBackupError> {
    let mut uploaded_record_ids = Vec::new();

    for metadata in all_local_wallets(db) {
        let entry = build_wallet_entry(&metadata, metadata.wallet_mode)?;
        let encrypted = wallet_crypto::encrypt_wallet_entry(&entry, critical_key)
            .map_err_str(CloudBackupError::Crypto)?;

        let record_id = wallet_record_id(metadata.id.as_ref());
        let wallet_json = serde_json::to_vec(&encrypted).map_err_str(CloudBackupError::Internal)?;

        cloud
            .upload_wallet_backup(namespace.to_string(), record_id.clone(), wallet_json)
            .map_err_str(CloudBackupError::Cloud)?;

        uploaded_record_ids.push(record_id);
    }

    Ok(uploaded_record_ids)
}

pub(super) fn persist_enabled_cloud_backup_state(
    db: &Database,
    wallet_count: u32,
) -> Result<(), CloudBackupError> {
    let now = jiff::Timestamp::now().as_second().try_into().unwrap_or(0);
    db.global_config
        .set_cloud_backup(&CloudBackup::Enabled {
            last_sync: Some(now),
            wallet_count: Some(wallet_count),
        })
        .map_err_prefix("persist cloud backup state", CloudBackupError::Internal)
}

/// All local wallets across every network and mode
pub(super) fn all_local_wallets(db: &Database) -> Vec<WalletMetadata> {
    Network::iter()
        .flat_map(|network| {
            LocalWalletMode::iter()
                .flat_map(move |mode| db.wallets.get_all(network, mode).unwrap_or_default())
        })
        .collect()
}

pub(super) fn count_all_wallets(db: &Database) -> u32 {
    all_local_wallets(db).len() as u32
}

pub(super) fn restore_single_wallet(
    cloud: &CloudStorage,
    namespace: &str,
    record_id: &str,
    critical_key: &[u8; 32],
    existing_fingerprints: &mut Vec<(
        crate::wallet::fingerprint::Fingerprint,
        Network,
        LocalWalletMode,
    )>,
) -> Result<(), CloudBackupError> {
    let wallet_json = cloud
        .download_wallet_backup(namespace.to_string(), record_id.to_string())
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

    let metadata: crate::wallet::metadata::WalletMetadata =
        serde_json::from_value(entry.metadata.clone())
            .map_err_prefix("parse wallet metadata", CloudBackupError::Internal)?;

    if crate::backup::import::is_wallet_duplicate(&metadata, existing_fingerprints)
        .inspect_err(|e| warn!("is_wallet_duplicate check failed for {}: {e}", metadata.name))
        .unwrap_or(false)
    {
        info!("Skipping duplicate wallet {}", metadata.name);
        return Ok(());
    }

    let backup_model = crate::backup::model::WalletBackup {
        metadata: entry.metadata.clone(),
        secret: convert_cloud_secret(&entry.secret),
        descriptors: entry.descriptors.as_ref().map(|descriptors| LocalDescriptorPair {
            external: descriptors.external.clone(),
            internal: descriptors.internal.clone(),
        }),
        xpub: entry.xpub.clone(),
        labels_jsonl: None,
    };

    match &backup_model.secret {
        LocalWalletSecret::Mnemonic(words) => {
            let mnemonic = bip39::Mnemonic::from_str(words)
                .map_err_prefix("invalid mnemonic", CloudBackupError::Internal)?;

            crate::backup::import::restore_mnemonic_wallet(&metadata, mnemonic).map_err(
                |(error, _)| {
                    CloudBackupError::Internal(format!("restore mnemonic wallet: {error}"))
                },
            )?;
        }
        _ => {
            crate::backup::import::restore_descriptor_wallet(&metadata, &backup_model).map_err(
                |(error, _)| {
                    CloudBackupError::Internal(format!("restore descriptor wallet: {error}"))
                },
            )?;
        }
    }

    if let Some(fingerprint) = &metadata.master_fingerprint {
        existing_fingerprints.push((**fingerprint, metadata.network, metadata.wallet_mode));
    }

    Ok(())
}

/// Create a fresh passkey and authenticate with PRF to get the wrapping key
///
/// Always creates a new passkey — the enable flow re-encrypts everything,
/// so there's no benefit to reusing stale cached credentials (which may
/// reference a passkey deleted from the user's password manager)
pub(super) fn obtain_prf_key(
    keychain: &Keychain,
    passkey: &PasskeyAccess,
) -> Result<([u8; 32], [u8; 32]), CloudBackupError> {
    keychain.delete(CREDENTIAL_ID_KEY.to_string());
    keychain.delete(PRF_SALT_KEY.to_string());

    info!("Creating new passkey");
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

    keychain
        .save(CREDENTIAL_ID_KEY.to_string(), hex::encode(&credential_id))
        .map_err_prefix("save credential", CloudBackupError::Internal)?;

    keychain
        .save(PRF_SALT_KEY.to_string(), hex::encode(prf_salt))
        .map_err_prefix("save prf_salt", CloudBackupError::Internal)?;

    Ok((prf_key, prf_salt))
}

pub(super) fn convert_cloud_secret(secret: &CloudWalletSecret) -> LocalWalletSecret {
    match secret {
        CloudWalletSecret::Mnemonic(mnemonic) => LocalWalletSecret::Mnemonic(mnemonic.clone()),
        CloudWalletSecret::TapSignerBackup(backup) => {
            LocalWalletSecret::TapSignerBackup(backup.clone())
        }
        CloudWalletSecret::Descriptor(_) | CloudWalletSecret::WatchOnly => LocalWalletSecret::None,
    }
}

pub(super) fn build_wallet_entry(
    metadata: &crate::wallet::metadata::WalletMetadata,
    mode: LocalWalletMode,
) -> Result<WalletEntry, CloudBackupError> {
    let keychain = Keychain::global();
    let id = &metadata.id;
    let name = &metadata.name;

    let secret = match metadata.wallet_type {
        WalletType::Hot => match keychain.get_wallet_key(id) {
            Ok(Some(mnemonic)) => CloudWalletSecret::Mnemonic(mnemonic.to_string()),
            Ok(None) => {
                return Err(CloudBackupError::Internal(format!(
                    "hot wallet '{name}' has no mnemonic"
                )));
            }
            Err(error) => {
                return Err(CloudBackupError::Internal(format!(
                    "failed to get mnemonic for '{name}': {error}"
                )));
            }
        },
        WalletType::Cold => {
            let is_tap_signer = metadata
                .hardware_metadata
                .as_ref()
                .is_some_and(|hardware| hardware.is_tap_signer());

            if is_tap_signer {
                match keychain.get_tap_signer_backup(id) {
                    Ok(Some(backup)) => CloudWalletSecret::TapSignerBackup(backup),
                    Ok(None) => {
                        warn!("Tap signer wallet '{name}' has no backup, exporting without it");
                        CloudWalletSecret::WatchOnly
                    }
                    Err(error) => {
                        return Err(CloudBackupError::Internal(format!(
                            "failed to read tap signer backup for '{name}': {error}"
                        )));
                    }
                }
            } else {
                CloudWalletSecret::WatchOnly
            }
        }
        WalletType::XpubOnly | WalletType::WatchOnly => CloudWalletSecret::WatchOnly,
    };

    let xpub = match keychain.get_wallet_xpub(id) {
        Ok(Some(xpub)) => Some(xpub.to_string()),
        Ok(None) => None,
        Err(error) => {
            return Err(CloudBackupError::Internal(format!(
                "failed to read xpub for '{name}': {error}"
            )));
        }
    };

    let descriptors = match keychain.get_public_descriptor(id) {
        Ok(Some((external, internal))) => {
            Some(DescriptorPair { external: external.to_string(), internal: internal.to_string() })
        }
        Ok(None) => None,
        Err(error) => {
            return Err(CloudBackupError::Internal(format!(
                "failed to read descriptors for '{name}': {error}"
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

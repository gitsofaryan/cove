@_exported import CoveCore
import Foundation

final class CloudStorageAccessImpl: CloudStorageAccess, @unchecked Sendable {
    private let helper = ICloudDriveHelper.shared

    // MARK: - Upload

    func uploadMasterKeyBackup(data: Data) throws {
        try upload(recordId: csppMasterKeyRecordId(), data: data)
    }

    func uploadWalletBackup(recordId: String, data: Data) throws {
        try upload(recordId: recordId, data: data)
    }

    func uploadManifest(data: Data) throws {
        try upload(recordId: csppManifestRecordId(), data: data)
    }

    // MARK: - Download

    func downloadMasterKeyBackup() throws -> Data {
        try download(recordId: csppMasterKeyRecordId())
    }

    func downloadWalletBackup(recordId: String) throws -> Data {
        try download(recordId: recordId)
    }

    func deleteWalletBackup(recordId: String) throws {
        let url = try helper.fileURL(for: recordId)
        guard FileManager.default.fileExists(atPath: url.path) else {
            throw CloudStorageError.NotFound(recordId)
        }
        try helper.coordinatedDelete(at: url)
    }

    /// Downloads the manifest with authoritative NotFound semantics
    ///
    /// Uses NSMetadataQuery to confirm the file truly doesn't exist in iCloud
    /// before returning NotFound, preventing false NotFound from sync lag
    func downloadManifest() throws -> Data {
        let recordId = csppManifestRecordId()
        let filename = ICloudDriveHelper.hashedFilename(for: recordId)
        let url = try helper.fileURL(for: recordId)

        // check if file exists locally and is already downloaded
        if FileManager.default.fileExists(atPath: url.path) {
            let values = try? url.resourceValues(forKeys: [.ubiquitousItemDownloadingStatusKey])
            if values?.ubiquitousItemDownloadingStatus == .current {
                return try helper.coordinatedRead(from: url)
            }

            // file exists locally but needs download (evicted)
            try helper.ensureDownloaded(url: url, recordId: recordId)
            return try helper.coordinatedRead(from: url)
        }

        // file not on local disk — use metadata query for authoritative check
        let existsInCloud: Bool
        do {
            existsInCloud = try helper.fileExistsInCloud(name: filename)
        } catch {
            throw CloudStorageError.NotAvailable("cannot verify manifest: \(error.localizedDescription)")
        }

        guard existsInCloud else {
            throw CloudStorageError.NotFound(recordId)
        }

        // file exists in cloud but not locally — download it
        try helper.ensureDownloaded(url: url, recordId: recordId)
        return try helper.coordinatedRead(from: url)
    }

    // MARK: - Presence check

    /// Checks that BOTH manifest AND master key files exist in iCloud
    func hasCloudBackup() throws -> Bool {
        let manifestName = ICloudDriveHelper.hashedFilename(for: csppManifestRecordId())
        let masterKeyName = ICloudDriveHelper.hashedFilename(for: csppMasterKeyRecordId())

        let manifestExists = try helper.fileExistsInCloud(name: manifestName)
        guard manifestExists else { return false }

        return try helper.fileExistsInCloud(name: masterKeyName)
    }

    // MARK: - Private

    private func upload(recordId: String, data: Data) throws {
        let url = try helper.fileURL(for: recordId)
        try helper.coordinatedWrite(data: data, to: url)
        try helper.waitForUpload(url: url)
    }

    private func download(recordId: String) throws -> Data {
        let url = try helper.fileURL(for: recordId)
        try helper.ensureDownloaded(url: url, recordId: recordId)
        return try helper.coordinatedRead(from: url)
    }
}

@_exported import CoveCore
import CryptoKit
import Foundation

final class ICloudDriveHelper: @unchecked Sendable {
    static let shared = ICloudDriveHelper()

    private let containerIdentifier = "iCloud.com.covebitcoinwallet"
    private let dataSubdirectory = "Data"
    private let defaultTimeout: TimeInterval = 60
    private let pollInterval: TimeInterval = 0.1

    // MARK: - Path mapping

    func containerURL() throws -> URL {
        guard let url = FileManager.default.url(
            forUbiquityContainerIdentifier: containerIdentifier
        ) else {
            throw CloudStorageError.NotAvailable("iCloud Drive is not available")
        }
        return url
    }

    func dataDirectoryURL() throws -> URL {
        let url = try containerURL().appendingPathComponent(dataSubdirectory, isDirectory: true)
        if !FileManager.default.fileExists(atPath: url.path) {
            try FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        }
        return url
    }

    /// Deterministic opaque filename: SHA256(recordId).json
    static func hashedFilename(for recordId: String) -> String {
        let hash = SHA256.hash(data: Data(recordId.utf8))
        return hash.compactMap { String(format: "%02x", $0) }.joined() + ".json"
    }

    func fileURL(for recordId: String) throws -> URL {
        try dataDirectoryURL().appendingPathComponent(Self.hashedFilename(for: recordId))
    }

    // MARK: - File coordination

    func coordinatedWrite(data: Data, to url: URL) throws {
        var coordinatorError: NSError?
        var writeError: Error?

        let coordinator = NSFileCoordinator()
        coordinator.coordinate(
            writingItemAt: url, options: .forReplacing, error: &coordinatorError
        ) { newURL in
            do {
                try data.write(to: newURL, options: .atomic)
            } catch {
                writeError = error
            }
        }

        if let error = coordinatorError ?? writeError {
            throw CloudStorageError.UploadFailed("write failed: \(error.localizedDescription)")
        }
    }

    func coordinatedDelete(at url: URL) throws {
        var coordinatorError: NSError?
        var deleteError: Error?

        let coordinator = NSFileCoordinator()
        coordinator.coordinate(
            writingItemAt: url, options: .forDeleting, error: &coordinatorError
        ) { newURL in
            do {
                try FileManager.default.removeItem(at: newURL)
            } catch {
                deleteError = error
            }
        }

        if let error = coordinatorError ?? deleteError {
            throw CloudStorageError.UploadFailed("delete failed: \(error.localizedDescription)")
        }
    }

    func coordinatedRead(from url: URL) throws -> Data {
        var coordinatorError: NSError?
        var readResult: Result<Data, Error>?

        let coordinator = NSFileCoordinator()
        coordinator.coordinate(readingItemAt: url, options: [], error: &coordinatorError) { newURL in
            do {
                readResult = try .success(Data(contentsOf: newURL))
            } catch {
                readResult = .failure(error)
            }
        }

        if let error = coordinatorError {
            throw CloudStorageError.DownloadFailed(
                "file coordination error: \(error.localizedDescription)"
            )
        }

        guard let readResult else {
            throw CloudStorageError.DownloadFailed("coordinated read produced no result")
        }

        switch readResult {
        case let .success(data): return data
        case let .failure(error):
            throw CloudStorageError.DownloadFailed(error.localizedDescription)
        }
    }

    // MARK: - Upload verification

    /// Blocks until the file at `url` is confirmed uploaded to iCloud, or times out
    func waitForUpload(url: URL) throws {
        let deadline = Date().addingTimeInterval(defaultTimeout)

        while Date() < deadline {
            let values = try? url.resourceValues(forKeys: [
                .ubiquitousItemIsUploadedKey,
                .ubiquitousItemUploadingErrorKey,
            ])

            if values?.ubiquitousItemIsUploaded == true {
                return
            }

            if let error = values?.ubiquitousItemUploadingError {
                throw CloudStorageError.UploadFailed(
                    "iCloud upload failed: \(error.localizedDescription)"
                )
            }

            Thread.sleep(forTimeInterval: pollInterval)
        }

        throw CloudStorageError.UploadFailed(
            "iCloud upload timed out after \(defaultTimeout)s"
        )
    }

    // MARK: - Download

    /// Ensures the file is downloaded locally, triggering a download if evicted
    func ensureDownloaded(url: URL, recordId: String) throws {
        // check if already downloaded
        if FileManager.default.fileExists(atPath: url.path) {
            let values = try? url.resourceValues(forKeys: [.ubiquitousItemDownloadingStatusKey])
            if values?.ubiquitousItemDownloadingStatus == .current {
                return
            }
        }

        // trigger download
        do {
            try FileManager.default.startDownloadingUbiquitousItem(at: url)
        } catch {
            let nsError = error as NSError
            if nsError.domain == NSCocoaErrorDomain,
               nsError.code == NSFileReadNoSuchFileError || nsError.code == 4
            {
                throw CloudStorageError.NotFound(recordId)
            }
            throw CloudStorageError.DownloadFailed(
                "failed to start download: \(error.localizedDescription)"
            )
        }

        // wait for download to complete
        let deadline = Date().addingTimeInterval(defaultTimeout)
        while Date() < deadline {
            let values = try? url.resourceValues(forKeys: [
                .ubiquitousItemDownloadingStatusKey,
                .ubiquitousItemDownloadingErrorKey,
            ])

            if values?.ubiquitousItemDownloadingStatus == .current {
                return
            }

            if let error = values?.ubiquitousItemDownloadingError {
                throw CloudStorageError.DownloadFailed(
                    "iCloud download failed: \(error.localizedDescription)"
                )
            }

            Thread.sleep(forTimeInterval: pollInterval)
        }

        throw CloudStorageError.DownloadFailed(
            "iCloud download timed out after \(defaultTimeout)s"
        )
    }

    // MARK: - Cloud presence via NSMetadataQuery

    /// Authoritatively checks whether a file exists in iCloud (finds evicted files too)
    ///
    /// Must NOT be called from the main thread
    func fileExistsInCloud(name: String) throws -> Bool {
        let semaphore = DispatchSemaphore(value: 0)
        var found = false
        var startFailed = false

        let query = NSMetadataQuery()
        query.searchScopes = [NSMetadataQueryUbiquitousDataScope]
        query.predicate = NSPredicate(format: "%K == %@", NSMetadataItemFSNameKey, name)

        // use a class wrapper to avoid sendable closure capture issues
        class ObserverBox {
            var observer: NSObjectProtocol?
            func remove() {
                if let obs = observer {
                    NotificationCenter.default.removeObserver(obs)
                    observer = nil
                }
            }
        }
        let box = ObserverBox()

        box.observer = NotificationCenter.default.addObserver(
            forName: .NSMetadataQueryDidFinishGathering,
            object: query,
            queue: .main
        ) { _ in
            query.disableUpdates()
            found = query.resultCount > 0
            query.stop()
            box.remove()
            semaphore.signal()
        }

        DispatchQueue.main.async {
            if !query.start() {
                startFailed = true
                box.remove()
                semaphore.signal()
            }
        }

        if semaphore.wait(timeout: .now() + defaultTimeout) == .timedOut {
            DispatchQueue.main.async {
                query.stop()
                box.remove()
            }
            throw CloudStorageError.NotAvailable("iCloud metadata query timed out")
        }

        if startFailed {
            throw CloudStorageError.NotAvailable("failed to start iCloud metadata query")
        }

        return found
    }

    // MARK: - Upload status for UI

    enum UploadStatus {
        case uploaded
        case uploading
        case failed(String)
        case unknown
    }

    func uploadStatus(for url: URL) -> UploadStatus {
        guard FileManager.default.fileExists(atPath: url.path) else {
            return .unknown
        }

        let values = try? url.resourceValues(forKeys: [
            .ubiquitousItemIsUploadedKey,
            .ubiquitousItemUploadingErrorKey,
        ])

        if values?.ubiquitousItemIsUploaded == true {
            return .uploaded
        }

        if let error = values?.ubiquitousItemUploadingError {
            return .failed(error.localizedDescription)
        }

        return .uploading
    }

    /// Checks sync health of all files in the Data/ directory
    func overallSyncHealth() -> SyncHealth {
        guard let dataDir = try? dataDirectoryURL() else {
            return .unavailable
        }

        guard
            let files = try? FileManager.default.contentsOfDirectory(
                at: dataDir, includingPropertiesForKeys: nil
            )
        else {
            return .unavailable
        }

        if files.isEmpty {
            return .noFiles
        }

        var allUploaded = true
        var anyFailed = false
        var failureMessage: String?

        for file in files where file.pathExtension == "json" {
            let status = uploadStatus(for: file)
            switch status {
            case .uploaded: continue
            case .uploading: allUploaded = false
            case let .failed(msg):
                anyFailed = true
                allUploaded = false
                failureMessage = msg
            case .unknown:
                allUploaded = false
            }
        }

        if anyFailed {
            return .failed(failureMessage ?? "upload error")
        }
        if allUploaded {
            return .allUploaded
        }
        return .uploading
    }

    enum SyncHealth {
        case allUploaded
        case uploading
        case failed(String)
        case noFiles
        case unavailable
    }
}

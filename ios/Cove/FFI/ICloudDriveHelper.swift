import CoveCore
import CryptoKit
import Foundation

final class ICloudDriveHelper: @unchecked Sendable {
    static let shared = ICloudDriveHelper()

    private let containerIdentifier = "iCloud.com.covebitcoinwallet"
    private let dataSubdirectory = "Data"
    private let namespacesSubdirectory = csppNamespacesSubdirectory()
    private let defaultTimeout: TimeInterval = 60
    private let pollInterval: TimeInterval = 0.1
    private let metadataSettleInterval: TimeInterval = 0.5
    private let progressLogInterval: TimeInterval = 1

    private final class ObserverBox {
        private var observers: [NSObjectProtocol] = []

        func add(_ observer: NSObjectProtocol) {
            observers.append(observer)
        }

        func removeAll() {
            for observer in observers {
                NotificationCenter.default.removeObserver(observer)
            }
            observers.removeAll()
        }
    }

    private struct ResolvedMetadataItem {
        let url: URL
        let metadataPath: String?
    }

    private enum UploadState: CustomStringConvertible {
        case uploaded
        case uploading
        case failed(Error)
        case notUbiquitous
        case unknown

        var description: String {
            switch self {
            case .uploaded: "uploaded"
            case .uploading: "uploading"
            case let .failed(error): "failed: \(error.localizedDescription)"
            case .notUbiquitous: "not ubiquitous"
            case .unknown: "unknown"
            }
        }
    }

    private enum DownloadState: CustomStringConvertible {
        case current
        case downloading
        case failed(Error)
        case notUbiquitous
        case notDownloaded
        case unknown

        var description: String {
            switch self {
            case .current: "current"
            case .downloading: "downloading"
            case let .failed(error): "failed: \(error.localizedDescription)"
            case .notUbiquitous: "not ubiquitous"
            case .notDownloaded: "not downloaded"
            case .unknown: "unknown"
            }
        }
    }

    private enum MetadataLookupError: LocalizedError {
        case startFailed(String)
        case timedOut(String)
        case missingURL(String)

        var errorDescription: String? {
            switch self {
            case let .startFailed(message),
                 let .timedOut(message),
                 let .missingURL(message):
                message
            }
        }
    }

    // MARK: - Path mapping

    func containerURL() throws -> URL {
        guard
            let url = FileManager.default.url(
                forUbiquityContainerIdentifier: containerIdentifier
            )
        else {
            throw CloudStorageError.NotAvailable("iCloud Drive is not available")
        }
        return url
    }

    func dataDirectoryURL() throws -> URL {
        let url = try containerURL().appendingPathComponent(dataSubdirectory, isDirectory: true)
        try coordinatedCreateDirectory(at: url)
        return url
    }

    /// Root directory for all namespaces: Data/cspp-namespaces/
    func namespacesRootURL() throws -> URL {
        let url = try dataDirectoryURL()
            .appendingPathComponent(namespacesSubdirectory, isDirectory: true)
        try coordinatedCreateDirectory(at: url)
        return url
    }

    /// Directory for a specific namespace: Data/cspp-namespaces/{namespace}/
    func namespaceDirectoryURL(namespace: String) throws -> URL {
        let url = try namespacesRootURL()
            .appendingPathComponent(namespace, isDirectory: true)
        try coordinatedCreateDirectory(at: url)
        return url
    }

    /// Master key file URL within a namespace
    ///
    /// Filename: masterkey-{SHA256(MASTER_KEY_RECORD_ID)}.json
    func masterKeyFileURL(namespace: String) throws -> URL {
        let recordId = csppMasterKeyRecordId()
        let hash = SHA256.hash(data: Data(recordId.utf8))
        let hexHash = hash.compactMap { String(format: "%02x", $0) }.joined()
        let filename = "masterkey-\(hexHash).json"
        return try namespaceDirectoryURL(namespace: namespace)
            .appendingPathComponent(filename)
    }

    /// Wallet backup file URL within a namespace
    ///
    /// Filename: wallet-{recordId}.json — recordId is already SHA256(wallet_id)
    func walletFileURL(namespace: String, recordId: String) throws -> URL {
        let filename = "wallet-\(recordId).json"
        return try namespaceDirectoryURL(namespace: namespace)
            .appendingPathComponent(filename)
    }

    func backupFileURL(namespace: String, recordId: String) throws -> URL {
        if recordId == csppMasterKeyRecordId() {
            return try masterKeyFileURL(namespace: namespace)
        }

        return try walletFileURL(namespace: namespace, recordId: recordId)
    }

    /// Legacy flat file URL (for migration/cleanup)
    func legacyFileURL(for recordId: String) throws -> URL {
        let hash = SHA256.hash(data: Data(recordId.utf8))
        let filename = hash.compactMap { String(format: "%02x", $0) }.joined() + ".json"
        return try dataDirectoryURL().appendingPathComponent(filename)
    }

    // MARK: - File coordination

    private func coordinatedCreateDirectory(at url: URL) throws {
        guard !FileManager.default.fileExists(atPath: url.path) else {
            return
        }

        var coordinatorError: NSError?
        var createError: Error?

        let coordinator = NSFileCoordinator()
        coordinator.coordinate(writingItemAt: url, options: [], error: &coordinatorError) {
            newURL in
            do {
                try FileManager.default.createDirectory(
                    at: newURL,
                    withIntermediateDirectories: true
                )
            } catch {
                createError = error
            }
        }

        if let error = coordinatorError ?? createError {
            throw CloudStorageError.UploadFailed(
                "create directory failed: \(error.localizedDescription)"
            )
        }
    }

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

    func writeForUpload(data: Data, to url: URL) throws {
        guard !FileManager.default.fileExists(atPath: url.path) else {
            try coordinatedWrite(data: data, to: url)
            return
        }

        let tempURL = FileManager.default.temporaryDirectory.appendingPathComponent(
            "icloud-upload-\(UUID().uuidString)-\(url.lastPathComponent)"
        )

        do {
            try data.write(to: tempURL, options: .atomic)
        } catch {
            throw CloudStorageError.UploadFailed(
                "temporary write failed: \(error.localizedDescription)"
            )
        }

        defer {
            if FileManager.default.fileExists(atPath: tempURL.path) {
                try? FileManager.default.removeItem(at: tempURL)
            }
        }

        Log.info(
            "writeForUpload: staging first upload via setUbiquitous for \(url.lastPathComponent)"
        )

        var coordinatorError: NSError?
        var moveError: Error?

        let coordinator = NSFileCoordinator()
        coordinator.coordinate(writingItemAt: url, options: [], error: &coordinatorError) {
            destinationURL in
            do {
                try FileManager.default.setUbiquitous(
                    true,
                    itemAt: tempURL,
                    destinationURL: destinationURL
                )
            } catch {
                moveError = error
            }
        }

        if let error = coordinatorError ?? moveError {
            throw CloudStorageError.UploadFailed(
                "setUbiquitous failed: \(error.localizedDescription)"
            )
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
        coordinator.coordinate(readingItemAt: url, options: [], error: &coordinatorError) {
            newURL in
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

    /// Downloads a file from iCloud via coordinated read
    ///
    /// Tries startDownloadingUbiquitousItem as a hint, then uses NSFileCoordinator
    /// which forces the download through a different (more reliable) path
    func downloadFile(url: URL, recordId _: String) throws -> Data {
        let filename = url.lastPathComponent

        // if already downloaded locally, just read it
        if FileManager.default.fileExists(atPath: url.path),
           case .current = downloadState(for: url)
        {
            Log.info("downloadFile: \(filename) already available locally")
            return try coordinatedRead(from: url)
        }

        // hint to iCloud daemon to start downloading
        try? FileManager.default.startDownloadingUbiquitousItem(at: url)

        // coordinated read blocks until file is downloaded
        Log.info("downloadFile: \(filename) reading via NSFileCoordinator")
        return try coordinatedRead(from: url)
    }

    // MARK: - Upload verification

    /// Blocks until the file is visible through iCloud metadata
    func waitForMetadataVisibility(url: URL) throws {
        let filename = url.lastPathComponent
        let deadline = Date().addingTimeInterval(defaultTimeout)

        do {
            let resolvedItem = try waitForMetadataItem(
                named: filename,
                parentDirectoryURL: url.deletingLastPathComponent(),
                deadline: deadline
            )
            if resolvedItem.url != url {
                Log.info(
                    "waitForMetadataVisibility: using metadata URL for \(filename) local=\(url.path) metadata=\(resolvedItem.url.path)"
                )
            }
        } catch {
            throw CloudStorageError.UploadFailed(
                "iCloud metadata lookup failed for \(filename): \(error.localizedDescription)"
            )
        }
    }

    /// Blocks until the file at `url` is confirmed uploaded to iCloud, or times out
    func waitForUpload(url: URL) throws {
        let filename = url.lastPathComponent
        Log.info("waitForUpload: waiting for \(filename)")
        let deadline = Date().addingTimeInterval(defaultTimeout)

        if case .uploaded = uploadState(for: url) {
            Log.info("waitForUpload: \(filename) already uploaded on local URL")
            return
        }

        let resolvedItem: ResolvedMetadataItem
        do {
            resolvedItem = try waitForMetadataItem(
                named: filename,
                parentDirectoryURL: url.deletingLastPathComponent(),
                deadline: deadline
            )
        } catch {
            throw CloudStorageError.UploadFailed(
                "iCloud metadata lookup failed for \(filename): \(error.localizedDescription)"
            )
        }

        if resolvedItem.url != url {
            Log.info(
                "waitForUpload: using metadata URL for \(filename) local=\(url.path) metadata=\(resolvedItem.url.path)"
            )
        }

        var lastProgressLog = Date.distantPast

        while Date() < deadline {
            let state = uploadState(for: resolvedItem.url)
            let now = Date()

            if now.timeIntervalSince(lastProgressLog) >= progressLogInterval {
                Log.info(
                    "waitForUpload: \(filename) state=\(state) metadataPath=\(resolvedItem.metadataPath ?? "<unknown>") diagnostics=\(uploadDiagnostics(for: resolvedItem.url))"
                )
                lastProgressLog = now
            }

            if case .uploaded = state {
                Log.info("waitForUpload: \(filename) uploaded")
                return
            }

            if case let .failed(error) = state {
                throw CloudStorageError.UploadFailed(
                    "iCloud upload failed for \(filename): \(error.localizedDescription)"
                )
            }

            Thread.sleep(forTimeInterval: pollInterval)
        }

        Log.info(
            "waitForUpload: timeout diagnostics \(filename) metadataPath=\(resolvedItem.metadataPath ?? "<unknown>") diagnostics=\(uploadDiagnostics(for: resolvedItem.url))"
        )
        logMetadataItems(
            under: url.deletingLastPathComponent(),
            reason: "waitForUpload timeout",
            focusName: filename
        )

        throw CloudStorageError.UploadFailed(
            "iCloud upload timed out for \(filename) after \(defaultTimeout)s"
        )
    }

    // MARK: - Download

    /// Ensures the file is downloaded locally, triggering a download if evicted
    func ensureDownloaded(url: URL, recordId: String) throws {
        // check if already downloaded
        if FileManager.default.fileExists(atPath: url.path), case .current = downloadState(for: url) {
            return
        }

        let deadline = Date().addingTimeInterval(defaultTimeout)
        let filename = url.lastPathComponent

        let resolvedItem: ResolvedMetadataItem
        do {
            resolvedItem = try waitForMetadataItem(
                named: filename,
                parentDirectoryURL: url.deletingLastPathComponent(),
                deadline: deadline
            )
        } catch {
            throw CloudStorageError.DownloadFailed(
                "iCloud metadata lookup failed for \(filename): \(error.localizedDescription)"
            )
        }

        if resolvedItem.url != url {
            Log.info(
                "ensureDownloaded: using metadata URL for \(filename) local=\(url.path) metadata=\(resolvedItem.url.path)"
            )
        }

        // trigger download via startDownloadingUbiquitousItem
        do {
            try FileManager.default.startDownloadingUbiquitousItem(at: resolvedItem.url)
        } catch {
            let nsError = error as NSError
            if nsError.domain == NSCocoaErrorDomain,
               nsError.code == NSFileReadNoSuchFileError || nsError.code == 4
            {
                throw CloudStorageError.NotFound(recordId)
            }
            Log.warn("ensureDownloaded: startDownloading failed for \(filename): \(error.localizedDescription)")
        }

        // poll with periodic re-triggers — the iCloud daemon can silently
        // drop the first request on fresh installs before it's fully ready
        let retriggerInterval: TimeInterval = 5
        var lastRetrigger = Date()
        var lastProgressLog = Date.distantPast

        while Date() < deadline {
            let now = Date()

            if now.timeIntervalSince(lastRetrigger) >= retriggerInterval {
                try? FileManager.default.startDownloadingUbiquitousItem(at: resolvedItem.url)
                lastRetrigger = now
            }

            let state = downloadState(for: resolvedItem.url)

            if now.timeIntervalSince(lastProgressLog) >= progressLogInterval {
                Log.info(
                    "ensureDownloaded: \(filename) state=\(state) metadataPath=\(resolvedItem.metadataPath ?? "<unknown>")"
                )
                lastProgressLog = now
            }

            if case .current = state {
                return
            }

            if case let .failed(error) = state {
                throw CloudStorageError.DownloadFailed(
                    "iCloud download failed: \(error.localizedDescription)"
                )
            }

            Thread.sleep(forTimeInterval: pollInterval)
        }

        // last resort: try coordinated read which forces download
        Log.info("ensureDownloaded: polling timed out, trying coordinated read for \(filename)")
        do {
            _ = try coordinatedRead(from: resolvedItem.url)
            return
        } catch {
            throw CloudStorageError.DownloadFailed(
                "iCloud download timed out after \(defaultTimeout)s (coordinated read also failed: \(error.localizedDescription))"
            )
        }
    }

    // MARK: - Cloud presence via NSMetadataQuery

    /// Runs an NSMetadataQuery and returns all matching items
    ///
    /// Must NOT be called from the main thread
    func metadataQuery(predicate: NSPredicate) throws -> [NSMetadataItem] {
        let semaphore = DispatchSemaphore(value: 0)
        var results: [NSMetadataItem] = []
        var startFailed = false
        let query = NSMetadataQuery()
        let box = ObserverBox()
        var finalizeWorkItem: DispatchWorkItem?
        var didSignal = false

        let captureResults = {
            (0 ..< query.resultCount).compactMap { query.result(at: $0) as? NSMetadataItem }
        }

        let finishQuery = { (reason: String) in
            guard !didSignal else { return }
            didSignal = true
            finalizeWorkItem?.cancel()
            query.disableUpdates()
            results = captureResults()
            Log.info(
                "metadataQuery: finalized reason=\(reason) count=\(results.count) predicate=\(predicate.predicateFormat)"
            )
            query.stop()
            box.removeAll()
            semaphore.signal()
        }

        DispatchQueue.main.async {
            query.searchScopes = [NSMetadataQueryUbiquitousDataScope]
            query.predicate = predicate

            let scheduleFinalize = { (reason: String) in
                finalizeWorkItem?.cancel()
                let workItem = DispatchWorkItem {
                    finishQuery(reason)
                }
                finalizeWorkItem = workItem
                DispatchQueue.main.asyncAfter(
                    deadline: .now() + self.metadataSettleInterval,
                    execute: workItem
                )
            }

            box.add(
                NotificationCenter.default.addObserver(
                    forName: .NSMetadataQueryDidFinishGathering,
                    object: query,
                    queue: .main
                ) { _ in
                    Log.info(
                        "metadataQuery: finish gathering count=\(query.resultCount) predicate=\(predicate.predicateFormat)"
                    )
                    scheduleFinalize("finish")
                }
            )
            box.add(
                NotificationCenter.default.addObserver(
                    forName: .NSMetadataQueryDidUpdate,
                    object: query,
                    queue: .main
                ) { _ in
                    Log.info(
                        "metadataQuery: update count=\(query.resultCount) predicate=\(predicate.predicateFormat)"
                    )
                    scheduleFinalize("update")
                }
            )

            Log.info("metadataQuery: starting predicate=\(predicate.predicateFormat)")
            if !query.start() {
                startFailed = true
                box.removeAll()
                semaphore.signal()
            }
        }

        if semaphore.wait(timeout: .now() + defaultTimeout) == .timedOut {
            DispatchQueue.main.async {
                finalizeWorkItem?.cancel()
                query.stop()
                box.removeAll()
            }
            throw CloudStorageError.NotAvailable("iCloud metadata query timed out")
        }

        if startFailed {
            throw CloudStorageError.NotAvailable("failed to start iCloud metadata query")
        }

        return results
    }

    /// Authoritatively checks whether a file exists in iCloud (finds evicted files too)
    ///
    /// Must NOT be called from the main thread
    func fileExistsInCloud(name: String) throws -> Bool {
        let predicate = NSPredicate(format: "%K == %@", NSMetadataItemFSNameKey, name)
        let results = try metadataQuery(predicate: predicate)
        return !results.isEmpty
    }

    /// Resolve symlinks so /var and /private/var compare correctly
    private static func resolvedPath(_ path: String) -> String {
        URL(fileURLWithPath: path).resolvingSymlinksInPath().path
    }

    private static func metadataPath(for item: NSMetadataItem) -> String? {
        if let path = item.value(forAttribute: NSMetadataItemPathKey) as? String {
            return resolvedPath(path)
        }
        if let url = item.value(forAttribute: NSMetadataItemURLKey) as? URL {
            return resolvedPath(url.path)
        }
        return nil
    }

    private static func resolvedItem(
        named name: String,
        under resolvedParent: String,
        in query: NSMetadataQuery
    ) -> ResolvedMetadataItem? {
        let prefix = resolvedParent + "/"

        for index in 0 ..< query.resultCount {
            guard let item = query.result(at: index) as? NSMetadataItem else {
                continue
            }
            guard let itemName = item.value(forAttribute: NSMetadataItemFSNameKey) as? String else {
                continue
            }
            guard itemName == name else { continue }
            guard let metadataURL = item.value(forAttribute: NSMetadataItemURLKey) as? URL else {
                continue
            }
            let metadataPath = Self.metadataPath(for: item)
            if let metadataPath, metadataPath.hasPrefix(prefix) {
                return ResolvedMetadataItem(url: metadataURL, metadataPath: metadataPath)
            }
        }

        return nil
    }

    private static func metadataItemSummary(_ item: NSMetadataItem) -> String {
        let name = (item.value(forAttribute: NSMetadataItemFSNameKey) as? String) ?? "<unknown>"
        let path = metadataPath(for: item) ?? "<no-path>"
        let url =
            ((item.value(forAttribute: NSMetadataItemURLKey) as? URL)?.path) ?? "<no-url>"
        return "name=\(name) path=\(path) url=\(url)"
    }

    private static func metadataItemSummaries(in query: NSMetadataQuery) -> [String] {
        (0 ..< query.resultCount).compactMap { index in
            guard let item = query.result(at: index) as? NSMetadataItem else {
                return nil
            }
            return metadataItemSummary(item)
        }
    }

    private func logMetadataItems(
        under parentDirectoryURL: URL,
        reason: String,
        focusName: String
    ) {
        let resolvedParent = Self.resolvedPath(parentDirectoryURL.path)
        let query = NSMetadataQuery()
        let semaphore = DispatchSemaphore(value: 0)
        let box = ObserverBox()
        var didSignal = false

        let finish = {
            guard !didSignal else { return }
            didSignal = true
            let summaries = Self.metadataItemSummaries(in: query)
            Log.info(
                "metadataItems: reason=\(reason) focus=\(focusName) parent=\(resolvedParent) count=\(summaries.count)"
            )
            for summary in summaries {
                Log.info("metadataItems: \(summary)")
            }
            query.stop()
            box.removeAll()
            semaphore.signal()
        }

        DispatchQueue.main.async {
            query.searchScopes = [parentDirectoryURL]
            query.predicate = NSPredicate(value: true)

            box.add(
                NotificationCenter.default.addObserver(
                    forName: .NSMetadataQueryDidFinishGathering,
                    object: query,
                    queue: .main
                ) { _ in
                    finish()
                }
            )
            box.add(
                NotificationCenter.default.addObserver(
                    forName: .NSMetadataQueryDidUpdate,
                    object: query,
                    queue: .main
                ) { _ in
                    finish()
                }
            )

            if !query.start() {
                Log.info(
                    "metadataItems: failed to start reason=\(reason) focus=\(focusName) parent=\(resolvedParent)"
                )
                box.removeAll()
                semaphore.signal()
            }
        }

        if semaphore.wait(timeout: .now() + 5) == .timedOut {
            DispatchQueue.main.async {
                query.stop()
                box.removeAll()
            }
            Log.info(
                "metadataItems: timed out reason=\(reason) focus=\(focusName) parent=\(resolvedParent)"
            )
        }
    }

    private func waitForMetadataItem(
        named name: String,
        parentDirectoryURL: URL,
        deadline: Date
    ) throws -> ResolvedMetadataItem {
        let resolvedParent = Self.resolvedPath(parentDirectoryURL.path)
        let predicate = NSPredicate(format: "%K == %@", NSMetadataItemFSNameKey, name)
        let semaphore = DispatchSemaphore(value: 0)
        var resolvedItem: ResolvedMetadataItem?
        var failure: MetadataLookupError?
        let query = NSMetadataQuery()
        let box = ObserverBox()
        var didSignal = false

        let finish = { (item: ResolvedMetadataItem?, error: MetadataLookupError?) in
            guard !didSignal else { return }
            didSignal = true
            resolvedItem = item
            failure = error
            query.stop()
            box.removeAll()
            semaphore.signal()
        }

        DispatchQueue.main.async {
            query.searchScopes = [NSMetadataQueryUbiquitousDataScope]
            query.predicate = predicate

            let evaluate = { (reason: String) in
                if let item = Self.resolvedItem(named: name, under: resolvedParent, in: query) {
                    Log.info(
                        "metadataLookup: resolved name=\(name) reason=\(reason) url=\(item.url.path) metadataPath=\(item.metadataPath ?? "<unknown>")"
                    )
                    finish(item, nil)
                    return
                }

                Log.info(
                    "metadataLookup: no match yet name=\(name) reason=\(reason) count=\(query.resultCount) parent=\(resolvedParent)"
                )
                for summary in Self.metadataItemSummaries(in: query) {
                    Log.info("metadataLookup: item \(summary)")
                }
            }

            box.add(
                NotificationCenter.default.addObserver(
                    forName: .NSMetadataQueryDidFinishGathering,
                    object: query,
                    queue: .main
                ) { _ in
                    evaluate("finish")
                }
            )
            box.add(
                NotificationCenter.default.addObserver(
                    forName: .NSMetadataQueryDidUpdate,
                    object: query,
                    queue: .main
                ) { _ in
                    evaluate("update")
                }
            )

            Log.info(
                "metadataLookup: starting name=\(name) parent=\(resolvedParent) predicate=\(predicate.predicateFormat)"
            )
            if !query.start() {
                finish(
                    nil,
                    .startFailed("failed to start iCloud metadata query for \(name)")
                )
            }
        }

        if semaphore.wait(timeout: .now() + deadline.timeIntervalSinceNow) == .timedOut {
            DispatchQueue.main.async {
                finish(
                    nil,
                    .timedOut("iCloud metadata query timed out for \(name)")
                )
            }
            _ = semaphore.wait(timeout: .now() + 1)
        }

        if let failure {
            throw failure
        }

        guard let resolvedItem else {
            throw MetadataLookupError.missingURL(
                "iCloud metadata query finished without a URL for \(name)"
            )
        }

        return resolvedItem
    }

    private func resolvedMetadataItemIfPresent(
        named name: String,
        parentDirectoryURL: URL
    ) -> ResolvedMetadataItem? {
        let resolvedParent = Self.resolvedPath(parentDirectoryURL.path)
        let predicate = NSPredicate(format: "%K == %@", NSMetadataItemFSNameKey, name)
        let semaphore = DispatchSemaphore(value: 0)
        let query = NSMetadataQuery()
        let box = ObserverBox()
        var match: ResolvedMetadataItem?
        var didSignal = false

        let finish = {
            guard !didSignal else { return }
            didSignal = true
            query.stop()
            box.removeAll()
            semaphore.signal()
        }

        DispatchQueue.main.async {
            query.searchScopes = [NSMetadataQueryUbiquitousDataScope]
            query.predicate = predicate

            let evaluate = {
                if let resolved = Self.resolvedItem(named: name, under: resolvedParent, in: query) {
                    match = resolved
                }
                finish()
            }

            box.add(
                NotificationCenter.default.addObserver(
                    forName: .NSMetadataQueryDidFinishGathering,
                    object: query,
                    queue: .main
                ) { _ in
                    evaluate()
                }
            )
            box.add(
                NotificationCenter.default.addObserver(
                    forName: .NSMetadataQueryDidUpdate,
                    object: query,
                    queue: .main
                ) { _ in
                    evaluate()
                }
            )

            if !query.start() {
                finish()
            }
        }

        if semaphore.wait(timeout: .now() + 5) == .timedOut {
            DispatchQueue.main.async {
                finish()
            }
            _ = semaphore.wait(timeout: .now() + 1)
        }

        return match
    }

    private func uploadState(for url: URL) -> UploadState {
        // clear cached resource values to prevent stale reads
        var freshURL = url
        freshURL.removeAllCachedResourceValues()

        guard
            let values = try? freshURL.resourceValues(forKeys: [
                .isUbiquitousItemKey,
                .ubiquitousItemIsUploadingKey,
                .ubiquitousItemIsUploadedKey,
                .ubiquitousItemUploadingErrorKey,
            ])
        else {
            return .unknown
        }

        guard values.isUbiquitousItem == true else {
            return .notUbiquitous
        }

        if values.ubiquitousItemIsUploaded == true {
            return .uploaded
        }

        if let error = values.ubiquitousItemUploadingError {
            return .failed(error)
        }

        return .uploading
    }

    private func uploadDiagnostics(for url: URL) -> String {
        let exists = FileManager.default.fileExists(atPath: url.path)
        let fileSize: String =
            if exists,
            let attributes = try? FileManager.default.attributesOfItem(atPath: url.path),
            let size = attributes[.size] as? NSNumber {
                size.stringValue
            } else {
                "nil"
            }

        guard
            let values = try? url.resourceValues(forKeys: [
                .isUbiquitousItemKey,
                .ubiquitousItemIsUploadingKey,
                .ubiquitousItemIsUploadedKey,
                .ubiquitousItemUploadingErrorKey,
            ])
        else {
            return "exists=\(exists) fileSize=\(fileSize) values=<unavailable>"
        }

        let errorDescription = values.ubiquitousItemUploadingError?.localizedDescription ?? "nil"

        return
            "exists=\(exists) fileSize=\(fileSize) isUbiquitous=\(String(describing: values.isUbiquitousItem)) isUploading=\(String(describing: values.ubiquitousItemIsUploading)) isUploaded=\(String(describing: values.ubiquitousItemIsUploaded)) uploadingError=\(errorDescription)"
    }

    private func downloadState(for url: URL) -> DownloadState {
        guard
            let values = try? url.resourceValues(forKeys: [
                .isUbiquitousItemKey,
                .ubiquitousItemIsDownloadingKey,
                .ubiquitousItemDownloadingStatusKey,
                .ubiquitousItemDownloadingErrorKey,
            ])
        else {
            return .unknown
        }

        guard values.isUbiquitousItem == true else {
            return .notUbiquitous
        }

        if values.ubiquitousItemDownloadingStatus == .current {
            return .current
        }

        if let error = values.ubiquitousItemDownloadingError {
            return .failed(error)
        }

        if values.ubiquitousItemIsDownloading == true {
            return .downloading
        }

        return .notDownloaded
    }

    /// Lists immediate subdirectory names within a parent path via FileManager
    ///
    /// Handles iCloud evicted directories that appear as hidden .icloud stubs
    func listSubdirectories(parentPath: String) throws -> [String] {
        let parentURL = URL(fileURLWithPath: parentPath, isDirectory: true)
        let contents = try FileManager.default.contentsOfDirectory(
            at: parentURL, includingPropertiesForKeys: [.isDirectoryKey],
            options: []
        )

        return contents.compactMap { url -> String? in
            var name = url.lastPathComponent

            // iCloud evicted entries appear as .Name.icloud
            if name.hasPrefix("."), name.hasSuffix(".icloud") {
                name = String(name.dropFirst().dropLast(".icloud".count))
                return name
            }

            guard url.hasDirectoryPath else { return nil }
            return name
        }.sorted()
    }

    /// Lists filenames matching a prefix within a namespace directory via FileManager
    ///
    /// Handles iCloud evicted files (.icloud stubs) by stripping the stub wrapper
    func listFiles(namespacePath: String, prefix: String) throws -> [String] {
        let dirURL = URL(fileURLWithPath: namespacePath, isDirectory: true)
        let contents = try FileManager.default.contentsOfDirectory(
            at: dirURL, includingPropertiesForKeys: nil,
            options: []
        )

        return contents.compactMap { url -> String? in
            var name = url.lastPathComponent

            // iCloud evicted files appear as .FileName.icloud
            if name.hasPrefix("."), name.hasSuffix(".icloud") {
                name = String(name.dropFirst().dropLast(".icloud".count))
            }

            guard name.hasPrefix(prefix) else { return nil }
            return name
        }.sorted()
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

        switch uploadState(for: url) {
        case .uploaded: return .uploaded
        case let .failed(error): return .failed(error.localizedDescription)
        case .uploading, .notUbiquitous, .unknown: return .uploading
        }
    }

    func isBackupUploaded(namespace: String, recordId: String) throws -> Bool {
        let url = try backupFileURL(namespace: namespace, recordId: recordId)
        let resolvedURL =
            resolvedMetadataItemIfPresent(
                named: url.lastPathComponent,
                parentDirectoryURL: url.deletingLastPathComponent()
            )?.url ?? url

        let state = uploadState(for: resolvedURL)
        let usedMetadata = resolvedURL != url
        Log.info(
            "isBackupUploaded: recordId=\(recordId.prefix(12))… state=\(state) usedMetadata=\(usedMetadata)"
        )

        return if case .uploaded = state {
            true
        } else {
            false
        }
    }

    /// Checks sync health of all files in namespace directories
    func overallSyncHealth() -> SyncHealth {
        guard let namespacesRoot = try? namespacesRootURL() else {
            return .unavailable
        }

        guard
            let namespaceDirs = try? FileManager.default.contentsOfDirectory(
                at: namespacesRoot, includingPropertiesForKeys: nil,
                options: .skipsHiddenFiles
            )
        else {
            return .unavailable
        }

        var hasFiles = false
        var allUploaded = true
        var anyFailed = false
        var failureMessage: String?

        for nsDir in namespaceDirs where nsDir.hasDirectoryPath {
            guard
                let files = try? FileManager.default.contentsOfDirectory(
                    at: nsDir, includingPropertiesForKeys: nil
                )
            else { continue }

            for file in files where file.pathExtension == "json" {
                hasFiles = true
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
        }

        if !hasFiles { return .noFiles }
        if anyFailed { return .failed(failureMessage ?? "upload error") }
        if allUploaded { return .allUploaded }
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

import Foundation

@_exported import CoveCore
import SwiftUI

extension WeakReconciler: CloudBackupManagerReconciler where Reconciler == CloudBackupManager {}

@Observable
final class CloudBackupManager: AnyReconciler, CloudBackupManagerReconciler, @unchecked Sendable {
    static let shared = CloudBackupManager()

    typealias Message = CloudBackupReconcileMessage

    @ObservationIgnored let rust: RustCloudBackupManager
    var state: CloudBackupState = .disabled
    var progress: (completed: UInt32, total: UInt32)?
    var restoreReport: CloudBackupRestoreReport?
    var syncError: String?
    var hasPendingUploadVerification = false

    private init() {
        let rust = RustCloudBackupManager()
        self.rust = rust
        rust.listenForUpdates(reconciler: WeakReconciler(self))
        state = rust.currentState()
        hasPendingUploadVerification = rust.hasPendingCloudUploadVerification()
    }

    private func apply(_ message: Message) {
        switch message {
        case let .stateChanged(newState):
            state = newState
        case let .progressUpdated(completed, total):
            progress = (completed, total)
        case .enableComplete:
            progress = nil
        case let .restoreComplete(report):
            restoreReport = report
            progress = nil
        case let .syncFailed(error):
            syncError = error
        case let .pendingUploadVerificationChanged(pending):
            hasPendingUploadVerification = pending
        }
    }

    func reconcile(message: Message) {
        DispatchQueue.main.async { [weak self] in
            self?.apply(message)
        }
    }

    func reconcileMany(messages: [Message]) {
        DispatchQueue.main.async { [weak self] in
            messages.forEach { self?.apply($0) }
        }
    }
}

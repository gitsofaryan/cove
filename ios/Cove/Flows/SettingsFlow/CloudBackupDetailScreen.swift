import SwiftUI

struct CloudBackupDetailScreen: View {
    @State private var detail: CloudBackupDetail?
    @State private var isSyncing = false
    @State private var syncError: String?
    @State private var cloudOnlyWallets: [CloudBackupWalletItem]?
    @State private var isLoadingCloudOnly = false
    @State private var syncHealth: ICloudDriveHelper.SyncHealth = .noFiles

    @State private var verificationReport: DeepVerificationReport?
    @State private var verificationFailure: DeepVerificationFailure?
    @State private var isVerifying = false
    @State private var hasStartedVerification = false
    @State private var isRecreatingManifest = false
    @State private var isReinitializingBackup = false
    @State private var isRepairingPasskey = false
    @State private var recoveryError: String?
    @State private var userCancelledVerification = false

    @State private var showRecreateConfirmation = false
    @State private var showReinitializeConfirmation = false

    var body: some View {
        Form {
            if isVerifying, verificationReport == nil, verificationFailure == nil,
               !userCancelledVerification
            {
                Section {
                    VStack {
                        ProgressView("Verifying cloud backup...")
                    }
                    .frame(maxWidth: .infinity)
                    .padding(.vertical, 8)
                }
            } else if let detail, !userCancelledVerification {
                DetailFormContent(
                    detail: detail,
                    syncHealth: syncHealth,
                    isSyncing: $isSyncing,
                    syncError: $syncError,
                    cloudOnlyWallets: $cloudOnlyWallets,
                    isLoadingCloudOnly: $isLoadingCloudOnly,
                    onCloudOnlyChanged: { Task { await refreshDetailOnly() } }
                )
            }

            VerificationSection(
                report: verificationReport,
                failure: verificationFailure,
                userCancelled: userCancelledVerification,
                isVerifying: isVerifying,
                isRecreatingManifest: isRecreatingManifest,
                isReinitializingBackup: isReinitializingBackup,
                isRepairingPasskey: isRepairingPasskey,
                recoveryError: recoveryError,
                onVerify: { startDeepVerification() },
                onRecreate: { showRecreateConfirmation = true },
                onReinitialize: { showReinitializeConfirmation = true },
                onRepairPasskey: { repairPasskey() }
            )
        }
        .navigationTitle("Cloud Backup")
        .navigationBarTitleDisplayMode(.inline)
        .task {
            startDeepVerificationIfNeeded()
        }
        .onChange(of: CloudBackupManager.shared.state) { _, newState in
            if isSyncing, newState != .enabling {
                Task { await refreshDetailOnly() }
                isSyncing = false
            }

            if isReinitializingBackup {
                switch newState {
                case .enabled:
                    isReinitializingBackup = false
                    startDeepVerification()
                case let .error(message):
                    isReinitializingBackup = false
                    recoveryError = message
                default:
                    break
                }
            }
        }
        .onChange(of: CloudBackupManager.shared.syncError) { _, error in
            syncError = error
            CloudBackupManager.shared.syncError = nil
        }
        .confirmationDialog(
            "Recreate Backup Index",
            isPresented: $showRecreateConfirmation,
            titleVisibility: .visible
        ) {
            Button("Recreate", role: .destructive) {
                recreateManifest()
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text(
                "This will rebuild the backup index from wallets on this device. Wallets that only exist in the cloud backup will no longer be referenced."
            )
        }
        .confirmationDialog(
            "Reinitialize Cloud Backup",
            isPresented: $showReinitializeConfirmation,
            titleVisibility: .visible
        ) {
            Button("Reinitialize", role: .destructive) {
                reinitializeBackup()
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text(
                "This will replace your entire cloud backup. Wallets that only exist in the current cloud backup will be lost."
            )
        }
    }

    private func startDeepVerificationIfNeeded() {
        guard !hasStartedVerification else { return }
        startDeepVerification()
    }

    private func startDeepVerification() {
        guard !isRecreatingManifest, !isReinitializingBackup, !isRepairingPasskey else { return }

        hasStartedVerification = true
        isVerifying = true
        verificationReport = nil
        verificationFailure = nil
        recoveryError = nil
        userCancelledVerification = false

        Task {
            let result = await Task.detached {
                CloudBackupManager.shared.rust.deepVerifyCloudBackup()
            }.value

            isVerifying = false

            switch result {
            case let .verified(report):
                verificationReport = report
                verificationFailure = nil
                if let reportDetail = report.detail {
                    detail = reportDetail
                }
            case let .userCancelled(cancelDetail):
                verificationReport = nil
                verificationFailure = nil
                userCancelledVerification = true
                if let cancelDetail {
                    detail = cancelDetail
                }
            case .notEnabled:
                break
            case let .failed(failure):
                verificationFailure = failure
                verificationReport = nil
                if let failureDetail = extractDetail(from: failure) {
                    detail = failureDetail
                }
            }

            syncHealth = ICloudDriveHelper.shared.overallSyncHealth()
        }
    }

    private func recreateManifest() {
        isRecreatingManifest = true
        recoveryError = nil

        Task {
            let error = await Task.detached {
                CloudBackupManager.shared.rust.reuploadAllWallets()
            }.value

            if let error {
                recoveryError = error
                isRecreatingManifest = false
                return
            }

            isRecreatingManifest = false
            startDeepVerification()
        }
    }

    private func reinitializeBackup() {
        isReinitializingBackup = true
        recoveryError = nil
        CloudBackupManager.shared.rust.enableCloudBackup()
    }

    private func repairPasskey() {
        isRepairingPasskey = true
        recoveryError = nil

        Task {
            let error = await Task.detached {
                CloudBackupManager.shared.rust.repairPasskeyWrapper()
            }.value

            if let error {
                recoveryError = error
                isRepairingPasskey = false
                return
            }

            isRepairingPasskey = false
            startDeepVerification()
        }
    }

    private func refreshDetailOnly() async {
        let result = await Task.detached {
            CloudBackupManager.shared.rust.refreshCloudBackupDetail()
        }.value

        guard let result else { return }

        switch result {
        case let .success(refreshedDetail):
            detail = refreshedDetail
        case .accessError:
            break
        }

        syncHealth = ICloudDriveHelper.shared.overallSyncHealth()
    }

    private func extractDetail(from failure: DeepVerificationFailure) -> CloudBackupDetail? {
        switch failure {
        case let .retry(_, detail),
             let .recreateManifest(_, detail, _),
             let .reinitializeBackup(_, detail, _),
             let .unsupportedVersion(_, detail):
            detail
        }
    }
}

// MARK: - Verification Section

private struct VerificationSection: View {
    let report: DeepVerificationReport?
    let failure: DeepVerificationFailure?
    let userCancelled: Bool
    let isVerifying: Bool
    let isRecreatingManifest: Bool
    let isReinitializingBackup: Bool
    let isRepairingPasskey: Bool
    let recoveryError: String?
    let onVerify: () -> Void
    let onRecreate: () -> Void
    let onReinitialize: () -> Void
    let onRepairPasskey: () -> Void

    private var isRecovering: Bool {
        isRecreatingManifest || isReinitializingBackup || isRepairingPasskey
    }

    var body: some View {
        if isVerifying {
            Section {
                HStack {
                    ProgressView()
                        .padding(.trailing, 8)
                    Text("Verifying backup integrity...")
                }
            }
        } else if let report {
            verifiedSection(report)
        } else if let failure {
            failureSection(failure)
        } else if userCancelled {
            cancelledSection
        }
    }

    private var cancelledSection: some View {
        Section {
            Label(
                "Verification was cancelled",
                systemImage: "exclamationmark.shield.fill"
            )
            .foregroundStyle(.orange)

            Text(
                "If your passkey was deleted, tap \"Create New Passkey\" to restore cloud backup protection. Otherwise tap \"Verify Now\" to try again."
            )
            .font(.caption)
            .foregroundStyle(.secondary)

            Button {
                onVerify()
            } label: {
                Label("Verify Now", systemImage: "checkmark.shield")
            }
            .disabled(isRecovering)

            repairPasskeyButton
        }
    }

    @ViewBuilder
    private func verifiedSection(_ report: DeepVerificationReport) -> some View {
        Section {
            Label("Backup verified", systemImage: "checkmark.shield.fill")
                .foregroundStyle(.green)

            if report.masterKeyWrapperRepaired {
                Label(
                    "Cloud master key protection was repaired",
                    systemImage: "wrench.and.screwdriver.fill"
                )
                .foregroundStyle(.blue)
                .font(.caption)
            }

            if report.localMasterKeyRepaired {
                Label(
                    "Local backup credentials were repaired from cloud",
                    systemImage: "wrench.and.screwdriver.fill"
                )
                .foregroundStyle(.blue)
                .font(.caption)
            }

            if report.walletsFailed > 0 {
                Label(
                    "\(report.walletsFailed) wallet backup(s) could not be decrypted",
                    systemImage: "exclamationmark.triangle.fill"
                )
                .foregroundStyle(.red)
                .font(.caption)
            }

            if report.walletsUnsupported > 0 {
                Label(
                    "\(report.walletsUnsupported) wallet(s) use a newer backup format",
                    systemImage: "info.circle.fill"
                )
                .foregroundStyle(.orange)
                .font(.caption)
            }

            if report.walletsVerified > 0 {
                Text("\(report.walletsVerified) wallet(s) verified")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }

        verifyButton
    }

    @ViewBuilder
    private func failureSection(_ failure: DeepVerificationFailure) -> some View {
        Section {
            switch failure {
            case let .retry(message, _):
                Label(message, systemImage: "exclamationmark.triangle.fill")
                    .foregroundStyle(.orange)

                retryButton
                repairPasskeyButton

            case let .recreateManifest(message, _, warning):
                Label(message, systemImage: "exclamationmark.triangle.fill")
                    .foregroundStyle(.red)

                Text(warning)
                    .font(.caption)
                    .foregroundStyle(.secondary)

                Button(role: .destructive) {
                    onRecreate()
                } label: {
                    if isRecreatingManifest {
                        HStack {
                            ProgressView()
                                .padding(.trailing, 4)
                            Text("Recreating...")
                        }
                    } else {
                        Label("Recreate Backup Index", systemImage: "arrow.clockwise")
                    }
                }
                .disabled(isVerifying || isRecovering)

            case let .reinitializeBackup(message, _, warning):
                Label(message, systemImage: "exclamationmark.triangle.fill")
                    .foregroundStyle(.red)

                Text(warning)
                    .font(.caption)
                    .foregroundStyle(.secondary)

                Button(role: .destructive) {
                    onReinitialize()
                } label: {
                    if isReinitializingBackup {
                        HStack {
                            ProgressView()
                                .padding(.trailing, 4)
                            Text("Reinitializing...")
                        }
                    } else {
                        Label("Reinitialize Cloud Backup", systemImage: "arrow.counterclockwise")
                    }
                }
                .disabled(isVerifying || isRecovering)

            case let .unsupportedVersion(message, _):
                Label(message, systemImage: "exclamationmark.triangle.fill")
                    .foregroundStyle(.orange)

                Text("Please update the app to the latest version")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }

        if let recoveryError {
            Section {
                Label(recoveryError, systemImage: "xmark.circle.fill")
                    .foregroundStyle(.red)
                    .font(.caption)
            }
        }
    }

    private var verifyButton: some View {
        Section {
            Button {
                onVerify()
            } label: {
                Label("Verify Again", systemImage: "checkmark.shield")
            }
            .disabled(isVerifying || isRecovering)
        }
    }

    private var retryButton: some View {
        Button {
            onVerify()
        } label: {
            Label("Try Again", systemImage: "arrow.clockwise")
        }
        .disabled(isVerifying || isRecovering)
    }

    private var repairPasskeyButton: some View {
        Button {
            onRepairPasskey()
        } label: {
            if isRepairingPasskey {
                HStack {
                    ProgressView()
                        .padding(.trailing, 4)
                    Text("Creating Passkey...")
                }
            } else {
                Label("Create New Passkey", systemImage: "person.badge.key")
            }
        }
        .disabled(isVerifying || isRecovering)
    }
}

// MARK: - Detail Form Content

private struct DetailFormContent: View {
    let detail: CloudBackupDetail
    let syncHealth: ICloudDriveHelper.SyncHealth
    @Binding var isSyncing: Bool
    @Binding var syncError: String?
    @Binding var cloudOnlyWallets: [CloudBackupWalletItem]?
    @Binding var isLoadingCloudOnly: Bool
    var onCloudOnlyChanged: () -> Void = {}

    var body: some View {
        HeaderSection(lastSync: detail.lastSync, syncHealth: syncHealth)
        if !detail.backedUp.isEmpty { BackedUpSections(wallets: detail.backedUp) }
        if !detail.notBackedUp.isEmpty {
            NotBackedUpSections(wallets: detail.notBackedUp)
            SyncSection(isSyncing: $isSyncing, syncError: $syncError)
        }
        if detail.cloudOnlyCount > 0 || cloudOnlyWallets?.isEmpty == false {
            CloudOnlySection(
                count: detail.cloudOnlyCount,
                wallets: $cloudOnlyWallets,
                isLoading: $isLoadingCloudOnly,
                onChanged: onCloudOnlyChanged
            )
        }
    }
}

// MARK: - Header

private struct HeaderSection: View {
    let lastSync: UInt64?
    let syncHealth: ICloudDriveHelper.SyncHealth

    var body: some View {
        Section {
            VStack(spacing: 8) {
                headerIcon
                    .font(.largeTitle)

                Text("Cloud Backup Active")
                    .fontWeight(.semibold)

                if let lastSync {
                    Text("Last synced \(formatDate(lastSync))")
                        .font(.caption)
                        .foregroundStyle(.secondary)

                    syncHealthLabel
                }
            }
            .frame(maxWidth: .infinity)
            .padding(.vertical, 8)
        }
    }

    @ViewBuilder
    private var headerIcon: some View {
        switch syncHealth {
        case .allUploaded, .noFiles:
            Image(systemName: "checkmark.icloud.fill")
                .foregroundColor(.green)
        case .uploading:
            Image(systemName: "arrow.clockwise.icloud.fill")
                .foregroundColor(.blue)
        case .failed:
            Image(systemName: "exclamationmark.icloud.fill")
                .foregroundColor(.red)
        case .unavailable:
            Image(systemName: "checkmark.icloud.fill")
                .foregroundColor(.green)
        }
    }

    @ViewBuilder
    private var syncHealthLabel: some View {
        switch syncHealth {
        case .allUploaded:
            Label("All files synced to iCloud", systemImage: "checkmark.circle.fill")
                .font(.caption)
                .foregroundStyle(.green)
        case .uploading:
            HStack(spacing: 4) {
                ProgressView()
                    .controlSize(.mini)
                Text("Syncing to iCloud...")
            }
            .font(.caption)
            .foregroundStyle(.secondary)
        case let .failed(message):
            Label("Sync error: \(message)", systemImage: "exclamationmark.triangle.fill")
                .font(.caption)
                .foregroundStyle(.red)
        case .noFiles, .unavailable:
            EmptyView()
        }
    }

    private func formatDate(_ timestamp: UInt64) -> String {
        let date = Date(timeIntervalSince1970: TimeInterval(timestamp))
        return date.formatted(date: .abbreviated, time: .shortened)
    }
}

// MARK: - Sync Section

private struct SyncSection: View {
    @Binding var isSyncing: Bool
    @Binding var syncError: String?

    var body: some View {
        Section {
            Button {
                syncError = nil
                isSyncing = true
                CloudBackupManager.shared.rust.syncUnsyncedWallets()
            } label: {
                HStack {
                    if isSyncing {
                        ProgressView()
                            .padding(.trailing, 8)
                        Text("Syncing...")
                    } else {
                        Image(systemName: "arrow.triangle.2.circlepath")
                        Text("Sync Now")
                    }
                }
            }
            .disabled(isSyncing)

            if let syncError {
                Text(syncError)
                    .font(.caption)
                    .foregroundStyle(.red)
            }
        }
    }
}

// MARK: - Cloud Only Section

private struct CloudOnlySection: View {
    let count: UInt32
    @Binding var wallets: [CloudBackupWalletItem]?
    @Binding var isLoading: Bool
    var onChanged: () -> Void = {}

    @State private var operatingOn: String?
    @State private var operationError: String?
    @State private var selectedWallet: CloudBackupWalletItem?
    @State private var walletToDelete: CloudBackupWalletItem?

    var body: some View {
        Section(header: Text("Not on This Device")) {
            if let wallets {
                ForEach(wallets, id: \.name) { item in
                    Button {
                        selectedWallet = item
                    } label: {
                        HStack {
                            if operatingOn == item.recordId {
                                ProgressView()
                                    .padding(.trailing, 8)
                            }
                            WalletItemRow(item: item)
                        }
                    }
                    .foregroundStyle(.primary)
                    .disabled(operatingOn != nil)
                }

                if let operationError {
                    Text(operationError)
                        .font(.caption)
                        .foregroundStyle(.red)
                }
            } else {
                HStack {
                    Image(systemName: "icloud.and.arrow.down")
                    Text("\(count) wallet(s) in cloud not on this device")
                }
                .foregroundStyle(.secondary)

                Button {
                    isLoading = true
                    Task.detached {
                        let items = CloudBackupManager.shared.rust.fetchCloudOnlyWallets()
                        await MainActor.run {
                            wallets = items
                            isLoading = false
                        }
                    }
                } label: {
                    HStack {
                        if isLoading {
                            ProgressView()
                                .padding(.trailing, 8)
                            Text("Loading...")
                        } else {
                            Image(systemName: "info.circle")
                            Text("Get More Info")
                        }
                    }
                }
                .disabled(isLoading)
            }
        }
        .confirmationDialog(
            selectedWallet?.name ?? "Wallet",
            isPresented: Binding(
                get: { selectedWallet != nil },
                set: { if !$0 { selectedWallet = nil } }
            ),
            titleVisibility: .visible
        ) {
            if let item = selectedWallet, let recordId = item.recordId {
                Button("Restore to This Device") {
                    restoreWallet(recordId: recordId, name: item.name)
                }
                Button("Delete from iCloud", role: .destructive) {
                    walletToDelete = item
                }
            }
            Button("Cancel", role: .cancel) {}
        }
        .alert(
            "Delete \(walletToDelete?.name ?? "wallet")?",
            isPresented: Binding(
                get: { walletToDelete != nil },
                set: { if !$0 { walletToDelete = nil } }
            )
        ) {
            if let item = walletToDelete, let recordId = item.recordId {
                Button("Delete", role: .destructive) {
                    deleteWallet(recordId: recordId, name: item.name)
                }
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("This wallet backup will be permanently removed from iCloud")
        }
    }

    private func restoreWallet(recordId: String, name: String) {
        operatingOn = recordId
        operationError = nil

        Task {
            let error = await Task.detached {
                CloudBackupManager.shared.rust.restoreCloudWallet(recordId: recordId)
            }.value

            if let error {
                operationError = "Restore \(name): \(error)"
            } else {
                wallets?.removeAll { $0.recordId == recordId }
                if wallets?.isEmpty == true { wallets = nil }
                onChanged()
            }
            operatingOn = nil
        }
    }

    private func deleteWallet(recordId: String, name: String) {
        operatingOn = recordId
        operationError = nil

        Task {
            let error = await Task.detached {
                CloudBackupManager.shared.rust.deleteCloudWallet(recordId: recordId)
            }.value

            if let error {
                operationError = "Delete \(name): \(error)"
            } else {
                wallets?.removeAll { $0.recordId == recordId }
                if wallets?.isEmpty == true { wallets = nil }
                onChanged()
            }
            operatingOn = nil
        }
    }
}

// MARK: - Backed Up Sections

private struct BackedUpSections: View {
    let wallets: [CloudBackupWalletItem]

    var body: some View {
        let grouped = Dictionary(grouping: wallets) {
            GroupKey(network: $0.network, walletMode: $0.walletMode)
        }

        ForEach(grouped.keys.sorted(), id: \.self) { key in
            Section(header: Text(key.title)) {
                ForEach(grouped[key]!, id: \.name) { item in
                    WalletItemRow(item: item)
                }
            }
        }
    }
}

// MARK: - Not Backed Up Sections

private struct NotBackedUpSections: View {
    let wallets: [CloudBackupWalletItem]

    var body: some View {
        let grouped = Dictionary(grouping: wallets) {
            GroupKey(network: $0.network, walletMode: $0.walletMode)
        }

        ForEach(grouped.keys.sorted(), id: \.self) { key in
            Section(
                header: HStack {
                    Text(key.title)
                    Text("NOT BACKED UP")
                        .font(.caption2)
                        .fontWeight(.semibold)
                        .foregroundStyle(.white)
                        .padding(.horizontal, 6)
                        .padding(.vertical, 2)
                        .background(.red, in: Capsule())
                }
            ) {
                ForEach(grouped[key]!, id: \.name) { item in
                    WalletItemRow(item: item)
                }
            }
        }
    }
}

// MARK: - Wallet Item Row

private struct WalletItemRow: View {
    let item: CloudBackupWalletItem

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                Text(item.name)
                    .fontWeight(.medium)
                Spacer()
                StatusBadge(status: item.status)
            }

            HStack(spacing: 12) {
                IconLabel("globe", item.network.displayName())
                IconLabel("wallet.bifold", item.walletType.displayName())
                if let fingerprint = item.fingerprint {
                    IconLabel("touchid", fingerprint)
                }
            }
            .font(.caption)
            .foregroundStyle(.secondary)
        }
        .padding(.vertical, 2)
    }
}

// MARK: - Status Badge

private struct StatusBadge: View {
    let status: CloudBackupWalletStatus

    private var label: String {
        switch status {
        case .backedUp: "Backed up"
        case .notBackedUp: "Not backed up"
        case .deletedFromDevice: "Not on device"
        }
    }

    private var color: Color {
        switch status {
        case .backedUp: .green
        case .notBackedUp: .red
        case .deletedFromDevice: .orange
        }
    }

    var body: some View {
        Text(label)
            .font(.caption)
            .fontWeight(.medium)
            .foregroundColor(color)
            .padding(.horizontal, 8)
            .padding(.vertical, 3)
            .background(color.opacity(0.15), in: Capsule())
    }
}

// MARK: - Group Key

private struct GroupKey: Hashable, Comparable {
    let network: Network
    let walletMode: WalletMode

    var title: String {
        switch walletMode {
        case .decoy: "\(network.displayName()) · Decoy"
        default: network.displayName()
        }
    }

    static func < (lhs: GroupKey, rhs: GroupKey) -> Bool {
        if lhs.network != rhs.network {
            return lhs.network.displayName() < rhs.network.displayName()
        }
        return lhs.walletMode == .main && rhs.walletMode != .main
    }
}

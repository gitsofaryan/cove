import SwiftUI

struct CloudBackupDetailScreen: View {
    @Environment(AppManager.self) private var app
    @State private var manager = CloudBackupManager.shared
    @State private var syncHealth: ICloudDriveHelper.SyncHealth = .noFiles
    @State private var showRecreateConfirmation = false
    @State private var showReinitializeConfirmation = false
    @State private var hasAutoVerified = false

    private var isVerifying: Bool {
        if case .verifying = manager.verification { return true }
        return false
    }

    private var hasVerificationResult: Bool {
        switch manager.verification {
        case .verified, .passkeyConfirmed, .failed, .cancelled: true
        default: false
        }
    }

    private var isCancelled: Bool {
        if case .cancelled = manager.verification { return true }
        return false
    }

    private var isRecovering: Bool {
        if case .recovering = manager.recovery { return true }
        return false
    }

    private var isPasskeyMissing: Bool {
        if case .passkeyMissing = manager.status { return true }
        return false
    }

    private var isUnsupportedPasskeyProvider: Bool {
        if case .unsupportedPasskeyProvider = manager.status { return true }
        return false
    }

    private var shouldShowLoadingState: Bool {
        manager.detail == nil && !isVerifying && !hasVerificationResult && !isCancelled
    }

    private func syncCloudBackupRootPromptBlockers() {
        app.setCloudBackupRootPromptBlocker(
            .cloudBackupDetailBusy,
            isActive: isVerifying || isRecovering
        )
        app.setCloudBackupRootPromptBlocker(
            .cloudBackupDetailDialog,
            isActive: showRecreateConfirmation ||
                showReinitializeConfirmation ||
                manager.showPasskeyChoiceDialog
        )
    }

    private func clearCloudBackupRootPromptBlockers() {
        app.clearCloudBackupRootPromptBlockers([
            .cloudBackupDetailBusy,
            .cloudBackupDetailDialog,
        ])
    }

    var body: some View {
        Form {
            formContent
        }
        .navigationTitle("Cloud Backup")
        .navigationBarTitleDisplayMode(.inline)
        .task {
            guard !isPasskeyMissing, !isUnsupportedPasskeyProvider else { return }

            refreshSyncHealth()
            manager.dispatch(action: .refreshDetail)

            if !hasAutoVerified {
                hasAutoVerified = true
                manager.dispatch(action: .startVerificationDiscoverable)
            }
        }
        .onAppear {
            syncCloudBackupRootPromptBlockers()
        }
        .onDisappear {
            clearCloudBackupRootPromptBlockers()
        }
        .onChange(of: manager.detail) { _, _ in
            refreshSyncHealth()
        }
        .onChange(of: manager.verification) { _, _ in
            refreshSyncHealth()
            syncCloudBackupRootPromptBlockers()
        }
        .onChange(of: manager.recovery) { _, _ in
            syncCloudBackupRootPromptBlockers()
        }
        .onChange(of: manager.showPasskeyChoiceDialog) { _, _ in
            syncCloudBackupRootPromptBlockers()
        }
        .onChange(of: showRecreateConfirmation) { _, _ in
            syncCloudBackupRootPromptBlockers()
        }
        .onChange(of: showReinitializeConfirmation) { _, _ in
            syncCloudBackupRootPromptBlockers()
        }
        .confirmationDialog(
            "Recreate Backup Index",
            isPresented: $showRecreateConfirmation,
            titleVisibility: .visible
        ) {
            Button("Recreate", role: .destructive) {
                manager.dispatch(action: .recreateManifest)
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
                manager.dispatch(action: .reinitializeBackup)
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text(
                "This will replace your entire cloud backup. Wallets that only exist in the current cloud backup will be lost."
            )
        }
        .alert(
            "Passkey Options",
            isPresented: $manager.showPasskeyChoiceDialog
        ) {
            Button("Use Existing Passkey") {
                manager.dispatch(action: .repairPasskey)
            }
            Button("Create New Passkey") {
                manager.dispatch(action: .repairPasskeyNoDiscovery)
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("Would you like to use an existing passkey or create a new one?")
        }
    }

    private func refreshSyncHealth() {
        syncHealth = ICloudDriveHelper.shared.overallSyncHealth()
    }

    @ViewBuilder
    private var formContent: some View {
        if isUnsupportedPasskeyProvider {
            UnsupportedPasskeyProviderContent(manager: manager)
        } else if isPasskeyMissing {
            MissingPasskeyContent(manager: manager)
        } else {
            backupStatusContent
            VerificationSection(
                manager: manager,
                onRecreate: { showRecreateConfirmation = true },
                onReinitialize: { showReinitializeConfirmation = true }
            )
        }
    }

    @ViewBuilder
    private var backupStatusContent: some View {
        if isVerifying, !hasVerificationResult {
            Section {
                VStack {
                    ProgressView("Verifying cloud backup...")
                }
                .frame(maxWidth: .infinity)
                .padding(.vertical, 8)
            }
        } else if let detail = manager.detail, !isCancelled {
            DetailFormContent(
                detail: detail,
                syncHealth: syncHealth,
                manager: manager
            )
        } else if shouldShowLoadingState {
            Section {
                VStack(spacing: 12) {
                    ProgressView("Loading cloud backup...")

                    Text("Finishing setup and fetching backup details")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .multilineTextAlignment(.center)
                }
                .frame(maxWidth: .infinity)
                .padding(.vertical, 8)
            }
        }
    }
}

struct UnsupportedPasskeyProviderContent: View {
    @Environment(\.dismiss) private var dismiss
    let manager: CloudBackupManager

    var body: some View {
        Section {
            VStack(spacing: 12) {
                Image(systemName: "exclamationmark.shield.fill")
                    .font(.system(size: 36))
                    .foregroundStyle(.red)

                Text("Passkey Not Supported for Cloud Backup")
                    .font(.headline)
                    .foregroundStyle(.red)

                Text(
                    "This passkey provider can't create the secure passkey required for Cloud Backup. No cloud backup was enabled from this attempt."
                )
                .font(.subheadline)
                .foregroundStyle(.red.opacity(0.85))
                .multilineTextAlignment(.center)

                Text(
                    "Try again with a supported password manager on iOS such as Apple Passwords, 1Password, or Bitwarden."
                )
                .font(.caption)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
            }
            .frame(maxWidth: .infinity)
            .padding(.vertical, 12)
        }

        Section {
            Button {
                manager.dispatch(action: .enableCloudBackupNoDiscovery)
            } label: {
                Label("Try Again", systemImage: "arrow.clockwise")
            }

            Button("Back", role: .cancel) {
                dismiss()
            }
        }
    }
}

import SwiftUI

@_exported import CoveCore

struct DeviceRestoreView: View {
    let onComplete: () -> Void
    let onError: (String) -> Void

    enum RestorePhase: Equatable {
        case restoring
        case complete(CloudBackupRestoreReport)
        case error(String)

        static func == (lhs: RestorePhase, rhs: RestorePhase) -> Bool {
            switch (lhs, rhs) {
            case (.restoring, .restoring): true
            case (.complete, .complete): true
            case let (.error(a), .error(b)): a == b
            default: false
            }
        }
    }

    @State private var phase: RestorePhase = .restoring
    @State private var backupManager = CloudBackupManager.shared
    @State private var hasStartedRestore = false
    @State private var hasCompletedFlow = false
    @State private var timeoutTask: Task<Void, Never>?

    private let restoreTimeout: Duration = .seconds(120)

    private var restoreProgress: CloudBackupRestoreProgress? {
        backupManager.restoreProgress
    }

    private var restoringSubtitle: String {
        guard let restoreProgress else {
            return "Preparing restore..."
        }

        switch restoreProgress.stage {
        case .finding:
            return "Finding wallets in your iCloud backup..."
        case .downloading:
            guard let total = restoreProgress.total else {
                return "Downloading wallets..."
            }
            return "Downloading wallets (\(restoreProgress.completed)/\(total))"
        case .restoring:
            guard let total = restoreProgress.total else {
                return "Restoring wallets..."
            }
            return "Restoring wallets (\(restoreProgress.completed)/\(total))"
        }
    }

    var body: some View {
        VStack(spacing: 24) {
            Spacer()

            heroIcon

            Spacer()

            HStack {
                DotMenuView(selected: 2, size: 5, total: 3)
                Spacer()
            }

            titleContent

            Divider().overlay(Color.coveLightGray.opacity(0.50))

            bottomContent
        }
        .padding()
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(
            Image(.newWalletPattern)
                .resizable()
                .aspectRatio(contentMode: .fill)
                .frame(height: screenHeight * 0.75, alignment: .topTrailing)
                .frame(maxWidth: .infinity)
                .opacity(0.75)
        )
        .background(Color.midnightBlue)
        .task {
            guard !hasStartedRestore else { return }
            startRestore()
        }
        .onDisappear {
            timeoutTask?.cancel()
        }
        .onChange(of: backupManager.status) { _, _ in
            syncPhaseWithManager()
        }
        .onChange(of: backupManager.restoreReport) { _, _ in
            syncPhaseWithManager()
        }
    }

    @ViewBuilder
    private var heroIcon: some View {
        switch phase {
        case .restoring:
            ZStack {
                Circle()
                    .fill(Color.duskBlue.opacity(0.5))
                    .frame(width: 100, height: 100)

                Circle()
                    .stroke(
                        LinearGradient(
                            colors: [.btnGradientLight, .btnGradientDark],
                            startPoint: .topLeading,
                            endPoint: .bottomTrailing
                        ),
                        lineWidth: 2
                    )
                    .frame(width: 100, height: 100)

                Image(systemName: restoreIconName)
                    .font(.system(size: 40))
                    .foregroundStyle(Color.btnGradientLight)
                    .symbolEffect(.pulse)
            }

        case .complete:
            Image(systemName: "checkmark.circle.fill")
                .font(.system(size: screenWidth * 0.30))
                .fontWeight(.light)
                .symbolRenderingMode(.palette)
                .foregroundStyle(.midnightBlue, Color.lightGreen)

        case .error:
            ZStack {
                Circle()
                    .fill(Color.red.opacity(0.1))
                    .frame(width: 100, height: 100)

                Image(systemName: "exclamationmark.triangle.fill")
                    .font(.system(size: 40))
                    .foregroundStyle(.red)
            }
        }
    }

    private var restoreIconName: String {
        guard let restoreProgress else { return "icloud.and.arrow.down" }

        switch restoreProgress.stage {
        case .finding:
            return "magnifyingglass"
        case .downloading:
            return "arrow.down.circle"
        case .restoring:
            return "externaldrive.badge.checkmark"
        }
    }

    @ViewBuilder
    private var titleContent: some View {
        switch phase {
        case .restoring:
            VStack(spacing: 12) {
                HStack {
                    Text("Restoring from Cloud")
                        .font(.system(size: 38, weight: .semibold))
                        .foregroundStyle(.white)
                    Spacer()
                }

                HStack {
                    Text(restoringSubtitle)
                        .font(.footnote)
                        .foregroundStyle(.coveLightGray.opacity(0.75))
                        .fixedSize(horizontal: false, vertical: true)
                    Spacer()
                }
            }

        case let .complete(report):
            VStack(spacing: 12) {
                HStack {
                    Text("Restore Complete")
                        .font(.system(size: 38, weight: .semibold))
                        .foregroundStyle(.white)
                    Spacer()
                }

                HStack {
                    VStack(alignment: .leading, spacing: 4) {
                        Text("Restored \(report.walletsRestored) wallet(s)")
                            .font(.footnote)
                            .foregroundStyle(.coveLightGray.opacity(0.75))

                        if report.walletsFailed > 0 {
                            Text("\(report.walletsFailed) wallet(s) could not be restored")
                                .font(.caption)
                                .foregroundStyle(.orange)
                        }
                    }
                    Spacer()
                }
            }

        case .error:
            VStack(spacing: 12) {
                HStack {
                    Text("Restore Failed")
                        .font(.system(size: 38, weight: .semibold))
                        .foregroundStyle(.white)
                    Spacer()
                }

                HStack {
                    Text("Something went wrong while restoring your wallets")
                        .font(.footnote)
                        .foregroundStyle(.coveLightGray.opacity(0.75))
                        .fixedSize(horizontal: false, vertical: true)
                    Spacer()
                }
            }
        }
    }

    @ViewBuilder
    private var bottomContent: some View {
        switch phase {
        case .restoring:
            if let restoreProgress, let total = restoreProgress.total {
                ProgressView(
                    value: Double(restoreProgress.completed),
                    total: Double(max(total, 1))
                )
                .tint(.btnGradientLight)
                .animation(.easeInOut(duration: 0.3), value: restoreProgress.completed)
            } else {
                ProgressView()
                    .tint(.white)
            }

        case .complete:
            EmptyView()

        case let .error(message):
            VStack(spacing: 16) {
                HStack(spacing: 8) {
                    Image(systemName: "exclamationmark.triangle.fill")
                        .foregroundStyle(.orange)

                    Text(message)
                        .font(.caption)
                        .foregroundStyle(.orange.opacity(0.9))
                        .fixedSize(horizontal: false, vertical: true)
                }
                .padding(12)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(
                    RoundedRectangle(cornerRadius: 10, style: .continuous)
                        .fill(Color.orange.opacity(0.1))
                )
                .overlay(
                    RoundedRectangle(cornerRadius: 10, style: .continuous)
                        .stroke(Color.orange.opacity(0.3), lineWidth: 1)
                )

                Button {
                    startRestore()
                } label: {
                    Text("Retry")
                }
                .buttonStyle(PrimaryButtonStyle())
            }
        }
    }

    private func startRestore() {
        timeoutTask?.cancel()
        phase = .restoring
        hasStartedRestore = true
        hasCompletedFlow = false
        backupManager.restoreFromCloudBackup()

        timeoutTask = Task {
            try? await Task.sleep(for: restoreTimeout)
            guard !Task.isCancelled else { return }

            await MainActor.run {
                guard case .restoring = phase else { return }
                phase = .error("Restore timed out. Please try again.")
            }
        }
    }

    private func syncPhaseWithManager() {
        switch backupManager.status {
        case let .error(message):
            timeoutTask?.cancel()
            if case .restoring = phase {
                phase = .error(message)
                onError(message)
            }

        case .enabled:
            guard let report = backupManager.restoreReport, !hasCompletedFlow else { return }
            timeoutTask?.cancel()
            hasCompletedFlow = true
            phase = .complete(report)

            Task {
                try? await Task.sleep(for: .seconds(1))
                await MainActor.run {
                    onComplete()
                }
            }

        default:
            break
        }
    }
}

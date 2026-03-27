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
        VStack(spacing: 0) {
            OnboardingStepIndicator(selected: 2)
                .padding(.top, 8)

            Spacer()
                .frame(height: 42)

            heroIcon

            Spacer()
                .frame(height: 44)

            titleContent

            Spacer(minLength: 30)

            bottomContent
        }
        .padding(.horizontal, 28)
        .padding(.top, 12)
        .padding(.bottom, 26)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        .onboardingRecoveryBackground()
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
                    .stroke(Color.btnGradientLight.opacity(0.12), lineWidth: 1)
                    .frame(width: 118, height: 118)

                Circle()
                    .stroke(Color.btnGradientLight.opacity(0.18), lineWidth: 1)
                    .frame(width: 86, height: 86)

                Circle()
                    .stroke(Color.btnGradientLight.opacity(0.24), lineWidth: 1)
                    .frame(width: 58, height: 58)

                Circle()
                    .fill(Color.duskBlue.opacity(0.42))
                    .frame(width: 58, height: 58)

                Circle()
                    .stroke(
                        LinearGradient(
                            colors: [.btnGradientLight, .btnGradientDark],
                            startPoint: .topLeading,
                            endPoint: .bottomTrailing
                        ),
                        lineWidth: 1.5
                    )
                    .frame(width: 58, height: 58)

                Image(systemName: restoreIconName)
                    .font(.system(size: 24, weight: .semibold))
                    .foregroundStyle(Color.btnGradientLight)
                    .symbolEffect(.pulse)
            }

        case .complete:
            ZStack {
                Circle()
                    .fill(Color.lightGreen.opacity(0.16))
                    .frame(width: 118, height: 118)

                Circle()
                    .stroke(Color.lightGreen.opacity(0.26), lineWidth: 1)
                    .frame(width: 118, height: 118)

                Image(systemName: "checkmark.circle.fill")
                    .font(.system(size: 68, weight: .light))
                    .symbolRenderingMode(.palette)
                    .foregroundStyle(.midnightBlue, Color.lightGreen)
            }

        case .error:
            ZStack {
                Circle()
                    .fill(Color.red.opacity(0.12))
                    .frame(width: 118, height: 118)

                Circle()
                    .stroke(Color.red.opacity(0.2), lineWidth: 1)
                    .frame(width: 118, height: 118)

                Image(systemName: "exclamationmark.triangle.fill")
                    .font(.system(size: 40, weight: .semibold))
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
                Text("Restoring from Cloud")
                    .font(.system(size: 40, weight: .bold, design: .rounded))
                    .foregroundStyle(.white)
                    .multilineTextAlignment(.center)

                Text(restoringSubtitle)
                    .font(.system(size: 20, weight: .medium, design: .rounded))
                    .foregroundStyle(.coveLightGray.opacity(0.76))
                    .multilineTextAlignment(.center)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .padding(.horizontal, 8)

        case let .complete(report):
            VStack(spacing: 12) {
                Text("Restore Complete")
                    .font(.system(size: 40, weight: .bold, design: .rounded))
                    .foregroundStyle(.white)
                    .multilineTextAlignment(.center)

                Text("Restored \(report.walletsRestored) wallet(s)")
                    .font(.system(size: 20, weight: .medium, design: .rounded))
                    .foregroundStyle(.coveLightGray.opacity(0.76))
                    .multilineTextAlignment(.center)

                if report.walletsFailed > 0 {
                    Text("\(report.walletsFailed) wallet(s) could not be restored")
                        .font(.system(size: 15, weight: .semibold, design: .rounded))
                        .foregroundStyle(.orange.opacity(0.95))
                        .multilineTextAlignment(.center)
                }
            }
            .padding(.horizontal, 8)

        case .error:
            VStack(spacing: 12) {
                Text("Restore Failed")
                    .font(.system(size: 40, weight: .bold, design: .rounded))
                    .foregroundStyle(.white)
                    .multilineTextAlignment(.center)

                Text("Something went wrong while restoring your wallets")
                    .font(.system(size: 20, weight: .medium, design: .rounded))
                    .foregroundStyle(.coveLightGray.opacity(0.76))
                    .multilineTextAlignment(.center)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .padding(.horizontal, 8)
        }
    }

    @ViewBuilder
    private var bottomContent: some View {
        switch phase {
        case .restoring:
            VStack(spacing: 14) {
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

                Text("Your encrypted backup is being downloaded and restored on this device.")
                    .font(.system(size: 15, weight: .medium, design: .rounded))
                    .foregroundStyle(.coveLightGray.opacity(0.58))
                    .multilineTextAlignment(.center)
                    .fixedSize(horizontal: false, vertical: true)
            }
            .frame(maxWidth: .infinity)

        case .complete:
            EmptyView()

        case let .error(message):
            VStack(spacing: 18) {
                HStack(spacing: 8) {
                    Image(systemName: "exclamationmark.triangle.fill")
                        .foregroundStyle(.orange)

                    Text(message)
                        .font(.system(size: 14, weight: .medium, design: .rounded))
                        .foregroundStyle(.orange.opacity(0.9))
                        .fixedSize(horizontal: false, vertical: true)
                }
                .padding(.horizontal, 14)
                .padding(.vertical, 14)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(
                    RoundedRectangle(cornerRadius: 18, style: .continuous)
                        .fill(Color.orange.opacity(0.1))
                )
                .overlay(
                    RoundedRectangle(cornerRadius: 18, style: .continuous)
                        .stroke(Color.orange.opacity(0.3), lineWidth: 1)
                )

                Button {
                    startRestore()
                } label: {
                    Text("Retry")
                }
                .buttonStyle(OnboardingPrimaryButtonStyle())
            }
        }
    }

    private func startRestore() {
        timeoutTask?.cancel()
        phase = .restoring
        hasStartedRestore = true
        hasCompletedFlow = false
        backupManager.dispatch(action: .restoreFromCloudBackup)

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

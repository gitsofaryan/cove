import SwiftUI

struct CloudBackupDetailScreen: View {
    @State var detail: CloudBackupDetail
    @State private var isSyncing = false

    var body: some View {
        Form {
            HeaderSection
            if !detail.backedUp.isEmpty { BackedUpSections }
            if !detail.notBackedUp.isEmpty {
                NotBackedUpSections
                SyncSection
            }
            if !detail.deletedFromDevice.isEmpty { DeletedFromDeviceSections }
        }
        .navigationTitle("Cloud Backup")
        .navigationBarTitleDisplayMode(.inline)
        .task {
            if let refreshed = CloudBackupManager.shared.rust.refreshCloudBackupDetail() {
                detail = refreshed
            }
        }
        .onChange(of: CloudBackupManager.shared.state) { _, newState in
            guard isSyncing, newState != .enabling else { return }

            if newState == .enabled,
               let refreshed = CloudBackupManager.shared.rust.refreshCloudBackupDetail()
            {
                detail = refreshed
            }
            isSyncing = false
        }
    }

    // MARK: Header

    private var HeaderSection: some View {
        Section {
            VStack(spacing: 8) {
                Image(systemName: "checkmark.icloud.fill")
                    .foregroundColor(.green)
                    .font(.largeTitle)

                Text("Cloud Backup Active")
                    .fontWeight(.semibold)

                if let lastSync = detail.lastSync {
                    Text("Last synced \(formatDate(lastSync))")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
            .frame(maxWidth: .infinity)
            .padding(.vertical, 8)
        }
    }

    // MARK: Sync

    private var SyncSection: some View {
        Section {
            Button {
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
        }
    }

    // MARK: Backed Up

    @ViewBuilder
    private var BackedUpSections: some View {
        let grouped = Dictionary(grouping: detail.backedUp) {
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

    // MARK: Not Backed Up

    @ViewBuilder
    private var NotBackedUpSections: some View {
        let grouped = Dictionary(grouping: detail.notBackedUp) {
            GroupKey(network: $0.network, walletMode: $0.walletMode)
        }

        ForEach(grouped.keys.sorted(), id: \.self) { key in
            Section(header: HStack {
                Text(key.title)
                Text("NOT BACKED UP")
                    .font(.caption2)
                    .fontWeight(.semibold)
                    .foregroundStyle(.white)
                    .padding(.horizontal, 6)
                    .padding(.vertical, 2)
                    .background(.red, in: Capsule())
            }) {
                ForEach(grouped[key]!, id: \.name) { item in
                    WalletItemRow(item: item)
                }
            }
        }
    }

    // MARK: Deleted From Device

    @ViewBuilder
    private var DeletedFromDeviceSections: some View {
        let grouped = Dictionary(grouping: detail.deletedFromDevice) {
            GroupKey(network: $0.network, walletMode: $0.walletMode)
        }

        ForEach(grouped.keys.sorted(), id: \.self) { key in
            Section(header: Text("\(key.title) · Deleted")) {
                ForEach(grouped[key]!, id: \.name) { item in
                    WalletItemRow(item: item)
                }
            }
        }
    }

    private func formatDate(_ timestamp: UInt64) -> String {
        let date = Date(timeIntervalSince1970: TimeInterval(timestamp))
        return date.formatted(date: .abbreviated, time: .shortened)
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
        case .deletedFromDevice: "Deleted"
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

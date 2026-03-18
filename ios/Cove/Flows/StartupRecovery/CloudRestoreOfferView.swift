import SwiftUI

@_exported import CoveCore

/// Shown on fresh install when a cloud backup is detected
/// TODO: replace with proper onboarding design
struct CloudRestoreOfferView: View {
    let onRestore: () -> Void
    let onSkip: () -> Void

    var body: some View {
        VStack(spacing: 24) {
            Spacer()

            Image(systemName: "icloud.and.arrow.down")
                .font(.system(size: 64))
                .foregroundStyle(.blue)

            Text("Cloud Backup Found")
                .font(.title)
                .fontWeight(.bold)

            Text("A cloud backup from a previous installation was found. Would you like to restore your wallets?")
                .multilineTextAlignment(.center)
                .foregroundStyle(.secondary)
                .padding(.horizontal, 32)

            Spacer()

            VStack(spacing: 16) {
                Button {
                    onRestore()
                } label: {
                    HStack {
                        Image(systemName: "arrow.down.circle")
                        Text("Restore from Cloud Backup")
                    }
                    .frame(maxWidth: .infinity)
                }
                .buttonStyle(.borderedProminent)

                Button {
                    onSkip()
                } label: {
                    Text("Skip — Start Fresh")
                        .frame(maxWidth: .infinity)
                }
                .buttonStyle(.bordered)
            }

            Spacer()
        }
        .padding()
    }
}

import SwiftUI

@_exported import CoveCore

/// Shown after the cloud backup check finds at least one backup
struct CloudRestoreOfferView: View {
    let onRestore: () -> Void
    let onSkip: () -> Void
    var errorMessage: String? = nil

    var body: some View {
        VStack(spacing: 24) {
            Spacer()

            // decorative icon
            ZStack {
                Circle()
                    .fill(Color.duskBlue.opacity(0.4))
                    .frame(width: 100, height: 100)
                    .shadow(color: Color(red: 0.165, green: 0.353, blue: 0.545).opacity(0.5), radius: 30)

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

                Image(systemName: "magnifyingglass")
                    .font(.system(size: 36, weight: .medium))
                    .foregroundStyle(.white)
            }

            Spacer()

            HStack {
                DotMenuView(selected: 1, size: 5, total: 3)
                Spacer()
            }

            // title + subtitle
            VStack(spacing: 12) {
                HStack {
                    Text("iCloud Backup Found")
                        .font(.system(size: 38, weight: .semibold))
                        .foregroundStyle(.white)
                    Spacer()
                }

                HStack {
                    Text("A previous iCloud backup was found. Restore your wallet securely using your passkey.")
                        .font(.footnote)
                        .foregroundStyle(.coveLightGray.opacity(0.75))
                        .fixedSize(horizontal: false, vertical: true)
                    Spacer()
                }
            }

            Divider().overlay(Color.coveLightGray.opacity(0.50))

            // passkey option card
            VStack(alignment: .leading, spacing: 12) {
                Text("Recommended")
                    .font(.caption2)
                    .fontWeight(.semibold)
                    .foregroundStyle(.white)
                    .padding(.horizontal, 8)
                    .padding(.vertical, 4)
                    .background(
                        LinearGradient(
                            colors: [.btnGradientLight, .btnGradientDark],
                            startPoint: .leading,
                            endPoint: .trailing
                        ),
                        in: Capsule()
                    )

                HStack(spacing: 12) {
                    Image(systemName: "person.badge.key")
                        .font(.title3)
                        .foregroundStyle(Color.btnGradientLight)
                        .frame(width: 40, height: 40)
                        .background(Color.btnGradientLight.opacity(0.15))
                        .clipShape(RoundedRectangle(cornerRadius: 8, style: .continuous))

                    VStack(alignment: .leading, spacing: 4) {
                        Text("Passkey Restore")
                            .font(.subheadline)
                            .fontWeight(.semibold)
                            .foregroundStyle(.white)

                        Text("Secured with iCloud Keychain")
                            .font(.caption)
                            .foregroundStyle(.coveLightGray.opacity(0.75))
                    }

                    Spacer()
                }

                Text("Your passkey is stored securely in iCloud Keychain and syncs across all your Apple devices.")
                    .font(.caption)
                    .foregroundStyle(.coveLightGray.opacity(0.60))
                    .fixedSize(horizontal: false, vertical: true)
            }
            .padding(16)
            .background(
                RoundedRectangle(cornerRadius: 10, style: .continuous)
                    .fill(Color.duskBlue.opacity(0.5))
            )
            .overlay(
                RoundedRectangle(cornerRadius: 10, style: .continuous)
                    .stroke(Color.coveLightGray.opacity(0.15), lineWidth: 1)
            )

            // error message
            if let errorMessage {
                HStack(spacing: 8) {
                    Image(systemName: "exclamationmark.triangle.fill")
                        .foregroundStyle(.orange)

                    Text(errorMessage)
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
                .transition(.opacity.combined(with: .move(edge: .top)))
            }

            Button {
                onRestore()
            } label: {
                Text("Restore with Passkey")
                    .font(.footnote)
                    .fontWeight(.medium)
                    .frame(maxWidth: .infinity)
                    .padding(.vertical, 20)
                    .padding(.horizontal, 10)
                    .background(Color.blue)
                    .foregroundStyle(.white)
                    .clipShape(RoundedRectangle(cornerRadius: 10, style: .continuous))
            }

            Button {
                onSkip()
            } label: {
                Text("Set Up as New")
                    .font(.subheadline)
                    .foregroundStyle(Color.btnGradientLight)
            }
        }
        .padding()
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background {
            ZStack {
                Color.midnightBlue

                // large upper-center radial glow
                RadialGradient(
                    stops: [
                        .init(color: Color(red: 0.165, green: 0.353, blue: 0.545).opacity(0.9), location: 0),
                        .init(color: Color(red: 0.118, green: 0.227, blue: 0.361).opacity(0.4), location: 0.45),
                        .init(color: .clear, location: 0.85),
                    ],
                    center: .init(x: 0.35, y: 0.18),
                    startRadius: 0,
                    endRadius: 400
                )

                // smaller right-offset radial glow
                RadialGradient(
                    stops: [
                        .init(color: Color(red: 0.118, green: 0.290, blue: 0.420).opacity(0.8), location: 0),
                        .init(color: .clear, location: 0.75),
                    ],
                    center: .init(x: 0.75, y: 0.12),
                    startRadius: 0,
                    endRadius: 300
                )
            }
            .ignoresSafeArea()
        }
        .animation(.easeInOut(duration: 0.3), value: errorMessage)
    }
}

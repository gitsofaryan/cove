import SwiftUI

struct OnboardingStepIndicator: View {
    let selected: Int
    var total: Int = 3

    var body: some View {
        HStack(spacing: 9) {
            ForEach(0 ..< total, id: \.self) { index in
                if index == selected {
                    Capsule()
                        .fill(.white)
                        .frame(width: 24, height: 6)
                } else {
                    Circle()
                        .fill(.white.opacity(0.22))
                        .frame(width: 6, height: 6)
                }
            }
        }
        .frame(maxWidth: .infinity)
    }
}

struct OnboardingPrimaryButtonStyle: ButtonStyle {
    @Environment(\.isEnabled) private var isEnabled

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.system(size: 18, weight: .semibold, design: .rounded))
            .frame(maxWidth: .infinity)
            .padding(.vertical, 18)
            .padding(.horizontal, 18)
            .foregroundStyle(.white.opacity(isEnabled ? 1 : 0.45))
            .background(
                LinearGradient(
                    colors: [.btnGradientLight, .btnGradientDark],
                    startPoint: .leading,
                    endPoint: .trailing
                ),
                in: RoundedRectangle(cornerRadius: 16, style: .continuous)
            )
            .opacity(isEnabled ? (configuration.isPressed ? 0.84 : 1) : 0.45)
    }
}

struct OnboardingSecondaryButtonStyle: ButtonStyle {
    var backgroundColor: Color = .duskBlue.opacity(0.58)
    var foregroundColor: Color = .white
    var borderColor: Color = .coveLightGray.opacity(0.12)

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.system(size: 16, weight: .semibold, design: .rounded))
            .frame(maxWidth: .infinity)
            .padding(.vertical, 17)
            .padding(.horizontal, 18)
            .foregroundStyle(foregroundColor)
            .background(
                RoundedRectangle(cornerRadius: 16, style: .continuous)
                    .fill(backgroundColor)
            )
            .overlay(
                RoundedRectangle(cornerRadius: 16, style: .continuous)
                    .stroke(borderColor, lineWidth: 1)
            )
            .opacity(configuration.isPressed ? 0.84 : 1)
    }
}

private struct OnboardingRecoveryBackgroundModifier: ViewModifier {
    func body(content: Content) -> some View {
        content.background {
            ZStack {
                Color.midnightBlue

                RadialGradient(
                    stops: [
                        .init(color: Color(red: 0.165, green: 0.353, blue: 0.545).opacity(0.92), location: 0),
                        .init(color: Color(red: 0.118, green: 0.227, blue: 0.361).opacity(0.45), location: 0.4),
                        .init(color: .clear, location: 0.84),
                    ],
                    center: .init(x: 0.33, y: 0.16),
                    startRadius: 0,
                    endRadius: 420
                )

                RadialGradient(
                    stops: [
                        .init(color: Color(red: 0.118, green: 0.290, blue: 0.420).opacity(0.82), location: 0),
                        .init(color: .clear, location: 0.74),
                    ],
                    center: .init(x: 0.78, y: 0.1),
                    startRadius: 0,
                    endRadius: 320
                )
            }
            .ignoresSafeArea()
        }
    }
}

extension View {
    func onboardingRecoveryBackground() -> some View {
        modifier(OnboardingRecoveryBackgroundModifier())
    }
}

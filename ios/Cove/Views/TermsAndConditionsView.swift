//
//  TermsAndConditionsView.swift
//  Cove
//
//  Created by Praveen Perera on 6/3/25.
//

import SwiftUI

struct TermsAndConditionsView: View {
    let onAgree: () -> Void

    @State private var checks: [Bool] = Array(repeating: false, count: 5)

    private var allChecked: Bool {
        checks.allSatisfy(\.self)
    }

    var body: some View {
        ViewThatFits(in: .vertical) {
            content(cardSpacing: 10, cardPadding: 18, footerTopSpacing: 16)
            content(cardSpacing: 8, cardPadding: 14, footerTopSpacing: 12)
        }
        .padding(.horizontal, 26)
        .padding(.top, 22)
        .padding(.bottom, 24)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        .onboardingRecoveryBackground()
    }

    private func content(cardSpacing: CGFloat, cardPadding: CGFloat, footerTopSpacing: CGFloat) -> some View {
        VStack(spacing: 0) {
            VStack(alignment: .trailing, spacing: 12) {
                Text("Terms & Conditions")
                    .font(.system(size: 28, weight: .bold, design: .rounded))
                    .foregroundStyle(.white)
                    .multilineTextAlignment(.trailing)
                    .frame(maxWidth: .infinity, alignment: .trailing)

                Text("By continuing, you agree to the following:")
                    .font(.system(size: 18, weight: .medium, design: .rounded))
                    .foregroundStyle(.coveLightGray.opacity(0.74))
                    .multilineTextAlignment(.trailing)
                    .frame(maxWidth: .infinity, alignment: .trailing)
            }

            Spacer()
                .frame(height: 20)

            VStack(spacing: cardSpacing) {
                TermsCheckboxCard(isOn: $checks[0], cardPadding: cardPadding) {
                    Text("I understand that I am responsible for securely managing and backing up my wallets. Cove does not store or recover wallet information.")
                }

                TermsCheckboxCard(isOn: $checks[1], cardPadding: cardPadding) {
                    Text("I understand that any unlawful use of Cove is strictly prohibited.")
                }

                TermsCheckboxCard(isOn: $checks[2], cardPadding: cardPadding) {
                    Text("I understand that Cove is not a bank, exchange, or licensed financial institution, and does not offer financial services.")
                }

                TermsCheckboxCard(isOn: $checks[3], cardPadding: cardPadding) {
                    Text("I understand that if I lose access to my wallet, Cove cannot recover my funds or credentials.")
                }

                TermsCheckboxCard(isOn: $checks[4], cardPadding: cardPadding, allowsCardToggle: false) {
                    Text(
                        .init(
                            "I have read and agree to Cove’s **[Privacy Policy](https://covebitcoinwallet.com/privacy)** and **[Terms & Conditions](https://covebitcoinwallet.com/terms)** as a condition of use."
                        )
                    )
                }
            }

            Spacer()
                .frame(height: footerTopSpacing)

            Text("By checking these boxes, you accept and agree to the above terms.")
                .font(.system(size: 17, weight: .medium, design: .rounded))
                .foregroundStyle(.coveLightGray.opacity(0.7))
                .multilineTextAlignment(.trailing)
                .frame(maxWidth: .infinity, alignment: .trailing)

            Spacer(minLength: 20)

            Button("Agree and Continue") {
                guard allChecked else { return }
                onAgree()
            }
            .buttonStyle(OnboardingPrimaryButtonStyle())
            .disabled(!allChecked)
        }
    }
}

private struct TermsCheckboxCard<Content: View>: View {
    @Binding var isOn: Bool
    var cardPadding: CGFloat
    var allowsCardToggle = true
    @ViewBuilder let content: () -> Content

    var body: some View {
        HStack(alignment: .top, spacing: 14) {
            Button {
                isOn.toggle()
            } label: {
                Image(systemName: isOn ? "checkmark.circle.fill" : "circle")
                    .font(.system(size: 18, weight: .medium))
                    .foregroundStyle(isOn ? Color.btnGradientLight : Color.btnGradientLight.opacity(0.92))
            }
            .buttonStyle(.plain)
            .padding(.top, 1)

            content()
                .font(.system(size: 15, weight: .medium, design: .rounded))
                .foregroundStyle(.white.opacity(0.82))
                .tint(.btnGradientLight)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(.horizontal, 14)
        .padding(.vertical, cardPadding)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(
            RoundedRectangle(cornerRadius: 16, style: .continuous)
                .fill(Color.duskBlue.opacity(0.48))
        )
        .overlay(
            RoundedRectangle(cornerRadius: 16, style: .continuous)
                .stroke(Color.coveLightGray.opacity(0.14), lineWidth: 1)
        )
        .contentShape(RoundedRectangle(cornerRadius: 16, style: .continuous))
        .onTapGesture {
            guard allowsCardToggle else { return }
            isOn.toggle()
        }
    }
}

#Preview {
    TermsAndConditionsView(onAgree: {})
}

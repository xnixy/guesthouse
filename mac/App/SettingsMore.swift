// SPDX-License-Identifier: GPL-2.0-or-later
import AppKit
import ServiceManagement
import GuesthouseCore
import SwiftUI

/// Opening the app when the person logs in, through the system's own login item.
enum LoginItem {
    static var isOn: Bool { SMAppService.mainApp.status == .enabled }

    /// Turns it on or off; returns why it did not work.
    static func set(_ on: Bool) -> String? {
        do {
            if on { try SMAppService.mainApp.register() } else { try SMAppService.mainApp.unregister() }
            return nil
        } catch {
            return String(localized: "Could not change it: \(error.localizedDescription)")
        }
    }
}

/// The app's version and the address of its releases.
enum AppInfo {
    static var version: String {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "0"
    }

    static var build: String {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleVersion") as? String ?? "0"
    }

    static let website = URL(string: "https://guesthouse.ruxn.com")!
    /// The public project's source.
    static let source = URL(string: "https://github.com/ruxnlabs/guesthouse")!
}

/// Looking for a newer Guesthouse with the shared check, and keeping what it found.
enum UpdateCheck {
    /// A day, which is how often the app looks by itself.
    static let every: TimeInterval = 86_400

    /// Looks once, off the main thread, and keeps the answer and the time. Nothing is downloaded.
    static func run() async -> UpdateState {
        let version = AppInfo.version
        let nightly = UserDefaults.standard.bool(forKey: SettingsKey.nightlyChannel)
        let answer = await Task.detached { () -> String in
            guard let text = guesthouse_update_check(version, nightly) else { return "{}" }
            defer { guesthouse_string_free(text) }
            return String(cString: text)
        }.value
        UserDefaults.standard.set(answer, forKey: SettingsKey.updateFound)
        UserDefaults.standard.set(Date.now.timeIntervalSince1970, forKey: SettingsKey.updateCheckedAt)
        return UpdateState(json: answer)
    }

    /// At launch: looks if the person has not turned it off and it has been a day.
    static func runIfDue() {
        let defaults = UserDefaults.standard
        guard defaults.object(forKey: SettingsKey.autoCheckUpdates) as? Bool ?? true else { return }
        let last = defaults.double(forKey: SettingsKey.updateCheckedAt)
        guard Date.now.timeIntervalSince1970 - last >= every else { return }
        Task { _ = await run() }
    }

    static var kept: (state: UpdateState, at: Date?)? {
        guard let found = UserDefaults.standard.string(forKey: SettingsKey.updateFound) else { return nil }
        let at = UserDefaults.standard.double(forKey: SettingsKey.updateCheckedAt)
        return (UpdateState(json: found), at > 0 ? Date(timeIntervalSince1970: at) : nil)
    }
}

/// Updates: which version this is, whether a newer one is out (the shared check, the same the Linux
/// and Windows apps use), and where to get it. It does not download or install anything. A driver's
/// version in a machine is not reported by the guest yet, so none is shown as if it were known.
struct UpdatesSettings: View {
    @AppStorage(SettingsKey.autoCheckUpdates) private var auto = true
    @AppStorage(SettingsKey.nightlyChannel) private var nightly = false
    @State private var state = UpdateCheck.kept?.state
    @State private var checkedAt = UpdateCheck.kept?.at
    @State private var checking = false

    var body: some View {
        Form {
            Section("Guesthouse") {
                LabeledContent {
                    HStack {
                        if checking { ProgressView().controlSize(.small) }
                        if case .newer(_, let page) = state, let url = URL(string: page) {
                            Link("Open Its Page", destination: url)
                        }
                        Button("Check Now", action: check).disabled(checking)
                    }
                } label: {
                    VStack(alignment: .leading, spacing: 2) {
                        Text((state ?? .cantTell).headline(current: AppInfo.version))
                        Text(state.map { $0.detail(lastChecked: checkedAt) }
                             ?? String(localized: "Not checked yet. Check Now looks once, and nothing is downloaded."))
                            .font(.callout).foregroundStyle(.secondary)
                    }
                }
                Toggle("Check for updates by itself", isOn: $auto)
                Toggle("Look for nightly builds", isOn: $nightly)
                Text("A nightly build is made from the newest source each night. It is for trying what is new, and it may be broken. Nothing is downloaded either way.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
            Section("Graphics drivers in your machines") {
                Text("A machine's driver is installed when it is made. Which version is in a machine, and updating it, need the machine to report its driver, which it does not yet.")
                    .font(.callout)
                    .foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
    }

    private func check() {
        checking = true
        Task {
            state = await UpdateCheck.run()
            checkedAt = .now
            checking = false
        }
    }
}

/// About: the name, the version, the licence and where to find the rest.
struct AboutSettings: View {
    var body: some View {
        VStack(spacing: 14) {
            Image(systemName: "house.fill")
                .font(.system(size: 44))
                .foregroundStyle(.white)
                .frame(width: 84, height: 84)
                .background(.tint, in: RoundedRectangle(cornerRadius: 20))
                .accessibilityHidden(true)
            VStack(spacing: 3) {
                Text("Guesthouse").font(.title2.weight(.semibold))
                Text("Version \(AppInfo.version) (\(AppInfo.build))").foregroundStyle(.secondary)
            }
            Text("Guesthouse is free software under the GPL, version 2 or later. The graphics driver it installs in Windows contains parts under the LGPL and parts under the MIT licence.")
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .frame(maxWidth: 440)
            HStack(spacing: 18) {
                Link("guesthouse.ruxn.com", destination: AppInfo.website)
                Link("Source Code", destination: AppInfo.source)
            }
            Text("ruxn").font(.caption).foregroundStyle(.secondary)
        }
        .padding(30)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

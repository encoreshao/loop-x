import Foundation
import UserNotifications
import LoopXKit

/// Posts task start/complete/fail changes to the macOS Notification Center.
/// A bare `swift run` binary has no bundle, and UserNotifications traps without one, so it stays silent there.
@MainActor
final class TaskNotifier: NSObject, UNUserNotificationCenterDelegate {
    var onActivate: () -> Void = {}
    private let center: UNUserNotificationCenter?

    override init() {
        center = Bundle.main.bundleIdentifier == nil ? nil : UNUserNotificationCenter.current()
        super.init()
        center?.delegate = self
    }

    func requestAuthorization() {
        center?.requestAuthorization(options: [.alert, .sound]) { _, _ in }
    }

    func post(_ notifications: [TaskNotification]) {
        guard let center else { return }
        for n in notifications {
            let c = UNMutableNotificationContent()
            c.title = n.title
            if let body = n.body { c.body = body }
            if n.kind == .failed { c.sound = .default }
            center.add(UNNotificationRequest(identifier: "\(n.id)@\(Date().timeIntervalSince1970)", content: c, trigger: nil))
        }
    }

    nonisolated func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification,
                                            withCompletionHandler done: @escaping (UNNotificationPresentationOptions) -> Void) {
        done([.banner, .list])   // LoopX is a regular app now, so it is often frontmost when a task changes
    }

    nonisolated func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse,
                                            withCompletionHandler done: @escaping () -> Void) {
        Task { @MainActor in self.onActivate() }
        done()
    }
}

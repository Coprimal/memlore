import BackgroundTasks
import ExpoModulesCore
import Foundation
import UIKit
import os

/// Registers the background tasks before launch ends, as BGTaskScheduler
/// requires (a module's `OnCreate` runs too late under the scene life cycle,
/// and never runs on a background launch, which has no scene). Declared in
/// `expo-module.config.json` → `appDelegateSubscribers`.
public class MemloreBackgroundPrefetchSubscriber: ExpoAppDelegateSubscriber {
  public func application(
    _ application: UIApplication,
    didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]? = nil
  ) -> Bool {
    BackgroundPrefetch.start()
    return true
  }
}

/// Background prefetch of the iCloud sync folder (M2c, plan
/// `docs/plans/2026-11-05-mobile-m2c-background-prefetch/`): iOS runs a short
/// task now and then that downloads the folder's placeholders, so the next
/// foreground sync finds them on disk. The task only lists and downloads
/// ciphertext through `ICloudFolder`: it never touches the engine,
/// `EngineHolder`, `DeviceSecurity`, `SyncDriver`, keys or `engineQueue`
/// (ADR §13). Removing the Info.plist keys turns it off (kill switch).
enum BackgroundPrefetch {
  static let refresh = "app.memlore.mobile.sync-refresh"
  static let processing = "app.memlore.mobile.sync-processing"

  private static let log = Logger(subsystem: "app.memlore.mobile", category: "background")

  /// The identifiers registered at launch: only those Info.plist permits.
  private static let registered = OSAllocatedUnfairLock<Set<String>>(initialState: [])

  /// Keeps the `didEnterBackground` observer alive. Only set in `start`.
  private static var observer: NSObjectProtocol?

  /// From `didFinishLaunching`: registers every identifier listed in
  /// `BGTaskSchedulerPermittedIdentifiers` (registering an unlisted one
  /// crashes) and schedules on each move to the background.
  static func start() {
    let permitted =
      Bundle.main.object(forInfoDictionaryKey: "BGTaskSchedulerPermittedIdentifiers") as? [String]
      ?? []
    for identifier in [refresh, processing] where permitted.contains(identifier) {
      let budget: TimeInterval = identifier == refresh ? 25 : 120
      let ok = BGTaskScheduler.shared.register(forTaskWithIdentifier: identifier, using: nil) {
        task in
        handle(task, budget: budget)
      }
      if ok {
        registered.withLock { _ = $0.insert(identifier) }
      } else {
        log.error("could not register \(kind(identifier), privacy: .public)")
      }
    }
    guard !registered.withLock({ $0.isEmpty }), observer == nil else { return }
    observer = NotificationCenter.default.addObserver(
      forName: UIApplication.didEnterBackgroundNotification, object: nil, queue: nil
    ) { _ in
      schedule()
    }
  }

  /// Submits both requests when the gate passes; a pending request with the
  /// same identifier is replaced.
  private static func schedule() {
    let open = gateOpen()
    log.notice("entered background, gate \(open ? "open" : "closed", privacy: .public)")
    guard open else { return }
    for identifier in registered.withLock({ $0 }) {
      submit(identifier)
    }
  }

  /// iCloud only: a connected folder (the bookmark) and a vault on this
  /// phone. The vault folder is excluded from backups, so a restored phone
  /// with a stale bookmark does nothing.
  /// TODO(later): Drive has no background prefetch; see docs/LATER.md
  /// "M2c: Drive background prefetch deferred".
  private static func gateOpen() -> Bool {
    guard ICloudFolder.hasBookmark else { return false }
    // The same folder as `EngineHolder.vaultFolder()`, without creating it.
    guard
      let support = try? FileManager.default.url(
        for: .applicationSupportDirectory, in: .userDomainMask, appropriateFor: nil, create: false)
    else { return false }
    let db = support.appendingPathComponent("vault", isDirectory: true)
      .appendingPathComponent("memlore.db")
    return FileManager.default.fileExists(atPath: db.path)
  }

  private static func submit(_ identifier: String) {
    let request: BGTaskRequest
    if identifier == processing {
      let processingRequest = BGProcessingTaskRequest(identifier: identifier)
      processingRequest.requiresNetworkConnectivity = true
      processingRequest.requiresExternalPower = false
      request = processingRequest
    } else {
      request = BGAppRefreshTaskRequest(identifier: identifier)
    }
    request.earliestBeginDate = Date(timeIntervalSinceNow: 15 * 60)
    do {
      try BGTaskScheduler.shared.submit(request)
      log.notice("submitted \(kind(identifier), privacy: .public)")
    } catch {
      // On the simulator: BGTaskSchedulerErrorDomain 1 (expected).
      let ns = error as NSError
      let name = kind(identifier)
      log.error(
        "submit \(name, privacy: .public) failed: \(ns.domain, privacy: .public) \(ns.code)")
    }
  }

  /// Shared by both task kinds; `budget` is how long the walk may run.
  private static func handle(_ task: BGTask, budget: TimeInterval) {
    let name = kind(task.identifier)
    let begin = Date()
    guard gateOpen() else {
      log.notice("\(name, privacy: .public) task: gate closed")
      task.setTaskCompleted(success: true)
      return
    }
    // First, so a crash or an expiry still leaves one scheduled.
    submit(task.identifier)
    // Before the expiry handler, which stops only this walk. Both kinds are
    // submitted together and may run in one window: the second one returns
    // at once rather than waiting behind the first (a success, so iOS does
    // not back off).
    guard let walk = ICloudFolder.shared.reserveBackground() else {
      log.notice("\(name, privacy: .public) task: a walk or a prepare is already running")
      task.setTaskCompleted(success: true)
      return
    }
    let completed = OSAllocatedUnfairLock(initialState: false)
    let complete = { (ok: Bool) in
      let first = completed.withLock { done in
        defer { done = true }
        return !done
      }
      if first { task.setTaskCompleted(success: ok) }
    }
    task.expirationHandler = {
      ICloudFolder.shared.cancelBackground(walk)
      let elapsed = Date().timeIntervalSince(begin)
      log.notice(
        "\(name, privacy: .public) task expired after \(elapsed, format: .fixed(precision: 1)) s")
      complete(false)
    }
    DispatchQueue.global(qos: .utility).async {
      let finished = ICloudFolder.shared.prefetchInBackground(
        walk, until: begin.addingTimeInterval(budget))
      let outcome = "\(name) task \(finished ? "finished" : "stopped")"
      let elapsed = Date().timeIntervalSince(begin)
      log.notice("\(outcome, privacy: .public) after \(elapsed, format: .fixed(precision: 1)) s")
      // A stopped walk is not a failed task: false would make iOS back off.
      complete(true)
    }
  }

  private static func kind(_ identifier: String) -> String {
    identifier == refresh ? "refresh" : "processing"
  }
}

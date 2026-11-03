import Darwin
import UIKit

/// The app-switcher privacy shield, the background auto-lock clock, save
/// before lock and screen-capture detection (decisions 5 and 6). Main thread
/// only. It never calls the engine itself: `lockVault` queues `lock()` and
/// `endLockSession` queues the second-lock and invisible-vault re-lock on the
/// one serial engine queue in `MemloreCoreModule.swift`, like every other
/// engine call.
@MainActor
final class DeviceSecurity {
  /// Whether the engine holds the keys, as seen from the main thread.
  /// Process-wide like the engine, which outlives a JS reload.
  /// `MemloreCoreModule` sets it after each successful unlock, vault creation
  /// and lock, in the order those ran on the engine queue.
  static var vaultUnlocked = false

  /// How long a pending auto-lock keeps the shield up when JS never calls
  /// `releasePrivacyShield()` (no listener yet, a reload), so the app is never
  /// stuck behind it.
  private static let shieldSafetyTimeout: TimeInterval = 2
  /// How long `didEnterBackground` holds the lock for JS's `flushDone` ack.
  /// No ack by then locks anyway (fail closed).
  private static let flushAckTimeout: TimeInterval = 2

  /// The `onFlushRequest` of the current inactive spell (sent at
  /// willResignActive); nil when the vault was locked then.
  private struct FlushRequest {
    let id: Int
    var acked = false
  }

  private let sendEvent: (String, [String: Any?]) -> Void
  /// Queues the engine's `lock()`; `done` runs on the main thread after it.
  private let lockVault: (@escaping @MainActor () -> Void) -> Void
  /// Queues the engine's `lockSecondLock()` and `closeInvisibleVault()`;
  /// `done` runs on the main thread after them.
  private let endLockSession: (@escaping @MainActor () -> Void) -> Void

  private var observers: [NSObjectProtocol] = []
  private var shields: [UIView] = []
  /// `CLOCK_MONOTONIC` nanoseconds at the last `didEnterBackground`.
  private var backgroundedAt: UInt64?
  /// Locked in the background (interval "immediate", after the flush wait);
  /// JS hears about it at the next `willEnterForeground`.
  private var lockedInBackground = false
  /// An auto-lock JS has not answered yet: the shield stays up until
  /// `releasePrivacyShield()` or the safety timeout.
  private var autoLockPending = false
  private var shieldTimeout: DispatchWorkItem?
  private var lastFlushId = 0
  private var flushRequest: FlushRequest?
  /// Ends the background task held while `didEnterBackground` waits for the
  /// ack; nil when no wait runs.
  private var endFlushWait: (() -> Void)?
  private var flushTimeout: DispatchWorkItem?

  init(
    sendEvent: @escaping (String, [String: Any?]) -> Void,
    lockVault: @escaping (@escaping @MainActor () -> Void) -> Void,
    endLockSession: @escaping (@escaping @MainActor () -> Void) -> Void
  ) {
    self.sendEvent = sendEvent
    self.lockVault = lockVault
    self.endLockSession = endLockSession
  }

  func start() {
    observe(UIApplication.willResignActiveNotification) { $0.willResignActive() }
    observe(UIApplication.didEnterBackgroundNotification) { $0.didEnterBackground() }
    observe(UIApplication.willEnterForegroundNotification) { $0.willEnterForeground() }
    observe(UIApplication.didBecomeActiveNotification) { $0.didBecomeActive() }
    observe(UIScreen.capturedDidChangeNotification) { $0.capturedDidChange() }
  }

  func stop() {
    observers.forEach(NotificationCenter.default.removeObserver)
    observers = []
    // A reload during the wait: lock now if it was due (fail closed).
    finishFlushWait()
    shieldTimeout?.cancel()
    shieldTimeout = nil
    hideShield()
  }

  /// JS has swapped to the lock screen after `onAutoLock`.
  func releasePrivacyShield() {
    autoLockPending = false
    shieldTimeout?.cancel()
    shieldTimeout = nil
    // JS usually answers while the app is still inactive (between
    // willEnterForeground and didBecomeActive); didBecomeActive then takes
    // the shield off.
    if UIApplication.shared.applicationState == .active {
      hideShield()
    }
  }

  /// JS saved every pending edit for request `id`; an older id is ignored.
  func flushDone(_ id: Int) {
    guard flushRequest?.id == id else { return }
    flushRequest?.acked = true
    finishFlushWait()
  }

  /// Recording, mirroring or AirPlay of any of the app's screens.
  static func isScreenCaptured() -> Bool {
    UIApplication.shared.connectedScenes
      .compactMap { $0 as? UIWindowScene }
      .contains { $0.screen.isCaptured }
  }

  // MARK: - Lifecycle

  private func observe(
    _ name: Notification.Name, _ handler: @escaping @MainActor (DeviceSecurity) -> Void
  ) {
    let token = NotificationCenter.default.addObserver(
      forName: name, object: nil, queue: .main
    ) { [weak self] _ in
      MainActor.assumeIsolated {
        guard let self else { return }
        handler(self)
      }
    }
    observers.append(token)
  }

  /// Before the app-switcher snapshot: iOS takes it after didEnterBackground,
  /// from whatever is on screen. The Face ID sheet and Control Center also
  /// resign active; the shield covering the app then is accepted.
  /// Then, with the shield already up, JS is asked to save while the
  /// WebView still runs; `didEnterBackground` waits for the answer.
  private func willResignActive() {
    showShield()
    flushRequest = nil
    if Self.vaultUnlocked { requestFlush() }
  }

  private func didEnterBackground() {
    shieldTimeout?.cancel()
    shieldTimeout = nil
    showShield()
    // CLOCK_MONOTONIC keeps counting while the device sleeps.
    // CLOCK_UPTIME_RAW, mach_absolute_time and systemUptime stop, so a phone
    // that slept in a pocket would come back unlocked (fail open).
    backgroundedAt = clock_gettime_nsec_np(CLOCK_MONOTONIC)
    // Every interval waits for the saves before the process is suspended; an
    // ack earlier in this spell counts (nothing can be typed since).
    guard Self.vaultUnlocked else { return }
    if flushRequest == nil { requestFlush() }
    waitForFlush()
  }

  private func willEnterForeground() {
    // Back during the wait: an immediate lock runs now, before onAutoLock.
    finishFlushWait()
    let stamp = backgroundedAt
    backgroundedAt = nil
    if lockedInBackground {
      lockedInBackground = false
      autoLockPending = true
      sendEvent("onAutoLock", [:])
      return
    }
    // Locked by the user, or never unlocked: nothing to tell JS.
    guard Self.vaultUnlocked, let stamp else { return }
    let now = clock_gettime_nsec_np(CLOCK_MONOTONIC)
    // The clock never goes back; if it did, lock (fail closed).
    let elapsed = now >= stamp ? now - stamp : UInt64.max
    guard elapsed >= UInt64(AutoLockCache.seconds) * NSEC_PER_SEC else { return }
    Self.vaultUnlocked = false
    autoLockPending = true
    lockVault {}
    sendEvent("onAutoLock", [:])
  }

  private func didBecomeActive() {
    guard autoLockPending else {
      hideShield()
      return
    }
    let timeout = DispatchWorkItem { [weak self] in
      MainActor.assumeIsolated { self?.releasePrivacyShield() }
    }
    shieldTimeout?.cancel()
    shieldTimeout = timeout
    DispatchQueue.main.asyncAfter(deadline: .now() + Self.shieldSafetyTimeout, execute: timeout)
  }

  private func capturedDidChange() {
    let captured = Self.isScreenCaptured()
    sendEvent("onScreenCaptureChange", ["captured": captured])
  }

  // MARK: - Save before lock

  private func requestFlush() {
    lastFlushId += 1
    flushRequest = FlushRequest(id: lastFlushId)
    sendEvent("onFlushRequest", ["id": lastFlushId])
  }

  /// Holds a background task until the spell's request is acked or
  /// `flushAckTimeout` passed, then `finishFlushWait` locks if due.
  private func waitForFlush() {
    guard endFlushWait == nil else { return }
    let app = UIApplication.shared
    var task = UIBackgroundTaskIdentifier.invalid
    let end = {
      guard task != .invalid else { return }
      app.endBackgroundTask(task)
      task = .invalid
    }
    task = app.beginBackgroundTask(withName: "memlore-save-before-lock") { [weak self] in
      MainActor.assumeIsolated { self?.finishFlushWait() }
      end()
    }
    endFlushWait = end
    if flushRequest?.acked == true {
      finishFlushWait()
      return
    }
    let timeout = DispatchWorkItem { [weak self] in
      MainActor.assumeIsolated { self?.finishFlushWait() }
    }
    flushTimeout = timeout
    DispatchQueue.main.asyncAfter(deadline: .now() + Self.flushAckTimeout, execute: timeout)
  }

  /// Ends the wait (ack, timeout, foreground return, expiry or reload), once
  /// per background transition. For every interval it ends the lock session
  /// (second lock and invisible vault, no-ops when there is none), then sends
  /// `onLockSessionEnded`. Only "immediate" locks the vault here: a suspended
  /// app runs no timer, so a 1/5/15-minute interval is measured at
  /// willEnterForeground. Both are queued on the engine queue after the ack,
  /// back to back, so they run after every save JS awaited (a re-lock first
  /// would gate the save of a revealed locked entry); the task ends once they
  /// have. Reads the live state: a manual lock that ran during the wait is not
  /// locked again nor reported as an auto-lock.
  private func finishFlushWait() {
    guard let end = endFlushWait else { return }
    endFlushWait = nil
    flushTimeout?.cancel()
    flushTimeout = nil
    let lockDue = Self.vaultUnlocked && AutoLockCache.seconds == 0
    let send = self.sendEvent
    endLockSession {
      send("onLockSessionEnded", [:])
      if !lockDue { end() }
    }
    guard lockDue else { return }
    Self.vaultUnlocked = false
    lockedInBackground = true
    lockVault { end() }
  }

  // MARK: - Shield

  /// An opaque view over each key window, added last so it sits above the
  /// React root and any presented screen.
  private func showShield() {
    for window in keyWindows() {
      if let shield = shields.first(where: { $0.superview === window }) {
        window.bringSubviewToFront(shield)
        continue
      }
      let shield = UIView(frame: window.bounds)
      shield.backgroundColor = .systemBackground
      shield.autoresizingMask = [.flexibleWidth, .flexibleHeight]
      window.addSubview(shield)
      shields.append(shield)
    }
  }

  private func hideShield() {
    shields.forEach { $0.removeFromSuperview() }
    shields = []
  }

  private func keyWindows() -> [UIWindow] {
    UIApplication.shared.connectedScenes
      .compactMap { $0 as? UIWindowScene }
      .compactMap { $0.keyWindow ?? $0.windows.first }
  }
}

import Foundation
import os

/// The foreground sync scheduler (ADR §13, plan M2a decision 5): a timer that
/// queues the engine's `syncTick` every `tickInterval` while the app is active
/// and the vault unlocked. The engine's desktop rules (on launch, on-save
/// debounce, interval, failure backoff, kick cooldown) decide whether a tick
/// starts a sync; the sync itself runs off the engine queue.
///
/// Process-wide like the engine and `DeviceSecurity.vaultUnlocked`, which
/// outlive a JS reload: a reload's old module is destroyed after the new one
/// is created, so a per-module timer stopped at destroy would kill the new
/// one. Main thread only; it never calls the engine itself: `tick` queues the
/// call on the engine queue in `MemloreCoreModule.swift`.
@MainActor
final class SyncDriver {
  static let shared = SyncDriver()

  /// The engine's `TICK`.
  private nonisolated static let tickInterval: DispatchTimeInterval = .seconds(5)
  private nonisolated static let log = Logger(subsystem: "app.memlore.mobile", category: "sync")

  /// Queues one `syncTick` on the engine queue.
  private var tick: (@Sendable () -> Void)?
  private var active = false
  private var unlocked = false
  private var timer: DispatchSourceTimer?

  private init() {}

  /// Called by every module instance at creation (idempotent). Seeds the
  /// state, since a reload sees no `didBecomeActive` and no unlock.
  func configure(tick: @escaping @Sendable () -> Void, active: Bool, unlocked: Bool) {
    self.tick = tick
    self.active = active
    self.unlocked = unlocked
    update()
  }

  /// `didBecomeActive` (true) and `willResignActive` (false).
  func setActive(_ active: Bool) {
    self.active = active
    update()
  }

  /// Every change of `DeviceSecurity.vaultUnlocked`: unlock, create,
  /// onboarding and every lock.
  func setUnlocked(_ unlocked: Bool) {
    self.unlocked = unlocked
    update()
  }

  private func update() {
    if active && unlocked && tick != nil {
      start()
    } else {
      stop()
    }
  }

  /// The first tick fires at once, so the sync on unlock and on return to
  /// the foreground does not wait a full interval.
  private func start() {
    guard timer == nil, let tick else { return }
    timer = Self.makeTimer(tick: tick)
    Self.log.debug("sync timer started")
  }

  /// Nonisolated, so the handler is not a main-actor closure: it runs on the
  /// timer's own queue, never on the main thread. A handler still waiting
  /// when the next fire comes is coalesced by the source, so ticks never pile
  /// up.
  private nonisolated static func makeTimer(tick: @escaping @Sendable () -> Void)
    -> DispatchSourceTimer
  {
    let source = DispatchSource.makeTimerSource(
      queue: DispatchQueue(label: "app.memlore.mobile.sync-timer", qos: .utility))
    source.schedule(deadline: .now(), repeating: tickInterval, leeway: .milliseconds(500))
    source.setEventHandler {
      log.debug("sync tick")
      tick()
    }
    source.resume()
    return source
  }

  /// A handler already running may still queue one tick; it then finds the
  /// vault locked (ignored) or starts a sync the background cancel ends.
  private func stop() {
    guard let source = timer else { return }
    source.cancel()
    timer = nil
    Self.log.debug("sync timer stopped")
  }
}

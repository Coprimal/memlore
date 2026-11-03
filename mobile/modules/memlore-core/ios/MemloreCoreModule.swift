import ExpoModulesCore
import Foundation
import LocalAuthentication
import UIKit
import UniformTypeIdentifiers

/// Every engine call runs on this serial background queue, so the lazy
/// creation in `EngineHolder` never races and the JS thread never blocks.
private let engineQueue = DispatchQueue(label: "app.memlore.mobile.engine", qos: .userInitiated)

/// One engine per process, rooted at Application Support/vault. Expo creates
/// a new module instance on a JS reload; the engine (and its DB connection)
/// stays. Only touched on `engineQueue`.
private final class EngineHolder: @unchecked Sendable {
  static let shared = EngineHolder()
  private var engine: MemloreEngine?

  func get() throws -> MemloreEngine {
    if let engine {
      return engine
    }
    let support = try FileManager.default.url(
      for: .applicationSupportDirectory, in: .userDomainMask, appropriateFor: nil, create: true)
    let vault = support.appendingPathComponent("vault", isDirectory: true)
    protectVaultFolder(vault)
    let appVersion =
      Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "0.0.0"
    let created = try MemloreEngine(appDataDir: vault.path, appVersion: appVersion)
    engine = created
    return created
  }
}

/// Applied before the engine opens `memlore.db`, on every launch (all steps
/// are idempotent). Best effort: a failure is logged and the vault still
/// opens, since the files work without these attributes.
private func protectVaultFolder(_ vault: URL) {
  let fileManager = FileManager.default
  // CompleteUntilFirstUserAuthentication, not Complete: M2's background sync
  // has to read the vault while the device is locked.
  let protection: [FileAttributeKey: Any] = [
    .protectionKey: FileProtectionType.completeUntilFirstUserAuthentication
  ]
  do {
    try fileManager.createDirectory(
      at: vault, withIntermediateDirectories: true, attributes: protection)
  } catch {
    NSLog("[MemloreCore] could not create the vault folder: \(error)")
  }
  // `createDirectory` only applies the class to a folder it creates. New
  // files (SQLite's -wal/-shm/-journal, the boot file's temp-then-rename)
  // inherit the folder's class, so an existing folder needs it set too.
  do {
    try fileManager.setAttributes(protection, ofItemAtPath: vault.path)
  } catch {
    NSLog("[MemloreCore] could not set the vault folder's protection class: \(error)")
  }
  // Non-recursive: the engine writes only siblings of `memlore.db` here
  // today; a subfolder added later (M4 media) inherits the folder's class.
  do {
    for file in try fileManager.contentsOfDirectory(at: vault, includingPropertiesForKeys: nil) {
      do {
        try fileManager.setAttributes(protection, ofItemAtPath: file.path)
      } catch {
        NSLog("[MemloreCore] could not set a vault file's protection class: \(error)")
      }
    }
  } catch {
    NSLog("[MemloreCore] could not list the vault folder: \(error)")
  }
  // Excluded from iCloud and computer backups (decision 4): the boot file
  // holds the master key wrapped by the password, and a backup copy would
  // allow offline password guessing.
  do {
    var values = URLResourceValues()
    values.isExcludedFromBackup = true
    var url = vault
    try url.setResourceValues(values)
  } catch {
    NSLog("[MemloreCore] could not exclude the vault from backups: \(error)")
  }
}

/// Maps engine errors to the codes `index.ts` documents. Rust messages carry
/// no key material; passwords and phrases are never part of a message here.
private func toException(_ error: Error) -> Exception {
  switch error {
  case MemloreError.Locked:
    return Exception(name: "MemloreLocked", description: "The vault is locked", code: "ERR_LOCKED")
  case MemloreError.WrongPassword:
    return Exception(
      name: "MemloreWrongPassword", description: "Wrong password", code: "ERR_WRONG_PASSWORD")
  case MemloreError.Io(let msg):
    return Exception(name: "MemloreIo", description: msg, code: "ERR_IO")
  case MemloreError.Format(let msg):
    return Exception(name: "MemloreFormat", description: msg, code: "ERR_FORMAT")
  case MemloreError.BiometricDisabled(let msg):
    return biometricDisabled(msg)
  case MemloreError.BiometricCancelled:
    return biometricCancelled()
  case MemloreError.NotFound:
    return Exception(name: "MemloreNotFound", description: "Not found", code: "ERR_NOT_FOUND")
  case MemloreError.NameTaken:
    return Exception(
      name: "MemloreNameTaken", description: "Another tag has this name", code: "ERR_NAME_TAKEN")
  case MemloreError.LastJournal:
    return Exception(
      name: "MemloreLastJournal", description: "The last journal cannot be deleted",
      code: "ERR_LAST_JOURNAL")
  case MemloreError.SecondLockRequired:
    return Exception(
      name: "MemloreSecondLockRequired", description: "The second lock is locked",
      code: "ERR_SECOND_LOCK_REQUIRED")
  default:
    return Exception(name: "MemloreIo", description: error.localizedDescription, code: "ERR_IO")
  }
}

/// Runs `body` with the shared engine, rethrowing engine errors as coded
/// Expo exceptions.
private func withEngine<T>(_ body: (MemloreEngine) throws -> T) throws -> T {
  do {
    return try body(EngineHolder.shared.get())
  } catch {
    throw toException(error)
  }
}

private func biometricDisabled(_ msg: String) -> Exception {
  Exception(name: "MemloreBiometricDisabled", description: msg, code: "ERR_BIOMETRIC_DISABLED")
}

private func biometricCancelled() -> Exception {
  Exception(
    name: "MemloreBiometricCancelled", description: "Biometric unlock was cancelled",
    code: "ERR_BIOMETRIC_CANCELLED")
}

/// A failed `evaluatePolicy`. A failed match counts as cancelled, like
/// `errSecAuthFailed` in the FFI keystore: nothing changed.
private func toBiometricException(_ error: Error?) -> Exception {
  guard let error = error as? LAError else {
    let msg = error?.localizedDescription ?? "The biometric check failed"
    return Exception(name: "MemloreIo", description: msg, code: "ERR_IO")
  }
  switch error.code {
  case .userCancel, .appCancel, .systemCancel, .userFallback, .authenticationFailed:
    return biometricCancelled()
  case .biometryNotAvailable, .biometryNotEnrolled, .biometryLockout, .passcodeNotSet:
    return biometricDisabled(error.localizedDescription)
  default:
    return Exception(name: "MemloreIo", description: error.localizedDescription, code: "ERR_IO")
  }
}

/// The auto-lock interval read from the vault at the last unlock, for
/// `DeviceSecurity`, which needs it while the vault is locked. Never read
/// from the engine while locked.
@MainActor
enum AutoLockCache {
  /// The engine's default (`AUTO_LOCK_DEFAULT`): used until the first
  /// unlock, and when the read fails. Never 0 and never "don't lock".
  nonisolated static let defaultSeconds: UInt32 = 60
  static var seconds: UInt32 = defaultSeconds
}

private func cacheAutoLockSeconds(_ seconds: UInt32) {
  DispatchQueue.main.async {
    MainActor.assumeIsolated { AutoLockCache.seconds = seconds }
  }
}

/// Call on `engineQueue` right after an unlock or a vault creation
/// succeeded. A failed read keeps the unlock and caches the default.
private func refreshAutoLockCache(_ engine: MemloreEngine) {
  do {
    cacheAutoLockSeconds(try engine.autoLockSeconds())
  } catch {
    NSLog("[MemloreCore] could not read the auto-lock interval: \(error)")
    cacheAutoLockSeconds(AutoLockCache.defaultSeconds)
  }
}

/// Mirrors the engine's lock state for `DeviceSecurity`. Call on
/// `engineQueue` right after the engine call succeeded: the main thread runs
/// these in engine-queue order, so the flag ends in the engine's state.
private func markVault(unlocked: Bool) {
  DispatchQueue.main.async {
    MainActor.assumeIsolated { DeviceSecurity.vaultUnlocked = unlocked }
  }
}

/// `DeviceSecurity`'s auto-lock. On `engineQueue` like every other engine
/// call: since 2.5, `lock()` clears the key and then swaps in the placeholder
/// while an unlock installs the connection and then sets the key, so a lock
/// and an unlock on two queues could leave the key set over the placeholder.
private func autoLockVault(done: @escaping @MainActor () -> Void) {
  engineQueue.async {
    do {
      try withEngine { engine in try engine.lock() }
      markVault(unlocked: false)
    } catch {
      NSLog("[MemloreCore] auto-lock failed: \(error)")
    }
    DispatchQueue.main.async {
      MainActor.assumeIsolated { done() }
    }
  }
}

/// `DeviceSecurity`'s background re-lock of the second lock and the invisible
/// vault, after the flush wait. On `engineQueue`, so it runs after the saves
/// queued before it. Both calls are infallible no-ops while the vault is
/// locked.
private func endLockSession(done: @escaping @MainActor () -> Void) {
  engineQueue.async {
    do {
      try withEngine { engine in
        engine.lockSecondLock()
        engine.closeInvisibleVault()
      }
    } catch {
      NSLog("[MemloreCore] could not end the lock session: \(error)")
    }
    DispatchQueue.main.async {
      MainActor.assumeIsolated { done() }
    }
  }
}

/// `BiometricStatus` in `index.ts`.
struct BiometricStatusRecord: Record {
  @Field var available: Bool = false
  /// `faceID` | `touchID` | `none`; the hardware's kind, even when
  /// `available` is false (not enrolled, locked out).
  @Field var kind: String = "none"

  init() {}

  init(available: Bool, kind: String) {
    self.available = available
    self.kind = kind
  }
}

private func biometricStatus() -> BiometricStatusRecord {
  let context = LAContext()
  // `biometryType` is only set once `canEvaluatePolicy` has run.
  let available = context.canEvaluatePolicy(
    .deviceOwnerAuthenticationWithBiometrics, error: nil)
  let kind: String
  switch context.biometryType {
  case .faceID: kind = "faceID"
  case .touchID: kind = "touchID"
  default: kind = "none"
  }
  return BiometricStatusRecord(available: available, kind: kind)
}

/// `EntryRow` in `index.ts`.
struct EntryRowRecord: Record {
  @Field var id: String = ""
  @Field var title: String? = nil
  @Field var previewText: String? = nil
  /// Unix seconds.
  @Field var entryDate: Double = 0
  @Field var emotion: String? = nil
  @Field var isFavorite: Bool = false
  @Field var journalId: String = ""
  @Field var isLocked: Bool = false
  @Field var isInvisible: Bool = false

  init() {}

  init(_ row: EntryRowFfi) {
    id = row.id
    title = row.title
    previewText = row.previewText
    entryDate = Double(row.entryDate)
    emotion = row.emotion
    isFavorite = row.isFavorite
    journalId = row.journalId
    isLocked = row.isLocked
    isInvisible = row.isInvisible
  }
}

/// `EntryPage` in `index.ts`.
struct EntryPageRecord: Record {
  @Field var items: [EntryRowRecord] = []
  @Field var total: Int = 0
  @Field var hasMore: Bool = false

  init() {}

  init(_ page: EntryPageFfi) {
    items = page.items.map { EntryRowRecord($0) }
    total = Int(clamping: page.total)
    hasMore = page.hasMore
  }
}

/// `Tag` in `index.ts`.
struct TagRecord: Record {
  @Field var id: String = ""
  @Field var name: String = ""
  @Field var color: String? = nil

  init() {}

  init(_ tag: TagFfi) {
    id = tag.id
    name = tag.name
    color = tag.color
  }
}

/// `EntryDetail` in `index.ts`.
struct EntryDetailRecord: Record {
  @Field var id: String = ""
  @Field var title: String? = nil
  @Field var previewText: String? = nil
  /// Unix seconds.
  @Field var entryDate: Double = 0
  @Field var emotion: String? = nil
  @Field var isFavorite: Bool = false
  @Field var journalId: String = ""
  @Field var isLocked: Bool = false
  @Field var isInvisible: Bool = false
  @Field var contentText: String? = nil
  @Field var tags: [TagRecord] = []

  init() {}

  init(_ entry: EntryDetailFfi) {
    id = entry.id
    title = entry.title
    previewText = entry.previewText
    entryDate = Double(entry.entryDate)
    emotion = entry.emotion
    isFavorite = entry.isFavorite
    journalId = entry.journalId
    isLocked = entry.isLocked
    isInvisible = entry.isInvisible
    contentText = entry.contentText
    tags = entry.tags.map { TagRecord($0) }
  }
}

/// `Journal` in `index.ts`.
struct JournalRecord: Record {
  @Field var id: String = ""
  @Field var name: String = ""
  @Field var color: String? = nil
  @Field var isLocked: Bool = false
  @Field var isInvisible: Bool = false

  init() {}

  init(_ journal: JournalFfi) {
    id = journal.id
    name = journal.name
    color = journal.color
    isLocked = journal.isLocked
    isInvisible = journal.isInvisible
  }
}

/// `TrashRow` in `index.ts`.
struct TrashRowRecord: Record {
  @Field var id: String = ""
  @Field var title: String? = nil
  @Field var previewText: String? = nil
  /// Unix seconds.
  @Field var trashedAt: Double = 0
  @Field var isLocked: Bool = false
  @Field var isInvisible: Bool = false

  init() {}

  init(_ row: TrashRowFfi) {
    id = row.id
    title = row.title
    previewText = row.previewText
    trashedAt = Double(row.trashedAt)
    isLocked = row.isLocked
    isInvisible = row.isInvisible
  }
}

/// `SearchFilters` in `index.ts` (JS to native only).
struct SearchFiltersRecord: Record {
  /// Unix seconds, inclusive; applied only together with `toExclusive`.
  @Field var from: Double? = nil
  /// Unix seconds, exclusive; applied only together with `from`.
  @Field var toExclusive: Double? = nil
  @Field var journalIds: [String] = []
  @Field var tagIds: [String] = []
  @Field var emotions: [String] = []

  init() {}
}

/// A JS date bound as whole Unix seconds (a fraction is dropped), like
/// `setEntryDate`; a non-finite or out-of-range value is ERR_FORMAT.
private func searchBound(_ value: Double?) throws -> Int64? {
  guard let value else { return nil }
  guard value.isFinite, let seconds = Int64(exactly: value.rounded(.towardZero)) else {
    throw Exception(
      name: "MemloreFormat", description: "search date out of range", code: "ERR_FORMAT")
  }
  return seconds
}

/// `SearchResult` in `index.ts`.
struct SearchResultRecord: Record {
  @Field var id: String = ""
  @Field var journalId: String = ""
  @Field var title: String? = nil
  @Field var previewText: String? = nil
  /// Unix seconds.
  @Field var entryDate: Double = 0

  init() {}

  init(_ result: SearchResultFfi) {
    id = result.id
    journalId = result.journalId
    title = result.title
    previewText = result.previewText
    entryDate = Double(result.entryDate)
  }
}

/// `Version` in `index.ts`.
struct VersionRecord: Record {
  @Field var id: String = ""
  /// Unix seconds.
  @Field var createdAt: Double = 0
  @Field var previewText: String = ""

  init() {}

  init(_ version: VersionFfi) {
    id = version.id
    createdAt = Double(version.createdAt)
    previewText = version.previewText
  }
}

/// `Template` in `index.ts`.
struct TemplateRecord: Record {
  @Field var id: String = ""
  @Field var name: String = ""
  @Field var description: String? = nil
  @Field var contentHtml: String? = nil
  @Field var isPredefined: Bool = false
  @Field var sortOrder: Int = 0

  init() {}

  init(_ template: TemplateFfi) {
    id = template.id
    name = template.name
    description = template.description
    contentHtml = template.contentHtml
    isPredefined = template.isPredefined
    sortOrder = Int(clamping: template.sortOrder)
  }
}

/// `LockState` in `index.ts`.
struct LockStateRecord: Record {
  @Field var enabled: Bool = false
  @Field var revealed: Bool = false
  @Field var showExistence: Bool = false
  @Field var autoLockMinutes: Int = 5
  @Field var invisibleAutoLockMinutes: Int = 5
  @Field var vaultOpen: Bool = false

  init() {}

  init(_ state: LockStateFfi) {
    enabled = state.enabled
    revealed = state.revealed
    showExistence = state.showExistence
    autoLockMinutes = Int(state.autoLockMinutes)
    invisibleAutoLockMinutes = Int(state.invisibleAutoLockMinutes)
    vaultOpen = state.vaultOpen
  }
}

/// A JS re-lock interval as the engine's `u32`; the engine checks the
/// allowed values, this only rejects what does not fit.
private func relockMinutes(_ minutes: Int) throws -> UInt32 {
  guard let value = UInt32(exactly: minutes) else {
    throw Exception(
      name: "MemloreFormat", description: "re-lock minutes out of range", code: "ERR_FORMAT")
  }
  return value
}

public class MemloreCoreModule: Module {
  /// Main thread only.
  private var deviceSecurity: DeviceSecurity?

  public func definition() -> ModuleDefinition {
    Name("MemloreCore")

    Events("onAutoLock", "onScreenCaptureChange", "onFlushRequest", "onLockSessionEnded")

    // Both hop to main in order, so a reload's destroy never runs before its
    // create. Destroy holds the module until the observers are gone.
    OnCreate {
      DispatchQueue.main.async {
        MainActor.assumeIsolated {
          let security = DeviceSecurity(
            sendEvent: { [weak module = self] name, body in module?.sendEvent(name, body) },
            lockVault: autoLockVault,
            endLockSession: endLockSession)
          security.start()
          self.deviceSecurity = security
        }
      }
    }

    OnDestroy {
      DispatchQueue.main.async {
        MainActor.assumeIsolated {
          self.deviceSecurity?.stop()
          self.deviceSecurity = nil
        }
      }
    }

    AsyncFunction("releasePrivacyShield") {
      MainActor.assumeIsolated { self.deviceSecurity?.releasePrivacyShield() }
    }.runOnQueue(.main)

    // Save before lock: JS's answer to `onFlushRequest`.
    AsyncFunction("flushDone") { (id: Int) in
      MainActor.assumeIsolated { self.deviceSecurity?.flushDone(id) }
    }.runOnQueue(.main)

    AsyncFunction("isScreenCaptured") { () -> Bool in
      MainActor.assumeIsolated { DeviceSecurity.isScreenCaptured() }
    }.runOnQueue(.main)

    AsyncFunction("startupMode") { () throws -> String in
      try withEngine { engine in
        switch engine.startupMode() {
        case .firstLaunch: return "first_launch"
        case .passwordLocked: return "password_locked"
        case .bootFileCorrupt: return "boot_file_corrupt"
        }
      }
    }.runOnQueue(engineQueue)

    AsyncFunction("createVault") { (password: String) throws -> String in
      try withEngine { engine in
        let phrase = try engine.createVault(password: password)
        refreshAutoLockCache(engine)
        markVault(unlocked: true)
        return phrase
      }
    }.runOnQueue(engineQueue)

    AsyncFunction("unlock") { (password: String) throws in
      try withEngine { engine in
        try engine.unlock(password: password)
        refreshAutoLockCache(engine)
        markVault(unlocked: true)
      }
    }.runOnQueue(engineQueue)

    AsyncFunction("lock") { () throws in
      try withEngine { engine in try engine.lock() }
      markVault(unlocked: false)
    }.runOnQueue(engineQueue)

    // Not on `engineQueue`: `unlockWithBiometric` holds it for the whole
    // Face ID prompt.
    AsyncFunction("biometricStatus") { () -> BiometricStatusRecord in
      biometricStatus()
    }

    AsyncFunction("isBiometricUnlockEnabled") { () throws -> Bool in
      try withEngine { engine in engine.isBiometricUnlockEnabled() }
    }.runOnQueue(engineQueue)

    // One Face ID check first, so the user confirms before the engine stores
    // a Keychain item (storing it shows no prompt). Touch ID shows the
    // reason; Face ID shows the Info.plist purpose string.
    AsyncFunction("enableBiometric") { (promise: Promise) in
      let context = LAContext()
      // No "Enter Password" button: this prompt has no fallback.
      context.localizedFallbackTitle = ""
      // The purpose string, localised (vi.lproj/InfoPlist.strings); the
      // default is app.config.ts's English text.
      let reason = Bundle.main.localizedString(
        forKey: "NSFaceIDUsageDescription",
        value:
          "Memlore uses Face ID to unlock your journal. You can always use your password instead.",
        table: "InfoPlist")
      context.evaluatePolicy(
        .deviceOwnerAuthenticationWithBiometrics,
        localizedReason: reason
      ) { success, error in
        // Keeps the context alive until the prompt has answered.
        withExtendedLifetime(context) {}
        guard success else {
          promise.reject(toBiometricException(error))
          return
        }
        engineQueue.async {
          do {
            try withEngine { engine in try engine.enableBiometric() }
            promise.resolve()
          } catch let exception as Exception {
            // Already coded by `withEngine`; `toException` would remap it
            // to ERR_IO.
            promise.reject(exception)
          } catch {
            promise.reject(toException(error))
          }
        }
      }
    }.runOnQueue(.main)

    AsyncFunction("disableBiometric") { () throws in
      try withEngine { engine in try engine.disableBiometric() }
    }.runOnQueue(engineQueue)

    AsyncFunction("unlockWithBiometric") { () throws in
      try withEngine { engine in
        try engine.unlockWithBiometric()
        refreshAutoLockCache(engine)
        markVault(unlocked: true)
      }
    }.runOnQueue(engineQueue)

    AsyncFunction("autoLockSeconds") { () throws -> Int in
      try withEngine { engine in try Int(engine.autoLockSeconds()) }
    }.runOnQueue(engineQueue)

    AsyncFunction("setAutoLockSeconds") { (seconds: Int) throws in
      guard let value = UInt32(exactly: seconds) else {
        throw Exception(
          name: "MemloreFormat", description: "auto-lock seconds out of range",
          code: "ERR_FORMAT")
      }
      try withEngine { engine in try engine.setAutoLockSeconds(seconds: value) }
      cacheAutoLockSeconds(value)
    }.runOnQueue(engineQueue)

    AsyncFunction("entryCount") { () throws -> Int in
      try withEngine { engine in try Int(engine.entryCount()) }
    }.runOnQueue(engineQueue)

    AsyncFunction("listEntries") { (journalId: String?, page: Int) throws -> EntryPageRecord in
      try withEngine { engine in
        try EntryPageRecord(
          engine.listEntries(journalId: journalId, page: UInt32(clamping: max(0, page))))
      }
    }.runOnQueue(engineQueue)

    AsyncFunction("getEntry") { (id: String) throws -> EntryDetailRecord in
      try withEngine { engine in try EntryDetailRecord(engine.getEntry(id: id)) }
    }.runOnQueue(engineQueue)

    AsyncFunction("createEntry") { (title: String, journalId: String?) throws -> String in
      try withEngine { engine in try engine.createEntry(title: title, journalId: journalId) }
    }.runOnQueue(engineQueue)

    AsyncFunction("setEntryTitle") { (id: String, title: String) throws in
      try withEngine { engine in try engine.setEntryTitle(id: id, title: title) }
    }.runOnQueue(engineQueue)

    AsyncFunction("setEntryEmotion") { (id: String, emotion: String?) throws in
      try withEngine { engine in try engine.setEntryEmotion(id: id, emotion: emotion) }
    }.runOnQueue(engineQueue)

    AsyncFunction("toggleEntryFavorite") { (id: String) throws -> Bool in
      try withEngine { engine in try engine.toggleEntryFavorite(id: id) }
    }.runOnQueue(engineQueue)

    AsyncFunction("setEntryDate") { (id: String, entryDate: Double) throws in
      guard entryDate.isFinite, let seconds = Int64(exactly: entryDate.rounded(.towardZero)) else {
        throw Exception(
          name: "MemloreFormat", description: "entry date out of range", code: "ERR_FORMAT")
      }
      try withEngine { engine in try engine.setEntryDate(id: id, entryDate: seconds) }
    }.runOnQueue(engineQueue)

    AsyncFunction("moveEntry") { (id: String, journalId: String) throws in
      try withEngine { engine in try engine.moveEntry(id: id, journalId: journalId) }
    }.runOnQueue(engineQueue)

    AsyncFunction("trashEntry") { (id: String) throws in
      try withEngine { engine in try engine.trashEntry(id: id) }
    }.runOnQueue(engineQueue)

    AsyncFunction("listTrash") { () throws -> [TrashRowRecord] in
      try withEngine { engine in try engine.listTrash().map { TrashRowRecord($0) } }
    }.runOnQueue(engineQueue)

    AsyncFunction("restoreEntry") { (id: String) throws in
      try withEngine { engine in try engine.restoreEntry(id: id) }
    }.runOnQueue(engineQueue)

    AsyncFunction("purgeEntry") { (id: String) throws in
      try withEngine { engine in try engine.purgeEntry(id: id) }
    }.runOnQueue(engineQueue)

    AsyncFunction("emptyTrash") { (ids: [String]) throws -> Int in
      try withEngine { engine in try Int(engine.emptyTrash(ids: ids)) }
    }.runOnQueue(engineQueue)

    AsyncFunction("listJournals") { () throws -> [JournalRecord] in
      try withEngine { engine in try engine.listJournals().map { JournalRecord($0) } }
    }.runOnQueue(engineQueue)

    AsyncFunction("createJournal") { (name: String) throws -> JournalRecord in
      try withEngine { engine in try JournalRecord(engine.createJournal(name: name)) }
    }.runOnQueue(engineQueue)

    AsyncFunction("renameJournal") { (id: String, name: String) throws -> JournalRecord in
      try withEngine { engine in try JournalRecord(engine.renameJournal(id: id, name: name)) }
    }.runOnQueue(engineQueue)

    AsyncFunction("journalEntryCount") { (id: String) throws -> Int in
      try withEngine { engine in try Int(clamping: engine.journalEntryCount(id: id)) }
    }.runOnQueue(engineQueue)

    AsyncFunction("deleteJournal") { (id: String) throws in
      try withEngine { engine in try engine.deleteJournal(id: id) }
    }.runOnQueue(engineQueue)

    AsyncFunction("listTags") { () throws -> [TagRecord] in
      try withEngine { engine in try engine.listTags().map { TagRecord($0) } }
    }.runOnQueue(engineQueue)

    AsyncFunction("createTag") { (name: String) throws -> TagRecord in
      try withEngine { engine in try TagRecord(engine.createTag(name: name)) }
    }.runOnQueue(engineQueue)

    AsyncFunction("renameTag") { (id: String, name: String) throws -> TagRecord in
      try withEngine { engine in try TagRecord(engine.renameTag(id: id, name: name)) }
    }.runOnQueue(engineQueue)

    AsyncFunction("deleteTag") { (id: String) throws in
      try withEngine { engine in try engine.deleteTag(id: id) }
    }.runOnQueue(engineQueue)

    AsyncFunction("addTagToEntry") { (entryId: String, tagId: String) throws in
      try withEngine { engine in try engine.addTagToEntry(entryId: entryId, tagId: tagId) }
    }.runOnQueue(engineQueue)

    AsyncFunction("removeTagFromEntry") { (entryId: String, tagId: String) throws in
      try withEngine { engine in try engine.removeTagFromEntry(entryId: entryId, tagId: tagId) }
    }.runOnQueue(engineQueue)

    // Yjs bytes cross the bridge as base64: the DOM editor decodes them, JS
    // never handles a byte array.
    AsyncFunction("loadEntryYjs") { (id: String) throws -> String in
      try withEngine { engine in try engine.loadEntryYjs(id: id).base64EncodedString() }
    }.runOnQueue(engineQueue)

    AsyncFunction("saveEntryYjs") {
      (id: String, fullStateB64: String, contentText: String, previewText: String) throws in
      guard let fullState = Data(base64Encoded: fullStateB64) else {
        throw Exception(
          name: "MemloreFormat", description: "fullState is not base64", code: "ERR_FORMAT")
      }
      try withEngine { engine in
        try engine.saveEntryYjs(
          id: id, fullState: fullState, contentText: contentText, previewText: previewText)
      }
    }.runOnQueue(engineQueue)

    AsyncFunction("searchEntries") {
      (query: String, filters: SearchFiltersRecord) throws -> [SearchResultRecord] in
      let ffiFilters = SearchFiltersFfi(
        from: try searchBound(filters.from), toExclusive: try searchBound(filters.toExclusive),
        journalIds: filters.journalIds, tagIds: filters.tagIds, emotions: filters.emotions)
      return try withEngine { engine in
        try engine.searchEntries(query: query, filters: ffiFilters).map { SearchResultRecord($0) }
      }
    }.runOnQueue(engineQueue)

    AsyncFunction("listEntryVersions") { (entryId: String) throws -> [VersionRecord] in
      try withEngine { engine in
        try engine.listEntryVersions(entryId: entryId).map { VersionRecord($0) }
      }
    }.runOnQueue(engineQueue)

    // Base64, like `loadEntryYjs`.
    AsyncFunction("getEntryVersionContent") {
      (entryId: String, versionId: String) throws -> String in
      try withEngine { engine in
        try engine.getEntryVersionContent(entryId: entryId, versionId: versionId)
          .base64EncodedString()
      }
    }.runOnQueue(engineQueue)

    AsyncFunction("snapshotEntryVersion") { (entryId: String) throws -> String? in
      try withEngine { engine in try engine.snapshotEntryVersion(entryId: entryId) }
    }.runOnQueue(engineQueue)

    AsyncFunction("listTemplates") { () throws -> [TemplateRecord] in
      try withEngine { engine in try engine.listTemplates().map { TemplateRecord($0) } }
    }.runOnQueue(engineQueue)

    AsyncFunction("createTemplate") {
      (name: String, description: String?) throws -> TemplateRecord in
      try withEngine { engine in
        try TemplateRecord(engine.createTemplate(name: name, description: description))
      }
    }.runOnQueue(engineQueue)

    AsyncFunction("updateTemplate") {
      (id: String, name: String, description: String?) throws -> TemplateRecord in
      try withEngine { engine in
        try TemplateRecord(engine.updateTemplate(id: id, name: name, description: description))
      }
    }.runOnQueue(engineQueue)

    AsyncFunction("deleteTemplate") { (id: String) throws in
      try withEngine { engine in try engine.deleteTemplate(id: id) }
    }.runOnQueue(engineQueue)

    // Second lock and invisible vault: the session lives in Rust. Never log
    // the passwords these take.
    AsyncFunction("lockState") { () throws -> LockStateRecord in
      try withEngine { engine in try LockStateRecord(engine.lockState()) }
    }.runOnQueue(engineQueue)

    AsyncFunction("setSecondLockPassword") { (password: String) throws in
      try withEngine { engine in try engine.setSecondLockPassword(password: password) }
    }.runOnQueue(engineQueue)

    AsyncFunction("unlockSecondLock") { (password: String) throws -> Bool in
      try withEngine { engine in try engine.unlockSecondLock(password: password) }
    }.runOnQueue(engineQueue)

    AsyncFunction("lockSecondLock") { () throws in
      try withEngine { engine in engine.lockSecondLock() }
    }.runOnQueue(engineQueue)

    AsyncFunction("changeSecondLockPassword") {
      (oldPassword: String, newPassword: String) throws in
      try withEngine { engine in
        try engine.changeSecondLockPassword(oldPassword: oldPassword, newPassword: newPassword)
      }
    }.runOnQueue(engineQueue)

    AsyncFunction("disableSecondLock") { (password: String) throws in
      try withEngine { engine in try engine.disableSecondLock(password: password) }
    }.runOnQueue(engineQueue)

    AsyncFunction("setSecondLockShowExistence") { (show: Bool) throws in
      try withEngine { engine in try engine.setSecondLockShowExistence(show: show) }
    }.runOnQueue(engineQueue)

    AsyncFunction("setSecondLockAutoLockMinutes") { (minutes: Int) throws in
      let value = try relockMinutes(minutes)
      try withEngine { engine in try engine.setSecondLockAutoLockMinutes(minutes: value) }
    }.runOnQueue(engineQueue)

    AsyncFunction("setInvisibleAutoLockMinutes") { (minutes: Int) throws in
      let value = try relockMinutes(minutes)
      try withEngine { engine in try engine.setInvisibleAutoLockMinutes(minutes: value) }
    }.runOnQueue(engineQueue)

    AsyncFunction("setEntryLocked") { (entryId: String, locked: Bool) throws in
      try withEngine { engine in try engine.setEntryLocked(entryId: entryId, locked: locked) }
    }.runOnQueue(engineQueue)

    AsyncFunction("setJournalLocked") { (journalId: String, locked: Bool) throws in
      try withEngine { engine in
        try engine.setJournalLocked(journalId: journalId, locked: locked)
      }
    }.runOnQueue(engineQueue)

    AsyncFunction("openInvisibleVault") { (password: String) throws in
      try withEngine { engine in try engine.openInvisibleVault(password: password) }
    }.runOnQueue(engineQueue)

    AsyncFunction("closeInvisibleVault") { () throws in
      try withEngine { engine in engine.closeInvisibleVault() }
    }.runOnQueue(engineQueue)

    AsyncFunction("changeInvisibleVaultPassword") {
      (oldPassword: String, newPassword: String) throws in
      try withEngine { engine in
        try engine.changeInvisibleVaultPassword(oldPassword: oldPassword, newPassword: newPassword)
      }
    }.runOnQueue(engineQueue)

    AsyncFunction("removeEmptyInvisibleVaults") { () throws in
      try withEngine { engine in try engine.removeEmptyInvisibleVaults() }
    }.runOnQueue(engineQueue)

    AsyncFunction("setEntryInvisible") { (entryId: String, invisible: Bool) throws in
      try withEngine { engine in
        try engine.setEntryInvisible(entryId: entryId, invisible: invisible)
      }
    }.runOnQueue(engineQueue)

    AsyncFunction("setJournalInvisible") { (journalId: String, invisible: Bool) throws in
      try withEngine { engine in
        try engine.setJournalInvisible(journalId: journalId, invisible: invisible)
      }
    }.runOnQueue(engineQueue)

    // Spike 17.1, dev route only (`app/(dev)/spike-icloud.tsx`); read-only,
    // see ICloudSpike.swift. Rejects with code ERR_SPIKE.
    // TODO(later): see docs/LATER.md (remove with the dev spike routes).
    AsyncFunction("spikeICloudPickFolder") { (promise: Promise) in
      let presenter = self.appContext?.utilities?.currentViewController()
      MainActor.assumeIsolated {
        guard let presenter else {
          promise.reject(ICloudSpike.spikeError("No view controller to present the picker"))
          return
        }
        let delegate = FolderPickerDelegate(promise: promise)
        ICloudSpike.activePicker = delegate
        let picker = UIDocumentPickerViewController(forOpeningContentTypes: [.folder])
        picker.allowsMultipleSelection = false
        picker.delegate = delegate
        presenter.present(picker, animated: true)
      }
    }.runOnQueue(.main)

    AsyncFunction("spikeICloudBookmarkStatus") { () -> [String: Any] in
      ICloudSpike.bookmarkStatus()
    }.runOnQueue(ICloudSpike.queue)

    AsyncFunction("spikeICloudForgetBookmark") {
      ICloudSpike.forgetBookmark()
    }.runOnQueue(ICloudSpike.queue)

    AsyncFunction("spikeICloudList") { () throws -> [String: Any] in
      try ICloudSpike.list()
    }.runOnQueue(ICloudSpike.queue)

    AsyncFunction("spikeICloudStartDownloads") { () throws -> [String: Any] in
      try ICloudSpike.startDownloads()
    }.runOnQueue(ICloudSpike.queue)

    AsyncFunction("spikeICloudContainerProbe") { () -> [String: Any] in
      ICloudSpike.containerProbe()
    }.runOnQueue(ICloudSpike.queue)

    AsyncFunction("spikeICloudScheduleBackgroundTask") { () throws -> [String: Any] in
      try ICloudSpike.scheduleBackgroundTask()
    }.runOnQueue(ICloudSpike.queue)

    AsyncFunction("spikeICloudListLaterInBackground") { (delaySeconds: Double) in
      MainActor.assumeIsolated { ICloudSpike.listLaterInBackground(delay: delaySeconds) }
    }.runOnQueue(.main)

    AsyncFunction("spikeICloudBackgroundResult") { () -> String? in
      ICloudSpike.backgroundResult()
    }.runOnQueue(ICloudSpike.queue)

    // Spike 17.2, dev route only (`app/(dev)/spike-oauth.tsx`); PKCE and one
    // Keychain item, see OAuthSpike.swift. Rejects with code ERR_SPIKE.
    // TODO(later): see docs/LATER.md (remove with the dev spike routes).
    AsyncFunction("spikeOAuthPkce") { () throws -> [String: Any] in
      try OAuthSpike.pkce()
    }.runOnQueue(OAuthSpike.queue)

    AsyncFunction("spikeOAuthKeychainWrite") { (value: String) throws in
      try OAuthSpike.write(value)
    }.runOnQueue(OAuthSpike.queue)

    AsyncFunction("spikeOAuthKeychainRead") { () throws -> [String: Any] in
      try OAuthSpike.read()
    }.runOnQueue(OAuthSpike.queue)

    AsyncFunction("spikeOAuthKeychainDelete") { () throws -> Bool in
      try OAuthSpike.delete()
    }.runOnQueue(OAuthSpike.queue)
  }
}

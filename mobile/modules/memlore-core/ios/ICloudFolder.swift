import ExpoModulesCore
import Foundation
import UIKit
import UniformTypeIdentifiers
import os

/// The iCloud Drive folder the user picked (M2b), handed to the engine as its
/// `FolderAccess`. Swift owns the picker, the security-scoped bookmark, the
/// scope and the placeholder prefetch; Rust reads and writes the folder with
/// plain I/O, like the desktop does in the same folder.
///
/// Everything that touches the bookmark, the scope or the files runs on
/// `queue`, never on `engineQueue`, and nothing here calls the engine: Rust
/// calls `prepare` and `forget` from `engineQueue` or its own threads and
/// waits. The scope stays open for the process once a folder is prepared
/// (Rust uses the path between calls), and only one is ever open.
///
/// A pick only stages the folder (`pending`, in memory: an app kill drops an
/// unconnected pick; a JS reload keeps it, which is safe because only the
/// connect promotes it). It never touches the bookmark or the
/// open scope, which a running sync may be using; `prepareConnect`, which
/// only `icloud_connect` calls under the sync guard, promotes it.
/// Logs name at most the folder's last component, never a path or a file.
///
/// A background task (M2c, `BackgroundPrefetch.swift`) runs the same walk
/// through `prefetchInBackground` with a deadline, one walk at a time. A
/// foreground `prepare` stops it and keeps any new one from starting while
/// it waits; a pick stops it too. A walk never drops the bookmark; it may
/// refresh a stale one (same folder, guarded by `isCurrent`).
final class ICloudFolder: FolderAccess, @unchecked Sendable {
  static let shared = ICloudFolder()

  private static let bookmarkKey = "app.memlore.icloud.folderBookmark"

  /// Serializes `prepare`, `prepareConnect`, `forget` and the staging of a pick.
  private let queue = DispatchQueue(label: "app.memlore.mobile.icloud-folder", qos: .userInitiated)

  /// Bumped by `forget` before it waits for `queue`: a `prepare` that
  /// started earlier stops its walk and touches nothing more ("forget wins").
  private let forgotten = OSAllocatedUnfairLock(initialState: UInt64(0))

  /// The coordinated read `prepare` is waiting on, if any: `forget` cancels
  /// it, since a directory the File Provider never fills would hold `queue`.
  private let inFlight = OSAllocatedUnfairLock<NSFileCoordinator?>(uncheckedState: nil)

  /// The background walk and the foreground calls waiting for `queue`.
  private struct Background {
    /// The next walk id `reserveBackground` hands out.
    var next: UInt64 = 0
    /// The reserved walk, running or waiting for `queue`.
    var active: UInt64?
    /// `cancelBackground`, a foreground `prepare` or a pick asked the active
    /// walk to stop. Reset when a walk is reserved and when it is released.
    var stopped = false
    /// The active walk holds `queue`.
    var walking = false
    /// Foreground `prepare`/`prepareConnect` calls not done yet.
    var foregroundWaiting = 0
  }

  private let background = OSAllocatedUnfairLock(initialState: Background())

  /// The background walk's summary line (M2c); foreground lines use NSLog.
  private static let log = Logger(subsystem: "app.memlore.mobile", category: "background")

  /// The URL whose security scope is open. Only touched on `queue`.
  private var scoped: URL?

  /// A picked folder not connected yet: its bookmark and the `forgotten`
  /// generation its pick started in. Only touched on `queue`.
  private var pending: (bookmark: Data, generation: UInt64)?

  /// Keeps the picker's delegate alive while the picker is up.
  @MainActor private var activePicker: FolderPickerDelegate?

  private init() {}

  /// Whether a folder is connected (read-only, any thread).
  static var hasBookmark: Bool {
    UserDefaults.standard.data(forKey: bookmarkKey) != nil
  }

  // MARK: FolderAccess

  func prepare() -> FolderPrepareFfi {
    inForeground {
      let started = forgotten.withLock { $0 }
      return queue.sync { prepareOnQueue(started) }
    }
  }

  /// `prepare`, after promoting a pick staged in this generation to the
  /// bookmark. Only `icloud_connect` calls it, under the sync guard.
  func prepareConnect() -> FolderPrepareFfi {
    inForeground {
      let started = forgotten.withLock { $0 }
      return queue.sync {
        if let staged = pending {
          pending = nil
          if staged.generation == started, isCurrent(started) {
            UserDefaults.standard.set(staged.bookmark, forKey: Self.bookmarkKey)
          }
        }
        return prepareOnQueue(started)
      }
    }
  }

  func forget() {
    forgotten.withLock { $0 &+= 1 }
    inFlight.withLockUnchecked { $0?.cancel() }
    queue.sync {
      pending = nil
      scoped?.stopAccessingSecurityScopedResource()
      scoped = nil
      UserDefaults.standard.removeObject(forKey: Self.bookmarkKey)
    }
  }

  // MARK: Background (M2c)

  /// Reserves the one background walk: its id, or nil when another walk is
  /// reserved (running or waiting for `queue`) or a foreground `prepare` is
  /// waiting. Pass the id to `prefetchInBackground`, which releases it, and
  /// to `cancelBackground`. Any thread.
  func reserveBackground() -> UInt64? {
    background.withLock { state in
      guard state.active == nil, state.foregroundWaiting == 0 else { return nil }
      let walk = state.next
      state.next &+= 1
      state.active = walk
      state.stopped = false
      return walk
    }
  }

  /// The walk `prepare` does (resolve, scope, prefetch, wait), from a
  /// background task, for the `walk` `reserveBackground` returned: it stops
  /// at `deadline`, at a `forget`, at its own `cancelBackground` or when a
  /// foreground `prepare` or a pick comes. True when it finished before any of them;
  /// false when it stopped, or the folder is gone or unreadable. It never
  /// touches `pending`, and never drops the bookmark; it may refresh a stale
  /// one (same folder, guarded by `isCurrent`). Blocks: call it off the main
  /// thread.
  func prefetchInBackground(_ walk: UInt64, until deadline: Date) -> Bool {
    // Outside `queue.sync`, so a walk refused at the start releases it too.
    defer {
      background.withLock { state in
        if state.active == walk {
          state.active = nil
          state.stopped = false
        }
      }
    }
    let started = forgotten.withLock { $0 }
    let begin = Date()
    // Latched by `keepGoing`, which only ever turns false: a `.gone` walk
    // that saw it fail stopped, one that didn't found the folder gone.
    var halted = false
    let keepGoing = {
      let going =
        self.isCurrent(started) && Date() < deadline
        && self.background.withLock {
          $0.active == walk && !$0.stopped && $0.foregroundWaiting == 0
        }
      if !going { halted = true }
      return going
    }
    return queue.sync {
      guard keepGoing() else { return false }
      background.withLock { $0.walking = true }
      // Cleared after the walk, when `list` has already cleared `inFlight`.
      defer { background.withLock { $0.walking = false } }
      var stats = WalkStats()
      let result = walkOnQueue(keepGoing, until: deadline, stats: &stats)
      let outcome: String
      switch result {
      case .ready: outcome = "finished"
      case .unreadable: outcome = "unreadable"
      case .gone: outcome = halted ? "stopped" : "gone"
      }
      let folder = stats.folder ?? "-"
      let (kicked, refused) = (stats.kicked, stats.refused)
      let elapsed = Date().timeIntervalSince(begin)
      Self.log.notice(
        """
        background walk …/\(folder, privacy: .private): \(outcome, privacy: .public), \
        \(kicked, privacy: .public) downloads started, \(refused, privacy: .public) refused, \
        \(elapsed, format: .fixed(precision: 1), privacy: .public) s
        """)
      if case .ready = result { return true }
      return false
    }
  }

  /// Stops `walk` (running or waiting for `queue`), from its task's expiry;
  /// another walk is never touched. Never touches the bookmark, `pending`,
  /// `scoped` or `forgotten` itself (the walk it stops never drops the
  /// bookmark; it may refresh a stale one, same folder, guarded by
  /// `isCurrent`), and cancels `inFlight` only while that walk holds
  /// `queue`: a foreground read is never cancelled. Any thread.
  func cancelBackground(_ walk: UInt64) {
    background.withLock { state in
      guard state.active == walk else { return }
      state.stopped = true
      // Under the lock: the walk can't clear `walking` and leave `queue` to
      // a foreground `prepare` whose read this would cancel.
      if state.walking {
        inFlight.withLockUnchecked { $0?.cancel() }
      }
    }
  }

  /// Runs a foreground `prepare`: while it waits for `queue` and runs, a
  /// background walk stops at its next check (its in-flight read cancelled
  /// here) and no new one starts, so the foreground never waits for one's
  /// deadline.
  private func inForeground(_ body: () -> FolderPrepareFfi) -> FolderPrepareFfi {
    background.withLock { state in
      state.foregroundWaiting += 1
      stopWalk(&state)
    }
    defer { background.withLock { $0.foregroundWaiting -= 1 } }
    return body()
  }

  /// Under `background`: stops the reserved walk, running or still waiting
  /// for `queue` (it sees `stopped` at its first check, even once the
  /// caller is done), and cancels `inFlight` only while that walk holds
  /// `queue`, as in `cancelBackground` (the walk can't clear `walking` and
  /// leave `queue` to a foreground read this would cancel).
  private func stopWalk(_ state: inout Background) {
    if state.active != nil { state.stopped = true }
    if state.walking {
      inFlight.withLockUnchecked { $0?.cancel() }
    }
  }

  // MARK: Picker

  /// Presents the folder picker. Resolves with `["name": <folder>]` once the
  /// folder is staged for `icloudConnect`; rejects with ERR_CANCELLED (the
  /// user closed the picker, or a `forget` came while it was up),
  /// ERR_NOT_ICLOUD_FOLDER (a folder outside iCloud Drive) or ERR_IO. The
  /// bookmark and the open scope stay as they were either way.
  @MainActor func pick(from presenter: UIViewController?, promise: Promise) {
    guard let presenter else {
      promise.reject(Self.ioError("No view controller to present the folder picker"))
      return
    }
    guard activePicker == nil else {
      promise.reject(Self.ioError("The folder picker is already open"))
      return
    }
    // A `forget` from here on (a Disconnect, a failed connect) voids the pick.
    let started = forgotten.withLock { $0 }
    let delegate = FolderPickerDelegate(promise: promise, folder: self, started: started)
    activePicker = delegate
    let picker = UIDocumentPickerViewController(forOpeningContentTypes: [.folder])
    picker.allowsMultipleSelection = false
    picker.delegate = delegate
    presenter.present(picker, animated: true)
  }

  @MainActor fileprivate func pickerClosed() {
    activePicker = nil
  }

  /// Stages the picked folder when it is in iCloud Drive and no `forget`
  /// came since the pick started. Its scope is open only while it is
  /// checked and bookmarked; the bookmark and `scoped` are not touched. A
  /// running or queued background walk is stopped first (a walk reserved in
  /// the moment after that can still run ahead of staging; rare).
  fileprivate func adopt(_ url: URL, started: UInt64, promise: Promise) {
    background.withLock { stopWalk(&$0) }
    queue.async {
      guard self.isCurrent(started) else {
        promise.reject(Self.cancelled())
        return
      }
      let opened = url.startAccessingSecurityScopedResource()
      defer { if opened { url.stopAccessingSecurityScopedResource() } }
      guard Self.isInICloudDrive(url) else {
        promise.reject(
          Exception(
            name: "MemloreNotICloudFolder", description: "The folder is not in iCloud Drive",
            code: "ERR_NOT_ICLOUD_FOLDER"))
        return
      }
      guard opened else {
        promise.reject(Self.ioError("No access to the picked folder"))
        return
      }
      guard
        let data = try? url.bookmarkData(
          options: .minimalBookmark, includingResourceValuesForKeys: nil, relativeTo: nil)
      else {
        promise.reject(Self.ioError("The picked folder could not be remembered"))
        return
      }
      guard self.isCurrent(started) else {
        promise.reject(Self.cancelled())
        return
      }
      self.pending = (data, started)
      NSLog("[ICloudFolder] picked …/%@", url.lastPathComponent)
      promise.resolve(["name": url.lastPathComponent])
    }
  }

  /// `isUbiquitousItem` is proven only for files inside a picked folder
  /// (spike 17.1); the fallback for the folder itself is a path with a
  /// folder under `Mobile Documents/com~apple~CloudDocs`. On My iPhone and
  /// third-party providers match neither.
  private static func isInICloudDrive(_ url: URL) -> Bool {
    if (try? url.resourceValues(forKeys: [.isUbiquitousItemKey]))?.isUbiquitousItem == true {
      return true
    }
    let parts = url.standardizedFileURL.pathComponents
    return parts.indices.dropLast(2).contains { index in
      parts[index] == "Mobile Documents" && parts[index + 1] == "com~apple~CloudDocs"
    }
  }

  // MARK: Prepare

  private func isCurrent(_ started: UInt64) -> Bool {
    forgotten.withLock { $0 } == started
  }

  /// On `queue`. The foreground walk: it stops only at a `forget`, and
  /// waits `downloadDeadline` for the downloads it started.
  private func prepareOnQueue(_ started: UInt64) -> FolderPrepareFfi {
    var stats = WalkStats()
    return walkOnQueue({ self.isCurrent(started) }, until: .distantFuture, stats: &stats)
  }

  /// What a walk did, for the background log line.
  private struct WalkStats {
    var folder: String?
    var kicked = 0
    var refused = 0
  }

  /// On `queue`. Checks `keepGoing` before every change, so a `forget` that
  /// came first leaves nothing behind (a later `forget` closes what this
  /// opened). Never drops the bookmark; may refresh a stale one (same
  /// folder, guarded by `isCurrent` through `keepGoing`). `limit` caps the
  /// download wait (`awaitDownloads`); a timed-out wait still makes the
  /// folder `Ready` when `keepGoing` holds.
  private func walkOnQueue(
    _ keepGoing: () -> Bool, until limit: Date, stats: inout WalkStats
  ) -> FolderPrepareFfi {
    guard keepGoing(), FileManager.default.ubiquityIdentityToken != nil,
      let data = UserDefaults.standard.data(forKey: Self.bookmarkKey)
    else { return .gone }
    var stale = false
    guard
      let resolved = try? URL(
        resolvingBookmarkData: data, options: [], relativeTo: nil, bookmarkDataIsStale: &stale)
    else { return .gone }
    // The scope is opened on the resolved URL itself (a derived URL may not
    // carry it); the standardized path is only compared and handed out.
    let path = resolved.standardizedFileURL.path
    stats.folder = resolved.lastPathComponent
    guard keepGoing(), openScope(resolved, path: path) else { return .gone }
    if stale {
      // Made while the scope is open, as at the pick.
      if let fresh = try? resolved.bookmarkData(
        options: .minimalBookmark, includingResourceValuesForKeys: nil, relativeTo: nil),
        keepGoing()
      {
        UserDefaults.standard.set(fresh, forKey: Self.bookmarkKey)
      }
    }
    var isDirectory: ObjCBool = false
    guard FileManager.default.fileExists(atPath: path, isDirectory: &isDirectory),
      isDirectory.boolValue, !(path as NSString).pathComponents.contains(".Trash")
    else { return .gone }
    switch prefetch(resolved, keepGoing, until: limit, stats: &stats) {
    case .stopped: return .gone
    case .unreadable: return .unreadable
    case .done: break
    }
    guard keepGoing() else { return .gone }
    return .ready(rootPath: path)
  }

  /// Opens `url`'s scope unless the open one is at `path`, closing the
  /// previous one. False when the system refuses it (fail closed: `Gone`).
  private func openScope(_ url: URL, path: String) -> Bool {
    if let scoped, scoped.standardizedFileURL.path == path {
      return true
    }
    guard url.startAccessingSecurityScopedResource() else { return false }
    scoped?.stopAccessingSecurityScopedResource()
    scoped = url
    return true
  }

  /// How long `prepare` waits for the downloads it started.
  private static let downloadDeadline: TimeInterval = 60

  /// How a `prefetch` ended.
  private enum Walk {
    case done
    /// Its stop condition fired: a `forget` (or, in the background, a
    /// `cancelBackground`, a foreground `prepare`, a pick or the deadline).
    case stopped
    /// iCloud did not list the root: try again later, the folder is there.
    case unreadable
  }

  /// Materializes every directory under `root` (the engine lists them with
  /// plain I/O, which blocks on a dataless one), downloads every placeholder
  /// file outside `media/` and waits for them (`awaitDownloads`). Hidden
  /// files included (`.meta`, legacy placeholders). A root without `.meta`
  /// is not walked (the iCloud Drive root, say): the engine refuses it as
  /// not a sync folder. A subdirectory that can't be listed is skipped.
  private func prefetch(
    _ root: URL, _ keepGoing: () -> Bool, until limit: Date, stats: inout WalkStats
  ) -> Walk {
    guard let top = list(root, keepGoing) else {
      return keepGoing() ? .unreadable : .stopped
    }
    guard top.contains(where: { $0.lastPathComponent == ".meta" }) else {
      return keepGoing() ? .done : .stopped
    }
    var downloading: [URL] = []
    var dirs: [(entries: [URL], inMedia: Bool)] = [(top, false)]
    while let (entries, inMedia) = dirs.popLast() {
      for item in entries {
        guard keepGoing() else { return .stopped }
        if (try? item.resourceValues(forKeys: [.isDirectoryKey]))?.isDirectory == true {
          // `media/` is materialized too (the engine may list it); only its
          // files are not downloaded ahead.
          if let children = list(item, keepGoing) {
            dirs.append((children, inMedia || item.lastPathComponent == "media"))
          } else if !keepGoing() {
            return .stopped
          }
          continue
        }
        if inMedia { continue }
        // Counted as they start: a walk stopped mid-tree still logs them.
        switch kick(item) {
        case true?:
          downloading.append(Self.realURL(item).0)
          stats.kicked += 1
        case false?: stats.refused += 1
        case nil: break
        }
      }
    }
    if downloading.count + stats.refused > 0 {
      NSLog(
        "[ICloudFolder] …/%@: %ld downloads started, %ld refused", root.lastPathComponent,
        downloading.count, stats.refused)
    }
    switch awaitDownloads(downloading, keepGoing, until: limit) {
    case .stopped: return .stopped
    case .completed, .timedOut: return .done
    }
  }

  /// How an `awaitDownloads` ended.
  private enum Wait {
    case completed
    /// Past the deadline with downloads still running.
    case timedOut
    /// Its stop condition fired.
    case stopped
  }

  /// Waits until no file in `files` is still a placeholder being downloaded,
  /// for at most `downloadDeadline` (or until `limit`, if sooner). A read the
  /// engine opens on a file whose download this started may never return
  /// once the download lands (seen in STOP 2), while a read of a dataless
  /// file nobody is downloading downloads it on demand (STOP 1 P2). A file
  /// with a local copy (an older version) or a failed download (offline) is
  /// not waited for. Past the deadline the engine reads anyway (iCloud is
  /// slow, the folder is not gone).
  /// TODO(later): the read can still hang past the deadline; see
  /// docs/LATER.md "M2b: dataless iCloud directories on a real iPhone".
  private func awaitDownloads(_ files: [URL], _ keepGoing: () -> Bool, until limit: Date)
    -> Wait
  {
    var waiting = files
    let begin = Date()
    let deadline = min(begin.addingTimeInterval(Self.downloadDeadline), limit)
    while true {
      guard keepGoing() else { return .stopped }
      waiting.removeAll { file in
        // Resource values are cached per URL (STOP 1 P7): read a fresh one.
        var fresh = file
        fresh.removeAllCachedResourceValues()
        guard
          let values = try? fresh.resourceValues(forKeys: [
            .ubiquitousItemDownloadingStatusKey, .ubiquitousItemDownloadingErrorKey,
          ])
        else { return true }
        return values.ubiquitousItemDownloadingError != nil
          || values.ubiquitousItemDownloadingStatus != .notDownloaded
      }
      if waiting.isEmpty { return .completed }
      if Date() >= deadline { break }
      Thread.sleep(forTimeInterval: 0.5)
    }
    NSLog(
      "[ICloudFolder] %ld downloads still running after %.0f s", waiting.count,
      Date().timeIntervalSince(begin))
    return .timedOut
  }

  /// Lists `dir`. A dataless directory (one the File Provider has not filled
  /// yet) is listed inside a coordinated read, which asks the provider to
  /// fill it (one level per call); a plain `readdir` of it blocks until the
  /// provider does, and that can be never. nil when it can't be read (logged)
  /// or `keepGoing` failed (a `forget`, or a `cancelBackground`, foreground
  /// `prepare` or pick during a background walk, cancels the wait).
  /// TODO(later): no timeout on the coordinated read yet; see docs/LATER.md
  /// "M2b: dataless iCloud directories on a real iPhone".
  private func list(_ dir: URL, _ keepGoing: () -> Bool) -> [URL]? {
    let read = {
      try? FileManager.default.contentsOfDirectory(
        at: $0, includingPropertiesForKeys: [.isDirectoryKey], options: [])
    }
    var probe = dir
    probe.removeAllCachedResourceValues()
    let status = (try? probe.resourceValues(forKeys: [.ubiquitousItemDownloadingStatusKey]))?
      .ubiquitousItemDownloadingStatus
    // nil: not an iCloud item, listed as it is.
    guard let status, status != .current else { return read(dir) }
    let coordinator = NSFileCoordinator()
    inFlight.withLockUnchecked { $0 = coordinator }
    defer { inFlight.withLockUnchecked { $0 = nil } }
    // After publishing it: a `forget` (or, during a background walk, a
    // `cancelBackground`, a foreground `prepare` or a pick) cancels it.
    guard keepGoing() else { return nil }
    var listed: [URL]?
    var error: NSError?
    coordinator.coordinate(readingItemAt: dir, options: [], error: &error) { listed = read($0) }
    guard keepGoing() else { return nil }
    if listed == nil {
      NSLog(
        "[ICloudFolder] …/%@ could not be listed: %@ %ld", dir.lastPathComponent,
        error?.domain ?? "no error", error?.code ?? 0)
    }
    return listed
  }

  /// Starts downloading `item` unless it is already current (nil). False
  /// when the system refused.
  private func kick(_ item: URL) -> Bool? {
    let (file, legacyPlaceholder) = Self.realURL(item)
    if !legacyPlaceholder {
      // Resource values are cached per URL (STOP 1 P7): read a fresh one.
      var fresh = file
      fresh.removeAllCachedResourceValues()
      let status = (try? fresh.resourceValues(forKeys: [.ubiquitousItemDownloadingStatusKey]))?
        .ubiquitousItemDownloadingStatus
      // nil: not an iCloud item.
      guard let status, status != .current else { return nil }
    }
    do {
      try FileManager.default.startDownloadingUbiquitousItem(at: file)
      return true
    } catch {
      return false
    }
  }

  /// `.a.bin.icloud` is the legacy name of a placeholder for `a.bin` (older
  /// iOS); returns the real file's URL and whether it was one.
  private static func realURL(_ url: URL) -> (URL, Bool) {
    let raw = url.lastPathComponent
    let suffix = ".icloud"
    guard raw.hasPrefix("."), raw.hasSuffix(suffix), raw.count > suffix.count + 1 else {
      return (url, false)
    }
    let name = String(raw.dropFirst().dropLast(suffix.count))
    return (url.deletingLastPathComponent().appendingPathComponent(name), true)
  }

  fileprivate static func ioError(_ description: String) -> Exception {
    Exception(name: "MemloreIo", description: description, code: "ERR_IO")
  }

  fileprivate static func cancelled() -> Exception {
    Exception(
      name: "MemloreCancelled", description: "The folder picker was closed", code: "ERR_CANCELLED")
  }
}

/// Folder picker callbacks; settles the module promise once.
final class FolderPickerDelegate: NSObject, UIDocumentPickerDelegate {
  private var promise: Promise?
  private let folder: ICloudFolder
  /// The `forgotten` generation when the picker opened.
  private let started: UInt64

  fileprivate init(promise: Promise, folder: ICloudFolder, started: UInt64) {
    self.promise = promise
    self.folder = folder
    self.started = started
  }

  func documentPicker(_ controller: UIDocumentPickerViewController, didPickDocumentsAt urls: [URL])
  {
    if let promise {
      self.promise = nil
      if let url = urls.first {
        folder.adopt(url, started: started, promise: promise)
      } else {
        promise.reject(ICloudFolder.cancelled())
      }
    }
    let folder = folder
    Task { @MainActor in folder.pickerClosed() }
  }

  func documentPickerWasCancelled(_ controller: UIDocumentPickerViewController) {
    promise?.reject(ICloudFolder.cancelled())
    promise = nil
    let folder = folder
    Task { @MainActor in folder.pickerClosed() }
  }
}


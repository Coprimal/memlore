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
final class ICloudFolder: FolderAccess, @unchecked Sendable {
  static let shared = ICloudFolder()

  private static let bookmarkKey = "app.memlore.icloud.folderBookmark"

  /// Serializes `prepare`, `prepareConnect`, `forget` and the staging of a pick.
  private let queue = DispatchQueue(label: "app.memlore.mobile.icloud-folder", qos: .userInitiated)

  /// Bumped by `forget` before it waits for `queue`: a `prepare` that
  /// started earlier stops its walk and touches nothing more ("forget wins").
  private let forgotten = OSAllocatedUnfairLock(initialState: UInt64(0))

  /// The URL whose security scope is open. Only touched on `queue`.
  private var scoped: URL?

  /// A picked folder not connected yet: its bookmark and the `forgotten`
  /// generation its pick started in. Only touched on `queue`.
  private var pending: (bookmark: Data, generation: UInt64)?

  /// Keeps the picker's delegate alive while the picker is up.
  @MainActor private var activePicker: FolderPickerDelegate?

  private init() {}

  // MARK: FolderAccess

  func prepare() -> FolderPrepareFfi {
    let started = forgotten.withLock { $0 }
    return queue.sync { prepareOnQueue(started) }
  }

  /// `prepare`, after promoting a pick staged in this generation to the
  /// bookmark. Only `icloud_connect` calls it, under the sync guard.
  func prepareConnect() -> FolderPrepareFfi {
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

  func forget() {
    forgotten.withLock { $0 &+= 1 }
    queue.sync {
      pending = nil
      scoped?.stopAccessingSecurityScopedResource()
      scoped = nil
      UserDefaults.standard.removeObject(forKey: Self.bookmarkKey)
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
  /// checked and bookmarked; the bookmark and `scoped` are not touched.
  fileprivate func adopt(_ url: URL, started: UInt64, promise: Promise) {
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

  /// On `queue`. Checks `started` before every change, so a `forget` that
  /// came first leaves nothing behind (a later `forget` closes what this
  /// opened).
  private func prepareOnQueue(_ started: UInt64) -> FolderPrepareFfi {
    guard isCurrent(started), FileManager.default.ubiquityIdentityToken != nil,
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
    guard isCurrent(started), openScope(resolved, path: path) else { return .gone }
    if stale {
      // Made while the scope is open, as at the pick.
      if let fresh = try? resolved.bookmarkData(
        options: .minimalBookmark, includingResourceValuesForKeys: nil, relativeTo: nil),
        isCurrent(started)
      {
        UserDefaults.standard.set(fresh, forKey: Self.bookmarkKey)
      }
    }
    var isDirectory: ObjCBool = false
    guard FileManager.default.fileExists(atPath: path, isDirectory: &isDirectory),
      isDirectory.boolValue, !(path as NSString).pathComponents.contains(".Trash")
    else { return .gone }
    guard prefetch(resolved, started), isCurrent(started) else { return .gone }
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

  /// Starts downloading every placeholder under `root` except `media/`,
  /// without waiting: a plain read of one still downloading downloads it on
  /// demand (STOP 1 P2), and the engine's reads land on downloads already
  /// in flight. Plain enumeration (no `NSFileCoordinator`), hidden files
  /// included (`.meta`, legacy placeholders). False when a `forget` came.
  private func prefetch(_ root: URL, _ started: UInt64) -> Bool {
    guard
      let walker = FileManager.default.enumerator(
        at: root, includingPropertiesForKeys: [.isDirectoryKey], options: [],
        errorHandler: { _, _ in true })
    else { return true }
    var kicked = 0
    var failed = 0
    for case let item as URL in walker {
      guard isCurrent(started) else { return false }
      if (try? item.resourceValues(forKeys: [.isDirectoryKey]))?.isDirectory == true {
        if item.lastPathComponent == "media" { walker.skipDescendants() }
        continue
      }
      let (file, legacyPlaceholder) = Self.realURL(item)
      if !legacyPlaceholder {
        // Resource values are cached per URL (STOP 1 P7): read a fresh one.
        var fresh = file
        fresh.removeAllCachedResourceValues()
        let status = (try? fresh.resourceValues(forKeys: [.ubiquitousItemDownloadingStatusKey]))?
          .ubiquitousItemDownloadingStatus
        // nil: not an iCloud item.
        guard let status, status != .current else { continue }
      }
      do {
        try FileManager.default.startDownloadingUbiquitousItem(at: file)
        kicked += 1
      } catch {
        failed += 1
      }
    }
    if kicked + failed > 0 {
      NSLog(
        "[ICloudFolder] …/%@: %ld downloads started, %ld refused", root.lastPathComponent, kicked,
        failed)
    }
    return true
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


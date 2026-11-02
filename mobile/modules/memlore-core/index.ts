import { NativeModule, requireNativeModule } from 'expo';

/** What the vault on this device needs at launch. */
export type StartupMode = 'first_launch' | 'password_locked' | 'boot_file_corrupt';

/**
 * `code` of the errors the native module rejects with.
 * ERR_BIOMETRIC_DISABLED: Face ID/Touch ID cannot be used now (not enrolled,
 * locked out, or the unlock found it broken and turned it off); use the
 * password. ERR_BIOMETRIC_CANCELLED: the user dismissed or failed the
 * prompt; nothing changed. ERR_NOT_FOUND: the entry, journal, tag or
 * template does not exist or is not listed on mobile (deleted, locked or
 * invisible; a predefined template for `updateTemplate`/`deleteTemplate`).
 * ERR_NAME_TAKEN: a tag rename onto a name another tag holds, a deleted one
 * included (no merge). ERR_LAST_JOURNAL: the last journal mobile lists
 * cannot be deleted, even when locked journals remain.
 * ERR_SECOND_LOCK_REQUIRED: the entry or journal is locked and shown
 * redacted (`showExistence` on, not revealed); reveal with the second-lock
 * password. Never returned while locked rows are hidden.
 */
export type MemloreErrorCode =
  | 'ERR_LOCKED'
  | 'ERR_WRONG_PASSWORD'
  | 'ERR_IO'
  | 'ERR_FORMAT'
  | 'ERR_BIOMETRIC_DISABLED'
  | 'ERR_BIOMETRIC_CANCELLED'
  | 'ERR_NOT_FOUND'
  | 'ERR_NAME_TAKEN'
  | 'ERR_LAST_JOURNAL'
  | 'ERR_SECOND_LOCK_REQUIRED';

/** The device's biometric hardware. */
export type BiometricKind = 'faceID' | 'touchID' | 'none';

export interface BiometricStatus {
  /** Enrolled and usable now. */
  available: boolean;
  /** The hardware's kind, reported even when `available` is false. */
  kind: BiometricKind;
}

/** Auto-lock interval: 0 (immediate), 1, 5 or 15 minutes. */
export type AutoLockSeconds = 0 | 60 | 300 | 900;

/** Second-lock and invisible-vault re-lock interval in minutes; 0 is never. */
export type RelockMinutes = 0 | 1 | 5 | 15 | 30;

/** The Rust-held lock session and its settings (never the open vault's id). */
export interface LockState {
  /** A second-lock password is set. */
  enabled: boolean;
  /** Locked entries and journals are open now. */
  revealed: boolean;
  /** Locked rows are listed redacted instead of left out. */
  showExistence: boolean;
  /** Any unexpected stored value reads as 5. */
  autoLockMinutes: RelockMinutes;
  /** Any unexpected stored value reads as 5. */
  invisibleAutoLockMinutes: RelockMinutes;
  /** An invisible vault is open. */
  vaultOpen: boolean;
}

/** The engine only stores these three; null is no emotion. */
export type Emotion = 'good' | 'neutral' | 'bad';

/**
 * One row of the entry list. Locked entries are left out, listed redacted
 * (`showExistence`) or listed in full (revealed); invisible ones only while
 * their vault is open.
 */
export interface EntryRow {
  id: string;
  title: string | null;
  previewText: string | null;
  /** Unix seconds. */
  entryDate: number;
  emotion: Emotion | null;
  isFavorite: boolean;
  journalId: string;
  /** Locked by its own flag or its journal's. */
  isLocked: boolean;
  /** Invisible by its own flag or its journal's. */
  isInvisible: boolean;
}

/** One page of the entry list, newest first. */
export interface EntryPage {
  items: EntryRow[];
  /** Entries in the whole list, every page. */
  total: number;
  hasMore: boolean;
}

export interface Tag {
  id: string;
  name: string;
  color: string | null;
}

/** One listed entry with its plain text and tags. */
export interface EntryDetail extends EntryRow {
  contentText: string | null;
  tags: Tag[];
}

export interface Journal {
  id: string;
  name: string;
  color: string | null;
  isLocked: boolean;
  isInvisible: boolean;
}

/** One row of Trash, with the visibility rules of `EntryRow`. */
export interface TrashRow {
  id: string;
  title: string | null;
  previewText: string | null;
  /** Unix seconds. */
  trashedAt: number;
  /** Locked by its own flag or its journal's. */
  isLocked: boolean;
  /** Invisible by its own flag or its journal's. */
  isInvisible: boolean;
}

/** Search filters; an empty list means no restriction. */
export interface SearchFilters {
  /** Unix seconds, inclusive; applied only together with `toExclusive`. */
  from?: number | null;
  /** Unix seconds, exclusive; applied only together with `from`. */
  toExclusive?: number | null;
  journalIds: string[];
  tagIds: string[];
  emotions: Emotion[];
}

/** One search hit (locked, invisible and trashed entries never match). */
export interface SearchResult {
  id: string;
  journalId: string;
  title: string | null;
  previewText: string | null;
  /** Unix seconds. */
  entryDate: number;
}

/** One saved version of an entry, without its document. */
export interface Version {
  id: string;
  /** Unix seconds. */
  createdAt: number;
  previewText: string;
}

export interface Template {
  id: string;
  name: string;
  description: string | null;
  contentHtml: string | null;
  /** Seeded by the app; cannot be edited or deleted. */
  isPredefined: boolean;
  sortOrder: number;
}

/** Spike 17.1: one file of the picked `MemloreSpike` folder. */
export interface SpikeICloudItem {
  name: string;
  /** Listed under the legacy `.name.icloud` placeholder name. */
  legacyPlaceholder: boolean;
  ubiquitous: boolean;
  /** `current` | `downloaded` | `notDownloaded` | `none` (not ubiquitous). */
  status: string;
  downloading: boolean;
  size: number;
  /** Bytes of a coordinated read; only for files already local. */
  bytesRead?: number;
}

export interface SpikeICloudListing {
  folder: string;
  wasStale: boolean;
  items: SpikeICloudItem[];
  /** Entries that are neither metadata.json nor *.bin (names never reported). */
  otherCount: number;
}

export interface SpikeICloudBookmarkStatus {
  hasBookmark: boolean;
  resolved?: boolean;
  wasStale?: boolean;
  accessGranted?: boolean;
  folder?: string;
  error?: string;
}

export interface SpikeICloudContainerProbe {
  identityToken: boolean;
  containerResolved: boolean;
  containerLeaf: string | null;
}

/** Spike 17.2: a PKCE pair (S256) and a `state` nonce. Secret: never log. */
export interface SpikeOAuthPkce {
  verifier: string;
  challenge: string;
  state: string;
}

/** Spike 17.2: the Keychain item holding the token JSON. */
export type SpikeOAuthKeychainItem =
  | { found: false }
  | {
      found: true;
      /** Secret: never log. */
      value: string;
      afterFirstUnlockThisDeviceOnly: boolean;
      accessGroup?: string;
      /** Unix milliseconds. */
      modifiedAt?: number;
    };

export interface ScreenCaptureChangeEvent {
  /** Recording, mirroring or AirPlay of the app's screen is on. */
  captured: boolean;
}

/** Save before lock: answer with `flushDone(id)` once the edits are saved. */
export interface FlushRequestEvent {
  id: number;
}

export type MemloreCoreEvents = {
  /**
   * The vault was locked after the auto-lock interval in the background.
   * Sent as the app returns to the foreground, before it is active; the
   * privacy shield stays up until `releasePrivacyShield()` (or a ~2 s safety
   * timeout). Never sent while the vault was already locked.
   */
  onAutoLock: () => void;
  onScreenCaptureChange: (event: ScreenCaptureChangeEvent) => void;
  /**
   * The app is leaving the foreground while the vault is unlocked: save every
   * pending edit, then call `flushDone(id)`. Native holds its background
   * lock for the matching ack, at most ~2 s; it never waits to show the
   * privacy shield.
   */
  onFlushRequest: (event: FlushRequestEvent) => void;
};

declare class MemloreCoreModule extends NativeModule<MemloreCoreEvents> {
  startupMode(): Promise<StartupMode>;
  /** Creates the vault, leaves it unlocked and resolves with the 24-word phrase. */
  createVault(password: string): Promise<string>;
  unlock(password: string): Promise<void>;
  lock(): Promise<void>;
  /** Answers while locked, and while a Face ID prompt is open. */
  biometricStatus(): Promise<BiometricStatus>;
  /** Whether Face ID/Touch ID unlocks this vault; answers while locked. */
  isBiometricUnlockEnabled(): Promise<boolean>;
  /**
   * Shows one Face ID/Touch ID prompt, then stores the unlock key. Rejects
   * with ERR_BIOMETRIC_CANCELLED, ERR_BIOMETRIC_DISABLED, or ERR_LOCKED while
   * the vault is locked.
   */
  enableBiometric(): Promise<void>;
  /** Idempotent. Rejects with ERR_LOCKED while the vault is locked. */
  disableBiometric(): Promise<void>;
  /**
   * Shows the Face ID/Touch ID prompt and unlocks. Rejects with
   * ERR_BIOMETRIC_CANCELLED or ERR_BIOMETRIC_DISABLED (use the password), and
   * with ERR_IO while already unlocked.
   */
  unlockWithBiometric(): Promise<void>;
  /** Rejects with ERR_LOCKED while the vault is locked. */
  autoLockSeconds(): Promise<AutoLockSeconds>;
  /** Rejects with ERR_LOCKED while locked, ERR_FORMAT for any other value. */
  setAutoLockSeconds(seconds: AutoLockSeconds): Promise<void>;
  /** Takes the privacy shield off after handling `onAutoLock`. */
  releasePrivacyShield(): Promise<void>;
  /** Answers `onFlushRequest`; an id other than the latest request is ignored. */
  flushDone(id: number): Promise<void>;
  /** Whether the screen is being recorded, mirrored or AirPlayed now. */
  isScreenCaptured(): Promise<boolean>;
  /** Rejects with ERR_LOCKED while the vault is locked. */
  entryCount(): Promise<number>;
  /**
   * Page `page` of the entry list (1-based; 0 reads as 1), newest first:
   * every listed journal, or only `journalId` (ERR_NOT_FOUND unless listed,
   * ERR_SECOND_LOCK_REQUIRED for a locked journal listed redacted).
   */
  listEntries(journalId: string | null, page: number): Promise<EntryPage>;
  /**
   * Rejects with ERR_NOT_FOUND for an entry the list does not show, and with
   * ERR_SECOND_LOCK_REQUIRED for a locked one listed redacted.
   */
  getEntry(id: string): Promise<EntryDetail>;
  /**
   * Creates an entry with an empty body in `journalId` (ERR_NOT_FOUND unless
   * listed, ERR_SECOND_LOCK_REQUIRED for a locked journal listed redacted),
   * or else in the first visible, unlocked journal; resolves with its id.
   */
  createEntry(title: string, journalId?: string | null): Promise<string>;
  /** Sets the title as typed; '' clears it. */
  setEntryTitle(id: string, title: string): Promise<void>;
  /** null clears the emotion. */
  setEntryEmotion(id: string, emotion: Emotion | null): Promise<void>;
  /** Resolves with the new value. */
  toggleEntryFavorite(id: string): Promise<boolean>;
  /** Unix seconds (a fraction is dropped); marks the date user-edited. */
  setEntryDate(id: string, entryDate: number): Promise<void>;
  /**
   * Both the entry and the journal must be listed (else ERR_NOT_FOUND) and
   * readable: a locked one listed redacted is ERR_SECOND_LOCK_REQUIRED.
   */
  moveEntry(id: string, journalId: string): Promise<void>;
  trashEntry(id: string): Promise<void>;
  /** Most recently trashed first. */
  listTrash(): Promise<TrashRow[]>;
  /**
   * ERR_NOT_FOUND unless `listTrash` shows the entry. A redacted locked row
   * (Covered) can be restored: nothing is lost.
   */
  restoreEntry(id: string): Promise<void>;
  /**
   * Deletes for good; ERR_NOT_FOUND unless `listTrash` shows the entry
   * readably. A redacted locked row (Covered) is ERR_NOT_FOUND until the
   * second lock is revealed.
   */
  purgeEntry(id: string): Promise<void>;
  /**
   * Deletes for good the confirmed `ids` that `listTrash` still shows
   * readably; any other id is skipped, a redacted locked row (Covered) too.
   * Resolves with how many were deleted.
   */
  emptyTrash(ids: string[]): Promise<number>;
  listJournals(): Promise<Journal[]>;
  createJournal(name: string): Promise<Journal>;
  /** ERR_SECOND_LOCK_REQUIRED for a locked journal listed redacted. */
  renameJournal(id: string, name: string): Promise<Journal>;
  /**
   * The delete confirmation's count (desktop's): live entries, locked ones
   * included. Trashed and invisible entries also go but are not counted.
   * ERR_SECOND_LOCK_REQUIRED for a locked journal listed redacted.
   */
  journalEntryCount(id: string): Promise<number>;
  /**
   * Deletes the journal and every entry in it, locked ones included
   * (irreversible). Rejects with ERR_LAST_JOURNAL for the last listed one,
   * ERR_SECOND_LOCK_REQUIRED for a locked journal listed redacted.
   */
  deleteJournal(id: string): Promise<void>;
  listTags(): Promise<Tag[]>;
  createTag(name: string): Promise<Tag>;
  /** Keeps the colour; '' is ERR_FORMAT, another tag's name ERR_NAME_TAKEN. */
  renameTag(id: string, name: string): Promise<Tag>;
  /** Also takes the tag off locked entries (desktop parity). */
  deleteTag(id: string): Promise<void>;
  addTagToEntry(entryId: string, tagId: string): Promise<void>;
  removeTagFromEntry(entryId: string, tagId: string): Promise<void>;
  /**
   * The entry's Yjs document (full state) as base64; '' when it has no body
   * yet. Rejects with ERR_NOT_FOUND for an entry the list does not show.
   */
  loadEntryYjs(id: string): Promise<string>;
  /**
   * The editor's save: the FULL document state (base64 of
   * `Y.encodeStateAsUpdate`), merged into the stored one like the desktop
   * save, plus the search text and the list preview.
   */
  saveEntryYjs(
    id: string,
    fullStateB64: string,
    contentText: string,
    previewText: string,
  ): Promise<void>;
  /**
   * At most 50 listed entries: a query ranks full-text matches; a blank query
   * with a filter lists the newest matching entries; a blank query without a
   * filter resolves with []. ERR_FORMAT for an unknown emotion or a
   * non-finite date (a fraction is dropped).
   */
  searchEntries(query: string, filters: SearchFilters): Promise<SearchResult[]>;
  /** Newest first; ERR_NOT_FOUND for an entry the list does not show. */
  listEntryVersions(entryId: string): Promise<Version[]>;
  /**
   * One version's Yjs document (full state) as base64; ERR_NOT_FOUND for a
   * version of another entry.
   */
  getEntryVersionContent(entryId: string, versionId: string): Promise<string>;
  /**
   * Keeps the entry's STORED document as a new version and prunes old ones
   * like the desktop; resolves with its id, or null when the text is blank.
   */
  snapshotEntryVersion(entryId: string): Promise<string | null>;
  /** Predefined templates first, then user templates. */
  listTemplates(): Promise<Template[]>;
  /** Creates an empty user template; '' is ERR_FORMAT. */
  createTemplate(name: string, description?: string | null): Promise<Template>;
  /**
   * Renames and redescribes a user template and clears its content (desktop
   * parity). ERR_NOT_FOUND for a predefined template.
   */
  updateTemplate(id: string, name: string, description?: string | null): Promise<Template>;
  /** ERR_NOT_FOUND for a predefined template. */
  deleteTemplate(id: string): Promise<void>;

  // Second lock and invisible vault. The session (reveal, open vault) lives
  // in Rust and ends on `lock()`. Every call but `lockSecondLock` and
  // `closeInvisibleVault` rejects with ERR_LOCKED while the vault is locked.
  // Never log the passwords these take.
  lockState(): Promise<LockState>;
  /** Sets the first password (ERR_FORMAT when too short) and reveals. */
  setSecondLockPassword(password: string): Promise<void>;
  /** Resolves true and reveals for the right password; false changes nothing. */
  unlockSecondLock(password: string): Promise<boolean>;
  /** Ends the reveal. */
  lockSecondLock(): Promise<void>;
  /** ERR_WRONG_PASSWORD for a wrong old one, ERR_FORMAT for a short new one. */
  changeSecondLockPassword(oldPassword: string, newPassword: string): Promise<void>;
  /**
   * Removes the password and every lock flag, then ends the session (the
   * reveal and the open vault). ERR_WRONG_PASSWORD for a wrong password.
   */
  disableSecondLock(password: string): Promise<void>;
  /**
   * List locked rows redacted (true) or leave them out (false). With the
   * second lock set, turning it on needs the reveal (else
   * ERR_SECOND_LOCK_REQUIRED); turning it off is always allowed.
   */
  setSecondLockShowExistence(show: boolean): Promise<void>;
  /**
   * ERR_FORMAT for any other value. With the second lock set and not
   * revealed it may only shorten (0, never, counts as the longest): a longer
   * value or 0 is ERR_SECOND_LOCK_REQUIRED.
   */
  setSecondLockAutoLockMinutes(minutes: RelockMinutes): Promise<void>;
  /**
   * ERR_FORMAT for any other value. With no vault open it may only shorten
   * (0, never, counts as the longest): a longer value or 0 is ERR_IO.
   */
  setInvisibleAutoLockMinutes(minutes: RelockMinutes): Promise<void>;
  /**
   * Unlocking a locked entry needs the reveal (ERR_SECOND_LOCK_REQUIRED or
   * ERR_NOT_FOUND otherwise). ERR_IO keeps the engine's text (second lock not
   * set, entry in Trash).
   */
  setEntryLocked(entryId: string, locked: boolean): Promise<void>;
  /** The rules of `setEntryLocked`, for a listed journal. */
  setJournalLocked(journalId: string, locked: boolean): Promise<void>;
  /**
   * Opens the vault this password opens, or creates an empty one: the same
   * result either way. ERR_FORMAT for a short password, else ERR_IO.
   */
  openInvisibleVault(password: string): Promise<void>;
  closeInvisibleVault(): Promise<void>;
  /**
   * Changes the open vault's password (merging into the vault the new one
   * opens, if any). ERR_FORMAT for a short new password; every other refusal
   * is ERR_IO.
   */
  changeInvisibleVaultPassword(oldPassword: string, newPassword: string): Promise<void>;
  /** Deletes every empty vault and closes the open one if it was removed. */
  removeEmptyInvisibleVaults(): Promise<void>;
  /**
   * Moves an entry into the open vault (ERR_IO with no vault open) or out of
   * it; clears its lock flag.
   */
  setEntryInvisible(entryId: string, invisible: boolean): Promise<void>;
  /** The rules of `setEntryInvisible`, for a listed journal and its entries. */
  setJournalInvisible(journalId: string, invisible: boolean): Promise<void>;

  // Spike 17.1 (dev route `app/(dev)/spike-icloud.tsx` only, read-only; see
  // ios/ICloudSpike.swift). Reject with code ERR_SPIKE.
  // TODO(later): see docs/LATER.md (remove with the dev spike routes).
  /** Presents the folder picker; resolves with the folder name once bookmarked. */
  spikeICloudPickFolder(): Promise<string>;
  spikeICloudBookmarkStatus(): Promise<SpikeICloudBookmarkStatus>;
  spikeICloudForgetBookmark(): Promise<void>;
  spikeICloudList(): Promise<SpikeICloudListing>;
  spikeICloudStartDownloads(): Promise<{ started: string[]; failed: string[] }>;
  spikeICloudContainerProbe(): Promise<SpikeICloudContainerProbe>;
  spikeICloudScheduleBackgroundTask(): Promise<{ submitted: string }>;
  spikeICloudListLaterInBackground(delaySeconds: number): Promise<void>;
  /** JSON of the last background listing, or null. */
  spikeICloudBackgroundResult(): Promise<string | null>;

  // Spike 17.2 (dev route `app/(dev)/spike-oauth.tsx` only; see
  // ios/OAuthSpike.swift). Never log what these pass or return. Reject with
  // code ERR_SPIKE.
  // TODO(later): see docs/LATER.md (remove with the dev spike routes).
  spikeOAuthPkce(): Promise<SpikeOAuthPkce>;
  /** Writes the one Keychain item (AfterFirstUnlockThisDeviceOnly). */
  spikeOAuthKeychainWrite(value: string): Promise<void>;
  spikeOAuthKeychainRead(): Promise<SpikeOAuthKeychainItem>;
  /** Resolves false when there was no item. */
  spikeOAuthKeychainDelete(): Promise<boolean>;
}

const MemloreCore = requireNativeModule<MemloreCoreModule>('MemloreCore');

export default MemloreCore;

import { NativeModule, requireNativeModule } from 'expo';

/** What the vault on this device needs at launch. */
export type StartupMode = 'first_launch' | 'password_locked' | 'boot_file_corrupt';

/**
 * `code` of the errors the native module rejects with.
 * ERR_BIOMETRIC_DISABLED: Face ID/Touch ID cannot be used now (not enrolled,
 * locked out, or the unlock found it broken and turned it off); use the
 * password. ERR_BIOMETRIC_CANCELLED: the user dismissed or failed the
 * prompt; nothing changed. ERR_NOT_FOUND: the entry, journal or tag does
 * not exist or is not listed on mobile (deleted, locked or invisible).
 */
export type MemloreErrorCode =
  | 'ERR_LOCKED'
  | 'ERR_WRONG_PASSWORD'
  | 'ERR_IO'
  | 'ERR_FORMAT'
  | 'ERR_BIOMETRIC_DISABLED'
  | 'ERR_BIOMETRIC_CANCELLED'
  | 'ERR_NOT_FOUND';

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

/** The engine only stores these three; null is no emotion. */
export type Emotion = 'good' | 'neutral' | 'bad';

/** One row of the entry list (locked and invisible entries are never listed). */
export interface EntryRow {
  id: string;
  title: string | null;
  previewText: string | null;
  /** Unix seconds. */
  entryDate: number;
  emotion: Emotion | null;
  isFavorite: boolean;
  journalId: string;
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
}

/** One row of Trash (locked and invisible entries are never listed). */
export interface TrashRow {
  id: string;
  title: string | null;
  previewText: string | null;
  /** Unix seconds. */
  trashedAt: number;
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

export type MemloreCoreEvents = {
  /**
   * The vault was locked after the auto-lock interval in the background.
   * Sent as the app returns to the foreground, before it is active; the
   * privacy shield stays up until `releasePrivacyShield()` (or a ~2 s safety
   * timeout). Never sent while the vault was already locked.
   */
  onAutoLock: () => void;
  onScreenCaptureChange: (event: ScreenCaptureChangeEvent) => void;
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
  /** Whether the screen is being recorded, mirrored or AirPlayed now. */
  isScreenCaptured(): Promise<boolean>;
  /** Rejects with ERR_LOCKED while the vault is locked. */
  entryCount(): Promise<number>;
  /**
   * Page `page` of the entry list (1-based; 0 reads as 1), newest first:
   * every listed journal, or only `journalId` (ERR_NOT_FOUND unless listed).
   */
  listEntries(journalId: string | null, page: number): Promise<EntryPage>;
  /** Rejects with ERR_NOT_FOUND for an entry the list does not show. */
  getEntry(id: string): Promise<EntryDetail>;
  /**
   * Creates an entry with an empty body in `journalId` (ERR_NOT_FOUND unless
   * listed), or else in the first visible, unlocked journal; resolves with
   * its id.
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
  /** Both the entry and the journal must be listed (else ERR_NOT_FOUND). */
  moveEntry(id: string, journalId: string): Promise<void>;
  trashEntry(id: string): Promise<void>;
  /** Most recently trashed first. */
  listTrash(): Promise<TrashRow[]>;
  /** ERR_NOT_FOUND unless `listTrash` shows the entry. */
  restoreEntry(id: string): Promise<void>;
  /** Deletes for good; ERR_NOT_FOUND unless `listTrash` shows the entry. */
  purgeEntry(id: string): Promise<void>;
  /**
   * Deletes for good the confirmed `ids` that `listTrash` still shows; any
   * other id is skipped. Resolves with how many were deleted.
   */
  emptyTrash(ids: string[]): Promise<number>;
  listJournals(): Promise<Journal[]>;
  createJournal(name: string): Promise<Journal>;
  renameJournal(id: string, name: string): Promise<Journal>;
  listTags(): Promise<Tag[]>;
  createTag(name: string): Promise<Tag>;
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

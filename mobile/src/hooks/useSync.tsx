import * as WebBrowser from 'expo-web-browser';
import { createContext, type ReactNode, useContext, useEffect, useRef, useState } from 'react';
import { AppState } from 'react-native';

import MemloreCore, {
  type ConnectOutcome,
  type DriveStatus,
  type SyncProgressEvent,
  type SyncStatus,
} from '@/modules/memlore-core';
import { markEntryListsDirty } from '@/src/hooks/useEntries';
import { errorCode, useVault } from '@/src/hooks/useVault';

/**
 * A nil field of a native Record may arrive missing rather than null: every
 * optional field reads null, whatever the bridge sent.
 */
function coerceStatus(raw: SyncStatus): SyncStatus {
  return {
    phase: raw.phase,
    running: raw.running,
    blocked: raw.blocked ?? null,
    error: raw.error ?? null,
    lastSyncMs: raw.lastSyncMs ?? null,
    pendingCount: raw.pendingCount,
  };
}

function coerceDriveStatus(raw: DriveStatus): DriveStatus {
  return {
    connected: raw.connected,
    email: raw.email ?? null,
    lastSyncMs: raw.lastSyncMs ?? null,
    pendingCount: raw.pendingCount,
    blocked: raw.blocked ?? null,
  };
}

// The engine's busy refusal (`SYNC_IN_PROGRESS_ERR`) and its "Drive not
// configured" message, both ERR_IO. Expo wraps native messages, so they are
// matched inside the message, never compared whole.
const SYNC_BUSY_MESSAGE = 'sync_in_progress';
const NOT_CONFIGURED_MESSAGE = 'not configured';

/** How long a connect waits for a running sync before its one retry. */
const SYNC_BUSY_WAIT_MS = 10_000;
const SYNC_BUSY_POLL_MS = 500;

/**
 * How long the lists wait for the first sync after onboarding. A first sync
 * the scheduler declined never reports an end, so the placeholder gives up.
 */
const FIRST_SYNC_WAIT_MS = 60_000;

function errorMessageOf(e: unknown): string {
  if (typeof e !== 'object' || e === null || !('message' in e)) return '';
  return typeof e.message === 'string' ? e.message : '';
}

/** ERR_IO because another sync (or sync task) holds the engine's sync guard. */
export function isSyncBusy(e: unknown): boolean {
  return errorCode(e) === 'ERR_IO' && errorMessageOf(e).includes(SYNC_BUSY_MESSAGE);
}

/** ERR_IO because this build has no Google client ID (Drive is not configured). */
export function isDriveNotConfigured(e: unknown): boolean {
  return errorCode(e) === 'ERR_IO' && errorMessageOf(e).includes(NOT_CONFIGURED_MESSAGE);
}

/** A sync has run to its end (synced or failed); `idle` is a vault that never synced. */
function syncEnded(status: SyncStatus): boolean {
  return !status.running && status.phase !== 'idle';
}

const sleep = (ms: number) => new Promise<void>((resolve) => setTimeout(resolve, ms));

/**
 * Waits (bounded) until no sync runs. A status that cannot be read (locked,
 * first launch: no sync can run) ends the wait.
 */
async function waitForSyncEnd(): Promise<void> {
  const deadline = Date.now() + SYNC_BUSY_WAIT_MS;
  while (Date.now() < deadline) {
    await sleep(SYNC_BUSY_POLL_MS);
    try {
      if (!(await MemloreCore.syncStatus()).running) return;
    } catch {
      return;
    }
  }
}

/**
 * Finishes a sign-in. A foreground tick may have started a sync as the auth
 * session returned: the engine then refuses before it uses the one-time
 * sign-in, so it is tried once more after that sync ends.
 */
async function completeConnect(sessionId: string, callbackUrl: string): Promise<ConnectOutcome> {
  try {
    return await MemloreCore.driveConnectComplete(sessionId, callbackUrl);
  } catch (e) {
    if (!isSyncBusy(e)) throw e;
    await waitForSyncEnd();
    return MemloreCore.driveConnectComplete(sessionId, callbackUrl);
  }
}

// __DEV__ only: the first `onSyncStatus` body's shape (keys and value types,
// never values) is logged once per JS load.
let statusShapeLogged = false;

function logStatusShape(body: object) {
  if (!__DEV__ || statusShapeLogged) return;
  statusShapeLogged = true;
  const shape = Object.fromEntries(
    Object.entries(body).map(([key, value]) => [key, value === null ? 'null' : typeof value]),
  );
  console.log('[useSync] onSyncStatus shape', shape);
}

/**
 * Google Drive and sync, once for the whole app: mount it inside
 * `VaultProvider` and outside the unlocked guard (onboarding signs in at first
 * launch). Components read it through `useSync()`, never the native module.
 */
export function SyncProvider({ children }: { children: ReactNode }) {
  const sync = useSyncState();
  return <SyncContext.Provider value={sync}>{children}</SyncContext.Provider>;
}

export type Sync = ReturnType<typeof useSyncState>;

const SyncContext = createContext<Sync | null>(null);

/** The app-wide sync state; only under `SyncProvider`. */
export function useSync(): Sync {
  const sync = useContext(SyncContext);
  if (sync === null) throw new Error('useSync() needs a SyncProvider above it');
  return sync;
}

/**
 * The provider's state. `status` follows `onSyncStatus` and is read with
 * `syncStatus()` whenever the vault is unlocked and the app becomes active;
 * null while locked or not read yet. `progress` is the running sync's last
 * step, null once it ended. A sync that pulls entries bumps the entry lists
 * (`markEntryListsDirty`), so they read again.
 *
 * Actions reject with the module's error (`errorCode(e)`: ERR_WRONG_PHRASE,
 * ERR_VAULT_NOT_EMPTY, ERR_WRONG_PASSWORD, ERR_IO…); ERR_LOCKED while the app
 * shows the vault unlocked also shows the lock screen. The callback URL, the
 * phrase and passwords are passed through, never stored or logged.
 */
function useSyncState() {
  const vault = useVault();
  const unlocked = vault.status === 'unlocked';
  const [status, setStatus] = useState<SyncStatus | null>(null);
  const [progress, setProgress] = useState<SyncProgressEvent | null>(null);
  const [appActive, setAppActive] = useState(AppState.currentState === 'active');
  // Bumped by every status event and lock: an older `syncStatus()` answer is dropped.
  const latest = useRef(0);
  // The live vault status, for actions that settle after a render.
  const unlockedRef = useRef(unlocked);
  unlockedRef.current = unlocked;
  const connecting = useRef(false);
  // Onboarding joined a vault whose entries the first sync has not pulled
  // yet: the lists show their placeholder, not "no entries", until it ends,
  // the vault locks or `FIRST_SYNC_WAIT_MS` passed.
  const [awaitingFirstSync, setAwaitingFirstSync] = useState(false);

  useEffect(() => {
    if (!awaitingFirstSync) return;
    const timer = setTimeout(() => setAwaitingFirstSync(false), FIRST_SYNC_WAIT_MS);
    return () => clearTimeout(timer);
  }, [awaitingFirstSync]);

  useEffect(() => {
    const appState = AppState.addEventListener('change', (state) => {
      setAppActive(state === 'active');
    });
    const statusSub = MemloreCore.addListener('onSyncStatus', (body) => {
      logStatusShape(body);
      latest.current += 1;
      // A cancelled sync reports after the lock: the unlock reads it again.
      if (!unlockedRef.current) return;
      const next = coerceStatus(body);
      setStatus(next);
      if (!next.running) setProgress(null);
      if (syncEnded(next)) setAwaitingFirstSync(false);
    });
    const progressSub = MemloreCore.addListener('onSyncProgress', (event) => {
      setProgress({ step: event.step, done: event.done, total: event.total });
    });
    // The only subscription: one bump, however many lists are mounted.
    const entriesSub = MemloreCore.addListener('onEntriesChanged', () => {
      markEntryListsDirty();
    });
    return () => {
      appState.remove();
      statusSub.remove();
      progressSub.remove();
      entriesSub.remove();
    };
  }, []);

  /** ERR_LOCKED while shown unlocked: the vault locked natively. */
  function noteError(e: unknown) {
    if (errorCode(e) === 'ERR_LOCKED' && unlockedRef.current) vault.markLocked();
  }

  /** Re-reads `syncStatus()`. Never rejects; a failure keeps the last status. */
  async function refresh(): Promise<void> {
    const seq = ++latest.current;
    try {
      const next = coerceStatus(await MemloreCore.syncStatus());
      if (seq !== latest.current || !unlockedRef.current) return;
      setStatus(next);
      if (!next.running) setProgress(null);
      if (syncEnded(next)) setAwaitingFirstSync(false);
    } catch (e) {
      noteError(e);
    }
  }

  // On unlock and on every return to the foreground; cleared on lock (no
  // event reports a locked vault).
  useEffect(() => {
    if (!unlocked) {
      latest.current += 1;
      setStatus(null);
      setProgress(null);
      setAwaitingFirstSync(false);
      return;
    }
    if (appActive) void refresh();
  }, [unlocked, appActive]);

  /** Runs one module call; ERR_LOCKED also shows the lock screen. */
  async function call<T>(action: () => Promise<T>): Promise<T> {
    try {
      return await action();
    } catch (e) {
      noteError(e);
      throw e;
    }
  }

  /**
   * Google sign-in: opens the auth session, then hands its one-time callback
   * URL to Rust (never kept or logged here). Resolves with what Drive held,
   * or null when the user cancelled (or a sign-in is already running).
   * Also the first step of onboarding at first launch.
   */
  async function connect(): Promise<ConnectOutcome | null> {
    if (connecting.current) return null;
    connecting.current = true;
    try {
      return await call(async () => {
        const { sessionId, authUrl, redirectUri } = await MemloreCore.driveConnectBegin();
        let result: WebBrowser.WebBrowserAuthSessionResult;
        try {
          // Ephemeral: no Safari cookies in or out (the OAuth spike's choice).
          result = await WebBrowser.openAuthSessionAsync(authUrl, redirectUri, {
            preferEphemeralSession: true,
          });
        } catch (e) {
          await MemloreCore.driveConnectCancel(sessionId).catch(() => {});
          throw e;
        }
        if (result.type !== 'success') {
          await MemloreCore.driveConnectCancel(sessionId);
          return null;
        }
        const outcome = await completeConnect(sessionId, result.url);
        if (unlockedRef.current) void refresh();
        return outcome;
      });
    } finally {
      connecting.current = false;
    }
  }

  /** A manual sync ("Sync now", pull-to-refresh): false when one is already running. */
  const syncNow = () => call(() => MemloreCore.syncNow(true));

  /** Forgets this phone's tokens and Drive settings (never revokes at Google). */
  const disconnect = () =>
    call(async () => {
      await MemloreCore.driveDisconnect();
      void refresh();
    });

  const driveStatus = () => call(async () => coerceDriveStatus(await MemloreCore.driveStatus()));

  const onboardValidatePhrase = (sessionId: string, phrase: string) =>
    call(() => MemloreCore.onboardValidatePhrase(sessionId, phrase));

  /** Joins the cloud vault at first launch; the app then shows the unlocked vault. */
  const onboardComplete = (sessionId: string, phrase: string, password: string) =>
    call(async () => {
      await MemloreCore.onboardComplete(sessionId, phrase, password);
      setAwaitingFirstSync(true);
      vault.markOnboarded();
    });

  /** Drops an onboarding session the user left (it holds the Drive token); never rejects. */
  const cancelOnboarding = (sessionId: string) =>
    MemloreCore.driveConnectCancel(sessionId).catch(() => {});

  const rePair = (phrase: string, password: string) =>
    call(async () => {
      await MemloreCore.rePair(phrase, password);
      void refresh();
      // The re-pair turned the Face ID unlock off (the vault key changed).
      void vault.refreshBiometric();
    });

  /** True when the re-pair block was cleared. */
  const recheckRePair = () =>
    call(async () => {
      const cleared = await MemloreCore.recheckRePair();
      void refresh();
      return cleared;
    });

  /**
   * Deletes this phone's vault (password-gated), then the app shows the
   * first-launch screen. Any other failure than a wrong password may have
   * locked the vault natively: the vault state is read again.
   */
  const deleteVault = (password: string) =>
    call(async () => {
      try {
        await MemloreCore.deleteVault(password);
      } catch (e) {
        if (errorCode(e) !== 'ERR_WRONG_PASSWORD') void vault.resyncVaultState();
        throw e;
      }
      latest.current += 1;
      setStatus(null);
      setProgress(null);
      setAwaitingFirstSync(false);
      vault.markDeleted();
    });

  return {
    status,
    progress,
    /**
     * The first sync after onboarding has not ended yet: an empty list shows
     * its placeholder, not "no entries". Any later sync reads `status.running`.
     */
    awaitingFirstSync,
    refresh,
    connect,
    syncNow,
    disconnect,
    driveStatus,
    onboardValidatePhrase,
    onboardComplete,
    cancelOnboarding,
    rePair,
    recheckRePair,
    deleteVault,
  };
}

/**
 * How long a pull shows its spinner at the least. iOS leaves the native
 * spinner stuck when `refreshing` turns back off while the pull is still
 * settling (a pull with no Drive answers in a few milliseconds).
 */
const MIN_REFRESH_MS = 800;

/**
 * Pull-to-refresh on a list: a manual sync, shown refreshing until it ends
 * (the next status with `running: false`, synced or failed). It ends at once
 * (after `MIN_REFRESH_MS`) when Drive is not connected, no sync started (one
 * already runs) or a call failed. The pull shows the spinner in the same frame.
 */
export function useSyncRefresh(): { refreshing: boolean; onRefresh: () => void } {
  const sync = useSync();
  const [refreshing, setRefreshing] = useState(false);
  // The live status, and the one seen when this pull's sync started: only a
  // later status can end it.
  const statusRef = useRef(sync.status);
  statusRef.current = sync.status;
  const startedAt = useRef<SyncStatus | null | undefined>(undefined);
  const pulledAtMs = useRef(0);
  const endTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(
    () => () => {
      if (endTimer.current !== null) clearTimeout(endTimer.current);
    },
    [],
  );

  function end() {
    startedAt.current = undefined;
    if (endTimer.current !== null) return;
    const wait = Math.max(0, pulledAtMs.current + MIN_REFRESH_MS - Date.now());
    endTimer.current = setTimeout(() => {
      endTimer.current = null;
      setRefreshing(false);
    }, wait);
  }

  useEffect(() => {
    if (!refreshing || startedAt.current === undefined) return;
    if (sync.status === startedAt.current || sync.status === null || sync.status.running) return;
    end();
  }, [refreshing, sync.status]);

  async function pull() {
    try {
      if (!(await sync.driveStatus()).connected) return end();
      // Before the call: a sync with nothing to do may report its end before
      // `syncNow` resolves.
      startedAt.current = statusRef.current;
      if (await sync.syncNow()) return;
    } catch {
      // ERR_LOCKED shows the lock screen; anything else just ends the pull.
    }
    end();
  }

  function onRefresh() {
    if (refreshing) return;
    pulledAtMs.current = Date.now();
    setRefreshing(true);
    void pull();
  }

  return { refreshing, onRefresh };
}

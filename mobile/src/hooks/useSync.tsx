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
        const outcome = await MemloreCore.driveConnectComplete(sessionId, result.url);
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

  /** Joins the cloud vault at first launch; the vault is unlocked afterwards. */
  const onboardComplete = (sessionId: string, phrase: string, password: string) =>
    call(() => MemloreCore.onboardComplete(sessionId, phrase, password));

  const rePair = (phrase: string, password: string) =>
    call(async () => {
      await MemloreCore.rePair(phrase, password);
      void refresh();
    });

  /** True when the re-pair block was cleared. */
  const recheckRePair = () =>
    call(async () => {
      const cleared = await MemloreCore.recheckRePair();
      void refresh();
      return cleared;
    });

  /** Deletes this phone's vault (password-gated); `startupMode()` then reads `first_launch`. */
  const deleteVault = (password: string) =>
    call(async () => {
      await MemloreCore.deleteVault(password);
      latest.current += 1;
      setStatus(null);
      setProgress(null);
    });

  return {
    status,
    progress,
    refresh,
    connect,
    syncNow,
    disconnect,
    driveStatus,
    onboardValidatePhrase,
    onboardComplete,
    rePair,
    recheckRePair,
    deleteVault,
  };
}

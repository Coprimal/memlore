import { createContext, type ReactNode, useContext, useEffect, useRef, useState } from 'react';
import { AppState } from 'react-native';

import MemloreCore, { type LockState, type RelockMinutes } from '@/modules/memlore-core';
import { errorCode, useVault } from '@/src/hooks/useVault';
import { flushAll } from '@/src/lib/flushRegistry';
import { dropCachedRows } from '@/src/lib/rowCache';

/** How long a re-lock waits for the open editor's saves (the manual vault lock's). */
const RELOCK_FLUSH_MS = 2000;

/** Until the first `lockState()` answers: nothing revealed, nothing open. */
const CLOSED: LockState = {
  enabled: false,
  revealed: false,
  showExistence: false,
  autoLockMinutes: 5,
  invisibleAutoLockMinutes: 5,
  vaultOpen: false,
};

// The last user activity, for the inactivity re-lock. Module-level so a touch
// or an editor change marks it without a render.
let lastActivity = Date.now();

/**
 * The user did something: restarts the second lock's and the invisible vault's
 * idle countdowns. The root layout calls it on every touch; the editor can call
 * it on its changes (the root touch capture may not see touches over the
 * editor's WebView).
 */
export function noteActivity(): void {
  lastActivity = Date.now();
}

/**
 * The second-lock session and its settings, read from Rust (which holds the
 * session and gates every call). Mount it inside the unlocked guard: a vault
 * lock unmounts it, which is its reset. Components read it through
 * `useLockSession()`, never the native module.
 */
export function LockSessionProvider({ children }: { children: ReactNode }) {
  const session = useLockSessionState();
  return <LockSessionContext.Provider value={session}>{children}</LockSessionContext.Provider>;
}

export type LockSession = ReturnType<typeof useLockSessionState>;

const LockSessionContext = createContext<LockSession | null>(null);

/** The app-wide lock session; only under `LockSessionProvider`. */
export function useLockSession(): LockSession {
  const session = useContext(LockSessionContext);
  if (session === null) throw new Error('useLockSession() needs a LockSessionProvider above it');
  return session;
}

/**
 * The provider's state. `generation` changes whenever `revealed`,
 * `vaultOpen`, `enabled` or `showExistence` does (the lock view changed):
 * lists refetch on it, an open locked entry pops.
 * Actions reject with the module's error (`errorCode(e)`: ERR_WRONG_PASSWORD,
 * ERR_FORMAT, ERR_SECOND_LOCK_REQUIRED…); ERR_LOCKED also shows the lock
 * screen. Passwords are passed through, never stored or logged.
 */
function useLockSessionState() {
  const vault = useVault();
  const [state, setState] = useState<LockState>(CLOSED);
  const [generation, setGeneration] = useState(0);
  // The first `lockState()` answered.
  const [loaded, setLoaded] = useState(false);
  // The state last applied, to compare without a stale closure.
  const current = useRef<LockState>(CLOSED);
  // Bumped by each read and local change: an older answer is dropped.
  const latest = useRef(0);
  const alive = useRef(true);

  function apply(next: LockState) {
    const prev = current.current;
    current.current = next;
    // `enabled` and `showExistence` pick the engine's view too (Hidden or
    // Covered): a sync that pulled a peer's change must refetch the lists.
    // Not on the first read: the lists already read in the engine's view, and
    // a refetch then would blank them once at every unlock.
    const viewChanged =
      prev !== CLOSED &&
      (prev.enabled !== next.enabled || prev.showExistence !== next.showExistence);
    if (prev.revealed !== next.revealed || prev.vaultOpen !== next.vaultOpen || viewChanged) {
      setGeneration((g) => g + 1);
    }
    setState(next);
  }

  /** A local change shown at once; any read in flight is dropped. */
  function applyLocal(patch: Partial<LockState>) {
    latest.current += 1;
    apply({ ...current.current, ...patch });
  }

  /**
   * Re-reads `lockState()`. Never rejects: ERR_LOCKED shows the lock screen,
   * any other failure keeps the last state.
   */
  async function refresh(): Promise<void> {
    const seq = ++latest.current;
    try {
      const next = await MemloreCore.lockState();
      if (!alive.current || seq !== latest.current) return;
      apply(next);
      setLoaded(true);
    } catch (e) {
      if (alive.current && errorCode(e) === 'ERR_LOCKED') vault.markLocked();
    }
  }

  /**
   * The reveal or the open vault ended (natively, by a re-lock or by
   * disabling): shown closed at once (fail closed), rows read while revealed
   * are dropped, then the state is read again. Native sends this on every
   * background, so the warm-start cache is kept when nothing was revealed or
   * open (it never holds a locked or invisible row anyway).
   */
  function endSession(): Promise<void> {
    if (current.current.revealed || current.current.vaultOpen) dropCachedRows();
    applyLocal({ revealed: false, vaultOpen: false });
    return refresh();
  }

  /**
   * The open vault ended (closed here or by a removal): shown closed at once,
   * rows read while it was open are dropped, then the state is read again.
   * The reveal is left alone: closing the vault does not end it natively.
   */
  function endVault(): Promise<void> {
    if (current.current.vaultOpen) dropCachedRows();
    applyLocal({ vaultOpen: false });
    return refresh();
  }

  /**
   * The reveal ended here (the re-lock timer, Lock now): shown at once, rows
   * read while revealed are dropped, then the state is read again. The open
   * vault is left alone: `lockSecondLock` does not close it natively, so it
   * must not flicker closed (and refetch every list twice) on the way.
   */
  function endReveal(): Promise<void> {
    if (current.current.revealed) dropCachedRows();
    applyLocal({ revealed: false });
    return refresh();
  }

  /** Runs a call; ERR_LOCKED shows the lock screen. Rejects with any error. */
  async function call<T>(action: () => Promise<T>): Promise<T> {
    try {
      return await action();
    } catch (e) {
      if (errorCode(e) === 'ERR_LOCKED') vault.markLocked();
      throw e;
    }
  }

  /**
   * Ends the reveal: the open editor's saves first (bounded), so the last edit
   * of a revealed entry is not refused by the gate. Never rejects.
   */
  async function relock(): Promise<void> {
    await flushAll(RELOCK_FLUSH_MS);
    try {
      // Infallible natively, even while the vault is locked.
      await MemloreCore.lockSecondLock();
    } catch {
      // Shown closed anyway; the refresh reads the real state.
    }
    await endReveal();
  }

  /**
   * Closes the invisible vault: the open editor's saves first (bounded), so
   * the last edit of an invisible entry is not refused. Never rejects.
   */
  async function closeVault(): Promise<void> {
    await flushAll(RELOCK_FLUSH_MS);
    try {
      // Infallible natively, even while the vault is locked.
      await MemloreCore.closeInvisibleVault();
    } catch {
      // Shown closed anyway; the refresh reads the real state.
    }
    await endVault();
  }

  useEffect(() => {
    alive.current = true;
    void refresh();
    const appState = AppState.addEventListener('change', (next) => {
      if (next === 'active') void refresh();
    });
    // Arrives before or after `onAutoLock`; a refresh that meets the locked
    // vault shows the lock screen, which unmounts this provider.
    const ended = MemloreCore.addListener('onLockSessionEnded', () => {
      void endSession();
    });
    // A sync may have pulled the lock settings (a peer's password, show
    // existence, minutes): the first read after onboarding predates them.
    const synced = MemloreCore.addListener('onSyncStatus', (body) => {
      if (!body.running) void refresh();
    });
    return () => {
      alive.current = false;
      appState.remove();
      ended.remove();
      synced.remove();
    };
  }, []);

  // The inactivity re-lock, JS only, while revealed; 0 (never) disables it.
  // A touch only moves `lastActivity`: the timeout checks it when it fires and
  // waits out the rest. Restarts on a new interval or a new reveal.
  useEffect(() => {
    if (!state.revealed || state.autoLockMinutes === 0) return;
    const interval = state.autoLockMinutes * 60_000;
    noteActivity();
    let timer: ReturnType<typeof setTimeout>;
    const check = () => {
      const idle = Date.now() - lastActivity;
      if (idle >= interval) void relock();
      else timer = setTimeout(check, interval - idle);
    };
    timer = setTimeout(check, interval);
    return () => clearTimeout(timer);
  }, [state.revealed, state.autoLockMinutes]);

  // The invisible vault's inactivity close, the same way, while it is open.
  useEffect(() => {
    if (!state.vaultOpen || state.invisibleAutoLockMinutes === 0) return;
    const interval = state.invisibleAutoLockMinutes * 60_000;
    noteActivity();
    let timer: ReturnType<typeof setTimeout>;
    const check = () => {
      const idle = Date.now() - lastActivity;
      if (idle >= interval) void closeVault();
      else timer = setTimeout(check, interval - idle);
    };
    timer = setTimeout(check, interval);
    return () => clearTimeout(timer);
  }, [state.vaultOpen, state.invisibleAutoLockMinutes]);

  /** Resolves true and reveals for the right password; false changes nothing. */
  async function reveal(password: string): Promise<boolean> {
    const ok = await call(() => MemloreCore.unlockSecondLock(password));
    await refresh();
    return ok;
  }

  /** Sets the first password; it also reveals. */
  async function setPassword(password: string): Promise<void> {
    await call(() => MemloreCore.setSecondLockPassword(password));
    await refresh();
  }

  async function changePassword(oldPassword: string, newPassword: string): Promise<void> {
    await call(() => MemloreCore.changeSecondLockPassword(oldPassword, newPassword));
    await refresh();
  }

  /** Removes the password and every lock flag, and ends the session. */
  async function disable(password: string): Promise<void> {
    await call(() => MemloreCore.disableSecondLock(password));
    await endSession();
  }

  /** Shown at once; the read after the call puts back what was stored. */
  async function setShowExistence(show: boolean): Promise<void> {
    applyLocal({ showExistence: show });
    try {
      await call(() => MemloreCore.setSecondLockShowExistence(show));
    } finally {
      await refresh();
    }
  }

  /** Shown at once; the read after the call puts back what was stored. */
  async function setAutoLockMinutes(minutes: RelockMinutes): Promise<void> {
    applyLocal({ autoLockMinutes: minutes });
    try {
      await call(() => MemloreCore.setSecondLockAutoLockMinutes(minutes));
    } finally {
      await refresh();
    }
  }

  /**
   * Opens the vault this password opens, or an empty new one: the same
   * answer either way. Rejects with ERR_FORMAT or ERR_IO (never says why).
   */
  async function openVault(password: string): Promise<void> {
    await call(() => MemloreCore.openInvisibleVault(password));
    await refresh();
  }

  /**
   * Re-keys the open vault, merging it into the vault the new password opens.
   * A merge keeps `vaultOpen` true but shows other rows: lists refetch.
   */
  async function changeVaultPassword(oldPassword: string, newPassword: string): Promise<void> {
    await call(() => MemloreCore.changeInvisibleVaultPassword(oldPassword, newPassword));
    setGeneration((g) => g + 1);
    await refresh();
  }

  /** Deletes every empty vault; the open one closes natively if it was removed. */
  async function removeEmptyVaults(): Promise<void> {
    await call(() => MemloreCore.removeEmptyInvisibleVaults());
    await refresh();
  }

  /**
   * Shown at once; the read after the call puts back what was stored. With no
   * vault open only a shorter interval is accepted (ERR_IO otherwise).
   */
  async function setInvisibleAutoLockMinutes(minutes: RelockMinutes): Promise<void> {
    applyLocal({ invisibleAutoLockMinutes: minutes });
    try {
      await call(() => MemloreCore.setInvisibleAutoLockMinutes(minutes));
    } finally {
      await refresh();
    }
  }

  return {
    ...state,
    generation,
    loaded,
    refresh,
    reveal,
    lockNow: relock,
    setPassword,
    changePassword,
    disable,
    setShowExistence,
    setAutoLockMinutes,
    openVault,
    closeVault,
    changeVaultPassword,
    removeEmptyVaults,
    setInvisibleAutoLockMinutes,
  };
}

import { createContext, type ReactNode, useContext, useEffect, useRef, useState } from 'react';
import { AppState } from 'react-native';

import MemloreCore, {
  type AutoLockSeconds,
  type BiometricKind,
  type MemloreErrorCode,
} from '@/modules/memlore-core';
import { flushAll } from '@/src/lib/flushRegistry';
import { clearPendingRestore } from '@/src/lib/pendingRestore';
import { clearRowCache, enableRowCache } from '@/src/lib/rowCache';

export type VaultStatus =
  | 'loading'
  | 'first_launch'
  | 'show_phrase'
  | 'locked'
  | 'unlocked'
  | 'boot_file_corrupt'
  | 'error';

/**
 * What the screen shows under the form: a wrong password, Face ID/Touch ID
 * that cannot be used now, or anything else.
 */
export type VaultError = 'wrong_password' | 'biometric_disabled' | 'failed';

/** Face ID/Touch ID on this device and for this vault. */
export interface VaultBiometric {
  /** Enrolled and usable now. */
  available: boolean;
  kind: BiometricKind;
  /** This vault unlocks with Face ID/Touch ID on this device. */
  enabled: boolean;
}

const NO_BIOMETRIC: VaultBiometric = { available: false, kind: 'none', enabled: false };

// Every code the module can reject with: a Record, so a code added to
// `MemloreErrorCode` and missing here fails the typecheck instead of reading
// as a generic error.
const KNOWN_CODES: Record<MemloreErrorCode, true> = {
  ERR_LOCKED: true,
  ERR_WRONG_PASSWORD: true,
  ERR_IO: true,
  ERR_FORMAT: true,
  ERR_BIOMETRIC_DISABLED: true,
  ERR_BIOMETRIC_CANCELLED: true,
  ERR_NOT_FOUND: true,
  ERR_NAME_TAKEN: true,
  ERR_LAST_JOURNAL: true,
  ERR_SECOND_LOCK_REQUIRED: true,
  ERR_VAULT_NOT_EMPTY: true,
  ERR_WRONG_PHRASE: true,
};
const ERROR_CODES = Object.keys(KNOWN_CODES) as MemloreErrorCode[];

/**
 * How long JS waits for the lock screen after an auto-lock before it stops
 * holding the shield. The native safety timeout (~2 s after the app is
 * active) takes the shield itself off; this only keeps the JS state in step,
 * so the Face ID prompt still arms if no lock screen ever calls
 * `lockScreenShown()`.
 */
const SHIELD_FALLBACK_MS = 2500;

/**
 * Save before lock: how long a lock waits for the open editor's saves. The
 * native wait is 2 s, so the ack for `onFlushRequest` leaves before it.
 */
const FLUSH_REQUEST_MS = 1800;
const MANUAL_LOCK_FLUSH_MS = 2000;

/** The module's error code of a rejected call, or null for anything else. */
export function errorCode(e: unknown): MemloreErrorCode | null {
  if (typeof e !== 'object' || e === null || !('code' in e)) return null;
  return ERROR_CODES.find((code) => code === e.code) ?? null;
}

/**
 * The vault lifecycle, once for the whole app: wrap the root layout in it.
 * Components read it through `useVault()`, never the native module.
 */
export function VaultProvider({ children }: { children: ReactNode }) {
  const vault = useVaultState();
  return <VaultContext.Provider value={vault}>{children}</VaultContext.Provider>;
}

export type Vault = ReturnType<typeof useVaultState>;

const VaultContext = createContext<Vault | null>(null);

/** The app-wide vault state; only under `VaultProvider`. */
export function useVault(): Vault {
  const vault = useContext(VaultContext);
  if (vault === null) throw new Error('useVault() needs a VaultProvider above it');
  return vault;
}

/** The provider's state; it holds the one `onAutoLock` listener of the app. */
function useVaultState() {
  const [status, setStatus] = useState<VaultStatus>('loading');
  const [error, setError] = useState<VaultError | null>(null);
  const [busy, setBusy] = useState(false);
  // A Face ID/Touch ID unlock prompt is up. Apart from `busy`, so the password
  // form does not read "Unlocking…" while the system sheet is shown.
  const [biometricPrompting, setBiometricPrompting] = useState(false);
  // Shown once after creation; cleared as soon as the user confirms. An
  // auto-lock on the phrase screen keeps it, and the next unlock shows it again.
  const [recoveryPhrase, setRecoveryPhrase] = useState<string | null>(null);
  const [biometric, setBiometric] = useState<VaultBiometric>(NO_BIOMETRIC);
  // Read from the vault after each unlock; null while locked.
  const [autoLockSeconds, setAutoLockValue] = useState<AutoLockSeconds | null>(null);
  const [appActive, setAppActive] = useState(AppState.currentState === 'active');
  // An auto-lock left the native privacy shield up; it comes off once the
  // lock screen has mounted (`lockScreenShown()`).
  const [shieldHeld, setShieldHeld] = useState(false);
  // The lock screen has mounted since the vault last locked. Cleared whenever
  // the status leaves 'locked', so it always refers to the current lock.
  const [lockScreenMounted, setLockScreenMounted] = useState(false);
  // One automatic Face ID/Touch ID prompt per lock event. Consumed when it
  // fires, so a cancel never brings it back; the button always prompts.
  const [autoPromptPending, setAutoPromptPending] = useState(false);
  // Set synchronously, unlike `busy`: a second tap in the same frame (or
  // onSubmitEditing racing the button) must not start a second engine call.
  const inFlight = useRef(false);

  function checkStartup(isCancelled: () => boolean = () => false) {
    MemloreCore.startupMode()
      .then((mode) => {
        if (__DEV__) console.log('[useVault] startupMode =', mode);
        if (isCancelled()) return;
        if (mode === 'first_launch') setStatus('first_launch');
        else if (mode === 'boot_file_corrupt') setStatus('boot_file_corrupt');
        else enterLocked();
        void refreshBiometric();
      })
      .catch(() => {
        if (!isCancelled()) setStatus('error');
      });
  }

  useEffect(() => {
    let cancelled = false;
    checkStartup(() => cancelled);
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    const appState = AppState.addEventListener('change', (state) => {
      setAppActive(state === 'active');
    });
    // The vault is already locked (or locking) natively, and the app is not
    // active yet. Never prompt from here: the effects below do, once the
    // shield is off and the app is active.
    // The only onAutoLock subscription of the app.
    const autoLock = MemloreCore.addListener('onAutoLock', () => {
      setAppActive(false);
      markLocked();
      setShieldHeld(true);
    });
    // Save before lock: native holds its background lock until this ack (or
    // ~2 s), so every edit reaches the engine queue before the lock does.
    const flushRequest = MemloreCore.addListener('onFlushRequest', ({ id }) => {
      const started = Date.now();
      void flushAll(FLUSH_REQUEST_MS)
        .then(() => {
          if (__DEV__) console.log(`[useVault] flush ${id} acked in ${Date.now() - started} ms`);
          return MemloreCore.flushDone(id);
        })
        // No ack: native locks at its timeout anyway (fail closed).
        .catch(() => {});
    });
    return () => {
      appState.remove();
      autoLock.remove();
      flushRequest.remove();
    };
  }, []);

  // Out of 'locked': the next lock waits for its own lock screen, and a shield
  // still held (an unlock before the lock screen answered) is not carried over
  // to a later manual lock, which must not prompt.
  useEffect(() => {
    if (status === 'locked') return;
    setLockScreenMounted(false);
    setShieldHeld(false);
  }, [status]);

  // After an auto-lock, once the lock screen has mounted: take the shield
  // off, then arm the Face ID prompt. Never while protected screens may still
  // be on screen, so not on the status change alone.
  // TODO(later): release on a native signal that the protected screens left the
  // view hierarchy (docs/LATER.md "Mobile M3a notes").
  useEffect(() => {
    if (!shieldHeld || status !== 'locked' || !lockScreenMounted) return;
    MemloreCore.releasePrivacyShield()
      // The native safety timeout takes it off anyway.
      .catch(() => {})
      .finally(() => {
        setShieldHeld(false);
        setAutoPromptPending(true);
      });
  }, [shieldHeld, status, lockScreenMounted]);

  // Safety: no lock screen mounted in time (should not happen). Native has
  // taken the shield off by now; stop holding it here so the prompt arms.
  useEffect(() => {
    if (!shieldHeld || status !== 'locked' || lockScreenMounted) return;
    const timer = setTimeout(() => {
      setShieldHeld(false);
      setAutoPromptPending(true);
    }, SHIELD_FALLBACK_MS);
    return () => clearTimeout(timer);
  }, [shieldHeld, status, lockScreenMounted]);

  /** Re-runs the startup check from the `error` screen. */
  function retry() {
    setStatus('loading');
    checkStartup();
  }

  /** Drops everything read from the vault and shows the lock screen. */
  function showLocked() {
    clearRowCache();
    clearPendingRestore();
    setError(null);
    setAutoLockValue(null);
    setStatus('locked');
  }

  /**
   * The vault is locked natively: a data hook got ERR_LOCKED, or the
   * auto-lock fired. The lock screen, with no prompt of its own (the
   * auto-lock arms one once its shield is off).
   */
  function markLocked() {
    showLocked();
    void refreshBiometric();
  }

  /**
   * The lock screen calls this from its mount effect. After an auto-lock it
   * takes the privacy shield off and arms the Face ID prompt; otherwise it
   * does nothing.
   */
  function lockScreenShown() {
    setLockScreenMounted(true);
  }

  /**
   * A cold start: the lock screen, then one prompt. The auto-lock arms its own
   * once the shield is off; a manual lock never does (it would unlock again at
   * once), so it calls `showLocked` instead.
   */
  function enterLocked() {
    showLocked();
    setAutoPromptPending(true);
  }

  /**
   * After a password or Face ID unlock: back to a pending recovery phrase,
   * else the vault.
   */
  function enterUnlocked() {
    enableRowCache();
    setAutoPromptPending(false);
    void refreshBiometric();
    void refreshAutoLock();
    setStatus(recoveryPhrase !== null ? 'show_phrase' : 'unlocked');
  }

  /**
   * Face ID/Touch ID state. Never fails: a failed read keeps the last state
   * (a stale button then gets ERR_BIOMETRIC_DISABLED and refreshes again).
   */
  async function refreshBiometric() {
    try {
      const [device, enabled] = await Promise.all([
        MemloreCore.biometricStatus(),
        MemloreCore.isBiometricUnlockEnabled(),
      ]);
      setBiometric({ available: device.available, kind: device.kind, enabled });
    } catch {
      // Keeps the last state; the password always works.
    }
  }

  /** Only while unlocked: the engine rejects the read while locked. */
  async function refreshAutoLock() {
    try {
      setAutoLockValue(await MemloreCore.autoLockSeconds());
    } catch {
      setAutoLockValue(null);
    }
  }

  /**
   * Runs one engine call with a pending flag (`busy` unless given another)
   * and the error mapping. A call made while another is in flight is ignored
   * and returns false.
   */
  async function run(
    action: () => Promise<void>,
    setPending: (pending: boolean) => void = setBusy,
  ): Promise<boolean> {
    if (inFlight.current) return false;
    inFlight.current = true;
    setPending(true);
    setError(null);
    try {
      await action();
      return true;
    } catch (e) {
      const code = errorCode(e);
      if (code === 'ERR_LOCKED') {
        // Not an error: the vault locked under us, so show the unlock screen.
        // No prompt: the auto-lock that did it arms its own, and an unlock
        // that hit this must not retry itself in a loop.
        showLocked();
      } else if (code === 'ERR_BIOMETRIC_CANCELLED') {
        // Not an error either: the prompt was dismissed; nothing changed.
      } else if (code === 'ERR_BIOMETRIC_DISABLED') {
        // The engine's message is English; the screen shows a translated one.
        // The refresh hides the button once Face ID is off.
        setError('biometric_disabled');
        void refreshBiometric();
      } else {
        setError(code === 'ERR_WRONG_PASSWORD' ? 'wrong_password' : 'failed');
      }
      return false;
    } finally {
      inFlight.current = false;
      setPending(false);
    }
  }

  const createVault = (password: string) =>
    run(async () => {
      setRecoveryPhrase(await MemloreCore.createVault(password));
      setStatus('show_phrase');
      void refreshBiometric();
      void refreshAutoLock();
    });

  /** The vault is already unlocked after creation: go straight to it. */
  function confirmPhraseSaved() {
    enableRowCache();
    setRecoveryPhrase(null);
    setStatus('unlocked');
  }

  const unlock = (password: string) =>
    run(async () => {
      await MemloreCore.unlock(password);
      enterUnlocked();
    });

  /** A cancel stays locked with no message. */
  const unlockWithBiometric = () =>
    run(async () => {
      await MemloreCore.unlockWithBiometric();
      enterUnlocked();
    }, setBiometricPrompting);

  /**
   * A manual lock: the open editor's saves first (bounded), then the lock
   * screen without a prompt (the button prompts).
   */
  const lock = () =>
    run(async () => {
      await flushAll(MANUAL_LOCK_FLUSH_MS);
      await MemloreCore.lock();
      showLocked();
      void refreshBiometric();
    });

  /** One Face ID/Touch ID prompt, then the key is stored. A cancel is silent. */
  const enableBiometric = () =>
    run(async () => {
      try {
        await MemloreCore.enableBiometric();
      } finally {
        // Inside the call, so `busy` covers it and the toggle never flickers.
        await refreshBiometric();
      }
    });

  const disableBiometric = () =>
    run(async () => {
      try {
        await MemloreCore.disableBiometric();
      } finally {
        await refreshBiometric();
      }
    });

  /** The selection switches at once and goes back if the write fails. */
  async function setAutoLockSeconds(seconds: AutoLockSeconds): Promise<boolean> {
    const previous = autoLockSeconds;
    setAutoLockValue(seconds);
    const ok = await run(() => MemloreCore.setAutoLockSeconds(seconds));
    // Unless a lock cleared it meanwhile.
    if (!ok) setAutoLockValue((current) => (current === seconds ? previous : current));
    return ok;
  }

  // The automatic prompt: once per lock event, only while the lock screen is
  // up, the app is active (the Keychain prompt needs it) and the shield is
  // off. Cleared before it fires, so a cancel does not prompt again.
  useEffect(() => {
    if (!autoPromptPending || status !== 'locked' || busy || biometricPrompting) return;
    if (shieldHeld || !appActive) return;
    if (!biometric.available || !biometric.enabled) return;
    setAutoPromptPending(false);
    void unlockWithBiometric();
  }, [autoPromptPending, status, busy, biometricPrompting, shieldHeld, appActive, biometric]);

  return {
    status,
    error,
    busy,
    biometricPrompting,
    recoveryPhrase,
    biometric,
    autoLockSeconds,
    createVault,
    confirmPhraseSaved,
    unlock,
    unlockWithBiometric,
    lock,
    enableBiometric,
    disableBiometric,
    setAutoLockSeconds,
    markLocked,
    lockScreenShown,
    retry,
  };
}

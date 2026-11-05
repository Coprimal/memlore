import { useEffect, useRef, useState } from 'react';
import { ActivityIndicator, Alert, Linking, Pressable, Text, View } from 'react-native';
import { StyleSheet } from 'react-native-unistyles';

import type { DriveStatus, SyncBlocked, SyncProgressEvent } from '@/modules/memlore-core';
import { ActionButton } from '@/src/components/ActionButton';
import { ErrorText } from '@/src/components/ErrorText';
import { RePairSheet } from '@/src/components/RePairSheet';
import { isDriveNotConfigured, isSyncBusy, useSync } from '@/src/hooks/useSync';
import { errorCode } from '@/src/hooks/useVault';
import { type TKey, t } from '@/src/i18n';

/** Where a Google account removes an app's access (decision 3: never revoked from here). */
const GOOGLE_PERMISSIONS_URL = 'https://myaccount.google.com/permissions';

/** How long an action may run before its spinner shows (fast calls never flash it). */
const SPINNER_DELAY_MS = 150;

/** How often "Last sync: 5 minutes ago" is recomputed. */
const CLOCK_TICK_MS = 30_000;

/** The product name, never translated. */
const PROVIDER = 'Google Drive';

type Action = 'connect' | 'disconnect' | 'sync';

/**
 * The message for a failed Google sign-in or connect (Settings and
 * onboarding). The refusal (ERR_VAULT_NOT_EMPTY) says what to do instead.
 */
export function connectErrorMessage(e: unknown): string {
  if (errorCode(e) === 'ERR_VAULT_NOT_EMPTY') return t('mobile.sync.vault_not_empty');
  if (isDriveNotConfigured(e)) return t('mobile.sync.not_configured');
  if (isSyncBusy(e)) return t('nav:sync.busy');
  return t('onboarding.drive.error_generic', { provider: PROVIDER });
}

/**
 * Settings > Sync: connect Google Drive, then the account, last sync,
 * pending entries and progress, "Sync now", Disconnect and the link to
 * remove Memlore's access at Google; a blocked sync says why (re-pair with
 * the phrase, restored elsewhere, reconnect). Rendered in the Settings tab,
 * behind the unlocked guard.
 */
export function SyncPanel() {
  const sync = useSync();
  const [drive, setDrive] = useState<DriveStatus | null>(null);
  const [loadFailed, setLoadFailed] = useState(false);
  const [action, setAction] = useState<Action | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [rePairOpen, setRePairOpen] = useState(false);
  const spinner = useDelayed(action === 'connect' || action === 'disconnect', SPINNER_DELAY_MS);
  const now = useNow(CLOCK_TICK_MS);
  const running = sync.status?.running ?? false;

  /** Connected and the account; never rejects (ERR_LOCKED shows the lock screen). */
  async function readDrive() {
    try {
      setDrive(await sync.driveStatus());
      setLoadFailed(false);
    } catch {
      setLoadFailed(true);
    }
  }

  // On mount (even while the first sync after onboarding runs)…
  useEffect(() => {
    void readDrive();
  }, []);

  // …and whenever a sync ends (it may have found the sign-in gone).
  const wasRunning = useRef(running);
  useEffect(() => {
    if (wasRunning.current && !running) void readDrive();
    wasRunning.current = running;
  }, [running]);

  async function connect() {
    if (action !== null) return;
    setAction('connect');
    setError(null);
    try {
      const outcome = await sync.connect();
      if (outcome?.kind === 'connected') {
        void sync.syncNow().catch(() => {});
      } else if (outcome?.kind === 'join_with_phrase') {
        setRePairOpen(true);
      } else if (outcome?.kind === 'needs_onboarding') {
        // First launch only; never with an unlocked vault. Drop its token.
        void sync.cancelOnboarding(outcome.sessionId);
        setError(t('mobile.error'));
      } else if (outcome?.kind === 'no_cloud_vault') {
        setError(t('mobile.error'));
      }
    } catch (e) {
      if (errorCode(e) !== 'ERR_LOCKED') setError(connectErrorMessage(e));
    } finally {
      setAction(null);
      void readDrive();
    }
  }

  function confirmDisconnect() {
    Alert.alert(
      t('settings:gdrive.disconnect_modal.title', { provider: PROVIDER }),
      t('mobile.sync.disconnect_body'),
      [
        { text: t('common:confirm_dialog.cancel'), style: 'cancel' },
        {
          text: t('settings:gdrive.disconnect_modal.confirm'),
          style: 'destructive',
          onPress: () => void disconnect(),
        },
      ],
    );
  }

  async function disconnect() {
    if (action !== null) return;
    setAction('disconnect');
    setError(null);
    try {
      await sync.disconnect();
    } catch (e) {
      if (errorCode(e) !== 'ERR_LOCKED') setError(t('mobile.error'));
    } finally {
      setAction(null);
      void readDrive();
    }
  }

  async function syncNow() {
    if (action !== null || running) return;
    setAction('sync');
    setError(null);
    try {
      await sync.syncNow();
    } catch (e) {
      if (errorCode(e) !== 'ERR_LOCKED') setError(t('mobile.error'));
    } finally {
      setAction(null);
    }
  }

  function confirmRemoveAccess() {
    Alert.alert(t('mobile.sync.remove_access_title'), t('mobile.sync.remove_access_body'), [
      { text: t('common:confirm_dialog.cancel'), style: 'cancel' },
      {
        text: t('mobile.sync.remove_access_confirm'),
        style: 'destructive',
        onPress: () => void Linking.openURL(GOOGLE_PERMISSIONS_URL).catch(() => {}),
      },
    ]);
  }

  function closeRePair() {
    setRePairOpen(false);
    void readDrive();
  }

  const header = (
    <>
      <View style={styles.titleRow}>
        <Text style={styles.sectionTitle}>{t('settings:categories.sync.label')}</Text>
        {spinner ? <ActivityIndicator /> : null}
      </View>
      <Text style={styles.body}>{t('settings:cloud.hint')}</Text>
    </>
  );

  if (drive === null) {
    return (
      <View testID="sync-panel" style={styles.section}>
        {header}
        {loadFailed ? (
          <ErrorText message={t('settings:gdrive.errors.load_status')} />
        ) : (
          // The cheap stand-in until the Drive state is read (same card shape as a row).
          <View style={styles.placeholder} />
        )}
      </View>
    );
  }

  const blocked = sync.status?.blocked ?? null;
  const lastSyncMs = sync.status?.lastSyncMs ?? drive.lastSyncMs;
  const pendingCount = drive.pendingCount;
  const failed = !running && sync.status?.phase === 'error';
  // A reconnect notice brings its own button.
  const showConnect = !drive.connected && blocked?.kind !== 'needs_reconnect';

  return (
    <View testID="sync-panel" style={styles.section}>
      {header}
      {showConnect ? (
        <ActionButton
          testID="sync-connect"
          label={
            action === 'connect'
              ? t('settings:gdrive.connecting')
              : t('settings:cloud.connect_with', { provider: PROVIDER })
          }
          disabled={action !== null}
          onPress={() => void connect()}
        />
      ) : null}
      {blocked !== null ? (
        <BlockedNotice
          blocked={blocked}
          busy={action !== null}
          onRePair={() => setRePairOpen(true)}
          onReconnect={() => void connect()}
        />
      ) : null}
      {drive.connected ? (
        <>
          <View testID="sync-account" style={styles.row}>
            <Text style={styles.rowLabel}>{t('settings:gdrive.signed_in_as')}</Text>
            <Text style={styles.rowValue} numberOfLines={1}>
              {drive.email ?? PROVIDER}
            </Text>
          </View>
          <View testID="sync-status" style={styles.card}>
            <Text style={styles.statusLine}>
              {`${t('settings:gdrive.last_sync')} ${lastSyncLabel(lastSyncMs, now)}`}
            </Text>
            {pendingCount > 0 || !failed ? (
              <Text style={styles.statusLine}>
                {pendingCount > 0
                  ? t('nav:sync.pending', { count: pendingCount })
                  : t('nav:sync.up_to_date')}
              </Text>
            ) : null}
            {running ? (
              <Text style={styles.statusLine}>{progressLabel(sync.progress)}</Text>
            ) : failed ? (
              <>
                <Text style={styles.errorLine}>{t('nav:sync.error')}</Text>
                {/* The backend's reason, as the desktop status tooltip shows it. */}
                {sync.status?.error ? (
                  <Text testID="sync-error-reason" style={styles.errorReason} numberOfLines={3}>
                    {sync.status.error}
                  </Text>
                ) : null}
              </>
            ) : null}
          </View>
          <ActionButton
            testID="sync-now"
            label={running ? t('nav:sync.syncing') : t('nav:sync.sync_now')}
            disabled={running || action !== null}
            onPress={() => void syncNow()}
          />
          <Pressable
            testID="sync-disconnect"
            accessibilityRole="button"
            disabled={action !== null}
            onPress={confirmDisconnect}
            style={styles.dangerButton}
          >
            <Text style={styles.dangerLabel}>
              {action === 'disconnect'
                ? t('settings:gdrive.disconnecting')
                : t('settings:gdrive.disconnect')}
            </Text>
          </Pressable>
          <Pressable
            testID="sync-remove-access"
            accessibilityRole="button"
            onPress={confirmRemoveAccess}
            style={styles.linkButton}
          >
            <Text style={styles.linkLabel}>{t('mobile.sync.remove_access')}</Text>
          </Pressable>
        </>
      ) : null}
      <ErrorText message={error} />
      <RePairSheet visible={rePairOpen} onClose={closeRePair} />
    </View>
  );
}

function BlockedNotice(props: {
  blocked: SyncBlocked;
  busy: boolean;
  onRePair: () => void;
  onReconnect: () => void;
}) {
  const { blocked } = props;
  switch (blocked.kind) {
    case 'needs_re_pair':
      return (
        <View testID="sync-blocked" style={styles.notice}>
          <Text style={styles.warning}>
            {blocked.reason === 'inconclusive'
              ? t('nav:sync.force_re_pair_inconclusive')
              : t('nav:sync.force_re_pair_rotated')}
          </Text>
          <ActionButton
            testID="sync-re-pair"
            label={t('force_re_pair.submit')}
            disabled={props.busy}
            onPress={props.onRePair}
          />
        </View>
      );
    case 'fenced':
      return (
        <View testID="sync-blocked" style={styles.notice}>
          <Text style={styles.warning}>{t('mobile.sync.fenced')}</Text>
        </View>
      );
    case 'needs_reconnect':
      return (
        <View testID="sync-blocked" style={styles.notice}>
          <Text style={styles.warning}>{t('settings:gdrive.token_revoked.body')}</Text>
          <ActionButton
            testID="sync-reconnect"
            label={t('settings:gdrive.token_revoked.cta')}
            disabled={props.busy}
            onPress={props.onReconnect}
          />
        </View>
      );
  }
}

/** "just now", "5 minutes ago", …; "Never" before the first sync. */
function lastSyncLabel(ms: number | null, now: number): string {
  if (ms === null) return t('mobile.sync.never');
  const minutes = Math.floor((now - ms) / 60_000);
  if (minutes < 1) return t('settings:security.devices.relative.just_now');
  if (minutes < 60) return t('settings:security.devices.relative.minutes_ago', { count: minutes });
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return t('settings:security.devices.relative.hours_ago', { count: hours });
  return t('settings:security.devices.relative.days_ago', { count: Math.floor(hours / 24) });
}

/** The desktop's words for the running step, with its count; "Syncing…" for an unknown step. */
function progressLabel(progress: SyncProgressEvent | null): string {
  if (progress === null) return t('nav:sync.syncing');
  // The step is a desktop progress key; `t` hands back the key it cannot find.
  const key = `nav:sync.progress.${progress.step}` as TKey;
  const found = t(key);
  const step = found === key ? t('nav:sync.syncing') : found;
  if (progress.total <= 0) return step;
  return t('mobile.sync.progress', { step, done: progress.done, total: progress.total });
}

/** `Date.now()`, refreshed every `ms` while mounted. */
function useNow(ms: number): number {
  const [now, setNow] = useState(Date.now);
  useEffect(() => {
    const timer = setInterval(() => setNow(Date.now()), ms);
    return () => clearInterval(timer);
  }, [ms]);
  return now;
}

/** `on`, but only once it has stayed true for `ms`. */
function useDelayed(on: boolean, ms: number): boolean {
  const [shown, setShown] = useState(false);
  useEffect(() => {
    if (!on) {
      setShown(false);
      return;
    }
    const timer = setTimeout(() => setShown(true), ms);
    return () => clearTimeout(timer);
  }, [on, ms]);
  return on && shown;
}

const styles = StyleSheet.create((theme) => ({
  section: {
    gap: theme.space(3),
  },
  titleRow: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: theme.space(2),
    marginTop: theme.space(3),
  },
  sectionTitle: {
    fontSize: 17,
    fontWeight: '600',
    color: theme.colors.fg,
  },
  body: {
    fontSize: 15,
    color: theme.colors.fgSecondary,
  },
  warning: {
    fontSize: 15,
    color: theme.colors.warningText,
  },
  notice: {
    gap: theme.space(3),
  },
  // The cheap stand-in until the Drive state is read (same card shape as a row).
  placeholder: {
    height: theme.space(14),
    borderWidth: 1,
    borderRadius: theme.radii.xl,
    borderColor: theme.colors.borderDefault,
    backgroundColor: theme.colors.elevated,
  },
  // The Security panel's row shape (SecurityPanel.tsx).
  row: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: theme.space(3),
    paddingVertical: theme.space(3),
    paddingHorizontal: theme.space(4),
    borderWidth: 1,
    borderRadius: theme.radii.xl,
    borderColor: theme.colors.borderDefault,
    backgroundColor: theme.colors.elevated,
  },
  rowLabel: {
    fontSize: 15,
    color: theme.colors.fgSecondary,
  },
  rowValue: {
    flex: 1,
    textAlign: 'right',
    fontSize: 17,
    color: theme.colors.fg,
  },
  card: {
    gap: theme.space(1),
    paddingVertical: theme.space(3),
    paddingHorizontal: theme.space(4),
    borderWidth: 1,
    borderRadius: theme.radii.xl,
    borderColor: theme.colors.borderDefault,
    backgroundColor: theme.colors.elevated,
  },
  statusLine: {
    fontSize: 15,
    color: theme.colors.fg,
  },
  errorLine: {
    fontSize: 15,
    color: theme.colors.dangerText,
  },
  errorReason: {
    fontSize: 13,
    color: theme.colors.fgSecondary,
  },
  dangerButton: {
    alignItems: 'center',
    paddingVertical: theme.space(3),
    borderWidth: 1,
    borderRadius: theme.radii.button,
    borderColor: theme.colors.dangerBorder,
    backgroundColor: theme.colors.dangerBg,
  },
  dangerLabel: {
    fontSize: 17,
    fontWeight: '600',
    color: theme.colors.dangerText,
  },
  linkButton: {
    alignItems: 'center',
    paddingVertical: theme.space(2),
  },
  linkLabel: {
    fontSize: 15,
    color: theme.colors.dangerText,
  },
}));

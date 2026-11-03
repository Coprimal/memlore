import { useRef, useState } from 'react';
import { Pressable, Switch, Text, View } from 'react-native';
import { StyleSheet, useUnistyles } from 'react-native-unistyles';

import type { RelockMinutes } from '@/modules/memlore-core';
import { ActionButton } from '@/src/components/ActionButton';
import { ErrorText } from '@/src/components/ErrorText';
import {
  MIN_LOCK_PASSWORD_LEN,
  PasswordSheet,
  type PasswordSheetProps,
} from '@/src/components/PasswordSheet';
import { type LockSession, useLockSession } from '@/src/hooks/useLockSession';
import { errorCode } from '@/src/hooks/useVault';
import { t } from '@/src/i18n';

/** 1 / 5 / 15 / 30 minutes, then never (0); the stored default is 5. */
const RELOCK_OPTIONS: readonly RelockMinutes[] = [1, 5, 15, 30, 0];

type SheetMode = 'set' | 'change' | 'disable' | 'reveal';

/**
 * Settings > Second lock: set the password, or (once set) reveal / lock now,
 * show existence, the auto re-lock interval, change and disable. Rendered in
 * the Settings tab, under the lock session provider.
 */
export function SecondLockPanel() {
  const session = useLockSession();
  const [sheet, setSheet] = useState<SheetMode | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [locking, setLocking] = useState(false);
  // A setting refused with ERR_SECOND_LOCK_REQUIRED, run again after a reveal.
  const retry = useRef<(() => Promise<void>) | null>(null);

  /**
   * Runs a setting change (shown at once by the hook). Needing the reveal
   * opens the reveal sheet and retries after it; other failures show `failed`.
   */
  async function change(action: () => Promise<void>, failed: string) {
    setError(null);
    try {
      await action();
    } catch (e) {
      const code = errorCode(e);
      if (code === 'ERR_SECOND_LOCK_REQUIRED') {
        retry.current = async () => {
          try {
            await action();
          } catch {
            setError(failed);
          }
        };
        setSheet('reveal');
      } else if (code !== 'ERR_LOCKED') {
        setError(failed);
      }
    }
  }

  async function lockNow() {
    if (locking) return;
    setLocking(true);
    try {
      await session.lockNow();
    } finally {
      setLocking(false);
    }
  }

  function closeSheet() {
    retry.current = null;
    setSheet(null);
  }

  if (!session.loaded) {
    return (
      <View testID="second-lock-panel" style={styles.section}>
        <Text style={styles.sectionTitle}>{t('settings:security.tabs.second_lock')}</Text>
        <View style={styles.placeholder} />
      </View>
    );
  }

  return (
    <View testID="second-lock-panel" style={styles.section}>
      <Text style={styles.sectionTitle}>{t('settings:security.tabs.second_lock')}</Text>
      <Text style={styles.body}>{t('settings:security.second_lock.description')}</Text>
      {session.enabled ? (
        <EnabledControls
          session={session}
          locking={locking}
          onLockNow={() => void lockNow()}
          onSheet={setSheet}
          onChange={(action, failed) => void change(action, failed)}
        />
      ) : (
        <>
          <Text style={styles.warning}>
            {t('settings:security.second_lock.no_recovery_warning')}
          </Text>
          <ActionButton
            testID="second-lock-set"
            label={t('settings:security.second_lock.set_button')}
            onPress={() => setSheet('set')}
          />
        </>
      )}
      <ErrorText message={error} />
      <PasswordSheet
        visible={sheet !== null}
        onClose={closeSheet}
        {...sheetProps(sheet, session, () => retry.current)}
      />
    </View>
  );
}

function EnabledControls(props: {
  session: LockSession;
  locking: boolean;
  onLockNow: () => void;
  onSheet: (mode: SheetMode) => void;
  onChange: (action: () => Promise<void>, failed: string) => void;
}) {
  const { session } = props;
  const { theme } = useUnistyles();
  return (
    <>
      {session.revealed ? (
        <ActionButton
          testID="second-lock-lock-now"
          label={t('mobile.second_lock_lock_now')}
          disabled={props.locking}
          onPress={props.onLockNow}
        />
      ) : (
        <ActionButton
          testID="second-lock-reveal"
          label={t('nav:sidebar.unlock_second_lock')}
          onPress={() => props.onSheet('reveal')}
        />
      )}
      <View style={styles.row}>
        <View style={styles.rowText}>
          <Text style={styles.rowLabel}>
            {t('settings:security.second_lock.show_existence_title')}
          </Text>
          <Text style={styles.hint}>{t('settings:security.second_lock.show_existence_hint')}</Text>
        </View>
        <Switch
          testID="second-lock-existence-toggle"
          accessibilityLabel={t('settings:security.second_lock.show_existence_title')}
          value={session.showExistence}
          onValueChange={(next) =>
            props.onChange(
              () => session.setShowExistence(next),
              t('settings:security.second_lock.visibility_failed'),
            )
          }
          trackColor={{ false: theme.colors.borderStrong, true: theme.colors.accent }}
          ios_backgroundColor={theme.colors.borderStrong}
        />
      </View>
      <Text style={styles.subTitle}>{t('settings:security.second_lock.auto_lock_title')}</Text>
      <Text style={styles.body}>{t('settings:security.second_lock.auto_lock_hint')}</Text>
      <View style={styles.pills}>
        {RELOCK_OPTIONS.map((minutes) => {
          const selected = session.autoLockMinutes === minutes;
          return (
            <Pressable
              key={minutes}
              testID={`second-lock-minutes-${minutes}`}
              accessibilityRole="button"
              accessibilityState={{ selected }}
              onPress={() => {
                if (selected) return;
                props.onChange(
                  () => session.setAutoLockMinutes(minutes),
                  t('settings:security.second_lock.auto_lock_failed'),
                );
              }}
              style={styles.pill(selected)}
            >
              <Text style={styles.pillLabel(selected)}>
                {minutes === 0
                  ? t('settings:security.second_lock.auto_lock_never')
                  : t('settings:security.second_lock.auto_lock_minutes', { count: minutes })}
              </Text>
            </Pressable>
          );
        })}
      </View>
      <Text style={styles.subTitle}>{t('settings:security.second_lock.password_title')}</Text>
      <Text style={styles.body}>{t('settings:security.second_lock.password_hint')}</Text>
      <ActionButton
        testID="second-lock-change"
        label={t('settings:security.second_lock.change_button')}
        onPress={() => props.onSheet('change')}
      />
      <Text style={styles.subTitle}>{t('settings:security.second_lock.disable_title')}</Text>
      <Text style={styles.body}>{t('settings:security.second_lock.disable_hint')}</Text>
      <Pressable
        testID="second-lock-disable"
        accessibilityRole="button"
        onPress={() => props.onSheet('disable')}
        style={styles.dangerButton}
      >
        <Text style={styles.dangerLabel}>{t('settings:security.second_lock.disable_title')}</Text>
      </Pressable>
    </>
  );
}

/** The sheet's text and call for each flow (visible and onClose come from the panel). */
function sheetProps(
  mode: SheetMode | null,
  session: LockSession,
  takeRetry: () => (() => Promise<void>) | null,
): Omit<PasswordSheetProps, 'visible' | 'onClose'> {
  const count = MIN_LOCK_PASSWORD_LEN;
  switch (mode) {
    case 'set':
      return {
        title: t('settings:security.second_lock.set_button'),
        description: t('settings:security.second_lock.description'),
        warning: t('settings:security.second_lock.no_recovery_warning'),
        placeholder: t('settings:security.second_lock.new_placeholder', { count }),
        confirmPlaceholder: t('settings:security.second_lock.confirm_placeholder'),
        submitLabel: t('settings:security.second_lock.enable_button'),
        failedMessage: t('settings:security.second_lock.set_failed'),
        onSubmit: (password) => session.setPassword(password),
      };
    case 'change':
      return {
        title: t('settings:security.second_lock.change_button'),
        description: t('settings:security.second_lock.password_hint'),
        currentPlaceholder: t('settings:security.second_lock.current_placeholder'),
        placeholder: t('settings:security.second_lock.change_new_placeholder', { count }),
        confirmPlaceholder: t('settings:security.second_lock.change_confirm_placeholder'),
        submitLabel: t('settings:security.second_lock.update_button'),
        failedMessage: t('settings:security.second_lock.change_failed'),
        onSubmit: (password, current) => session.changePassword(current, password),
      };
    case 'disable':
      return {
        title: t('settings:security.second_lock.disable_title'),
        description: t('settings:security.second_lock.disable_modal_description'),
        warning: t('settings:security.second_lock.disable_hint'),
        placeholder: t('editor:second_lock_prompt.password_placeholder'),
        submitLabel: t('settings:security.second_lock.disable_title'),
        failedMessage: t('settings:security.second_lock.disable_failed'),
        onSubmit: (password) => session.disable(password),
      };
    case 'reveal':
    case null:
      // null: the sheet is hidden; any text keeps the props complete.
      return {
        title: t('nav:sidebar.unlock_second_lock'),
        description: t('nav:sidebar.unlock_second_lock_description'),
        placeholder: t('editor:second_lock_prompt.password_placeholder'),
        submitLabel: t('editor:second_lock_prompt.unlock'),
        onSubmit: async (password) => {
          if (!(await session.reveal(password))) return false;
          // The setting that needed the reveal runs again; the sheet closes now.
          void takeRetry()?.();
          return true;
        },
      };
  }
}

const styles = StyleSheet.create((theme) => ({
  section: {
    gap: theme.space(3),
  },
  sectionTitle: {
    fontSize: 17,
    fontWeight: '600',
    color: theme.colors.fg,
    marginTop: theme.space(3),
  },
  subTitle: {
    fontSize: 15,
    fontWeight: '600',
    color: theme.colors.fg,
    marginTop: theme.space(2),
  },
  body: {
    fontSize: 15,
    color: theme.colors.fgSecondary,
  },
  warning: {
    fontSize: 15,
    color: theme.colors.warningText,
  },
  // The cheap stand-in until the lock state is read (same card shape as a row).
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
  rowText: {
    flex: 1,
    gap: theme.space(1),
  },
  rowLabel: {
    fontSize: 17,
    color: theme.colors.fg,
  },
  hint: {
    fontSize: 13,
    color: theme.colors.fgSecondary,
  },
  pills: {
    flexDirection: 'row',
    flexWrap: 'wrap',
    gap: theme.space(2),
  },
  pill: (selected: boolean) => ({
    paddingVertical: theme.space(2),
    paddingHorizontal: theme.space(4),
    borderWidth: 1,
    borderRadius: theme.radii.button,
    borderColor: selected ? theme.colors.accent : theme.colors.borderDefault,
    backgroundColor: selected ? theme.colors.accent : theme.colors.elevated,
  }),
  pillLabel: (selected: boolean) => ({
    fontSize: 15,
    fontWeight: selected ? '600' : '400',
    color: selected ? theme.colors.fgInverse : theme.colors.fg,
  }),
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
}));

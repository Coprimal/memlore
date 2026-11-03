import { useState } from 'react';
import { Alert, Pressable, Text, View } from 'react-native';
import { StyleSheet } from 'react-native-unistyles';

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
const CLOSE_OPTIONS: readonly RelockMinutes[] = [1, 5, 15, 30, 0];

type SheetMode = 'open' | 'change';

/**
 * Settings > Invisible lock: open (or silently create) a vault, close it,
 * change its password, remove empty vaults and the auto-close interval. Every
 * refusal shows the same generic text: the UI never tells whether a vault
 * existed. Rendered in the Settings tab, under the lock session provider.
 */
export function InvisibleVaultPanel() {
  const session = useLockSession();
  const [sheet, setSheet] = useState<SheetMode | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  /** Runs a call; any refusal shows only the generic text (ERR_LOCKED: the lock screen). */
  async function run(action: () => Promise<void>) {
    setError(null);
    try {
      await action();
    } catch (e) {
      if (errorCode(e) !== 'ERR_LOCKED') {
        setError(t('editor:entry_context.invisible_mark_error'));
      }
    }
  }

  async function closeVault() {
    if (busy) return;
    setBusy(true);
    try {
      await session.closeVault();
    } finally {
      setBusy(false);
    }
  }

  function confirmRemoveEmpty() {
    Alert.alert(
      t('settings:security.invisible_lock.remove_empty_confirm_title'),
      t('settings:security.invisible_lock.remove_empty_confirm_body'),
      [
        { text: t('settings:security.invisible_lock.cancel'), style: 'cancel' },
        {
          text: t('settings:security.invisible_lock.remove_empty_confirm'),
          style: 'destructive',
          onPress: () => void removeEmpty(),
        },
      ],
    );
  }

  async function removeEmpty() {
    if (busy) return;
    setBusy(true);
    try {
      await run(() => session.removeEmptyVaults());
    } finally {
      setBusy(false);
    }
  }

  if (!session.loaded) {
    return (
      <View testID="invisible-vault-panel" style={styles.section}>
        <Text style={styles.sectionTitle}>{t('settings:security.invisible_lock.title')}</Text>
        <View style={styles.placeholder} />
      </View>
    );
  }

  return (
    <View testID="invisible-vault-panel" style={styles.section}>
      <Text style={styles.sectionTitle}>{t('settings:security.invisible_lock.title')}</Text>
      <Text style={styles.body}>{t('settings:security.invisible_lock.description')}</Text>
      <Text style={styles.warning}>
        {t('settings:security.invisible_lock.no_recovery_warning')}
      </Text>
      {session.vaultOpen ? (
        <>
          <ActionButton
            testID="invisible-vault-close"
            label={t('nav:sidebar.lock_invisible')}
            disabled={busy}
            onPress={() => void closeVault()}
          />
          <Text style={styles.subTitle}>
            {t('settings:security.invisible_lock.change_title')}
          </Text>
          <Text style={styles.body}>
            {t('settings:security.invisible_lock.change_description')}
          </Text>
          <ActionButton
            testID="invisible-vault-change"
            label={t('settings:security.invisible_lock.change_button')}
            onPress={() => setSheet('change')}
          />
        </>
      ) : (
        <>
          <Text style={styles.body}>
            {t('settings:security.invisible_lock.open_description')}
          </Text>
          <ActionButton
            testID="invisible-vault-open"
            label={t('settings:security.invisible_lock.open_title')}
            onPress={() => setSheet('open')}
          />
        </>
      )}
      <Text style={styles.subTitle}>{t('settings:security.invisible_lock.auto_lock_title')}</Text>
      <Text style={styles.body}>{t('settings:security.invisible_lock.auto_lock_description')}</Text>
      <AutoClosePills session={session} onChange={(action) => void run(action)} />
      <Text style={styles.subTitle}>
        {t('settings:security.invisible_lock.remove_empty_title')}
      </Text>
      <Text style={styles.body}>
        {t('settings:security.invisible_lock.remove_empty_description')}
      </Text>
      <Pressable
        testID="invisible-vault-remove-empty"
        accessibilityRole="button"
        disabled={busy}
        onPress={confirmRemoveEmpty}
        style={styles.dangerButton}
      >
        <Text style={styles.dangerLabel}>
          {t('settings:security.invisible_lock.remove_empty_button')}
        </Text>
      </Pressable>
      <ErrorText message={error} />
      <PasswordSheet
        visible={sheet !== null}
        onClose={() => setSheet(null)}
        {...sheetProps(sheet, session)}
      />
    </View>
  );
}

function AutoClosePills(props: {
  session: LockSession;
  onChange: (action: () => Promise<void>) => void;
}) {
  const { session } = props;
  return (
    <View style={styles.pills}>
      {CLOSE_OPTIONS.map((minutes) => {
        const selected = session.invisibleAutoLockMinutes === minutes;
        return (
          <Pressable
            key={minutes}
            testID={`invisible-vault-minutes-${minutes}`}
            accessibilityRole="button"
            accessibilityState={{ selected }}
            onPress={() => {
              if (selected) return;
              props.onChange(() => session.setInvisibleAutoLockMinutes(minutes));
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
  );
}

/**
 * The sheet's text and call for each flow (visible and onClose come from the
 * panel). Both show the generic text for any refusal: a wrong current
 * password is ERR_IO, never ERR_WRONG_PASSWORD.
 */
function sheetProps(
  mode: SheetMode | null,
  session: LockSession,
): Omit<PasswordSheetProps, 'visible' | 'onClose'> {
  const failedMessage = t('editor:entry_context.invisible_mark_error');
  switch (mode) {
    case 'change':
      return {
        title: t('settings:security.invisible_lock.change_modal_title'),
        description: t('settings:security.invisible_lock.change_modal_description'),
        warning: t('settings:security.invisible_lock.change_description'),
        currentPlaceholder: t('settings:security.invisible_lock.current_placeholder'),
        placeholder: t('settings:security.invisible_lock.new_placeholder', {
          count: MIN_LOCK_PASSWORD_LEN,
        }),
        confirmPlaceholder: t('settings:security.invisible_lock.confirm_placeholder'),
        submitLabel: t('settings:security.invisible_lock.password_save'),
        failedMessage,
        onSubmit: (password, current) => session.changeVaultPassword(current, password),
      };
    case 'open':
    case null:
      // null: the sheet is hidden; any text keeps the props complete. Open
      // and create are one call with one answer: nothing here tells them apart.
      return {
        title: t('settings:security.invisible_lock.open_title'),
        description: t('settings:security.invisible_lock.open_description'),
        placeholder: t('settings:security.invisible_lock.open_placeholder'),
        submitLabel: t('settings:security.invisible_lock.open_button'),
        failedMessage,
        onSubmit: (password) => session.openVault(password),
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

import { Stack } from 'expo-router';
import { SymbolView } from 'expo-symbols';
import { useEffect, useRef, useState } from 'react';
import {
  ActivityIndicator,
  Alert,
  Animated,
  FlatList,
  Platform,
  Pressable,
  Text,
  View,
} from 'react-native';
import { StyleSheet, useUnistyles } from 'react-native-unistyles';

import type { TrashRow } from '@/modules/memlore-core';
import { ErrorText } from '@/src/components/ErrorText';
import { PasswordSheet } from '@/src/components/PasswordSheet';
import { useLockSession } from '@/src/hooks/useLockSession';
import { trashDaysLeft, useTrash } from '@/src/hooks/useOrganize';
import { t } from '@/src/i18n';

/** How long a load runs before its spinner shows (fast loads never flash it). */
const SPINNER_DELAY_MS = 150;

/**
 * Recently deleted entries, most recently trashed first. Each row restores at
 * once or deletes for good after a confirmation; Empty trash deletes the rows
 * shown when the user tapped it, after a confirmation with their count.
 * A locked entry shows redacted (lock and days left) unless revealed; its
 * Delete forever asks for the reveal first (the engine refuses the purge
 * while covered), and Empty trash leaves it out.
 */
export default function TrashScreen() {
  // A reveal, re-lock or lock change reloads the list (the engine decides what it shows).
  const trash = useTrash();
  const session = useLockSession();
  // A restore or delete that was refused (the list has reloaded already).
  const [changeFailed, setChangeFailed] = useState(false);
  // A locked row while not revealed shows redacted (Covered). Under Hidden
  // (show existence off) the next read omits it; until then it is left out.
  const shown = session.showExistence
    ? trash.rows
    : trash.rows.filter((row) => !row.isLocked || session.revealed);
  // The rows Empty trash deletes: the engine skips covered ones.
  const deletable = shown.filter((row) => !isCovered(row));
  const showSpinner = useDelayed(trash.loading && shown.length === 0, SPINNER_DELAY_MS);
  const nowSecs = Math.floor(Date.now() / 1000);

  // The reveal sheet is open; what runs once it closed after a successful reveal.
  const [revealOpen, setRevealOpen] = useState(false);
  const afterReveal = useRef<(() => void) | null>(null);
  const revealed = useRef(false);

  /** Locked and not revealed: shown redacted. */
  function isCovered(row: TrashRow) {
    return row.isLocked && !session.revealed;
  }

  /** Shows the reveal sheet at once; `then` runs after a successful reveal. */
  function askReveal(then: () => void) {
    afterReveal.current = then;
    revealed.current = false;
    setRevealOpen(true);
  }

  /**
   * The sheet closed: after a reveal, what it was for runs once the sheet has
   * slid away (iOS), else nothing. An Alert shown while the sheet is still
   * dismissing may never appear.
   */
  function closeReveal() {
    if (!revealed.current) afterReveal.current = null;
    revealed.current = false;
    setRevealOpen(false);
    if (Platform.OS !== 'ios') runAfterReveal();
  }

  function runAfterReveal() {
    const then = afterReveal.current;
    afterReveal.current = null;
    if (then) then();
  }

  /** Runs `action` at once, or after a reveal for a covered row. */
  function gated(row: TrashRow, action: () => void) {
    if (isCovered(row)) askReveal(action);
    else action();
  }

  async function run(change: Promise<boolean | number | null>) {
    setChangeFailed(false);
    const done = await change;
    if (done === false || done === null) setChangeFailed(true);
  }

  function confirmPurge(id: string) {
    Alert.alert(
      t('settings:data_section.trash.delete_forever_title'),
      t('settings:data_section.trash.delete_forever_body'),
      [
        { text: t('common:confirm_dialog.cancel'), style: 'cancel' },
        {
          text: t('settings:data_section.trash.delete_forever'),
          style: 'destructive',
          onPress: () => void run(trash.purge(id)),
        },
      ],
    );
  }

  function confirmEmpty() {
    // The deletable rows on screen now: the count the user confirms is what
    // gets deleted (covered rows stay).
    const ids = deletable.map((row) => row.id);
    if (ids.length === 0) return;
    Alert.alert(
      t('settings:data_section.trash.empty_trash_title'),
      t('settings:data_section.trash.empty_trash_body', { count: ids.length }),
      [
        { text: t('common:confirm_dialog.cancel'), style: 'cancel' },
        {
          text: t('settings:data_section.trash.empty_trash'),
          style: 'destructive',
          onPress: () => void run(trash.emptyTrash(ids)),
        },
      ],
    );
  }

  return (
    <View style={styles.screen}>
      <Stack.Screen
        options={{
          title: t('settings:data_section.tabs.trash'),
          headerRight: () =>
            deletable.length > 0 ? (
              <Pressable
                testID="empty-trash-button"
                accessibilityRole="button"
                hitSlop={8}
                onPress={confirmEmpty}
              >
                <Text style={styles.headerButton}>
                  {t('settings:data_section.trash.empty_trash')}
                </Text>
              </Pressable>
            ) : null,
        }}
      />
      <ErrorText message={trash.error || changeFailed ? t('mobile.error') : null} />
      <FlatList
        data={shown}
        keyExtractor={(row) => row.id}
        renderItem={({ item, index }) => (
          <TrashListRow
            row={item}
            index={index}
            // Under Hidden the next read omits it; until then it shows redacted.
            covered={isCovered(item)}
            daysLeft={trashDaysLeft(item.trashedAt, nowSecs)}
            onRestore={() => void run(trash.restore(item.id))}
            onPurge={() => gated(item, () => confirmPurge(item.id))}
          />
        )}
        ListEmptyComponent={
          trash.loading ? (
            <FadeIn visible={showSpinner} />
          ) : trash.error ? undefined : (
            <View testID="trash-empty" style={styles.empty}>
              <Text style={styles.emptyTitle}>{t('settings:data_section.trash.empty')}</Text>
              <Text style={styles.emptyHint}>{t('settings:data_section.trash.hint')}</Text>
            </View>
          )
        }
      />
      {/* Outside the list: a list ancestor would take the sheet's first tap. */}
      <PasswordSheet
        visible={revealOpen}
        title={t('nav:sidebar.unlock_second_lock')}
        description={t('nav:sidebar.unlock_second_lock_description')}
        placeholder={t('editor:second_lock_prompt.password_placeholder')}
        submitLabel={t('editor:second_lock_prompt.unlock')}
        onSubmit={async (password) => {
          if (!(await session.reveal(password))) return false;
          revealed.current = true;
          return true;
        }}
        onClose={closeReveal}
        onDismissed={runAfterReveal}
      />
    </View>
  );
}

/**
 * One trashed entry. Its buttons share one testID across rows
 * (`restore-button`, `delete-forever-button`): Maestro picks a row with `index`
 * or `childOf: { id: trash-row-<index> }`.
 */
function TrashListRow(props: {
  row: TrashRow;
  index: number;
  /** Locked and not revealed: the title is not shown. */
  covered: boolean;
  daysLeft: number;
  onRestore: () => void;
  onPurge: () => void;
}) {
  const { theme } = useUnistyles();
  return (
    <View testID={`trash-row-${props.index}`} style={styles.row}>
      <View style={styles.rowText}>
        {props.covered ? (
          <View style={styles.lockedTop}>
            <SymbolView name="lock.fill" tintColor={theme.colors.fgMuted} size={14} />
            <Text style={styles.lockedTitle} numberOfLines={1}>
              {t('editor:entry_card.locked_entry')}
            </Text>
          </View>
        ) : (
          <Text style={styles.title} numberOfLines={1}>
            {props.row.title?.trim() || t('nav:media_gallery.untitled')}
          </Text>
        )}
        <Text style={styles.daysLeft}>
          {t('settings:data_section.trash.days_left', { count: props.daysLeft })}
        </Text>
      </View>
      <Pressable
        testID="restore-button"
        accessibilityRole="button"
        accessibilityLabel={t('settings:data_section.trash.restore')}
        hitSlop={8}
        onPress={props.onRestore}
      >
        <SymbolView
          name="arrow.uturn.backward.circle"
          tintColor={theme.colors.accentText}
          size={26}
        />
      </Pressable>
      <Pressable
        testID="delete-forever-button"
        accessibilityRole="button"
        accessibilityLabel={t('settings:data_section.trash.delete_forever')}
        hitSlop={8}
        onPress={props.onPurge}
      >
        <SymbolView name="trash.circle" tintColor={theme.colors.dangerText} size={26} />
      </Pressable>
    </View>
  );
}

/** A spinner that fades in once `visible` turns true. */
function FadeIn({ visible }: { visible: boolean }) {
  const { theme } = useUnistyles();
  const opacity = useRef(new Animated.Value(0)).current;
  useEffect(() => {
    if (!visible) {
      opacity.setValue(0);
      return;
    }
    const fade = Animated.timing(opacity, { toValue: 1, duration: 200, useNativeDriver: true });
    fade.start();
    return () => fade.stop();
  }, [visible, opacity]);
  if (!visible) return null;
  return (
    <Animated.View style={[styles.spinner, { opacity }]}>
      <ActivityIndicator color={theme.colors.fgMuted} />
    </Animated.View>
  );
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
  screen: {
    flex: 1,
  },
  headerButton: {
    fontSize: 17,
    color: theme.colors.dangerText,
  },
  row: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: theme.space(4),
    paddingVertical: theme.space(3),
    paddingHorizontal: theme.space(4),
    borderBottomWidth: StyleSheet.hairlineWidth,
    borderBottomColor: theme.colors.borderSubtle,
  },
  rowText: {
    flex: 1,
    gap: theme.space(1),
  },
  title: {
    fontSize: 17,
    fontWeight: '600',
    color: theme.colors.fg,
  },
  lockedTop: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: theme.space(2),
  },
  lockedTitle: {
    flexShrink: 1,
    fontSize: 17,
    fontWeight: '600',
    color: theme.colors.fgMuted,
  },
  daysLeft: {
    fontSize: 13,
    color: theme.colors.fgMuted,
  },
  empty: {
    gap: theme.space(2),
    paddingVertical: theme.space(10),
    paddingHorizontal: theme.space(6),
  },
  emptyTitle: {
    textAlign: 'center',
    fontSize: 15,
    color: theme.colors.fgSecondary,
  },
  emptyHint: {
    textAlign: 'center',
    fontSize: 13,
    color: theme.colors.fgMuted,
  },
  spinner: {
    paddingVertical: theme.space(6),
    alignItems: 'center',
  },
}));

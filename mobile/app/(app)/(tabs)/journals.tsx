import { useRouter } from 'expo-router';
import { SymbolView } from 'expo-symbols';
import { useEffect, useRef, useState } from 'react';
import {
  ActionSheetIOS,
  ActivityIndicator,
  Alert,
  FlatList,
  Platform,
  Pressable,
  Text,
  View,
} from 'react-native';
import { StyleSheet, useUnistyles } from 'react-native-unistyles';

import type { Journal, MemloreErrorCode } from '@/modules/memlore-core';
import { ErrorText } from '@/src/components/ErrorText';
import { PasswordSheet } from '@/src/components/PasswordSheet';
import { useLockSession } from '@/src/hooks/useLockSession';
import { useJournals } from '@/src/hooks/useOrganize';
import { errorCode } from '@/src/hooks/useVault';
import { t } from '@/src/i18n';

/** How long a load runs before its spinner shows (fast loads never flash it). */
const SPINNER_DELAY_MS = 150;

/**
 * The listed journals. A tap shows the Entries tab filtered by that journal;
 * a long press renames or deletes it, and locks or unlocks it while a
 * second-lock password is set; the header button creates one.
 *
 * Second lock: a locked journal shows its name with a lock (Covered or
 * revealed; under Hidden the engine omits it). While not revealed, a tap or
 * any menu action on it asks for the reveal first and runs after a
 * successful one; a call refused with ERR_SECOND_LOCK_REQUIRED does the same
 * and is tried once more.
 */
export default function JournalsScreen() {
  const router = useRouter();
  const { theme } = useUnistyles();
  const list = useJournals();
  const session = useLockSession();
  // The reveal sheet is open; what runs once it closed after a successful reveal.
  const [revealOpen, setRevealOpen] = useState(false);
  const afterReveal = useRef<(() => void) | null>(null);
  const revealed = useRef(false);
  // Under Hidden (show existence off) the next read omits a locked journal;
  // until then it is left out.
  const shown = session.showExistence
    ? list.journals
    : list.journals.filter((journal) => !journal.isLocked || session.revealed);
  const showSpinner = useDelayed(list.loading && shown.length === 0, SPINNER_DELAY_MS);

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

  /** Runs `action` at once, or after a reveal for a locked journal not revealed. */
  function gated(journal: Journal, action: () => void) {
    if (journal.isLocked && !session.revealed) askReveal(action);
    else action();
  }

  function openJournal(journal: Journal) {
    gated(journal, () => router.navigate({ pathname: '/', params: { journal: journal.id } }));
  }

  /**
   * Locks or unlocks the journal. A refusal for want of the reveal shows the
   * sheet and tries once more after a reveal.
   */
  async function changeLock(journal: Journal, lock: boolean, retried = false) {
    try {
      await list.setLocked(journal.id, lock);
    } catch (e) {
      const code = errorCode(e);
      // The lock screen shows, or the journal is gone (the list reloaded).
      if (code === 'ERR_LOCKED' || code === 'ERR_NOT_FOUND') return;
      if (code === 'ERR_SECOND_LOCK_REQUIRED' && !retried) {
        askReveal(() => void changeLock(journal, lock, true));
        return;
      }
      Alert.alert(t('mobile.journal_lock.failed'));
    }
  }

  function newJournal() {
    Alert.prompt(
      t('settings:journals_section.form.title_new'),
      undefined,
      [
        { text: t('settings:journals_section.form.cancel'), style: 'cancel' },
        {
          text: t('settings:journals_section.form.create'),
          // Trimmed by the hook; an empty name creates nothing.
          onPress: (name?: string) => void list.createJournal(name ?? ''),
        },
      ],
      'plain-text',
    );
  }

  function rename(journal: Journal) {
    Alert.prompt(
      t('mobile.journal.rename_title'),
      undefined,
      [
        { text: t('settings:journals_section.form.cancel'), style: 'cancel' },
        {
          text: t('settings:journals_section.form.save'),
          onPress: (name?: string) => void list.renameJournal(journal.id, name ?? ''),
        },
      ],
      'plain-text',
      journal.name,
    );
  }

  /**
   * Asks first, with desktop's copy and count (live entries, locked ones
   * included), then deletes the journal and every entry in it.
   */
  async function confirmDelete(journal: Journal, retried = false) {
    const count = await list.entryCount(journal.id);
    if (typeof count !== 'number') {
      if (count === 'ERR_SECOND_LOCK_REQUIRED' && !retried) {
        askReveal(() => void confirmDelete(journal, true));
      } else {
        showFailure(count);
      }
      return;
    }
    let body = `${journal.name} ${t('settings:journals_section.delete_removed')}`;
    if (count > 0) {
      const noun = t('settings:journals_section.entry', { count });
      body += ` ${t('settings:journals_section.delete_with_entries', { count, noun })}`;
    }
    body += `\n\n${t('settings:journals_section.delete_sync_warning')}`;
    Alert.alert(t('settings:journals_section.delete_title'), body, [
      { text: t('settings:journals_section.form.cancel'), style: 'cancel' },
      {
        text: t('settings:journals_section.delete'),
        style: 'destructive',
        onPress: () => void remove(journal.id),
      },
    ]);
  }

  /** Already confirmed: a refusal for want of the reveal deletes after one. */
  async function remove(id: string, retried = false) {
    const code = await list.deleteJournal(id);
    if (code === 'ERR_SECOND_LOCK_REQUIRED' && !retried) {
      askReveal(() => void remove(id, true));
    } else if (code !== null) {
      showFailure(code);
    }
  }

  function showFailure(code: MemloreErrorCode) {
    // A lock shows the unlock screen instead.
    if (code === 'ERR_LOCKED') return;
    Alert.alert(
      code === 'ERR_LAST_JOURNAL'
        ? t('settings:journals_section.errors.last_journal')
        : t('settings:journals_section.delete_failed'),
    );
  }

  /** Lock journal / Unlock journal, only while a second-lock password is set. */
  function lockLabel(journal: Journal): string | null {
    if (!session.enabled) return null;
    return journal.isLocked ? t('mobile.journal_lock.unlock') : t('mobile.journal_lock.lock');
  }

  /**
   * Rename, Delete (then its confirmation), Lock journal / Unlock journal
   * (index 2, when `lockLabel`), Cancel. On a locked journal not revealed,
   * each asks for the reveal first.
   */
  function openMenu(journal: Journal) {
    const lock = lockLabel(journal);
    // Decided now, as the menu shows it: a refresh before the tap never flips it.
    const wantLock = !journal.isLocked;
    const options = [t('mobile.journal.rename'), t('settings:journals_section.delete')];
    if (lock !== null) options.push(lock);
    options.push(t('settings:journals_section.form.cancel'));
    ActionSheetIOS.showActionSheetWithOptions(
      {
        title: journal.name,
        options,
        destructiveButtonIndex: 1,
        cancelButtonIndex: options.length - 1,
      },
      (index) => {
        if (index === 0) gated(journal, () => rename(journal));
        else if (index === 1) gated(journal, () => void confirmDelete(journal));
        else if (index === 2 && lock !== null) {
          gated(journal, () => void changeLock(journal, wantLock));
        }
      },
    );
  }

  return (
    <View style={styles.screen}>
      <View style={styles.header}>
        <Pressable
          testID="new-journal-button"
          accessibilityRole="button"
          accessibilityLabel={t('settings:journals_section.form.title_new')}
          hitSlop={8}
          onPress={newJournal}
        >
          <SymbolView name="plus" tintColor={theme.colors.accentText} size={24} />
        </Pressable>
      </View>
      <ErrorText message={list.error ? t('mobile.error') : null} />
      <FlatList
        data={shown}
        keyExtractor={(journal) => journal.id}
        renderItem={({ item, index }) => {
          const lock = lockLabel(item);
          return (
            <Pressable
              // Locked and not revealed (Covered): `journal-row-locked`, like `entry-row-locked`.
              testID={
                item.isLocked && !session.revealed ? 'journal-row-locked' : `journal-row-${index}`
              }
              accessibilityRole="button"
              accessibilityValue={
                item.isLocked ? { text: t('nav:media_gallery.badge.locked') } : undefined
              }
              accessibilityActions={[
                { name: 'rename', label: t('mobile.journal.rename') },
                { name: 'delete', label: t('settings:journals_section.delete') },
                ...(lock !== null ? [{ name: 'lock', label: lock }] : []),
              ]}
              onAccessibilityAction={(event) => {
                const action = event.nativeEvent.actionName;
                if (action === 'rename') gated(item, () => rename(item));
                else if (action === 'delete') gated(item, () => void confirmDelete(item));
                else if (action === 'lock') {
                  const wantLock = !item.isLocked;
                  gated(item, () => void changeLock(item, wantLock));
                }
              }}
              onPress={() => openJournal(item)}
              onLongPress={() => openMenu(item)}
              style={styles.row}
            >
              {item.isLocked ? (
                <SymbolView name="lock.fill" tintColor={theme.colors.fgMuted} size={14} />
              ) : null}
              <Text style={styles.name} numberOfLines={1}>
                {item.name}
              </Text>
              <SymbolView name="chevron.right" tintColor={theme.colors.fgMuted} size={14} />
            </Pressable>
          );
        }}
        ListEmptyComponent={
          showSpinner ? (
            <ActivityIndicator style={styles.spinner} color={theme.colors.fgMuted} />
          ) : undefined
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
  header: {
    flexDirection: 'row',
    justifyContent: 'flex-end',
    paddingVertical: theme.space(2),
    paddingHorizontal: theme.space(4),
  },
  row: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: theme.space(2),
    paddingVertical: theme.space(3.5),
    paddingHorizontal: theme.space(4),
    borderBottomWidth: StyleSheet.hairlineWidth,
    borderBottomColor: theme.colors.borderSubtle,
  },
  name: {
    flex: 1,
    fontSize: 17,
    color: theme.colors.fg,
  },
  spinner: {
    paddingVertical: theme.space(6),
  },
}));

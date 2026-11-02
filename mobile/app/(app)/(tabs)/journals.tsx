import { useRouter } from 'expo-router';
import { SymbolView } from 'expo-symbols';
import { useEffect, useState } from 'react';
import {
  ActionSheetIOS,
  ActivityIndicator,
  Alert,
  FlatList,
  Pressable,
  Text,
  View,
} from 'react-native';
import { StyleSheet, useUnistyles } from 'react-native-unistyles';

import type { Journal, MemloreErrorCode } from '@/modules/memlore-core';
import { ErrorText } from '@/src/components/ErrorText';
import { useJournals } from '@/src/hooks/useOrganize';
import { t } from '@/src/i18n';

/** How long a load runs before its spinner shows (fast loads never flash it). */
const SPINNER_DELAY_MS = 150;

/**
 * The listed journals. A tap shows the Entries tab filtered by that journal;
 * a long press renames or deletes it; the header button creates one.
 */
export default function JournalsScreen() {
  const router = useRouter();
  const { theme } = useUnistyles();
  const list = useJournals();
  const showSpinner = useDelayed(list.loading && list.journals.length === 0, SPINNER_DELAY_MS);

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
  async function confirmDelete(journal: Journal) {
    const count = await list.entryCount(journal.id);
    if (typeof count !== 'number') {
      showFailure(count);
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

  async function remove(id: string) {
    const code = await list.deleteJournal(id);
    if (code !== null) showFailure(code);
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

  function openMenu(journal: Journal) {
    ActionSheetIOS.showActionSheetWithOptions(
      {
        title: journal.name,
        options: [
          t('mobile.journal.rename'),
          t('settings:journals_section.delete'),
          t('settings:journals_section.form.cancel'),
        ],
        destructiveButtonIndex: 1,
        cancelButtonIndex: 2,
      },
      (index) => {
        if (index === 0) rename(journal);
        else if (index === 1) void confirmDelete(journal);
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
        data={list.journals}
        keyExtractor={(journal) => journal.id}
        renderItem={({ item, index }) => (
          <Pressable
            testID={`journal-row-${index}`}
            accessibilityRole="button"
            accessibilityActions={[
              { name: 'rename', label: t('mobile.journal.rename') },
              { name: 'delete', label: t('settings:journals_section.delete') },
            ]}
            onAccessibilityAction={(event) => {
              if (event.nativeEvent.actionName === 'rename') rename(item);
              else if (event.nativeEvent.actionName === 'delete') void confirmDelete(item);
            }}
            onPress={() => router.navigate({ pathname: '/', params: { journal: item.id } })}
            onLongPress={() => openMenu(item)}
            style={styles.row}
          >
            <Text style={styles.name} numberOfLines={1}>
              {item.name}
            </Text>
            <SymbolView name="chevron.right" tintColor={theme.colors.fgMuted} size={14} />
          </Pressable>
        )}
        ListEmptyComponent={
          showSpinner ? (
            <ActivityIndicator style={styles.spinner} color={theme.colors.fgMuted} />
          ) : undefined
        }
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

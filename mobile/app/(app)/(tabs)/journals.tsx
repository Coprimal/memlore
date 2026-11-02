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

import type { Journal } from '@/modules/memlore-core';
import { ErrorText } from '@/src/components/ErrorText';
import { useJournals } from '@/src/hooks/useOrganize';
import { t } from '@/src/i18n';

/** How long a load runs before its spinner shows (fast loads never flash it). */
const SPINNER_DELAY_MS = 150;

/**
 * The listed journals. A tap shows the Entries tab filtered by that journal;
 * a long press renames it; the header button creates one.
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

  function openMenu(journal: Journal) {
    ActionSheetIOS.showActionSheetWithOptions(
      {
        title: journal.name,
        options: [t('mobile.journal.rename'), t('settings:journals_section.form.cancel')],
        cancelButtonIndex: 1,
      },
      (index) => {
        if (index === 0) rename(journal);
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
            accessibilityActions={[{ name: 'rename', label: t('mobile.journal.rename') }]}
            onAccessibilityAction={(event) => {
              if (event.nativeEvent.actionName === 'rename') rename(item);
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

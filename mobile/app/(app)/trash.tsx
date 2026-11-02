import { Stack } from 'expo-router';
import { SymbolView } from 'expo-symbols';
import { useEffect, useRef, useState } from 'react';
import { ActivityIndicator, Alert, Animated, FlatList, Pressable, Text, View } from 'react-native';
import { StyleSheet, useUnistyles } from 'react-native-unistyles';

import type { TrashRow } from '@/modules/memlore-core';
import { ErrorText } from '@/src/components/ErrorText';
import { trashDaysLeft, useTrash } from '@/src/hooks/useOrganize';
import { t } from '@/src/i18n';

/** How long a load runs before its spinner shows (fast loads never flash it). */
const SPINNER_DELAY_MS = 150;

/**
 * Recently deleted entries, most recently trashed first. Each row restores at
 * once or deletes for good after a confirmation; Empty trash deletes the rows
 * shown when the user tapped it, after a confirmation with their count.
 */
export default function TrashScreen() {
  const trash = useTrash();
  // A restore or delete that was refused (the list has reloaded already).
  const [changeFailed, setChangeFailed] = useState(false);
  const showSpinner = useDelayed(trash.loading && trash.rows.length === 0, SPINNER_DELAY_MS);
  const nowSecs = Math.floor(Date.now() / 1000);

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
    // The rows on screen now: the count the user confirms is what gets deleted.
    const ids = trash.rows.map((row) => row.id);
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
            trash.rows.length > 0 ? (
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
        data={trash.rows}
        keyExtractor={(row) => row.id}
        renderItem={({ item, index }) => (
          <TrashListRow
            row={item}
            index={index}
            daysLeft={trashDaysLeft(item.trashedAt, nowSecs)}
            onRestore={() => void run(trash.restore(item.id))}
            onPurge={() => confirmPurge(item.id)}
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
  daysLeft: number;
  onRestore: () => void;
  onPurge: () => void;
}) {
  const { theme } = useUnistyles();
  return (
    <View testID={`trash-row-${props.index}`} style={styles.row}>
      <View style={styles.rowText}>
        <Text style={styles.title} numberOfLines={1}>
          {props.row.title?.trim() || t('nav:media_gallery.untitled')}
        </Text>
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

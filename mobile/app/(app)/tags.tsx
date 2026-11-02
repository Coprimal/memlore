import { Stack } from 'expo-router';
import { SymbolView } from 'expo-symbols';
import { useEffect, useMemo, useRef, useState } from 'react';
import {
  ActionSheetIOS,
  ActivityIndicator,
  Alert,
  Animated,
  FlatList,
  Pressable,
  Text,
  View,
} from 'react-native';
import { StyleSheet, useUnistyles } from 'react-native-unistyles';

import type { MemloreErrorCode, Tag } from '@/modules/memlore-core';
import { ErrorText } from '@/src/components/ErrorText';
import { useTags } from '@/src/hooks/useOrganize';
import { t } from '@/src/i18n';

/** How long a load runs before its spinner shows (fast loads never flash it). */
const SPINNER_DELAY_MS = 150;

/**
 * Every tag, sorted by name. A long press renames it (a name another tag
 * holds is refused, no merge) or deletes it after a confirmation, which takes
 * it off every entry, locked ones included (desktop parity).
 */
export default function TagsScreen() {
  const list = useTags();
  const { theme } = useUnistyles();
  // A rename or delete that failed for another reason than its name.
  const [changeFailed, setChangeFailed] = useState(false);
  const showSpinner = useDelayed(list.loading && list.tags.length === 0, SPINNER_DELAY_MS);
  // Sorted here: a rename replaces the tag in place.
  const tags = useMemo(
    () => [...list.tags].sort((a, b) => a.name.localeCompare(b.name)),
    [list.tags],
  );

  function showRefusal(code: MemloreErrorCode) {
    if (code === 'ERR_NAME_TAKEN') Alert.alert(t('nav:tag_editor.errors.name_taken'));
    else if (code === 'ERR_FORMAT') Alert.alert(t('nav:tag_editor.name_required'));
    // A lock shows the unlock screen instead.
    else if (code !== 'ERR_LOCKED') setChangeFailed(true);
  }

  async function saveName(tag: Tag, name: string) {
    setChangeFailed(false);
    const result = await list.renameTag(tag.id, name);
    if (typeof result === 'string') showRefusal(result);
  }

  async function remove(tag: Tag) {
    setChangeFailed(false);
    const code = await list.deleteTag(tag.id);
    if (code !== null) showRefusal(code);
  }

  function rename(tag: Tag) {
    Alert.prompt(
      t('nav:tag_editor.edit_title'),
      undefined,
      [
        { text: t('nav:tag_editor.cancel'), style: 'cancel' },
        {
          text: t('nav:tag_editor.save'),
          // Trimmed by the hook; an empty name is refused without a call.
          onPress: (name?: string) => void saveName(tag, name ?? ''),
        },
      ],
      'plain-text',
      tag.name,
    );
  }

  function confirmDelete(tag: Tag) {
    // Desktop renders the <b> through <Trans>; an alert shows plain text.
    // Stripped before the name goes in, so a name with "<b>" stays whole.
    const body = t('nav:tag_editor.delete_body')
      .replace(/<\/?b>/g, '')
      .replace('{{name}}', () => tag.name);
    Alert.alert(t('nav:tag_editor.delete_title'), body, [
      { text: t('nav:tag_editor.cancel'), style: 'cancel' },
      {
        text: t('nav:tag_editor.delete'),
        style: 'destructive',
        onPress: () => void remove(tag),
      },
    ]);
  }

  function openMenu(tag: Tag) {
    ActionSheetIOS.showActionSheetWithOptions(
      {
        title: tag.name,
        options: [
          t('mobile.journal.rename'),
          t('nav:tag_editor.delete'),
          t('nav:tag_editor.cancel'),
        ],
        destructiveButtonIndex: 1,
        cancelButtonIndex: 2,
      },
      (index) => {
        if (index === 0) rename(tag);
        else if (index === 1) confirmDelete(tag);
      },
    );
  }

  return (
    <View style={styles.screen}>
      <Stack.Screen options={{ title: t('nav:tags') }} />
      <ErrorText message={list.error || changeFailed ? t('mobile.error') : null} />
      <FlatList
        data={tags}
        keyExtractor={(tag) => tag.id}
        renderItem={({ item, index }) => (
          <Pressable
            testID={`tag-row-${index}`}
            accessibilityRole="button"
            accessibilityActions={[
              { name: 'rename', label: t('mobile.journal.rename') },
              { name: 'delete', label: t('nav:tag_editor.delete') },
            ]}
            onAccessibilityAction={(event) => {
              if (event.nativeEvent.actionName === 'rename') rename(item);
              else if (event.nativeEvent.actionName === 'delete') confirmDelete(item);
            }}
            onLongPress={() => openMenu(item)}
            style={styles.row}
          >
            <SymbolView name="tag" tintColor={theme.colors.fgMuted} size={16} />
            <Text style={styles.name} numberOfLines={1}>
              {item.name}
            </Text>
          </Pressable>
        )}
        ListEmptyComponent={
          list.loading ? (
            <FadeIn visible={showSpinner} />
          ) : list.error ? undefined : (
            <View testID="tags-empty" style={styles.empty}>
              <Text style={styles.emptyTitle}>{t('nav:tags_view.empty_title')}</Text>
            </View>
          )
        }
      />
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
  row: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: theme.space(3),
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
  empty: {
    paddingVertical: theme.space(10),
    paddingHorizontal: theme.space(6),
  },
  emptyTitle: {
    textAlign: 'center',
    fontSize: 15,
    color: theme.colors.fgSecondary,
  },
  spinner: {
    paddingVertical: theme.space(6),
    alignItems: 'center',
  },
}));

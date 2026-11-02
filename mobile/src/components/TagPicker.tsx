import { SymbolView } from 'expo-symbols';
import { useEffect, useMemo, useState } from 'react';
import {
  ActivityIndicator,
  FlatList,
  Modal,
  Pressable,
  Text,
  TextInput,
  View,
} from 'react-native';
import { StyleSheet, useUnistyles } from 'react-native-unistyles';

import type { Tag } from '@/modules/memlore-core';
import { ErrorText } from '@/src/components/ErrorText';
import { useTags } from '@/src/hooks/useOrganize';
import { t } from '@/src/i18n';

/** How long a load runs before its spinner shows (fast loads never flash it). */
const SPINNER_DELAY_MS = 150;

/** Case- and diacritic-blind form for search ("viec" finds "Việc"). */
function fold(value: string): string {
  return value.normalize('NFD').replace(/[̀-ͯ]/g, '').toLowerCase();
}

type Props = {
  visible: boolean;
  onClose: () => void;
  /** The ids of the tags on the entry; they show a checkmark. */
  selectedTagIds: readonly string[];
  onAdd: (tag: Tag) => void;
  onRemove: (tagId: string) => void;
};

/**
 * Puts tags on or off an entry: search, toggle, and create (get-or-create,
 * then added). The checkmarks follow `selectedTagIds`, so a toggle shows as
 * soon as the parent's optimistic update lands. The list is read each time
 * the sheet opens (its content mounts only while open).
 */
export function TagPicker(props: Props) {
  return (
    <Modal
      testID="tag-picker"
      visible={props.visible}
      animationType="slide"
      presentationStyle="pageSheet"
      onRequestClose={props.onClose}
    >
      <TagPickerContent {...props} />
    </Modal>
  );
}

function TagPickerContent(props: Props) {
  const { theme } = useUnistyles();
  const { tags, loading, error, createTag } = useTags();
  const [query, setQuery] = useState('');
  const [creating, setCreating] = useState(false);
  const showSpinner = useDelayed(loading && tags.length === 0, SPINNER_DELAY_MS);

  const trimmed = query.trim();
  const shown = useMemo(() => {
    const needle = fold(trimmed);
    return [...tags]
      .sort((a, b) => a.name.localeCompare(b.name, undefined, { sensitivity: 'base' }))
      .filter((tag) => fold(tag.name).includes(needle));
  }, [tags, trimmed]);
  // As on the desktop: only a case-blind exact name hides Create ("cafe" may
  // sit next to "café").
  const canCreate =
    trimmed !== '' && !tags.some((tag) => tag.name.toLowerCase() === trimmed.toLowerCase());
  const selected = new Set(props.selectedTagIds);

  function toggle(tag: Tag) {
    if (selected.has(tag.id)) props.onRemove(tag.id);
    else props.onAdd(tag);
  }

  async function create() {
    if (!canCreate || creating) return;
    setCreating(true);
    const tag = await createTag(trimmed);
    if (tag) {
      props.onAdd(tag);
      setQuery('');
    }
    setCreating(false);
  }

  return (
    <View style={styles.sheet}>
      <View style={styles.header}>
        <Text style={styles.heading} accessibilityRole="header">
          {t('editor:tags_popover.dialog_aria')}
        </Text>
        <Pressable
          testID="tag-picker-done"
          accessibilityRole="button"
          onPress={props.onClose}
          hitSlop={8}
        >
          <Text style={styles.done}>{t('mobile.done')}</Text>
        </Pressable>
      </View>
      <TextInput
        testID="tag-search-input"
        style={styles.search}
        value={query}
        onChangeText={setQuery}
        onSubmitEditing={() => void create()}
        placeholder={t('editor:tags_popover.search_placeholder')}
        placeholderTextColor={theme.colors.fgMuted}
        accessibilityLabel={t('editor:tags_popover.search_aria')}
        autoCapitalize="none"
        autoCorrect={false}
        clearButtonMode="while-editing"
        returnKeyType="done"
      />
      <FlatList
        data={shown}
        keyExtractor={(tag) => tag.id}
        keyboardShouldPersistTaps="handled"
        automaticallyAdjustKeyboardInsets
        renderItem={({ item, index }) => (
          <TagRow
            tag={item}
            index={index}
            checked={selected.has(item.id)}
            onPress={() => toggle(item)}
          />
        )}
        ListHeaderComponent={
          canCreate ? (
            <Pressable
              testID="tag-create-row"
              accessibilityRole="button"
              disabled={creating}
              onPress={() => void create()}
              style={[styles.row, creating && styles.busy]}
            >
              <SymbolView name="plus" tintColor={theme.colors.accentText} size={16} />
              <Text style={styles.createLabel} numberOfLines={1}>
                {t('editor:tags_popover.create', { name: trimmed })}
              </Text>
            </Pressable>
          ) : undefined
        }
        ListEmptyComponent={
          loading ? (
            showSpinner ? (
              <View style={styles.spinner}>
                <ActivityIndicator color={theme.colors.fgMuted} />
              </View>
            ) : undefined
          ) : error ? (
            <View style={styles.message}>
              <ErrorText message={t('mobile.error')} />
            </View>
          ) : tags.length === 0 && trimmed === '' ? (
            <Text style={styles.empty}>{t('editor:tags_popover.empty')}</Text>
          ) : undefined
        }
      />
    </View>
  );
}

function TagRow(props: { tag: Tag; index: number; checked: boolean; onPress: () => void }) {
  const { theme } = useUnistyles();
  return (
    <Pressable
      testID={`tag-option-${props.index}`}
      accessibilityRole="checkbox"
      accessibilityState={{ checked: props.checked }}
      onPress={props.onPress}
      style={styles.row}
    >
      <Text style={styles.name} numberOfLines={1}>
        {props.tag.name}
      </Text>
      {props.checked ? (
        <SymbolView name="checkmark" tintColor={theme.colors.accentText} size={16} />
      ) : null}
    </Pressable>
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
  sheet: {
    flex: 1,
    backgroundColor: theme.colors.panel1,
  },
  header: {
    flexDirection: 'row',
    alignItems: 'center',
    justifyContent: 'space-between',
    paddingTop: theme.space(4),
    paddingBottom: theme.space(2),
    paddingHorizontal: theme.space(4),
  },
  heading: {
    fontSize: 17,
    fontWeight: '600',
    color: theme.colors.fg,
  },
  done: {
    fontSize: 17,
    fontWeight: '600',
    color: theme.colors.accentText,
  },
  search: {
    marginHorizontal: theme.space(4),
    marginBottom: theme.space(2),
    paddingVertical: theme.space(2.5),
    paddingHorizontal: theme.space(3),
    borderRadius: theme.radii.sm,
    backgroundColor: theme.colors.surfaceControl,
    fontSize: 17,
    color: theme.colors.fg,
  },
  row: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: theme.space(2),
    paddingVertical: theme.space(3),
    paddingHorizontal: theme.space(4),
    borderBottomWidth: StyleSheet.hairlineWidth,
    borderBottomColor: theme.colors.borderSubtle,
  },
  busy: {
    opacity: 0.4,
  },
  name: {
    flex: 1,
    fontSize: 17,
    color: theme.colors.fg,
  },
  createLabel: {
    flex: 1,
    fontSize: 17,
    color: theme.colors.accentText,
  },
  message: {
    padding: theme.space(4),
  },
  empty: {
    paddingVertical: theme.space(10),
    paddingHorizontal: theme.space(4),
    textAlign: 'center',
    fontSize: 15,
    color: theme.colors.fgMuted,
  },
  spinner: {
    paddingVertical: theme.space(6),
    alignItems: 'center',
  },
}));

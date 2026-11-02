import { SymbolView } from 'expo-symbols';
import { type ReactNode, useState } from 'react';
import { ActionSheetIOS, Pressable, ScrollView, Text, View } from 'react-native';
import { StyleSheet, useUnistyles } from 'react-native-unistyles';

import type { Emotion, Tag } from '@/modules/memlore-core';
import { DateSheet } from '@/src/components/DateSheet';
import { JournalPicker } from '@/src/components/JournalPicker';
import { TagPicker } from '@/src/components/TagPicker';
import type { EntryMeta as Meta } from '@/src/hooks/useEntries';
import { useJournals } from '@/src/hooks/useOrganize';
import { activeLocale, t } from '@/src/i18n';

/**
 * The row's fixed height: it scrolls sideways instead of wrapping, so the
 * editor below always starts at the same place (the placeholder leaves the
 * same room under its title).
 */
export const META_ROW_HEIGHT = 44;

// Compact, so the row's controls (add tag last) fit a phone's width: the year
// only when it is not the current one.
const DATE_PARTS: Intl.DateTimeFormatOptions = {
  day: 'numeric',
  month: 'short',
  hour: 'numeric',
  minute: '2-digit',
};
const dateFormat = new Intl.DateTimeFormat(activeLocale, DATE_PARTS);
const dateWithYearFormat = new Intl.DateTimeFormat(activeLocale, {
  ...DATE_PARTS,
  year: 'numeric',
});

function formatEntryDate(unixSeconds: number): string {
  const date = new Date(unixSeconds * 1000);
  return date.getFullYear() === new Date().getFullYear()
    ? dateFormat.format(date)
    : dateWithYearFormat.format(date);
}

/** The action sheet's order; the index after them is Clear, then Cancel. */
const EMOTIONS: readonly Emotion[] = ['bad', 'neutral', 'good'];

/** What `useEntryActions` gives the row (every action shows its change at once). */
export type EntryMetaActions = {
  setEmotion: (emotion: Emotion | null) => Promise<boolean>;
  toggleFavorite: () => Promise<boolean>;
  setDate: (unixSeconds: number) => Promise<boolean>;
  move: (journalId: string) => Promise<boolean>;
  addTag: (tag: Tag) => Promise<boolean>;
  removeTag: (tagId: string) => Promise<boolean>;
};

type Props = {
  /** null until the list's cached row or `getEntry` is known: every control is off. */
  meta: Meta | null;
  actions: EntryMetaActions;
  /** The tags are known (`getEntry` answered), so adding one can't hide one already on. */
  canAddTags: boolean;
};

/**
 * The entry's meta row under its title: date, emotion, favorite, journal and
 * tags, each opening its sheet or picker. Changes do not touch the document,
 * so they stay on for a read-only entry too.
 */
export function EntryMeta({ meta, actions, canAddTags }: Props) {
  const { theme } = useUnistyles();
  const { journals } = useJournals();
  const [sheet, setSheet] = useState<'date' | 'journal' | 'tags' | null>(null);
  const off = meta === null;

  function pickEmotion() {
    const labels = EMOTIONS.map((e) => t(`editor:emotion.${e}`));
    ActionSheetIOS.showActionSheetWithOptions(
      {
        title: t('editor:emotion.title'),
        options: [...labels, t('editor:emotion.clear'), t('common:confirm_dialog.cancel')],
        cancelButtonIndex: EMOTIONS.length + 1,
      },
      (index) => {
        if (index < EMOTIONS.length) void actions.setEmotion(EMOTIONS[index]);
        else if (index === EMOTIONS.length) void actions.setEmotion(null);
      },
    );
  }

  const emotion = meta?.emotion ?? null;
  const emotionColor =
    emotion === 'good'
      ? theme.colors.emotionGood
      : emotion === 'neutral'
        ? theme.colors.emotionNeutral
        : theme.colors.emotionBad;
  const journalName = journals.find((j) => j.id === meta?.journalId)?.name ?? null;
  const favorite = meta?.isFavorite ?? false;

  return (
    <View style={styles.root}>
      <ScrollView
        horizontal
        showsHorizontalScrollIndicator={false}
        keyboardShouldPersistTaps="handled"
        contentContainerStyle={styles.content}
      >
        <Pill
          testID="entry-date-pill"
          label={t('editor:pills.date_edit_aria')}
          disabled={off}
          onPress={() => setSheet('date')}
        >
          <SymbolView name="calendar" tintColor={theme.colors.fgSecondary} size={14} />
          <Text style={styles.pillText} numberOfLines={1}>
            {meta ? formatEntryDate(meta.entryDate) : ''}
          </Text>
        </Pill>
        <Pill
          testID="emotion-pill"
          label={t('editor:pills.mood_aria')}
          disabled={off}
          onPress={pickEmotion}
        >
          {emotion ? (
            <>
              <View style={[styles.dot, { backgroundColor: emotionColor }]} />
              <Text style={styles.pillText} numberOfLines={1}>
                {t(`editor:emotion.${emotion}`)}
              </Text>
            </>
          ) : (
            <Text style={styles.pillMuted} numberOfLines={1}>
              {t('editor:pills.mood_add')}
            </Text>
          )}
        </Pill>
        <Pressable
          testID="favorite-button"
          accessibilityRole="button"
          accessibilityLabel={t(favorite ? 'editor:pills.fav_remove' : 'editor:pills.fav_add')}
          accessibilityState={{ selected: favorite, disabled: off }}
          disabled={off}
          hitSlop={6}
          onPress={() => void actions.toggleFavorite()}
          style={[styles.pill, off && styles.off]}
        >
          <SymbolView
            name={favorite ? 'star.fill' : 'star'}
            tintColor={favorite ? theme.colors.warning : theme.colors.fgSecondary}
            size={16}
          />
        </Pressable>
        <Pill
          testID="journal-pill"
          label={
            journalName
              ? t('editor:pills.journal_switch_aria', { name: journalName })
              : t('editor:pills.journal_aria')
          }
          disabled={off}
          onPress={() => setSheet('journal')}
        >
          <SymbolView name="book.closed" tintColor={theme.colors.fgSecondary} size={14} />
          <Text style={journalName ? styles.pillText : styles.pillMuted} numberOfLines={1}>
            {journalName ?? t('editor:pills.journal_aria')}
          </Text>
        </Pill>
        {meta?.tags.map((tag, index) => (
          <View key={tag.id} testID={`tag-pill-${index}`} style={styles.pill}>
            <Text style={styles.pillText} numberOfLines={1}>
              {tag.name}
            </Text>
            <Pressable
              testID={`tag-pill-remove-${index}`}
              accessibilityRole="button"
              accessibilityLabel={t('editor:pills.tag_remove', { name: tag.name })}
              hitSlop={8}
              onPress={() => void actions.removeTag(tag.id)}
            >
              <SymbolView name="xmark" tintColor={theme.colors.fgMuted} size={10} />
            </Pressable>
          </View>
        ))}
        <Pill
          testID="add-tag-button"
          label={t('editor:pills.tag_list_aria')}
          disabled={off || !canAddTags}
          onPress={() => setSheet('tags')}
        >
          <Text style={styles.pillMuted} numberOfLines={1}>
            {t('editor:pills.tag_add')}
          </Text>
        </Pill>
      </ScrollView>
      {meta ? (
        <>
          <DateSheet
            visible={sheet === 'date'}
            value={meta.entryDate}
            onApply={(secs) => {
              setSheet(null);
              void actions.setDate(secs);
            }}
            onClose={() => setSheet(null)}
          />
          <JournalPicker
            visible={sheet === 'journal'}
            title={t('editor:pills.journal_switch_menu_aria')}
            selectedId={meta.journalId}
            onSelect={(journalId) => {
              if (journalId === null) return;
              setSheet(null);
              if (journalId !== meta.journalId) void actions.move(journalId);
            }}
            onClose={() => setSheet(null)}
          />
          <TagPicker
            visible={sheet === 'tags'}
            onClose={() => setSheet(null)}
            selectedTagIds={meta.tags.map((tag) => tag.id)}
            onAdd={(tag) => void actions.addTag(tag)}
            onRemove={(tagId) => void actions.removeTag(tagId)}
          />
        </>
      ) : null}
    </View>
  );
}

function Pill(props: {
  testID: string;
  label: string;
  disabled: boolean;
  onPress: () => void;
  children: ReactNode;
}) {
  return (
    <Pressable
      testID={props.testID}
      accessibilityRole="button"
      accessibilityLabel={props.label}
      accessibilityState={{ disabled: props.disabled }}
      disabled={props.disabled}
      onPress={props.onPress}
      style={[styles.pill, props.disabled && styles.off]}
    >
      {props.children}
    </Pressable>
  );
}

const styles = StyleSheet.create((theme) => ({
  root: {
    height: META_ROW_HEIGHT,
  },
  content: {
    alignItems: 'center',
    gap: theme.space(2),
    paddingHorizontal: 12,
  },
  pill: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: theme.space(1.5),
    height: 30,
    paddingHorizontal: theme.space(2.5),
    borderRadius: theme.radii.xl,
    backgroundColor: theme.colors.surfaceControl,
  },
  off: {
    opacity: 0.4,
  },
  pillText: {
    fontSize: 14,
    color: theme.colors.fgSecondary,
  },
  pillMuted: {
    fontSize: 14,
    color: theme.colors.fgMuted,
  },
  dot: {
    width: 8,
    height: 8,
    borderRadius: 4,
  },
}));

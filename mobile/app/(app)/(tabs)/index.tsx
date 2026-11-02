import { useLocalSearchParams, useRouter } from 'expo-router';
import { SymbolView } from 'expo-symbols';
import { useEffect, useRef, useState } from 'react';
import { ActivityIndicator, Animated, Pressable, SectionList, Text, View } from 'react-native';
import { StyleSheet, useUnistyles } from 'react-native-unistyles';

import type { EntryRow } from '@/modules/memlore-core';
import { ErrorText } from '@/src/components/ErrorText';
import { JournalPicker } from '@/src/components/JournalPicker';
import { useEntries } from '@/src/hooks/useEntries';
import { useJournals } from '@/src/hooks/useOrganize';
import { activeLocale, t } from '@/src/i18n';
import { type DayKey, groupByDay } from '@/src/lib/dayGroups';

const timeFormat = new Intl.DateTimeFormat(activeLocale, { hour: 'numeric', minute: '2-digit' });
// The desktop's day labels (formatGroupDateLabel in src/lib/dates.ts).
const dayFormat = new Intl.DateTimeFormat(activeLocale, {
  weekday: 'short',
  month: 'short',
  day: 'numeric',
});
const dayYearFormat = new Intl.DateTimeFormat(activeLocale, {
  year: 'numeric',
  month: 'short',
  day: 'numeric',
});

/** How long a load runs before its spinner shows (fast loads never flash it). */
const SPINNER_DELAY_MS = 150;

function dayLabel(key: DayKey, now: Date): string {
  if (key === 'today') return t('nav:entry_list.today');
  if (key === 'yesterday') return t('nav:entry_list.yesterday');
  const [year, month, day] = key.split('-').map(Number);
  const date = new Date(year, month - 1, day);
  return (year === now.getFullYear() ? dayFormat : dayYearFormat).format(date);
}

/**
 * The entries of every listed journal, or of the one in the `journal` param
 * (set by the Journals tab and the filter picker), newest first, grouped by day.
 */
export default function EntriesScreen() {
  const router = useRouter();
  const params = useLocalSearchParams<{ journal?: string }>();
  // '' (the picker's "All journals") and a missing param both mean no filter.
  const journalId = typeof params.journal === 'string' && params.journal ? params.journal : null;
  const list = useEntries(journalId);
  const journalList = useJournals();
  const { journals } = journalList;
  const [pickerOpen, setPickerOpen] = useState(false);
  // The new-entry button dims at once while `createEntry` runs.
  const [creating, setCreating] = useState(false);
  const showSpinner = useDelayed(list.loading && list.rows.length === 0, SPINNER_DELAY_MS);

  const now = new Date();
  let index = 0;
  const sections = groupByDay(list.rows, now).map((group) => ({
    key: group.key,
    title: dayLabel(group.key, now),
    // The flat index across sections, for `entry-row-<index>`.
    data: group.items.map((row) => ({ row, index: index++ })),
  }));

  // The journal the list last started loading, so an older failure never counts.
  const loadingFor = useRef<string | null | undefined>(undefined);
  // A filter journal that is gone (deleted, or locked on the desktop) shows All
  // journals: the list read for it failed and the journal list, read on the
  // same focus, no longer has it. (useEntries reports a failure, not its code;
  // its ERR_NOT_FOUND means the same: the journal is not listed.)
  useEffect(() => {
    if (list.loading) {
      loadingFor.current = journalId;
      return;
    }
    const gone =
      journalId !== null &&
      loadingFor.current === journalId &&
      list.error &&
      !journalList.loading &&
      !journalList.error &&
      !journals.some((journal) => journal.id === journalId);
    if (gone) router.setParams({ journal: '' });
  }, [journalId, list.loading, list.error, journalList.loading, journalList.error, journals]);

  // The filtered journal's name; "All journals" too until the list knows it.
  const filterLabel =
    journals.find((journal) => journal.id === journalId)?.name ??
    t('nav:entry_list.filter.journal.all');

  function selectJournal(id: string | null) {
    setPickerOpen(false);
    router.setParams({ journal: id ?? '' });
  }

  // The tap navigates in the same frame; the entry screen shows the cached row.
  function open(id: string) {
    router.push({ pathname: '/entry/[id]', params: { id } });
  }

  async function newEntry() {
    if (creating) return;
    setCreating(true);
    try {
      const id = await list.createEntry();
      if (id !== null) open(id);
    } finally {
      setCreating(false);
    }
  }

  return (
    <View style={styles.screen}>
      <View style={styles.header}>
        <View style={styles.headerStart}>
          <Pressable
            testID="journal-filter-button"
            accessibilityRole="button"
            accessibilityLabel={t('nav:entry_list.filter.journal.menu_label')}
            accessibilityValue={{ text: filterLabel }}
            hitSlop={8}
            onPress={() => setPickerOpen(true)}
            style={styles.filter}
          >
            <Text style={styles.filterLabel} numberOfLines={1}>
              {filterLabel}
            </Text>
            <FilterChevron />
          </Pressable>
          <Text testID="entry-count" style={styles.count}>
            {list.total === null ? '…' : t('nav:entry_list.entry_count', { count: list.total })}
          </Text>
        </View>
        <View style={styles.headerEnd}>
          <Pressable
            testID="search-button"
            accessibilityRole="button"
            accessibilityLabel={t('mobile.search.title')}
            hitSlop={8}
            onPress={() => router.push('/search')}
          >
            <SearchIcon />
          </Pressable>
          <Pressable
            testID="new-entry-button"
            accessibilityRole="button"
            accessibilityLabel={t('nav:new_entry')}
            hitSlop={8}
            disabled={creating}
            onPress={() => void newEntry()}
            style={creating ? styles.busy : undefined}
          >
            <NewEntryIcon />
          </Pressable>
        </View>
      </View>
      <ErrorText message={list.error ? t('mobile.error') : null} />
      <SectionList
        sections={sections}
        keyExtractor={(item) => item.row.id}
        renderItem={({ item }) => (
          <EntryListRow row={item.row} index={item.index} onPress={() => open(item.row.id)} />
        )}
        renderSectionHeader={({ section }) => (
          <Text style={styles.sectionHeader}>{section.title}</Text>
        )}
        stickySectionHeadersEnabled
        onEndReached={() => void list.loadMore()}
        onEndReachedThreshold={0.5}
        ListEmptyComponent={
          list.loading ? (
            <FadeIn visible={showSpinner} />
          ) : list.error ? undefined : (
            <Text style={styles.empty}>{t('nav:entry_list.empty.no_entries')}</Text>
          )
        }
        ListFooterComponent={list.loadingMore ? <DelayedFooterSpinner /> : undefined}
      />
      <JournalPicker
        visible={pickerOpen}
        title={t('nav:entry_list.filter.journal.menu_label')}
        selectedId={journalId}
        includeAll
        onSelect={selectJournal}
        onClose={() => setPickerOpen(false)}
      />
    </View>
  );
}

function FilterChevron() {
  const { theme } = useUnistyles();
  return <SymbolView name="chevron.down" tintColor={theme.colors.fgSecondary} size={13} />;
}

function SearchIcon() {
  const { theme } = useUnistyles();
  return <SymbolView name="magnifyingglass" tintColor={theme.colors.accentText} size={22} />;
}

function NewEntryIcon() {
  const { theme } = useUnistyles();
  return <SymbolView name="square.and.pencil" tintColor={theme.colors.accentText} size={24} />;
}

function EntryListRow(props: { row: EntryRow; index: number; onPress: () => void }) {
  const { theme } = useUnistyles();
  const { row } = props;
  return (
    <Pressable
      testID={`entry-row-${props.index}`}
      accessibilityRole="button"
      onPress={props.onPress}
      style={styles.row}
    >
      <View style={styles.rowTop}>
        <Text style={styles.title} numberOfLines={1}>
          {row.title?.trim() || t('nav:media_gallery.untitled')}
        </Text>
        {row.emotion ? (
          <View
            accessibilityLabel={t('editor:entry_card.mood_aria', {
              mood: t(`editor:emotion.${row.emotion}`),
            })}
            style={[
              styles.dot,
              row.emotion === 'good'
                ? styles.dotGood
                : row.emotion === 'neutral'
                  ? styles.dotNeutral
                  : styles.dotBad,
            ]}
          />
        ) : null}
        {row.isFavorite && (
          <SymbolView
            accessibilityLabel={t('editor:entry_card.favorite_aria')}
            name="star.fill"
            tintColor={theme.colors.warning}
            size={14}
          />
        )}
        <Text style={styles.time}>{timeFormat.format(row.entryDate * 1000)}</Text>
      </View>
      {row.previewText ? (
        <Text style={styles.preview} numberOfLines={2}>
          {row.previewText}
        </Text>
      ) : null}
    </Pressable>
  );
}

function DelayedFooterSpinner() {
  const visible = useDelayed(true, SPINNER_DELAY_MS);
  return <FadeIn visible={visible} />;
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
  header: {
    flexDirection: 'row',
    alignItems: 'center',
    justifyContent: 'space-between',
    paddingVertical: theme.space(2),
    paddingHorizontal: theme.space(4),
  },
  headerStart: {
    flex: 1,
    gap: theme.space(0.5),
  },
  headerEnd: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: theme.space(5),
  },
  filter: {
    flexDirection: 'row',
    alignItems: 'center',
    alignSelf: 'flex-start',
    gap: theme.space(1),
  },
  filterLabel: {
    flexShrink: 1,
    fontSize: 17,
    fontWeight: '600',
    color: theme.colors.fg,
  },
  count: {
    fontSize: 15,
    color: theme.colors.fgSecondary,
  },
  busy: {
    opacity: 0.4,
  },
  sectionHeader: {
    paddingVertical: theme.space(1.5),
    paddingHorizontal: theme.space(4),
    fontSize: 13,
    fontWeight: '600',
    color: theme.colors.fgMuted,
    backgroundColor: theme.colors.surfaceGroupHeader,
  },
  row: {
    gap: theme.space(1),
    paddingVertical: theme.space(3),
    paddingHorizontal: theme.space(4),
    borderBottomWidth: StyleSheet.hairlineWidth,
    borderBottomColor: theme.colors.borderSubtle,
  },
  rowTop: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: theme.space(2),
  },
  title: {
    flex: 1,
    fontSize: 17,
    fontWeight: '600',
    color: theme.colors.fg,
  },
  dot: {
    width: theme.space(2.5),
    height: theme.space(2.5),
    borderRadius: theme.space(1.25),
  },
  dotGood: {
    backgroundColor: theme.colors.emotionGood,
  },
  dotNeutral: {
    backgroundColor: theme.colors.emotionNeutral,
  },
  dotBad: {
    backgroundColor: theme.colors.emotionBad,
  },
  time: {
    fontSize: 13,
    color: theme.colors.fgMuted,
  },
  preview: {
    fontSize: 15,
    lineHeight: 20,
    color: theme.colors.fgSecondary,
  },
  empty: {
    paddingVertical: theme.space(10),
    textAlign: 'center',
    fontSize: 15,
    color: theme.colors.fgMuted,
  },
  spinner: {
    paddingVertical: theme.space(6),
    alignItems: 'center',
  },
}));

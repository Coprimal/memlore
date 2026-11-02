import { Stack, useRouter } from 'expo-router';
import { SymbolView } from 'expo-symbols';
import { type ReactNode, useEffect, useRef, useState } from 'react';
import {
  ActivityIndicator,
  Animated,
  FlatList,
  Pressable,
  ScrollView,
  Text,
  TextInput,
  View,
} from 'react-native';
import { StyleSheet, useUnistyles } from 'react-native-unistyles';

import type { Emotion, SearchResult } from '@/modules/memlore-core';
import { ErrorText } from '@/src/components/ErrorText';
import { JournalPicker } from '@/src/components/JournalPicker';
import { TagPicker } from '@/src/components/TagPicker';
import { useJournals } from '@/src/hooks/useOrganize';
import { type SearchUiFilters, useSearch } from '@/src/hooks/useSearch';
import { activeLocale, t } from '@/src/i18n';
import { highlightParts, RANGE_PRESETS } from '@/src/lib/searchText';

/** How long a load runs before its spinner shows (fast loads never flash it). */
const SPINNER_DELAY_MS = 150;

const EMOTIONS: readonly Emotion[] = ['bad', 'neutral', 'good'];

const NO_FILTERS: SearchUiFilters = { journalId: null, tagIds: [], emotions: [], range: 'all' };

const dateFormat = new Intl.DateTimeFormat(activeLocale, {
  year: 'numeric',
  month: 'short',
  day: 'numeric',
});

/**
 * Keyword search over the listed entries, accent-insensitive, with filters
 * (journal, tags, emotions, time range). Matches are highlighted in each
 * result's title and preview; a tap opens the entry.
 */
export default function SearchScreen() {
  const router = useRouter();
  const { theme } = useUnistyles();
  const [query, setQuery] = useState('');
  const [filters, setFilters] = useState<SearchUiFilters>(NO_FILTERS);
  const [journalPickerOpen, setJournalPickerOpen] = useState(false);
  const [tagPickerOpen, setTagPickerOpen] = useState(false);
  const search = useSearch(query, filters);
  const { journals } = useJournals();
  const showSpinner = useDelayed(search.loading && search.results.length === 0, SPINNER_DELAY_MS);
  const trimmed = query.trim();

  const journalName = journals.find((journal) => journal.id === filters.journalId)?.name;
  const tagCount = filters.tagIds.length;

  function selectJournal(id: string | null) {
    setJournalPickerOpen(false);
    setFilters((f) => ({ ...f, journalId: id }));
  }

  function toggleEmotion(emotion: Emotion) {
    setFilters((f) => ({
      ...f,
      emotions: f.emotions.includes(emotion)
        ? f.emotions.filter((e) => e !== emotion)
        : [...f.emotions, emotion],
    }));
  }

  // The tap navigates in the same frame; the entry screen loads the entry.
  function open(id: string) {
    router.push({ pathname: '/entry/[id]', params: { id } });
  }

  return (
    <View style={styles.screen}>
      <Stack.Screen options={{ title: t('mobile.search.title') }} />
      <TextInput
        testID="search-input"
        style={styles.input}
        value={query}
        onChangeText={setQuery}
        autoFocus
        placeholder={t('mobile.search.input_placeholder')}
        placeholderTextColor={theme.colors.fgMuted}
        accessibilityLabel={t('mobile.search.input_aria')}
        autoCapitalize="none"
        autoCorrect={false}
        clearButtonMode="while-editing"
        returnKeyType="search"
      />
      <ScrollView
        horizontal
        showsHorizontalScrollIndicator={false}
        keyboardShouldPersistTaps="handled"
        style={styles.pillRow}
        contentContainerStyle={styles.pills}
      >
        <Pill
          testID="search-journal-filter"
          role="button"
          on={filters.journalId !== null}
          onPress={() => setJournalPickerOpen(true)}
        >
          <PillText on={filters.journalId !== null}>
            {journalName ?? t('mobile.search.filter.journal')}
          </PillText>
          <SymbolView name="chevron.down" tintColor={theme.colors.fgMuted} size={11} />
        </Pill>
        <Pill
          testID="search-tag-filter"
          role="button"
          on={tagCount > 0}
          onPress={() => setTagPickerOpen(true)}
        >
          <PillText on={tagCount > 0}>
            {tagCount > 0
              ? t('mobile.search.filter.tags_count', { count: tagCount })
              : t('mobile.search.filter.tags')}
          </PillText>
          <SymbolView name="chevron.down" tintColor={theme.colors.fgMuted} size={11} />
        </Pill>
        {EMOTIONS.map((emotion) => {
          const on = filters.emotions.includes(emotion);
          return (
            <Pill
              key={emotion}
              testID={`search-emotion-${emotion}`}
              role="checkbox"
              on={on}
              onPress={() => toggleEmotion(emotion)}
            >
              <View
                style={[
                  styles.dot,
                  emotion === 'good'
                    ? styles.dotGood
                    : emotion === 'neutral'
                      ? styles.dotNeutral
                      : styles.dotBad,
                ]}
              />
              <PillText on={on}>{t(`editor:emotion.${emotion}`)}</PillText>
            </Pill>
          );
        })}
      </ScrollView>
      <ScrollView
        horizontal
        showsHorizontalScrollIndicator={false}
        keyboardShouldPersistTaps="handled"
        style={styles.pillRow}
        contentContainerStyle={styles.pills}
        accessibilityLabel={t('nav:entry_list.filter.range_menu_label')}
      >
        {RANGE_PRESETS.map((range) => {
          const on = filters.range === range;
          return (
            <Pill
              key={range}
              testID={`search-range-${range}`}
              role="radio"
              on={on}
              onPress={() => setFilters((f) => ({ ...f, range }))}
            >
              <PillText on={on}>{t(`nav:entry_list.filter.range.${range}`)}</PillText>
            </Pill>
          );
        })}
      </ScrollView>
      <ErrorText message={search.error ? t('mobile.error') : null} />
      <FlatList
        data={search.results}
        keyExtractor={(result) => result.id}
        keyboardShouldPersistTaps="handled"
        keyboardDismissMode="on-drag"
        automaticallyAdjustKeyboardInsets
        renderItem={({ item, index }) => (
          <ResultRow
            result={item}
            index={index}
            query={trimmed}
            onPress={() => open(item.id)}
          />
        )}
        ListEmptyComponent={
          !search.active ? (
            <View testID="search-hint" style={styles.message}>
              <Text style={styles.messageTitle}>{t('mobile.search.placeholder_title')}</Text>
              <Text style={styles.messageHint}>{t('mobile.search.placeholder_hint')}</Text>
            </View>
          ) : search.loading ? (
            <FadeIn visible={showSpinner} />
          ) : search.error ? undefined : (
            <View testID="search-empty" style={styles.message}>
              <Text style={styles.messageTitle}>
                {trimmed
                  ? t('mobile.search.empty_keyword', { query: trimmed })
                  : t('mobile.search.empty_filtered_no_query')}
              </Text>
            </View>
          )
        }
      />
      <JournalPicker
        visible={journalPickerOpen}
        title={t('mobile.search.filter.journal')}
        selectedId={filters.journalId}
        includeAll
        onSelect={selectJournal}
        onClose={() => setJournalPickerOpen(false)}
      />
      <TagPicker
        visible={tagPickerOpen}
        onClose={() => setTagPickerOpen(false)}
        selectedTagIds={filters.tagIds}
        onAdd={(tag) =>
          setFilters((f) =>
            f.tagIds.includes(tag.id) ? f : { ...f, tagIds: [...f.tagIds, tag.id] },
          )
        }
        onRemove={(tagId) =>
          setFilters((f) => ({ ...f, tagIds: f.tagIds.filter((id) => id !== tagId) }))
        }
        allowCreate={false}
      />
    </View>
  );
}

function Pill(props: {
  testID: string;
  role: 'button' | 'checkbox' | 'radio';
  on: boolean;
  onPress: () => void;
  children: ReactNode;
}) {
  return (
    <Pressable
      testID={props.testID}
      accessibilityRole={props.role}
      accessibilityState={
        props.role === 'checkbox'
          ? { checked: props.on }
          : props.role === 'radio'
            ? { selected: props.on }
            : undefined
      }
      onPress={props.onPress}
      style={[styles.pill, props.on && styles.pillOn]}
    >
      {props.children}
    </Pressable>
  );
}

function PillText(props: { on: boolean; children: ReactNode }) {
  return (
    <Text style={props.on ? styles.pillTextOn : styles.pillText} numberOfLines={1}>
      {props.children}
    </Text>
  );
}

/** `text` with the query's words marked. */
function Highlighted(props: { text: string; query: string }) {
  return highlightParts(props.text, props.query).map((part, i) =>
    part.match ? (
      <Text key={i} style={styles.match}>
        {part.text}
      </Text>
    ) : (
      part.text
    ),
  );
}

function ResultRow(props: {
  result: SearchResult;
  index: number;
  query: string;
  onPress: () => void;
}) {
  const { result } = props;
  const title = result.title?.trim();
  return (
    <Pressable
      testID={`search-result-${props.index}`}
      accessibilityRole="button"
      onPress={props.onPress}
      style={styles.row}
    >
      <View style={styles.rowTop}>
        <Text style={styles.title} numberOfLines={1}>
          {title ? (
            <Highlighted text={title} query={props.query} />
          ) : (
            t('nav:media_gallery.untitled')
          )}
        </Text>
        <Text style={styles.date}>{dateFormat.format(result.entryDate * 1000)}</Text>
      </View>
      {result.previewText ? (
        <Text style={styles.preview} numberOfLines={2}>
          <Highlighted text={result.previewText} query={props.query} />
        </Text>
      ) : null}
    </Pressable>
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
  input: {
    marginHorizontal: theme.space(4),
    marginTop: theme.space(2),
    marginBottom: theme.space(2),
    paddingVertical: theme.space(2.5),
    paddingHorizontal: theme.space(3),
    borderRadius: theme.radii.sm,
    backgroundColor: theme.colors.surfaceControl,
    fontSize: 17,
    color: theme.colors.fg,
  },
  pillRow: {
    flexGrow: 0,
  },
  pills: {
    alignItems: 'center',
    gap: theme.space(2),
    paddingHorizontal: theme.space(4),
    paddingBottom: theme.space(2),
  },
  pill: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: theme.space(1.5),
    height: 30,
    paddingHorizontal: theme.space(3),
    borderRadius: theme.radii.xl,
    backgroundColor: theme.colors.surfaceControl,
  },
  pillOn: {
    backgroundColor: theme.colors.accentSoft,
  },
  pillText: {
    fontSize: 14,
    color: theme.colors.fgSecondary,
  },
  pillTextOn: {
    fontSize: 14,
    fontWeight: '600',
    color: theme.colors.accentText,
  },
  dot: {
    width: 8,
    height: 8,
    borderRadius: 4,
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
  date: {
    fontSize: 13,
    color: theme.colors.fgMuted,
  },
  preview: {
    fontSize: 15,
    lineHeight: 20,
    color: theme.colors.fgSecondary,
  },
  match: {
    backgroundColor: theme.colors.accentSoft,
    color: theme.colors.accentText,
  },
  message: {
    gap: theme.space(1),
    paddingVertical: theme.space(10),
    paddingHorizontal: theme.space(6),
    alignItems: 'center',
  },
  messageTitle: {
    textAlign: 'center',
    fontSize: 15,
    color: theme.colors.fgSecondary,
  },
  messageHint: {
    textAlign: 'center',
    fontSize: 13,
    color: theme.colors.fgMuted,
  },
  spinner: {
    paddingVertical: theme.space(6),
    alignItems: 'center',
  },
}));

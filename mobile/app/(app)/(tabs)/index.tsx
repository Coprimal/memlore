import { useLocalSearchParams, useRouter } from 'expo-router';
import { SymbolView } from 'expo-symbols';
import { useEffect, useRef, useState } from 'react';
import {
  ActivityIndicator,
  Animated,
  Platform,
  Pressable,
  SectionList,
  Text,
  View,
} from 'react-native';
import { StyleSheet, useUnistyles } from 'react-native-unistyles';

import type { EntryRow, Template } from '@/modules/memlore-core';
import { ErrorText } from '@/src/components/ErrorText';
import { JournalPicker } from '@/src/components/JournalPicker';
import { PasswordSheet } from '@/src/components/PasswordSheet';
import { TemplatePicker } from '@/src/components/TemplatePicker';
import { useEntries } from '@/src/hooks/useEntries';
import { useLockSession } from '@/src/hooks/useLockSession';
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
 * While an invisible vault is open, an eye in the header closes it; a new
 * entry goes into the filtered journal (a vault journal included).
 */
export default function EntriesScreen() {
  const router = useRouter();
  const params = useLocalSearchParams<{ journal?: string }>();
  // '' (the picker's "All journals") and a missing param both mean no filter.
  const journalId = typeof params.journal === 'string' && params.journal ? params.journal : null;
  const list = useEntries(journalId);
  const { journals } = useJournals();
  const session = useLockSession();
  const [pickerOpen, setPickerOpen] = useState(false);
  const [templatePickerOpen, setTemplatePickerOpen] = useState(false);
  // Both new-entry buttons dim at once while `createEntry` runs.
  const [creating, setCreating] = useState(false);
  // The covered row whose reveal sheet is open; the entry it opens once revealed.
  const [revealFor, setRevealFor] = useState<string | null>(null);
  const openAfterReveal = useRef<string | null>(null);
  const showSpinner = useDelayed(list.loading && list.rows.length === 0, SPINNER_DELAY_MS);
  // The vault indicator was tapped: it and the vault's rows go in the tap's
  // frame, while the close saves the open edits first.
  const [closingVault, setClosingVault] = useState(false);
  const vaultShown = session.vaultOpen && !closingVault;

  const now = new Date();
  // A locked row while not revealed is redacted by the engine (Covered). Under
  // Hidden (show existence off) the next read omits it; until then it is left out.
  // So is an invisible row once its vault closed.
  const listed = list.rows.filter((row) => !row.isInvisible || vaultShown);
  const shown = session.showExistence
    ? listed
    : listed.filter((row) => !row.isLocked || session.revealed);
  let index = 0;
  const sections = groupByDay(shown, now).map((group) => ({
    key: group.key,
    title: dayLabel(group.key, now),
    // The flat index across sections, for `entry-row-<index>`.
    data: group.items.map((row) => ({ row, index: index++ })),
  }));

  // The journal the list last started loading, so an older failure never counts.
  const loadingFor = useRef<string | null | undefined>(undefined);
  // A filter journal that is gone (deleted), or locked and not revealed (a
  // re-lock, or locked here or on the desktop) shows All journals: its list
  // read answered ERR_NOT_FOUND (Hidden, or gone) or ERR_SECOND_LOCK_REQUIRED
  // (Covered). The new param reloads the list.
  useEffect(() => {
    if (list.loading) {
      loadingFor.current = journalId;
      return;
    }
    const unreadable =
      journalId !== null &&
      loadingFor.current === journalId &&
      (list.failure === 'ERR_SECOND_LOCK_REQUIRED' || list.failure === 'ERR_NOT_FOUND');
    if (unreadable) router.setParams({ journal: '' });
  }, [journalId, list.loading, list.failure]);

  // The filtered journal's name; "All journals" too until the list knows it.
  // A locked journal left out under Hidden, or an invisible one once its vault
  // closed, never lends its name.
  const unhidden = journals.filter((journal) => !journal.isInvisible || vaultShown);
  const listedJournals = session.showExistence
    ? unhidden
    : unhidden.filter((journal) => !journal.isLocked || session.revealed);
  const filterLabel =
    listedJournals.find((journal) => journal.id === journalId)?.name ??
    t('nav:entry_list.filter.journal.all');

  function selectJournal(id: string | null) {
    setPickerOpen(false);
    router.setParams({ journal: id ?? '' });
  }

  // The tap navigates in the same frame; the entry screen shows the cached row.
  function open(id: string) {
    router.push({ pathname: '/entry/[id]', params: { id } });
  }

  /** A covered row: its sheet opens; no earlier reveal's entry is left to open. */
  function askReveal(id: string) {
    openAfterReveal.current = null;
    setRevealFor(id);
  }

  /**
   * The sheet closes: after a successful reveal, the entry opens once the
   * sheet has slid away (iOS); a push while it is still dismissing may be lost.
   */
  function closeReveal() {
    setRevealFor(null);
    if (Platform.OS !== 'ios') openRevealed();
  }

  function openRevealed() {
    const id = openAfterReveal.current;
    openAfterReveal.current = null;
    if (id !== null) open(id);
  }

  /** Never rejects; the list refetches on the session's new generation. */
  async function closeVault() {
    if (closingVault) return;
    setClosingVault(true);
    try {
      await session.closeVault();
    } finally {
      setClosingVault(false);
    }
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

  /** A blank entry in the filtered journal; the entry screen applies the template. */
  async function newEntryFromTemplate(template: Template) {
    setTemplatePickerOpen(false);
    if (creating) return;
    setCreating(true);
    try {
      const id = await list.createEntry();
      if (id !== null) {
        router.push({ pathname: '/entry/[id]', params: { id, template: template.id } });
      }
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
          {vaultShown ? (
            <Pressable
              testID="invisible-vault-indicator"
              accessibilityRole="button"
              accessibilityLabel={t('nav:sidebar.lock_invisible')}
              hitSlop={8}
              onPress={() => void closeVault()}
            >
              <VaultOpenIcon />
            </Pressable>
          ) : null}
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
            testID="new-from-template-button"
            accessibilityRole="button"
            accessibilityLabel={t('editor:templates.modal_title')}
            hitSlop={8}
            disabled={creating}
            onPress={() => setTemplatePickerOpen(true)}
            style={creating ? styles.busy : undefined}
          >
            <NewFromTemplateIcon />
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
        renderItem={({ item }) => {
          const { row } = item;
          if (row.isLocked && !session.revealed) {
            // The tap shows the reveal sheet at once (no data needed).
            return <CoveredRow row={row} onPress={() => askReveal(row.id)} />;
          }
          return <EntryListRow row={row} index={item.index} onPress={() => open(row.id)} />;
        }}
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
      <TemplatePicker
        visible={templatePickerOpen}
        onSelect={(template) => void newEntryFromTemplate(template)}
        onClose={() => setTemplatePickerOpen(false)}
      />
      {/* Outside the list: a list ancestor would take the sheet's first tap. */}
      <PasswordSheet
        visible={revealFor !== null}
        title={t('nav:sidebar.unlock_second_lock')}
        description={t('nav:sidebar.unlock_second_lock_description')}
        placeholder={t('editor:second_lock_prompt.password_placeholder')}
        submitLabel={t('editor:second_lock_prompt.unlock')}
        onSubmit={async (password) => {
          if (!(await session.reveal(password))) return false;
          openAfterReveal.current = revealFor;
          return true;
        }}
        onClose={closeReveal}
        onDismissed={openRevealed}
      />
    </View>
  );
}

function FilterChevron() {
  const { theme } = useUnistyles();
  return <SymbolView name="chevron.down" tintColor={theme.colors.fgSecondary} size={13} />;
}

function VaultOpenIcon() {
  const { theme } = useUnistyles();
  return <SymbolView name="eye" tintColor={theme.colors.accentText} size={22} />;
}

function SearchIcon() {
  const { theme } = useUnistyles();
  return <SymbolView name="magnifyingglass" tintColor={theme.colors.accentText} size={22} />;
}

function NewFromTemplateIcon() {
  const { theme } = useUnistyles();
  return <SymbolView name="doc.badge.plus" tintColor={theme.colors.accentText} size={22} />;
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

/** A locked entry shown redacted (Covered): a lock and its time, nothing else. */
function CoveredRow(props: { row: EntryRow; onPress: () => void }) {
  const { theme } = useUnistyles();
  return (
    <Pressable
      testID="entry-row-locked"
      accessibilityRole="button"
      onPress={props.onPress}
      style={styles.row}
    >
      <View style={styles.rowTop}>
        <SymbolView name="lock.fill" tintColor={theme.colors.fgMuted} size={14} />
        <Text style={styles.lockedTitle} numberOfLines={1}>
          {t('editor:entry_card.locked_entry')}
        </Text>
        <Text style={styles.time}>{timeFormat.format(props.row.entryDate * 1000)}</Text>
      </View>
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
  lockedTitle: {
    flex: 1,
    fontSize: 17,
    fontWeight: '600',
    color: theme.colors.fgMuted,
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

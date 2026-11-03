import { SymbolView } from 'expo-symbols';
import { useEffect, useRef, useState } from 'react';
import { FlatList, Modal, Platform, Pressable, Text, View } from 'react-native';
import { StyleSheet, useUnistyles } from 'react-native-unistyles';

import { ErrorText } from '@/src/components/ErrorText';
import { PasswordSheet } from '@/src/components/PasswordSheet';
import { useLockSession } from '@/src/hooks/useLockSession';
import { useJournals } from '@/src/hooks/useOrganize';
import { t } from '@/src/i18n';

type Props = {
  visible: boolean;
  title: string;
  /** The checked journal; null checks the "All journals" row. */
  selectedId: string | null;
  /** Adds an "All journals" row first, which selects null. */
  includeAll?: boolean;
  /** A row was tapped; the picker stays open until the caller hides it. */
  onSelect: (journalId: string | null) => void;
  /** Done, or the sheet swiped down. */
  onClose: () => void;
};

/**
 * A page sheet listing the journals with a checkmark on the selected one: the
 * Entries filter, the search filter and the entry's move use it. The list
 * reloads each time the sheet opens. A locked journal shows a lock; choosing
 * it while not revealed (Covered) asks for the second-lock password first and
 * selects it only after a successful reveal.
 */
export function JournalPicker(props: Props) {
  const { journals, error, refresh } = useJournals();
  const session = useLockSession();
  // The covered journal whose reveal sheet is open; the one selected once revealed.
  const [revealFor, setRevealFor] = useState<string | null>(null);
  const selectAfterReveal = useRef<string | null>(null);

  useEffect(() => {
    if (props.visible) void refresh();
  }, [props.visible, refresh]);

  // A reveal left open when the picker hides is not shown again on the next open.
  useEffect(() => {
    if (props.visible) return;
    selectAfterReveal.current = null;
    setRevealFor(null);
  }, [props.visible]);

  // Under Hidden (show existence off) the next read omits a locked journal;
  // until then it is left out. So is an invisible one once its vault closed.
  const listed = journals.filter((journal) => !journal.isInvisible || session.vaultOpen);
  const shown = session.showExistence
    ? listed
    : listed.filter((journal) => !journal.isLocked || session.revealed);

  /**
   * The sheet closes: after a successful reveal, the journal is selected once
   * it has slid away (iOS). Selecting may hide this picker, and hiding it while
   * the sheet inside it is still dismissing leaves a stuck blank sheet.
   */
  function closeReveal() {
    setRevealFor(null);
    if (Platform.OS !== 'ios') selectRevealed();
  }

  function selectRevealed() {
    const id = selectAfterReveal.current;
    selectAfterReveal.current = null;
    if (id !== null) props.onSelect(id);
  }

  return (
    <Modal
      visible={props.visible}
      animationType="slide"
      presentationStyle="pageSheet"
      onRequestClose={props.onClose}
    >
      <View testID="journal-picker" style={styles.sheet}>
        <View style={styles.header}>
          <Text style={styles.title} numberOfLines={1}>
            {props.title}
          </Text>
          <Pressable accessibilityRole="button" hitSlop={8} onPress={props.onClose}>
            <Text style={styles.done}>{t('mobile.done')}</Text>
          </Pressable>
        </View>
        <ErrorText message={error ? t('mobile.error') : null} />
        <FlatList
          data={shown}
          keyExtractor={(journal) => journal.id}
          ListHeaderComponent={
            props.includeAll ? (
              <Option
                testID="journal-option-all"
                label={t('nav:entry_list.filter.journal.all')}
                checked={props.selectedId === null}
                onPress={() => props.onSelect(null)}
              />
            ) : undefined
          }
          renderItem={({ item, index }) => (
            <Option
              testID={`journal-option-${index}`}
              label={item.name}
              locked={item.isLocked}
              checked={props.selectedId === item.id}
              // The reveal sheet shows at once (no data needed).
              onPress={() =>
                item.isLocked && !session.revealed
                  ? setRevealFor(item.id)
                  : props.onSelect(item.id)
              }
            />
          )}
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
            selectAfterReveal.current = revealFor;
            return true;
          }}
          onClose={closeReveal}
          onDismissed={selectRevealed}
        />
      </View>
    </Modal>
  );
}

function Option(props: {
  testID: string;
  label: string;
  /** Second-locked: a lock shows before the name. */
  locked?: boolean;
  checked: boolean;
  onPress: () => void;
}) {
  const { theme } = useUnistyles();
  return (
    <Pressable
      testID={props.testID}
      accessibilityRole="button"
      accessibilityState={{ selected: props.checked }}
      accessibilityValue={props.locked ? { text: t('nav:media_gallery.badge.locked') } : undefined}
      onPress={props.onPress}
      style={styles.row}
    >
      {props.locked ? (
        <SymbolView name="lock.fill" tintColor={theme.colors.fgMuted} size={14} />
      ) : null}
      <Text style={styles.label} numberOfLines={1}>
        {props.label}
      </Text>
      {props.checked && (
        <SymbolView name="checkmark" tintColor={theme.colors.accentText} size={17} />
      )}
    </Pressable>
  );
}

const styles = StyleSheet.create((theme) => ({
  sheet: {
    flex: 1,
    backgroundColor: theme.colors.panel1,
  },
  header: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: theme.space(3),
    paddingVertical: theme.space(4),
    paddingHorizontal: theme.space(4),
  },
  title: {
    flex: 1,
    fontSize: 17,
    fontWeight: '600',
    color: theme.colors.fg,
  },
  done: {
    fontSize: 17,
    fontWeight: '600',
    color: theme.colors.accentText,
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
  label: {
    flex: 1,
    fontSize: 17,
    color: theme.colors.fg,
  },
}));

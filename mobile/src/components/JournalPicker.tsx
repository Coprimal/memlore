import { SymbolView } from 'expo-symbols';
import { useEffect } from 'react';
import { FlatList, Modal, Pressable, Text, View } from 'react-native';
import { StyleSheet, useUnistyles } from 'react-native-unistyles';

import { ErrorText } from '@/src/components/ErrorText';
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
 * A page sheet listing the journals (visible, unlocked) with a checkmark on
 * the selected one: the Entries filter and the entry's move use it. The list
 * reloads each time the sheet opens.
 */
export function JournalPicker(props: Props) {
  const { journals, error, refresh } = useJournals();

  useEffect(() => {
    if (props.visible) void refresh();
  }, [props.visible, refresh]);

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
          data={journals}
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
              checked={props.selectedId === item.id}
              onPress={() => props.onSelect(item.id)}
            />
          )}
        />
      </View>
    </Modal>
  );
}

function Option(props: { testID: string; label: string; checked: boolean; onPress: () => void }) {
  const { theme } = useUnistyles();
  return (
    <Pressable
      testID={props.testID}
      accessibilityRole="button"
      accessibilityState={{ selected: props.checked }}
      onPress={props.onPress}
      style={styles.row}
    >
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

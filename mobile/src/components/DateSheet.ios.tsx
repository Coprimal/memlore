import { DatePicker, Host } from '@expo/ui/swift-ui';
import { datePickerStyle } from '@expo/ui/swift-ui/modifiers';
import { useEffect, useState } from 'react';
import { Modal, Pressable, Text, View } from 'react-native';
import { StyleSheet, useUnistyles } from 'react-native-unistyles';

import { t } from '@/src/i18n';

export type DateSheetProps = {
  visible: boolean;
  /** The entry's date, Unix seconds; the picker starts there each time it opens. */
  value: number;
  /** Apply: the picked date, Unix seconds (the sheet stays open until the caller hides it). */
  onApply: (unixSeconds: number) => void;
  /** Cancel, or the sheet swiped down. */
  onClose: () => void;
};

/**
 * A page sheet with SwiftUI's calendar and time picker (`@expo/ui`). Nothing
 * changes until Apply.
 */
export function DateSheet(props: DateSheetProps) {
  const { theme } = useUnistyles();
  const [draft, setDraft] = useState(() => new Date(props.value * 1000));

  useEffect(() => {
    if (props.visible) setDraft(new Date(props.value * 1000));
  }, [props.visible, props.value]);

  return (
    <Modal
      visible={props.visible}
      animationType="slide"
      presentationStyle="pageSheet"
      onRequestClose={props.onClose}
    >
      <View testID="date-sheet" style={styles.sheet}>
        <View style={styles.header}>
          <Pressable
            testID="date-sheet-cancel"
            accessibilityRole="button"
            hitSlop={8}
            onPress={props.onClose}
          >
            <Text style={styles.cancel}>{t('common:mini_date_picker.cancel')}</Text>
          </Pressable>
          <Text style={styles.title} numberOfLines={1} accessibilityRole="header">
            {t('editor:pills.date_aria')}
          </Text>
          <Pressable
            testID="date-sheet-apply"
            accessibilityRole="button"
            hitSlop={8}
            onPress={() => props.onApply(Math.floor(draft.getTime() / 1000))}
          >
            <Text style={styles.apply}>{t('common:mini_date_picker.apply')}</Text>
          </Pressable>
        </View>
        <Host
          matchContents={{ vertical: true }}
          seedColor={theme.colors.accent}
          style={styles.host}
        >
          <DatePicker
            title={t('editor:pills.date_aria')}
            selection={draft}
            displayedComponents={['date', 'hourAndMinute']}
            onDateChange={setDraft}
            modifiers={[datePickerStyle('graphical')]}
          />
        </Host>
      </View>
    </Modal>
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
    textAlign: 'center',
    fontSize: 17,
    fontWeight: '600',
    color: theme.colors.fg,
  },
  cancel: {
    fontSize: 17,
    color: theme.colors.accentText,
  },
  apply: {
    fontSize: 17,
    fontWeight: '600',
    color: theme.colors.accentText,
  },
  host: {
    alignSelf: 'stretch',
    marginHorizontal: theme.space(4),
  },
}));

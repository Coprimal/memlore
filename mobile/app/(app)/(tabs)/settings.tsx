import { useRouter } from 'expo-router';
import { SymbolView } from 'expo-symbols';
import { Pressable, ScrollView, Text } from 'react-native';
import { StyleSheet, useUnistyles } from 'react-native-unistyles';

import { ActionButton } from '@/src/components/ActionButton';
import { SecurityPanel } from '@/src/components/SecurityPanel';
import { useVault } from '@/src/hooks/useVault';
import { t } from '@/src/i18n';

/** Security (Face ID, auto-lock), Tags, Recently deleted and the manual Lock. */
export default function SettingsScreen() {
  const vault = useVault();
  const router = useRouter();
  const { theme } = useUnistyles();
  return (
    <ScrollView contentContainerStyle={styles.content}>
      <SecurityPanel vault={vault} />
      <Pressable
        testID="tags-row"
        accessibilityRole="button"
        onPress={() => router.push('/tags')}
        style={styles.row}
      >
        <SymbolView name="tag" tintColor={theme.colors.fgSecondary} size={20} />
        <Text style={styles.rowLabel}>{t('nav:tags')}</Text>
        <SymbolView name="chevron.right" tintColor={theme.colors.fgMuted} size={14} />
      </Pressable>
      <Pressable
        testID="trash-row"
        accessibilityRole="button"
        onPress={() => router.push('/trash')}
        style={styles.row}
      >
        <SymbolView name="trash" tintColor={theme.colors.fgSecondary} size={20} />
        <Text style={styles.rowLabel}>{t('settings:data_section.tabs.trash')}</Text>
        <SymbolView name="chevron.right" tintColor={theme.colors.fgMuted} size={14} />
      </Pressable>
      <ActionButton
        testID="lock-button"
        label={t('mobile.lock')}
        disabled={vault.busy}
        onPress={vault.lock}
      />
    </ScrollView>
  );
}

const styles = StyleSheet.create((theme) => ({
  content: {
    gap: theme.space(3),
    paddingVertical: theme.space(6),
    paddingHorizontal: theme.space(6),
  },
  // The Security panel's row shape (SecurityPanel.tsx).
  row: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: theme.space(3),
    paddingVertical: theme.space(3),
    paddingHorizontal: theme.space(4),
    borderWidth: 1,
    borderRadius: theme.radii.xl,
    borderColor: theme.colors.borderDefault,
    backgroundColor: theme.colors.elevated,
  },
  rowLabel: {
    flex: 1,
    fontSize: 17,
    color: theme.colors.fg,
  },
}));

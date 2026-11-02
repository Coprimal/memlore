import { ScrollView } from 'react-native';
import { StyleSheet } from 'react-native-unistyles';

import { ActionButton } from '@/src/components/ActionButton';
import { SecurityPanel } from '@/src/components/SecurityPanel';
import { useVault } from '@/src/hooks/useVault';
import { t } from '@/src/i18n';

/** Security (Face ID, auto-lock) and the manual Lock. */
export default function SettingsScreen() {
  const vault = useVault();
  return (
    <ScrollView contentContainerStyle={styles.content}>
      <SecurityPanel vault={vault} />
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
}));

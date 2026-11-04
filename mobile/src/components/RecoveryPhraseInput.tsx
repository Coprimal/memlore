import { Text, TextInput, View } from 'react-native';
import { StyleSheet, useUnistyles } from 'react-native-unistyles';

import { useScreenPrivacy } from '@/src/hooks/useScreenPrivacy';
import { t } from '@/src/i18n';

/** A recovery phrase is 24 words. */
export const PHRASE_WORD_COUNT = 24;

/** The phrase as the engine reads it: lower case, single spaces, no edges. */
export function normalizePhrase(raw: string): string {
  return raw.trim().toLowerCase().split(/\s+/).filter(Boolean).join(' ');
}

/** How many words the field holds (whatever the spacing). */
export function phraseWordCount(raw: string): number {
  const normalized = normalizePhrase(raw);
  return normalized === '' ? 0 : normalized.split(' ').length;
}

/**
 * The 24-word recovery phrase field (onboarding, re-pair). A secure field:
 * masked, never copied or cut, no autocorrect, suggestions or keyboard
 * learning; pasting still works. It is not rendered while the app is
 * inactive or the screen is recorded, mirrored or AirPlayed (the M1 rule for
 * the phrase); the value stays in the parent's state and is never logged.
 * Only the word count shows under it, and a hint: iOS clears a secure field
 * on the first key typed after it regains focus, so the phrase goes in at once.
 */
export function RecoveryPhraseInput(props: {
  testID: string;
  value: string;
  onChangeText: (text: string) => void;
  autoFocus?: boolean;
  onSubmitEditing?: () => void;
}) {
  const { theme } = useUnistyles();
  const { visible } = useScreenPrivacy();
  const label = t('onboard.passphrase_label');

  return (
    <View style={styles.field}>
      <Text style={styles.label}>{label}</Text>
      {visible ? (
        <TextInput
          testID={props.testID}
          accessibilityLabel={label}
          value={props.value}
          onChangeText={props.onChangeText}
          autoFocus={props.autoFocus}
          onSubmitEditing={props.onSubmitEditing}
          placeholder={t('onboard.passphrase_placeholder')}
          placeholderTextColor={theme.colors.fgMuted}
          style={styles.input}
          // Secure: masked, and iOS offers no copy or cut. Single line (iOS
          // ignores secure entry on a multiline field).
          secureTextEntry
          autoCapitalize="none"
          autoCorrect={false}
          spellCheck={false}
          // No iCloud Keychain prompts: the phrase never goes to the OS.
          autoComplete="off"
          textContentType="none"
        />
      ) : (
        // The same box, empty: nothing of the phrase while it may be seen.
        <View style={styles.hidden} />
      )}
      <Text style={styles.count}>
        {t('onboard.passphrase_word_count', { count: phraseWordCount(props.value) })}
      </Text>
      <Text style={styles.count}>{t('mobile.phrase_hint')}</Text>
    </View>
  );
}

const styles = StyleSheet.create((theme) => ({
  field: {
    gap: theme.space(2),
  },
  label: {
    fontSize: 15,
    fontWeight: '600',
    color: theme.colors.fg,
  },
  // One fixed height for the field and its stand-in, so nothing jumps.
  input: {
    height: theme.space(12),
    fontSize: 17,
    color: theme.colors.fg,
    paddingHorizontal: theme.space(4),
    borderWidth: 1,
    borderRadius: theme.radii.xl,
    borderColor: theme.colors.borderDefault,
    backgroundColor: theme.colors.elevated,
  },
  hidden: {
    height: theme.space(12),
    borderWidth: 1,
    borderRadius: theme.radii.xl,
    borderColor: theme.colors.borderDefault,
    backgroundColor: theme.colors.elevated,
  },
  count: {
    fontSize: 13,
    color: theme.colors.fgMuted,
  },
}));

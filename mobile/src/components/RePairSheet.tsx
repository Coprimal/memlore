import { useEffect, useState } from 'react';
import {
  ActivityIndicator,
  KeyboardAvoidingView,
  Modal,
  Pressable,
  ScrollView,
  Text,
  TextInput,
  View,
} from 'react-native';
import { StyleSheet, useUnistyles } from 'react-native-unistyles';

import { ErrorText } from '@/src/components/ErrorText';
import {
  normalizePhrase,
  PHRASE_WORD_COUNT,
  phraseWordCount,
  RecoveryPhraseInput,
} from '@/src/components/RecoveryPhraseInput';
import { noteActivity } from '@/src/hooks/useLockSession';
import { useSync } from '@/src/hooks/useSync';
import { errorCode } from '@/src/hooks/useVault';
import { t } from '@/src/i18n';

// Mirrors MIN_PASSWORD_LEN in memlore-engine (vault/mod.rs), which still
// validates it (ERR_FORMAT).
const MIN_PASSWORD_LEN = 8;

/** How long a call may run before its spinner shows (fast calls never flash it). */
const SPINNER_DELAY_MS = 150;

/**
 * Re-pairs this phone with the cloud vault through the recovery phrase: after
 * another device rotated the keys, or when an empty phone vault joins the
 * vault in this Drive. The vault takes the cloud's keys and the new password
 * typed here (twice: nothing checks the old one, so a typo would lock the
 * phone out). The phone's entries are kept. The phrase and passwords live
 * only in this sheet's state, are cleared whenever it is hidden and are
 * never logged.
 *
 * Mount it outside any ScrollView, or under one with keyboardShouldPersistTaps
 * "handled" (see `PasswordSheet`).
 */
export function RePairSheet(props: { visible: boolean; onClose: () => void }) {
  const { visible } = props;
  const sync = useSync();
  const [phrase, setPhrase] = useState('');
  const [password, setPassword] = useState('');
  const [confirm, setConfirm] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const spinner = useDelayed(busy, SPINNER_DELAY_MS);

  // One place that forgets the values: whenever the sheet is not shown.
  useEffect(() => {
    if (visible) return;
    setPhrase('');
    setPassword('');
    setConfirm('');
    setError(null);
    setBusy(false);
  }, [visible]);

  function close() {
    if (!busy) props.onClose();
  }

  async function submit() {
    if (busy) return;
    if (phraseWordCount(phrase) !== PHRASE_WORD_COUNT) {
      setError(t('onboard.passphrase_wrong'));
      return;
    }
    if (password.length < MIN_PASSWORD_LEN) {
      setError(t('mobile.password_too_short', { count: MIN_PASSWORD_LEN }));
      return;
    }
    if (password !== confirm) {
      setError(t('force_re_pair.mismatch'));
      return;
    }
    // The tap shows at once: the button disables now, the call runs after.
    setError(null);
    setBusy(true);
    let message: string | null = null;
    try {
      await sync.rePair(normalizePhrase(phrase), password);
    } catch (e) {
      const code = errorCode(e);
      if (code === 'ERR_WRONG_PHRASE') {
        message = t('onboard.passphrase_wrong');
      } else if (code === 'ERR_FORMAT') {
        message = t('mobile.password_too_short', { count: MIN_PASSWORD_LEN });
      } else if (code !== 'ERR_LOCKED') {
        // ERR_LOCKED: the lock screen covers everything; just close.
        message = t('force_re_pair.error_fallback');
      }
    }
    setBusy(false);
    if (message === null) {
      props.onClose();
      return;
    }
    setError(message);
    // The passwords are retyped from scratch; the phrase stays for a retry.
    setPassword('');
    setConfirm('');
  }

  return (
    <Modal
      visible={visible}
      animationType="slide"
      presentationStyle="pageSheet"
      onRequestClose={close}
    >
      <View testID="re-pair-sheet" style={styles.sheet} onTouchStartCapture={noteActivity}>
        <View style={styles.header}>
          <Pressable
            testID="re-pair-cancel"
            accessibilityRole="button"
            hitSlop={8}
            disabled={busy}
            onPress={close}
          >
            <Text style={styles.cancel}>{t('common:confirm_dialog.cancel')}</Text>
          </Pressable>
          <Text style={styles.title} numberOfLines={1}>
            {t('force_re_pair.title')}
          </Text>
          {spinner ? <ActivityIndicator /> : null}
          <Pressable
            testID="re-pair-submit"
            accessibilityRole="button"
            accessibilityState={{ disabled: busy }}
            hitSlop={8}
            disabled={busy}
            onPress={() => void submit()}
            style={busy ? styles.busy : undefined}
          >
            <Text style={styles.submit}>{t('mobile.sync.re_pair_submit')}</Text>
          </Pressable>
        </View>
        <KeyboardAvoidingView behavior="padding" style={styles.body}>
          <ScrollView contentContainerStyle={styles.form} keyboardShouldPersistTaps="handled">
            <Text style={styles.description}>{t('mobile.sync.re_pair_body')}</Text>
            <RecoveryPhraseInput
              testID="re-pair-phrase-input"
              value={phrase}
              onChangeText={setPhrase}
              autoFocus
            />
            <Text style={styles.label}>{t('force_re_pair.new_password_label')}</Text>
            <PasswordInput
              testID="re-pair-password"
              placeholder={t('force_re_pair.new_password_placeholder')}
              value={password}
              onChangeText={setPassword}
            />
            <PasswordInput
              testID="re-pair-password-confirm"
              placeholder={t('force_re_pair.confirm_password_placeholder')}
              value={confirm}
              onChangeText={setConfirm}
              onSubmitEditing={() => void submit()}
            />
            <ErrorText message={error} />
          </ScrollView>
        </KeyboardAvoidingView>
      </View>
    </Modal>
  );
}

function PasswordInput(props: {
  testID: string;
  placeholder: string;
  value: string;
  onChangeText: (text: string) => void;
  onSubmitEditing?: () => void;
}) {
  const { theme } = useUnistyles();
  return (
    <TextInput
      {...props}
      accessibilityLabel={props.placeholder}
      style={styles.input}
      placeholderTextColor={theme.colors.fgMuted}
      secureTextEntry
      autoCapitalize="none"
      autoCorrect={false}
      // No iCloud Keychain prompts: the vault password never goes to the OS.
      autoComplete="off"
      textContentType="none"
    />
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
  submit: {
    fontSize: 17,
    fontWeight: '600',
    color: theme.colors.accentText,
  },
  busy: {
    opacity: 0.5,
  },
  body: {
    flex: 1,
  },
  form: {
    gap: theme.space(3),
    paddingHorizontal: theme.space(4),
    paddingBottom: theme.space(6),
  },
  description: {
    fontSize: 15,
    color: theme.colors.fgSecondary,
  },
  label: {
    fontSize: 15,
    fontWeight: '600',
    color: theme.colors.fg,
    marginTop: theme.space(2),
  },
  input: {
    fontSize: 17,
    color: theme.colors.fg,
    paddingVertical: theme.space(3),
    paddingHorizontal: theme.space(4),
    borderWidth: 1,
    borderRadius: theme.radii.xl,
    borderColor: theme.colors.borderDefault,
    backgroundColor: theme.colors.elevated,
  },
}));

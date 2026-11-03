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
import { noteActivity } from '@/src/hooks/useLockSession';
import { errorCode } from '@/src/hooks/useVault';
import { t } from '@/src/i18n';

// Mirrors MIN_SECOND_LOCK_PASSWORD_LEN and MIN_INVISIBLE_PASSWORD_LEN in
// memlore-engine (ops/), which still validate it. Counted in code points.
export const MIN_LOCK_PASSWORD_LEN = 4;

/** How long a call may run before its spinner shows (fast calls never flash it). */
const SPINNER_DELAY_MS = 150;

export type PasswordSheetProps = {
  visible: boolean;
  title: string;
  description?: string;
  /** Shown in the danger colour, e.g. what a disable clears. */
  warning?: string;
  /** The main field: the password, or the new one when `currentPlaceholder` is set. */
  placeholder: string;
  /** Adds a first field for the current password (a change flow). */
  currentPlaceholder?: string;
  /** Adds a confirm field that must match the main one (a new password). */
  confirmPlaceholder?: string;
  submitLabel: string;
  /**
   * Runs the call. Resolve `false`, or reject with ERR_WRONG_PASSWORD, for a
   * wrong password: the sheet stays open with an inline error. Anything else
   * that resolves closes it. `current` is '' without `currentPlaceholder`.
   */
  onSubmit: (password: string, current: string) => Promise<boolean | void>;
  /** Shown when the call rejects with an unexpected error; default `mobile.error`. */
  failedMessage?: string;
  /** Cancel, the sheet swiped down, or a successful submit. */
  onClose: () => void;
};

/**
 * A page sheet asking for a password (second lock, invisible vault). The
 * values live only in this component's state and are cleared when it closes;
 * they are never logged.
 *
 * Mount it outside any ScrollView, or under one with keyboardShouldPersistTaps
 * "handled": the Modal is still a React child of it, and a "never" ancestor
 * takes the first tap on Submit/Cancel just to dismiss the keyboard.
 */
export function PasswordSheet(props: PasswordSheetProps) {
  const { visible } = props;
  const [password, setPassword] = useState('');
  const [current, setCurrent] = useState('');
  const [confirm, setConfirm] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const spinner = useDelayed(busy, SPINNER_DELAY_MS);

  // One place that forgets the values: whenever the sheet is not shown.
  useEffect(() => {
    if (visible) return;
    setPassword('');
    setCurrent('');
    setConfirm('');
    setError(null);
    setBusy(false);
  }, [visible]);

  function close() {
    if (!busy) props.onClose();
  }

  async function submit() {
    if (busy) return;
    if (props.currentPlaceholder !== undefined && current.length === 0) {
      setError(t('settings:security.second_lock.current_required'));
      return;
    }
    if ([...password].length < MIN_LOCK_PASSWORD_LEN) {
      setError(t('settings:security.second_lock.too_short', { count: MIN_LOCK_PASSWORD_LEN }));
      return;
    }
    if (props.confirmPlaceholder !== undefined && password !== confirm) {
      setError(t('settings:security.second_lock.mismatch'));
      return;
    }
    // The click shows at once: the button disables now, the call runs after.
    setError(null);
    setBusy(true);
    let message: string | null = null;
    try {
      const ok = await props.onSubmit(password, current);
      if (ok === false) message = t('editor:second_lock_prompt.error_incorrect');
    } catch (e) {
      const code = errorCode(e);
      if (code === 'ERR_WRONG_PASSWORD') {
        message = t('editor:second_lock_prompt.error_incorrect');
      } else if (code === 'ERR_FORMAT') {
        message = t('settings:security.second_lock.too_short', { count: MIN_LOCK_PASSWORD_LEN });
      } else if (code !== 'ERR_LOCKED') {
        // ERR_LOCKED: the lock screen covers everything; just close.
        message = props.failedMessage ?? t('mobile.error');
      }
    }
    setBusy(false);
    if (message === null) {
      props.onClose();
      return;
    }
    setError(message);
    // A wrong password is retyped from scratch.
    setPassword('');
    setCurrent('');
    setConfirm('');
  }

  return (
    <Modal
      visible={visible}
      animationType="slide"
      presentationStyle="pageSheet"
      onRequestClose={close}
    >
      {/* Belt and braces: harmless if the root capture already sees these touches. */}
      <View testID="password-sheet" style={styles.sheet} onTouchStartCapture={noteActivity}>
        <View style={styles.header}>
          <Pressable
            testID="password-sheet-cancel"
            accessibilityRole="button"
            hitSlop={8}
            disabled={busy}
            onPress={close}
          >
            <Text style={styles.cancel}>{t('editor:second_lock_prompt.cancel')}</Text>
          </Pressable>
          <Text style={styles.title} numberOfLines={1}>
            {props.title}
          </Text>
          {spinner ? <ActivityIndicator /> : null}
          <Pressable
            testID="password-sheet-submit"
            accessibilityRole="button"
            accessibilityState={{ disabled: busy }}
            hitSlop={8}
            disabled={busy}
            onPress={() => void submit()}
            style={busy ? styles.busy : undefined}
          >
            <Text style={styles.submit}>{props.submitLabel}</Text>
          </Pressable>
        </View>
        <KeyboardAvoidingView behavior="padding" style={styles.body}>
          <ScrollView contentContainerStyle={styles.form} keyboardShouldPersistTaps="handled">
            {props.description ? <Text style={styles.description}>{props.description}</Text> : null}
            {props.warning ? <Text style={styles.warning}>{props.warning}</Text> : null}
            {props.currentPlaceholder !== undefined ? (
              <PasswordInput
                testID="password-sheet-current"
                placeholder={props.currentPlaceholder}
                value={current}
                onChangeText={setCurrent}
                autoFocus
              />
            ) : null}
            <PasswordInput
              testID="password-sheet-input"
              placeholder={props.placeholder}
              value={password}
              onChangeText={setPassword}
              autoFocus={props.currentPlaceholder === undefined}
              onSubmitEditing={
                props.confirmPlaceholder === undefined ? () => void submit() : undefined
              }
            />
            {props.confirmPlaceholder !== undefined ? (
              <PasswordInput
                testID="password-sheet-confirm"
                placeholder={props.confirmPlaceholder}
                value={confirm}
                onChangeText={setConfirm}
                onSubmitEditing={() => void submit()}
              />
            ) : null}
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
  autoFocus?: boolean;
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
      // No iCloud Keychain prompts: these passwords never go to the OS.
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
  warning: {
    fontSize: 15,
    color: theme.colors.dangerText,
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

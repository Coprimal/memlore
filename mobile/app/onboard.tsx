import { useRouter } from 'expo-router';
import { type ReactNode, useEffect, useRef, useState } from 'react';
import {
  ActivityIndicator,
  KeyboardAvoidingView,
  Pressable,
  ScrollView,
  Text,
  TextInput,
  View,
} from 'react-native';
import { StyleSheet, useUnistyles } from 'react-native-unistyles';

import { ActionButton } from '@/src/components/ActionButton';
import { ErrorText } from '@/src/components/ErrorText';
import {
  normalizePhrase,
  PHRASE_WORD_COUNT,
  phraseWordCount,
  RecoveryPhraseInput,
} from '@/src/components/RecoveryPhraseInput';
import { connectErrorMessage } from '@/src/components/SyncPanel';
import { useSync } from '@/src/hooks/useSync';
import { errorCode } from '@/src/hooks/useVault';
import { t } from '@/src/i18n';

// Mirrors MIN_PASSWORD_LEN in memlore-engine (vault/mod.rs), which still
// validates it (ERR_FORMAT).
const MIN_PASSWORD_LEN = 8;

/** How long a call may run before its spinner shows (fast calls never flash it). */
const SPINNER_DELAY_MS = 150;

/** The product name, never translated. */
const PROVIDER = 'Google Drive';

type Step =
  /** The Google sign-in sheet is up (or about to be). */
  | { kind: 'signing_in' }
  /** Cancelled or failed: sign in again, or go back. */
  | { kind: 'sign_in'; error: string | null }
  | { kind: 'no_cloud_vault' }
  | { kind: 'phrase'; sessionId: string }
  | { kind: 'password'; sessionId: string };

/**
 * First launch, "I already use Memlore on another device": Google sign-in,
 * then the recovery phrase of the vault in that Drive, then a new password
 * for this phone. `onboardComplete` leaves the vault unlocked, the app
 * switches to it (the root guard drops this route) and the first sync starts
 * natively. The phrase and passwords live only in this screen's state and are
 * never logged; a session left unfinished is dropped on the way out.
 */
export default function OnboardScreen() {
  const sync = useSync();
  const router = useRouter();
  const [step, setStep] = useState<Step>({ kind: 'signing_in' });
  const [phrase, setPhrase] = useState('');
  const [password, setPassword] = useState('');
  const [confirm, setConfirm] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const spinner = useDelayed(busy || step.kind === 'signing_in', SPINNER_DELAY_MS);
  // The tap that opened this screen asked for the sign-in: start it once.
  const started = useRef(false);
  // The onboarding session Drive left (it holds the token), until it is used.
  const session = useRef<string | null>(null);

  function dropSession() {
    if (session.current === null) return;
    void sync.cancelOnboarding(session.current);
    session.current = null;
  }

  useEffect(() => {
    if (!started.current) {
      started.current = true;
      void signIn();
    }
    return dropSession;
  }, []);

  async function signIn() {
    dropSession();
    setError(null);
    setStep({ kind: 'signing_in' });
    try {
      const outcome = await sync.connect();
      if (outcome === null) {
        setStep({ kind: 'sign_in', error: null });
      } else if (outcome.kind === 'needs_onboarding') {
        session.current = outcome.sessionId;
        setStep({ kind: 'phrase', sessionId: outcome.sessionId });
      } else if (outcome.kind === 'no_cloud_vault') {
        setStep({ kind: 'no_cloud_vault' });
      } else {
        // `connected` and `join_with_phrase` need a vault on this phone.
        setStep({ kind: 'sign_in', error: t('onboard.error_fallback') });
      }
    } catch (e) {
      setStep({ kind: 'sign_in', error: connectErrorMessage(e) });
    }
  }

  // While the sign-in runs, its outcome (and any session) still lands here.
  const locked = busy || step.kind === 'signing_in';

  function leave() {
    if (locked) return;
    if (router.canGoBack()) router.back();
    else router.replace('/vault');
  }

  /** The phrase's own error, or the refusal; anything else is generic. */
  function phraseError(e: unknown): string {
    const code = errorCode(e);
    if (code === 'ERR_WRONG_PHRASE') return t('onboard.passphrase_wrong');
    if (code === 'ERR_VAULT_NOT_EMPTY') return t('mobile.sync.vault_not_empty');
    return t('onboard.error_fallback');
  }

  async function submitPhrase(sessionId: string) {
    if (busy) return;
    if (phraseWordCount(phrase) !== PHRASE_WORD_COUNT) {
      setError(t('onboard.passphrase_wrong'));
      return;
    }
    setError(null);
    setBusy(true);
    try {
      await sync.onboardValidatePhrase(sessionId, normalizePhrase(phrase));
      setStep({ kind: 'password', sessionId });
    } catch (e) {
      setError(phraseError(e));
    } finally {
      setBusy(false);
    }
  }

  async function submitPassword(sessionId: string) {
    if (busy) return;
    if (password.length < MIN_PASSWORD_LEN) {
      setError(t('mobile.password_too_short', { count: MIN_PASSWORD_LEN }));
      return;
    }
    if (password !== confirm) {
      setError(t('set_password.mismatch'));
      return;
    }
    setError(null);
    setBusy(true);
    // The call uses the session: the unmount that follows a success (the root
    // guard replaces this route) must not cancel it. A failure hands it back.
    session.current = null;
    try {
      await sync.onboardComplete(sessionId, normalizePhrase(phrase), password);
    } catch (e) {
      session.current = sessionId;
      const code = errorCode(e);
      setPassword('');
      setConfirm('');
      if (code === 'ERR_WRONG_PHRASE') {
        setStep({ kind: 'phrase', sessionId });
        setError(t('onboard.passphrase_wrong'));
      } else if (code === 'ERR_FORMAT') {
        setError(t('mobile.password_too_short', { count: MIN_PASSWORD_LEN }));
      } else {
        setError(phraseError(e));
      }
      setBusy(false);
    }
  }

  let body: ReactNode;
  switch (step.kind) {
    case 'signing_in':
      body = <Text style={styles.body}>{t('onboarding.drive.progress.exchanging_token')}</Text>;
      break;
    case 'sign_in':
      body = (
        <>
          <Text style={styles.body}>{t('mobile.onboard.intro')}</Text>
          <ErrorText message={step.error} />
          <ActionButton
            testID="onboard-sign-in"
            label={t('onboarding.drive.connect_button', { provider: PROVIDER })}
            onPress={() => void signIn()}
          />
        </>
      );
      break;
    case 'no_cloud_vault':
      body = (
        <>
          <Text testID="onboard-no-vault" style={styles.body}>
            {t('mobile.onboard.no_cloud_vault')}
          </Text>
          <ActionButton testID="onboard-create" label={t('mobile.create_vault')} onPress={leave} />
        </>
      );
      break;
    case 'phrase': {
      const { sessionId } = step;
      body = (
        <>
          <Text style={styles.body}>{t('onboard.body', { provider: PROVIDER })}</Text>
          <RecoveryPhraseInput
            testID="onboard-phrase-input"
            value={phrase}
            onChangeText={setPhrase}
            autoFocus
            onSubmitEditing={() => void submitPhrase(sessionId)}
          />
          <ErrorText message={error} />
          <ActionButton
            testID="onboard-phrase-next"
            label={t('onboard.continue')}
            disabled={busy}
            onPress={() => void submitPhrase(sessionId)}
          />
        </>
      );
      break;
    }
    case 'password': {
      const { sessionId } = step;
      body = (
        <>
          <Text style={styles.subtitle}>{t('onboard.set_password_title')}</Text>
          <Text style={styles.body}>{t('onboard.set_password_body')}</Text>
          <PasswordInput
            testID="onboard-password"
            placeholder={t('set_password.password_label')}
            value={password}
            onChangeText={setPassword}
            autoFocus
          />
          <PasswordInput
            testID="onboard-password-confirm"
            placeholder={t('set_password.confirm_label')}
            value={confirm}
            onChangeText={setConfirm}
            onSubmitEditing={() => void submitPassword(sessionId)}
          />
          <ErrorText message={error} />
          <ActionButton
            testID="onboard-submit"
            label={busy ? t('onboard.unlock_in_progress') : t('onboard.finish')}
            disabled={busy}
            onPress={() => void submitPassword(sessionId)}
          />
        </>
      );
      break;
    }
  }

  // Back from the password goes to the phrase (same session); elsewhere it leaves.
  const back =
    step.kind === 'password'
      ? () => {
          if (busy) return;
          setError(null);
          setPassword('');
          setConfirm('');
          setStep({ kind: 'phrase', sessionId: step.sessionId });
        }
      : leave;

  return (
    <KeyboardAvoidingView behavior="padding" style={styles.screen}>
      <ScrollView contentContainerStyle={styles.content} keyboardShouldPersistTaps="handled">
        <View style={styles.titleRow}>
          <Text style={styles.title}>{t('onboard.title')}</Text>
          {spinner ? <ActivityIndicator /> : null}
        </View>
        {body}
        <Pressable
          testID="onboard-back"
          accessibilityRole="button"
          disabled={locked}
          onPress={back}
          style={styles.backButton}
        >
          <Text style={styles.backLabel}>{t('onboard.back')}</Text>
        </Pressable>
      </ScrollView>
    </KeyboardAvoidingView>
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

const styles = StyleSheet.create((theme, rt) => ({
  screen: {
    flex: 1,
    backgroundColor: theme.colors.panel1,
  },
  content: {
    flexGrow: 1,
    justifyContent: 'center',
    gap: theme.space(3),
    paddingTop: rt.insets.top + theme.space(6),
    paddingBottom: rt.insets.bottom + theme.space(6),
    paddingHorizontal: theme.space(6),
  },
  titleRow: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: theme.space(3),
  },
  title: {
    flex: 1,
    fontSize: 28,
    fontWeight: '700',
    color: theme.colors.fg,
  },
  subtitle: {
    fontSize: 17,
    fontWeight: '600',
    color: theme.colors.fg,
  },
  body: {
    fontSize: 15,
    color: theme.colors.fgSecondary,
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
  backButton: {
    alignItems: 'center',
    paddingVertical: theme.space(2),
  },
  backLabel: {
    fontSize: 17,
    color: theme.colors.accentText,
  },
}));

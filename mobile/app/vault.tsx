import { useRouter } from 'expo-router';
import { type ReactNode, useEffect, useState } from 'react';
import {
  ActivityIndicator,
  KeyboardAvoidingView,
  Pressable,
  ScrollView,
  Text,
  TextInput,
} from 'react-native';
import { StyleSheet, useUnistyles } from 'react-native-unistyles';

import { ActionButton } from '@/src/components/ActionButton';
import { ErrorText, errorMessage } from '@/src/components/ErrorText';
import { biometricName } from '@/src/components/SecurityPanel';
import { useScreenPrivacy } from '@/src/hooks/useScreenPrivacy';
import { useVault, type Vault } from '@/src/hooks/useVault';
import { t } from '@/src/i18n';

// Mirrors MIN_PASSWORD_LEN in memlore-engine (vault/mod.rs), which still
// validates it.
const MIN_PASSWORD_LEN = 8;

/**
 * Everything shown while the vault is not unlocked. The root layout guards
 * this route with `!unlocked`, so an unlock replaces it with the tabs.
 */
export default function VaultScreen() {
  const vault = useVault();

  let body: ReactNode;
  switch (vault.status) {
    case 'loading':
      body = <ActivityIndicator />;
      break;
    case 'first_launch':
      body = <CreateVault vault={vault} />;
      break;
    case 'show_phrase':
      body = <RecoveryPhrase vault={vault} />;
      break;
    case 'locked':
      body = <Unlock vault={vault} />;
      break;
    case 'unlocked':
      // Only for the frame before the guard swaps this route for the tabs.
      body = null;
      break;
    case 'boot_file_corrupt':
      body = (
        <>
          <Text style={styles.title}>{t('boot_file_corrupt.title')}</Text>
          <Text style={styles.body}>{t('boot_file_corrupt.body')}</Text>
          <Text style={styles.body}>{t('boot_file_corrupt.hint')}</Text>
        </>
      );
      break;
    case 'error':
      body = (
        <>
          <Text style={styles.error}>{t('mobile.error')}</Text>
          <ActionButton testID="retry-button" label={t('mobile.retry')} onPress={vault.retry} />
        </>
      );
      break;
  }

  return (
    <KeyboardAvoidingView behavior="padding" style={styles.screen}>
      <ScrollView contentContainerStyle={styles.content} keyboardShouldPersistTaps="handled">
        {body}
      </ScrollView>
    </KeyboardAvoidingView>
  );
}

function CreateVault({ vault }: { vault: Vault }) {
  const router = useRouter();
  const [password, setPassword] = useState('');
  const [confirm, setConfirm] = useState('');
  const [invalid, setInvalid] = useState<string | null>(null);

  async function submit() {
    if (vault.busy) return;
    if (password.length < MIN_PASSWORD_LEN) {
      setInvalid(t('mobile.password_too_short', { count: MIN_PASSWORD_LEN }));
      return;
    }
    if (password !== confirm) {
      setInvalid(t('set_password.mismatch'));
      return;
    }
    setInvalid(null);
    if (await vault.createVault(password)) {
      setPassword('');
      setConfirm('');
    }
  }

  return (
    <>
      <Text style={styles.title}>{t('set_password.title')}</Text>
      <Text style={styles.body}>{t('set_password.description')}</Text>
      <Text testID="backup-notice" style={styles.notice}>
        {t('mobile.backup_notice')}
      </Text>
      <PasswordInput
        testID="password-input"
        placeholder={t('set_password.password_label')}
        value={password}
        onChangeText={setPassword}
      />
      <PasswordInput
        testID="confirm-password-input"
        placeholder={t('set_password.confirm_label')}
        value={confirm}
        onChangeText={setConfirm}
      />
      <ErrorText message={invalid ?? errorMessage(vault.error)} />
      <ActionButton
        testID="create-vault-button"
        label={vault.busy ? t('set_password.creating') : t('mobile.create_vault')}
        disabled={vault.busy}
        onPress={submit}
      />
      {/* Joins the vault another device syncs through Google Drive or iCloud Drive (app/onboard.tsx). */}
      <Pressable
        testID="onboard-start"
        accessibilityRole="button"
        disabled={vault.busy}
        onPress={() => router.push('/onboard')}
        style={styles.secondaryButton}
      >
        <Text style={styles.secondaryLabel}>{t('mobile.onboard.start')}</Text>
      </Pressable>
    </>
  );
}

function RecoveryPhrase({ vault }: { vault: Vault }) {
  // Hidden while the app is inactive or the screen is recorded, mirrored or
  // AirPlayed (decision 6). Never selectable, so it cannot be copied.
  const { visible } = useScreenPrivacy();
  const phrase = vault.recoveryPhrase ?? '';

  return (
    <>
      <Text style={styles.title}>{t('recovery.intro_title')}</Text>
      <Text style={styles.body}>{t('recovery.intro_body')}</Text>
      <Text
        testID="recovery-phrase"
        accessibilityLabel={t('recovery.words_aria')}
        style={styles.phrase}
      >
        {/* One dot per letter keeps the box the same size, so nothing jumps. */}
        {visible ? phrase : phrase.replace(/\S/g, '•')}
      </Text>
      <ActionButton
        testID="phrase-saved-button"
        label={t('recovery.continue_to_confirm')}
        onPress={vault.confirmPhraseSaved}
      />
    </>
  );
}

function Unlock({ vault }: { vault: Vault }) {
  const [password, setPassword] = useState('');
  const { available, kind, enabled } = vault.biometric;
  const biometric = biometricName(kind);

  // Mounted only while locked: after an auto-lock, the shield comes off now.
  // The root layout swaps to this route without animation, so the protected
  // screen is already covered when this runs.
  useEffect(() => {
    vault.lockScreenShown();
  }, []);

  async function submit() {
    if (vault.busy || vault.biometricPrompting || password.length === 0) return;
    await vault.unlock(password);
    // Cleared either way: a wrong password is retyped from scratch.
    setPassword('');
  }

  return (
    <>
      <Text style={styles.title}>{t('lock.welcome_back')}</Text>
      <Text style={styles.body}>{t('lock.encrypted_device')}</Text>
      <PasswordInput
        testID="password-input"
        placeholder={t('lock.password_placeholder')}
        value={password}
        onChangeText={setPassword}
        onSubmitEditing={submit}
      />
      <ErrorText message={errorMessage(vault.error)} />
      <ActionButton
        testID="unlock-button"
        label={vault.busy ? t('lock.unlocking') : t('lock.unlock')}
        disabled={vault.busy || password.length === 0}
        onPress={submit}
      />
      {available && enabled ? (
        <ActionButton
          testID="biometric-unlock-button"
          label={
            vault.biometricPrompting
              ? t('mobile.biometric_waiting', { kind: biometric })
              : t('mobile.unlock_with_biometric', { kind: biometric })
          }
          disabled={vault.busy || vault.biometricPrompting}
          onPress={() => void vault.unlockWithBiometric()}
        />
      ) : null}
    </>
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
  title: {
    fontSize: 28,
    fontWeight: '700',
    color: theme.colors.fg,
  },
  body: {
    fontSize: 15,
    color: theme.colors.fgSecondary,
  },
  notice: {
    fontSize: 13,
    color: theme.colors.fgMuted,
  },
  phrase: {
    fontSize: 17,
    lineHeight: 26,
    color: theme.colors.fg,
    padding: theme.space(4),
    borderWidth: 1,
    borderRadius: theme.radii.xl,
    borderColor: theme.colors.borderDefault,
    backgroundColor: theme.colors.elevated,
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
  error: {
    fontSize: 15,
    color: theme.colors.dangerText,
  },
  secondaryButton: {
    alignItems: 'center',
    paddingVertical: theme.space(3),
  },
  secondaryLabel: {
    fontSize: 17,
    color: theme.colors.accentText,
  },
}));

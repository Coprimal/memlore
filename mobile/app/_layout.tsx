// Registers the Unistyles themes. First import: configure must run before any
// route module is evaluated, and the root layout loads first.
import '@/src/theme/unistyles';

import { Stack } from 'expo-router';
import { useUnistyles } from 'react-native-unistyles';

import { useVault, VaultProvider } from '@/src/hooks/useVault';

export default function RootLayout() {
  return (
    <VaultProvider>
      <RootStack />
    </VaultProvider>
  );
}

/**
 * Only `vault` exists while the vault is not unlocked, and only the app (and
 * the dev spikes) while it is: a lock removes every protected screen, so the
 * swipe back cannot return to them. The explicit redirects keep the fallback
 * off the route sort order, which puts groups before `vault`.
 *
 * The lock and unlock swaps never animate. A guard swap is a native replace
 * that runs as a push, so the arriving screen's `animation` decides: with the
 * default slide, `vault` would move in over the protected screen for ~300 ms
 * after the lock screen mounted and the privacy shield came off.
 */
function RootStack() {
  const { theme } = useUnistyles();
  const unlocked = useVault().status === 'unlocked';
  return (
    <Stack
      screenOptions={{
        headerShown: false,
        contentStyle: { backgroundColor: theme.colors.panel1 },
      }}
    >
      {/* Declare every new route file in one of these groups: Expo Router registers an
          undeclared one outside both, where it stays reachable while locked. */}
      <Stack.Protected guard={unlocked} redirectTo="/vault">
        <Stack.Screen name="(app)" options={{ animation: 'none' }} />
        {/* Dev-only spikes; the editor spike reads entries, so it needs the vault. */}
        <Stack.Screen name="(dev)/editor-spike" />
        <Stack.Screen name="(dev)/spike-icloud" />
        <Stack.Screen name="(dev)/spike-oauth" />
      </Stack.Protected>
      <Stack.Protected guard={!unlocked} redirectTo="/">
        <Stack.Screen name="vault" options={{ animation: 'none' }} />
      </Stack.Protected>
    </Stack>
  );
}

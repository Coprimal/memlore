import { Stack } from 'expo-router';
import { View } from 'react-native';
import { StyleSheet, useUnistyles } from 'react-native-unistyles';

import { LockSessionProvider, noteActivity } from '@/src/hooks/useLockSession';

// Deep links to a pushed route (entry, trash) still get the tabs below them.
export const unstable_settings = { anchor: '(tabs)' };

/**
 * Everything behind the unlocked guard: the tabs, and the routes pushed over
 * them (entry, trash), which get the default native header with its back button.
 * The lock session lives here, so a vault lock ends it with the tree; every
 * touch (capture phase, never claiming it) counts as activity for its re-lock.
 */
export default function AppLayout() {
  const { theme } = useUnistyles();
  return (
    <LockSessionProvider>
      <View style={styles.root} onTouchStartCapture={noteActivity}>
        <Stack
          screenOptions={{
            contentStyle: { backgroundColor: theme.colors.panel1 },
            // The same header as the tabs' (tabs/_layout.tsx).
            headerStyle: { backgroundColor: theme.colors.panel1 },
            headerTintColor: theme.colors.fg,
            headerShadowVisible: false,
            // A bare chevron: the tabs' group name would otherwise show as "(tabs)".
            headerBackButtonDisplayMode: 'minimal',
          }}
        >
          <Stack.Screen name="(tabs)" options={{ headerShown: false }} />
        </Stack>
      </View>
    </LockSessionProvider>
  );
}

const styles = StyleSheet.create({
  root: { flex: 1 },
});

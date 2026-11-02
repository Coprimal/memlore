import { Stack } from 'expo-router';
import { useUnistyles } from 'react-native-unistyles';

// Deep links to a pushed route (entry, trash) still get the tabs below them.
export const unstable_settings = { anchor: '(tabs)' };

/**
 * Everything behind the unlocked guard: the tabs, and the routes pushed over
 * them (entry, trash), which get the default native header with its back button.
 */
export default function AppLayout() {
  const { theme } = useUnistyles();
  return (
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
  );
}

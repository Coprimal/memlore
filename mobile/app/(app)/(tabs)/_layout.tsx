import { Tabs } from 'expo-router';
import { type SFSymbol, SymbolView } from 'expo-symbols';
import type { ColorValue } from 'react-native';
import { useUnistyles } from 'react-native-unistyles';

import { t } from '@/src/i18n';

/** The tab bar: Entries, Journals and Settings. */
export default function TabsLayout() {
  const { theme } = useUnistyles();
  return (
    <Tabs
      screenOptions={{
        headerStyle: { backgroundColor: theme.colors.panel1 },
        headerTintColor: theme.colors.fg,
        headerShadowVisible: false,
        sceneStyle: { backgroundColor: theme.colors.panel1 },
        tabBarStyle: {
          backgroundColor: theme.colors.panel1,
          borderTopColor: theme.colors.borderSubtle,
        },
        tabBarActiveTintColor: theme.colors.accentText,
        tabBarInactiveTintColor: theme.colors.fgMuted,
      }}
    >
      <Tabs.Screen
        name="index"
        options={{
          title: t('nav:entries'),
          tabBarButtonTestID: 'entries-tab',
          tabBarIcon: ({ color, size }) => tabIcon('book', color, size),
        }}
      />
      <Tabs.Screen
        name="journals"
        options={{
          title: t('mobile.tab_journals'),
          tabBarButtonTestID: 'journals-tab',
          tabBarIcon: ({ color, size }) => tabIcon('books.vertical', color, size),
        }}
      />
      <Tabs.Screen
        name="settings"
        options={{
          title: t('nav:settings'),
          tabBarButtonTestID: 'settings-tab',
          tabBarIcon: ({ color, size }) => tabIcon('gearshape', color, size),
        }}
      />
    </Tabs>
  );
}

function tabIcon(name: SFSymbol, color: ColorValue, size: number) {
  return <SymbolView name={name} tintColor={color} size={size} />;
}

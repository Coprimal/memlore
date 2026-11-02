import { Stack, useLocalSearchParams, useRouter } from 'expo-router';
import { useEffect } from 'react';
import { View } from 'react-native';
import { StyleSheet } from 'react-native-unistyles';

import { EntryPlaceholder, placeholderBody } from '@/src/components/EntryPlaceholder';
import { ErrorText } from '@/src/components/ErrorText';
import { getCachedRow, useEntryDetail } from '@/src/hooks/useEntries';
import { t } from '@/src/i18n';

/** How long "Entry not found" stays readable before the screen goes back. */
const NOT_FOUND_BACK_MS = 1000;

/**
 * One entry, read-only for now. The placeholder paints in the push's frame
 * from the list's row cache (title and preview; nothing for a brand-new entry),
 * then shows the entry's plain text once `getEntry` answers.
 */
export default function EntryScreen() {
  const { id } = useLocalSearchParams<{ id: string }>();
  const router = useRouter();
  const row = getCachedRow(id);
  const entry = useEntryDetail(id);

  useEffect(() => {
    if (!entry.notFound) return;
    const timer = setTimeout(() => {
      if (router.canGoBack()) router.back();
      else router.replace('/');
    }, NOT_FOUND_BACK_MS);
    return () => clearTimeout(timer);
  }, [entry.notFound, router]);

  const title = (entry.detail ?? row)?.title ?? null;
  // Once the entry is read, its own text (even empty) replaces the preview.
  const body = entry.detail
    ? placeholderBody(entry.detail.contentText ?? '')
    : (row?.previewText ?? null);

  return (
    <View style={styles.screen}>
      <Stack.Screen options={{ headerTitle: '' }} />
      {entry.notFound || entry.failed ? (
        <View style={styles.message}>
          <ErrorText
            message={entry.notFound ? t('editor:panel.not_found') : t('mobile.error')}
          />
        </View>
      ) : (
        <EntryPlaceholder title={title} body={body} />
      )}
    </View>
  );
}

const styles = StyleSheet.create((theme) => ({
  screen: {
    flex: 1,
  },
  message: {
    padding: theme.space(4),
  },
}));

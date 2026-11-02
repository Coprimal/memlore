import { Stack, useLocalSearchParams, useRouter } from 'expo-router';
import { useCallback, useEffect, useRef, useState } from 'react';
import {
  ActivityIndicator,
  Alert,
  Animated,
  FlatList,
  Modal,
  Pressable,
  Text,
  View,
} from 'react-native';
import { StyleSheet, useUnistyles } from 'react-native-unistyles';

import EntryEditorDom from '@/editor/EntryEditorDom';
import type { Version } from '@/modules/memlore-core';
import { ErrorText } from '@/src/components/ErrorText';
import { setPendingRestore, useVersions } from '@/src/hooks/useVersions';
import { activeLocale, t } from '@/src/i18n';

/** How long a load runs before its spinner shows (fast loads never flash it). */
const SPINNER_DELAY_MS = 150;
/** How long "Entry not found" shows before the screen goes back. */
const NOT_FOUND_BACK_MS = 1000;
/** The entry screen saves while this one opens: the list reads again after this. */
const AFTER_SAVE_REFRESH_MS = 1000;
const DAY_MS = 24 * 60 * 60 * 1000;

// Desktop `formatEntryDate` and `formatTime` (src/lib/dates.ts).
const dateFormat = new Intl.DateTimeFormat(activeLocale, {
  weekday: 'long',
  month: 'long',
  day: 'numeric',
});
const timeFormat = new Intl.DateTimeFormat(activeLocale, { hour: 'numeric', minute: '2-digit' });

/**
 * Desktop `formatRelativeDate`: "Today", "Yesterday", "N days ago" up to 6
 * days, else the date. Hermes may ship without `Intl.RelativeTimeFormat`:
 * then every day shows its date.
 */
function dayLabel(createdAt: number, now: Date): string {
  const date = new Date(createdAt * 1000);
  const today = new Date(now.getFullYear(), now.getMonth(), now.getDate());
  const day = new Date(date.getFullYear(), date.getMonth(), date.getDate());
  const diffDays = Math.round((today.getTime() - day.getTime()) / DAY_MS);
  if (diffDays >= 0 && diffDays <= 6 && typeof Intl.RelativeTimeFormat === 'function') {
    // 'auto' for today and yesterday only ("Hôm kia" would replace "2 ngày trước").
    const numeric = diffDays <= 1 ? 'auto' : 'always';
    const raw = new Intl.RelativeTimeFormat(activeLocale, { numeric }).format(-diffDays, 'day');
    return raw.charAt(0).toUpperCase() + raw.slice(1);
  }
  return dateFormat.format(date);
}

function versionLabel(version: Version, now: Date): string {
  return `${dayLabel(version.createdAt, now)} · ${timeFormat.format(version.createdAt * 1000)}`;
}

/**
 * An entry's saved versions, newest first. A tap previews one read-only in a
 * sheet; Restore (hidden when the entry opened read-only, `editable=0`) asks
 * first, then leaves the restore for the entry screen and goes back to it.
 */
export default function VersionHistoryScreen() {
  const params = useLocalSearchParams<{ id: string; editable?: string | string[] }>();
  const id = params.id;
  const canRestore = params.editable !== '0';
  const router = useRouter();
  const history = useVersions(id);
  // The version previewed in the sheet; set on the tap's frame.
  const [selected, setSelected] = useState<Version | null>(null);
  const showSpinner = useDelayed(
    history.loading && history.versions.length === 0,
    SPINNER_DELAY_MS,
  );
  const now = new Date();

  // The save's session snapshot may land after the first read.
  useEffect(() => {
    const timer = setTimeout(() => void history.refresh(), AFTER_SAVE_REFRESH_MS);
    return () => clearTimeout(timer);
  }, [history.refresh]);

  useEffect(() => {
    if (!history.notFound) return;
    const timer = setTimeout(() => {
      if (router.canGoBack()) router.back();
      else router.replace('/');
    }, NOT_FOUND_BACK_MS);
    return () => clearTimeout(timer);
  }, [history.notFound, router]);

  function confirmRestore(version: Version) {
    Alert.alert(
      t('editor:versionHistory.confirm_title'),
      t('editor:versionHistory.confirm_description'),
      [
        { text: t('common:confirm_dialog.cancel'), style: 'cancel' },
        {
          text: t('editor:versionHistory.restore'),
          style: 'destructive',
          onPress: () => {
            setPendingRestore({ entryId: id, versionId: version.id });
            setSelected(null);
            router.back();
          },
        },
      ],
    );
  }

  if (history.notFound) {
    return (
      <View style={styles.screen}>
        <Stack.Screen options={{ title: '' }} />
        <View style={styles.message}>
          <ErrorText message={t('editor:panel.not_found')} />
        </View>
      </View>
    );
  }

  return (
    <View style={styles.screen}>
      <Stack.Screen options={{ title: t('editor:entry_context.version_history') }} />
      <ErrorText message={history.error ? t('mobile.error') : null} />
      <FlatList
        data={history.versions}
        keyExtractor={(version) => version.id}
        renderItem={({ item, index }) => (
          <Pressable
            testID={`version-row-${index}`}
            accessibilityRole="button"
            onPress={() => setSelected(item)}
            style={styles.row}
          >
            <Text style={styles.rowLabel}>{versionLabel(item, now)}</Text>
            {item.previewText ? (
              <Text style={styles.rowPreview} numberOfLines={2}>
                {item.previewText}
              </Text>
            ) : null}
          </Pressable>
        )}
        ListEmptyComponent={
          history.loading ? (
            <FadeIn visible={showSpinner} />
          ) : history.error ? undefined : (
            <View testID="version-empty" style={styles.empty}>
              <Text style={styles.emptyText}>{t('editor:versionHistory.empty')}</Text>
            </View>
          )
        }
      />
      <VersionPreview
        version={selected}
        label={selected ? versionLabel(selected, now) : ''}
        canRestore={canRestore}
        content={history.content}
        onRestore={confirmRestore}
        onClose={() => setSelected(null)}
      />
    </View>
  );
}

/** The version's document, or `failed` when it could not be read. */
type Loaded = { versionId: string; b64: string | null };

const noop = async () => {};

/**
 * A page sheet with one version read-only. It opens at once; the document
 * loads after (its preview text stands in until the editor is ready, and a
 * spinner shows only when the read takes over 150 ms).
 */
function VersionPreview(props: {
  version: Version | null;
  label: string;
  canRestore: boolean;
  content: (versionId: string) => Promise<string | null>;
  onRestore: (version: Version) => void;
  onClose: () => void;
}) {
  const { version, content } = props;
  const [loaded, setLoaded] = useState<Loaded | null>(null);
  const [readyId, setReadyId] = useState<string | null>(null);
  const versionId = version?.id ?? null;
  const current = loaded && loaded.versionId === versionId ? loaded : null;
  const showSpinner = useDelayed(versionId !== null && current === null, SPINNER_DELAY_MS);

  useEffect(() => {
    if (versionId === null) return;
    // The sheet's editor mounts again: the stand-in covers it until it is ready.
    setReadyId(null);
    let active = true;
    void content(versionId).then((b64) => {
      if (active) setLoaded({ versionId, b64 });
    });
    return () => {
      active = false;
    };
  }, [versionId, content]);

  const onReady = useCallback(async () => setReadyId(versionId), [versionId]);

  const ready = versionId !== null && readyId === versionId;
  const failed = current !== null && current.b64 === null;

  return (
    <Modal
      visible={version !== null}
      animationType="slide"
      presentationStyle="pageSheet"
      onRequestClose={props.onClose}
    >
      <View testID="version-preview" style={styles.sheet}>
        <View style={styles.sheetHeader}>
          <Pressable accessibilityRole="button" hitSlop={8} onPress={props.onClose}>
            <Text style={styles.close}>{t('editor:versionHistory.close')}</Text>
          </Pressable>
          <Text style={styles.sheetTitle} numberOfLines={1}>
            {props.label}
          </Text>
          {props.canRestore && version ? (
            <Pressable
              testID="version-restore-button"
              accessibilityRole="button"
              hitSlop={8}
              onPress={() => props.onRestore(version)}
            >
              <Text style={styles.restore}>{t('editor:versionHistory.restore')}</Text>
            </Pressable>
          ) : null}
        </View>
        <ErrorText message={failed ? t('mobile.error') : null} />
        <View style={styles.preview}>
          {current?.b64 != null ? (
            <EntryEditorDom
              key={current.versionId}
              initialStateB64={current.b64}
              onUpdate={noop}
              onFlushed={noop}
              onReady={onReady}
              onFormatState={noop}
              onLinkPress={noop}
              onRestored={noop}
              readOnly
              dom={{ style: { flex: 1 } }}
            />
          ) : null}
          {!ready && !failed && version ? (
            <View style={[StyleSheet.absoluteFill, styles.standIn]}>
              {version.previewText ? (
                <Text style={styles.standInText}>{version.previewText}</Text>
              ) : null}
              <FadeIn visible={showSpinner} />
            </View>
          ) : null}
        </View>
      </View>
    </Modal>
  );
}

/** A spinner that fades in once `visible` turns true. */
function FadeIn({ visible }: { visible: boolean }) {
  const { theme } = useUnistyles();
  const opacity = useRef(new Animated.Value(0)).current;
  useEffect(() => {
    if (!visible) {
      opacity.setValue(0);
      return;
    }
    const fade = Animated.timing(opacity, { toValue: 1, duration: 200, useNativeDriver: true });
    fade.start();
    return () => fade.stop();
  }, [visible, opacity]);
  if (!visible) return null;
  return (
    <Animated.View style={[styles.spinner, { opacity }]}>
      <ActivityIndicator color={theme.colors.fgMuted} />
    </Animated.View>
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
  screen: {
    flex: 1,
  },
  message: {
    padding: theme.space(4),
  },
  row: {
    gap: theme.space(1),
    paddingVertical: theme.space(3),
    paddingHorizontal: theme.space(4),
    borderBottomWidth: StyleSheet.hairlineWidth,
    borderBottomColor: theme.colors.borderSubtle,
  },
  rowLabel: {
    fontSize: 15,
    fontWeight: '600',
    color: theme.colors.fg,
  },
  rowPreview: {
    fontSize: 13,
    color: theme.colors.fgMuted,
  },
  empty: {
    paddingVertical: theme.space(10),
    paddingHorizontal: theme.space(6),
  },
  emptyText: {
    textAlign: 'center',
    fontSize: 15,
    color: theme.colors.fgSecondary,
  },
  spinner: {
    paddingVertical: theme.space(6),
    alignItems: 'center',
  },
  sheet: {
    flex: 1,
    backgroundColor: theme.colors.panel1,
  },
  sheetHeader: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: theme.space(3),
    paddingVertical: theme.space(4),
    paddingHorizontal: theme.space(4),
  },
  sheetTitle: {
    flex: 1,
    textAlign: 'center',
    fontSize: 17,
    fontWeight: '600',
    color: theme.colors.fg,
  },
  close: {
    fontSize: 17,
    color: theme.colors.accentText,
  },
  restore: {
    fontSize: 17,
    fontWeight: '600',
    color: theme.colors.accentText,
  },
  preview: {
    flex: 1,
  },
  // The editor's metrics (EntryEditorDom: padding 12, 17 px, line height 1.5).
  standIn: {
    padding: 12,
    backgroundColor: theme.colors.panel1,
  },
  standInText: {
    fontSize: 17,
    lineHeight: 25.5,
    color: theme.colors.fg,
  },
}));

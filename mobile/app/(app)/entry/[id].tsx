import { Stack, useLocalSearchParams, usePreventRemove, useRouter } from 'expo-router';
import { useHeaderHeight } from 'expo-router/react-navigation';
import { SymbolView } from 'expo-symbols';
import { useCallback, useEffect, useRef, useState } from 'react';
import {
  ActionSheetIOS,
  Alert,
  KeyboardAvoidingView,
  Linking,
  Pressable,
  TextInput,
  View,
} from 'react-native';
import { StyleSheet, useUnistyles } from 'react-native-unistyles';

import EntryEditorDom, { type EntryEditorHandle, type FormatState } from '@/editor/EntryEditorDom';
import { EditorToolbar } from '@/src/components/EditorToolbar';
import { EntryMeta, META_ROW_HEIGHT } from '@/src/components/EntryMeta';
import { EntryPlaceholder, placeholderBody } from '@/src/components/EntryPlaceholder';
import { ErrorText } from '@/src/components/ErrorText';
import { getCachedRow, useEntryActions, useEntryDetail } from '@/src/hooks/useEntries';
import { useEntryDoc } from '@/src/hooks/useEntryDoc';
import { useVault } from '@/src/hooks/useVault';
import { t } from '@/src/i18n';
import { registerFlusher } from '@/src/lib/flushRegistry';

/** How long "Entry not found" stays readable before the screen goes back. */
const NOT_FOUND_BACK_MS = 1000;
/** The placeholder goes even if the editor never says it is ready. */
const OVERLAY_FALLBACK_MS = 8000;
/** A title being typed is saved once it has not changed for this long. */
const TITLE_SAVE_DEBOUNCE_MS = 800;
/** Leaving waits at most this long for the title and body saves. */
const FLUSH_CAP_MS = 3000;
/** The DOM side only reports these; checked again before handing a URL to iOS. */
const EXTERNAL_URL = /^(https?:|mailto:)/i;

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/**
 * One entry: its title field and meta row above the DOM editor. The
 * placeholder paints in the push's frame from the list's row cache (then from
 * `getEntry`) and lies over all three until the editor says it is ready; it
 * leaves the meta row's fixed height under its title, so the swap does not
 * jump. Leaving (Back, swipe back) first saves the title and flushes the
 * editor; Move to Trash (header menu) leaves without it.
 */
export default function EntryScreen() {
  const { id } = useLocalSearchParams<{ id: string }>();
  const router = useRouter();
  const vault = useVault();
  const { theme } = useUnistyles();
  // The keyboard's frame is in window coordinates; this screen starts below the header.
  const headerHeight = useHeaderHeight();
  const row = getCachedRow(id);
  // Both reads start in the same frame and run in parallel.
  const entry = useEntryDetail(id);
  const doc = useEntryDoc(id);
  const actions = useEntryActions(id, entry.detail);
  const notFound = entry.notFound || doc.status === 'notFound' || actions.notFound;
  const failed = !notFound && (entry.failed || doc.status === 'failed');

  const editorRef = useRef<EntryEditorHandle>(null);
  const [overlay, setOverlay] = useState(true);
  // From `onReady`: null until the editor is ready, false when it opened read-only.
  const [editable, setEditable] = useState<boolean | null>(null);
  // From `onFormatState`: the toolbar shows while `focused`.
  const [format, setFormat] = useState<FormatState | null>(null);
  // A tap on the placeholder: focus the editor once it is ready.
  const pendingFocus = useRef(false);

  // The title as typed; null until `getEntry` answered (the field is read-only till then).
  const [title, setTitle] = useState<string | null>(null);
  const titleRef = useRef<string | null>(null);
  // The title last saved (or read); null when unknown, so the next flush saves.
  const savedTitle = useRef<string | null>(null);
  const titleTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  // Title saves run one after the other; this never rejects.
  const titleSaving = useRef<Promise<void>>(Promise.resolve());
  // Callers waiting for the editor's next `onFlushed`.
  const flushWaiters = useRef(new Set<() => void>());
  const [leaving, setLeaving] = useState(false);
  const flushing = useRef(false);
  // Moved to Trash: the leave must not save (the entry is gone).
  const trashed = useRef(false);
  const mounted = useRef(true);

  useEffect(() => {
    if (!notFound) return;
    const timer = setTimeout(() => {
      if (router.canGoBack()) router.back();
      else router.replace('/');
    }, NOT_FOUND_BACK_MS);
    return () => clearTimeout(timer);
  }, [notFound, router]);

  useEffect(() => {
    const timer = setTimeout(() => setOverlay(false), OVERLAY_FALLBACK_MS);
    return () => clearTimeout(timer);
  }, []);

  useEffect(() => {
    if (!entry.detail) return;
    const stored = entry.detail.title ?? '';
    savedTitle.current = stored;
    titleRef.current = stored;
    setTitle(stored);
  }, [entry.detail]);

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      if (titleTimer.current) clearTimeout(titleTimer.current);
    };
  }, []);

  /** Saves the typed title now if it changed; resolves once every title save ended. */
  const flushTitle = useCallback((): Promise<void> => {
    if (titleTimer.current) clearTimeout(titleTimer.current);
    titleTimer.current = null;
    const value = titleRef.current;
    if (value !== null && value !== savedTitle.current) {
      savedTitle.current = value;
      titleSaving.current = titleSaving.current
        .then(() => doc.saveTitle(id, value))
        .catch(() => {
          // Saved again on the next flush (useEntryDoc already handled the error).
          if (savedTitle.current === value) savedTitle.current = null;
        });
    }
    return titleSaving.current;
  }, [id, doc.saveTitle]);

  /**
   * Asks the editor to save what its debounce holds and resolves on its
   * `onFlushed`, or after `FLUSH_CAP_MS`; at once when the editor has not
   * registered its handle yet (nothing typed). Any `onFlushed` answers every
   * flush waiting: two flushes overlap only within one leave or lock.
   */
  // TODO(later): expo/dom can log an unhandled DomWebView rejection after Back
  // (docs/LATER.md "Mobile M3a notes").
  const flushEditor = useCallback((): Promise<void> => {
    const flush = editorRef.current?.flush;
    if (!flush) return Promise.resolve();
    return new Promise<void>((resolve) => {
      const done = () => {
        clearTimeout(timer);
        flushWaiters.current.delete(done);
        resolve();
      };
      const timer = setTimeout(done, FLUSH_CAP_MS);
      flushWaiters.current.add(done);
      flush();
    });
  }, []);

  /** Title and body saved (or the cap passed). */
  const flushEntry = useCallback(
    // A trashed entry is never saved again (the engine refuses it anyway).
    () =>
      trashed.current
        ? Promise.resolve()
        : Promise.race([Promise.all([flushTitle(), flushEditor()]), delay(FLUSH_CAP_MS)]),
    [flushTitle, flushEditor],
  );

  // Save before lock: every lock flushes this entry first while it is open.
  useEffect(
    () =>
      registerFlusher(async () => {
        await flushEntry();
      }),
    [flushEntry],
  );

  // Back and swipe back wait for the saves. Never while locked: the lock's
  // removal of this screen must go through at once.
  usePreventRemove(vault.status === 'unlocked' && !leaving && !notFound, ({ repeat }) => {
    // `leaving` from a trash may not have rendered yet: let the pop through.
    if (trashed.current) {
      repeat();
      return;
    }
    if (flushing.current) return;
    flushing.current = true;
    void flushEntry().then(() => {
      // A lock during the flush already removed the screen.
      if (!mounted.current) return;
      setLeaving(true);
      repeat();
    });
  });

  const onFlushed = useCallback(async () => {
    for (const done of [...flushWaiters.current]) done();
  }, []);

  const onReady = useCallback(async (canEdit: boolean) => {
    setEditable(canEdit);
    setOverlay(false);
    if (!pendingFocus.current || !canEdit) return;
    pendingFocus.current = false;
    editorRef.current?.exec?.('focusEnd');
  }, []);

  const onFormatState = useCallback(async (state: FormatState) => setFormat(state), []);

  const exec = useCallback<EntryEditorHandle['exec']>((cmd, arg) => {
    editorRef.current?.exec?.(cmd, arg);
  }, []);

  const onLinkPress = useCallback(async (url: string) => {
    if (!EXTERNAL_URL.test(url)) return;
    Alert.alert(t('mobile.link.open_title'), url, [
      { text: t('common:confirm_dialog.cancel'), style: 'cancel' },
      {
        text: t('mobile.link.open'),
        onPress: () => void Linking.openURL(url).catch(() => {}),
      },
    ]);
  }, []);

  /** `actions.trash()` saves the open edits first; on success the screen goes back. */
  const moveToTrash = useCallback(async () => {
    if (!(await actions.trash())) return;
    // A lock during the trash's flush already removed the screen.
    if (!mounted.current) return;
    trashed.current = true;
    setLeaving(true);
    if (router.canGoBack()) router.back();
    else router.replace('/');
  }, [actions.trash, router]);

  /** The header menu: Move to Trash, then its confirmation (decision 7). */
  const openMenu = useCallback(() => {
    ActionSheetIOS.showActionSheetWithOptions(
      {
        options: [t('editor:entry_card.delete_title'), t('common:confirm_dialog.cancel')],
        destructiveButtonIndex: 0,
        cancelButtonIndex: 1,
      },
      (index) => {
        if (index !== 0) return;
        Alert.alert(t('editor:entry_card.delete_title'), t('mobile.trash_confirm_body'), [
          { text: t('editor:entry_card.delete_cancel'), style: 'cancel' },
          {
            text: t('editor:entry_card.delete_confirm'),
            style: 'destructive',
            onPress: () => void moveToTrash(),
          },
        ]);
      },
    );
  }, [moveToTrash]);

  const onChangeTitle = (value: string) => {
    titleRef.current = value;
    setTitle(value);
    if (titleTimer.current) clearTimeout(titleTimer.current);
    titleTimer.current = setTimeout(() => void flushTitle(), TITLE_SAVE_DEBOUNCE_MS);
  };

  if (notFound || failed) {
    return (
      <View style={styles.screen}>
        <Stack.Screen options={{ headerTitle: '', headerRight: () => null }} />
        <View style={styles.message}>
          <ErrorText message={notFound ? t('editor:panel.not_found') : t('mobile.error')} />
        </View>
      </View>
    );
  }

  const placeholderTitle = (entry.detail ?? row)?.title ?? null;
  // Once the entry is read, its own text (even empty) replaces the preview.
  const body = entry.detail
    ? placeholderBody(entry.detail.contentText ?? '')
    : (row?.previewText ?? null);

  return (
    <View style={styles.screen}>
      <Stack.Screen
        options={{
          headerTitle: '',
          headerRight: () => (
            <Pressable
              testID="entry-menu"
              accessibilityRole="button"
              accessibilityLabel={t('editor:pills.more_aria')}
              hitSlop={8}
              onPress={openMenu}
            >
              <SymbolView name="ellipsis.circle" tintColor={theme.colors.accentText} size={24} />
            </Pressable>
          ),
        }}
      />
      {/* The WebView frame ends at the toolbar, which sits right above the keyboard. */}
      <KeyboardAvoidingView
        style={styles.screen}
        behavior="padding"
        keyboardVerticalOffset={headerHeight}
      >
        <TextInput
          testID="entry-title-input"
          style={styles.title}
          value={title ?? placeholderTitle ?? ''}
          onChangeText={onChangeTitle}
          onBlur={() => void flushTitle()}
          onSubmitEditing={() => void flushTitle()}
          editable={title !== null && editable !== false}
          placeholder={t('nav:media_gallery.untitled')}
          placeholderTextColor={theme.colors.fgMuted}
          returnKeyType="done"
        />
        <EntryMeta meta={actions.meta} actions={actions} canAddTags={entry.detail !== null} />
        <View style={styles.editor}>
          {doc.status === 'ready' && doc.stateB64 !== null ? (
            <EntryEditorDom
              key={id}
              ref={editorRef}
              initialStateB64={doc.stateB64}
              onUpdate={doc.save}
              onFlushed={onFlushed}
              onReady={onReady}
              onFormatState={onFormatState}
              onLinkPress={onLinkPress}
              readOnly={false}
              dom={{ style: { flex: 1 }, hideKeyboardAccessoryView: true }}
            />
          ) : null}
        </View>
        {editable === true && format?.focused ? <EditorToolbar state={format} exec={exec} /> : null}
      </KeyboardAvoidingView>
      {overlay ? (
        <EntryPlaceholder
          title={placeholderTitle}
          body={body}
          style={StyleSheet.absoluteFill}
          spacerHeight={META_ROW_HEIGHT}
          onPress={() => {
            pendingFocus.current = true;
          }}
        />
      ) : null}
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
  // EntryPlaceholder's title metrics, so the swap does not jump.
  title: {
    paddingTop: 12,
    paddingBottom: 0,
    paddingHorizontal: 12,
    fontSize: 22,
    fontWeight: '600',
    color: theme.colors.fg,
  },
  editor: {
    flex: 1,
  },
}));

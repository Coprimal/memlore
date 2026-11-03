import {
  Stack,
  useFocusEffect,
  useLocalSearchParams,
  usePreventRemove,
  useRouter,
} from 'expo-router';
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
import { type SetContentPayload, templateBody } from '@/editor/templateBody';
import type { MemloreErrorCode, Template } from '@/modules/memlore-core';
import { EditorToolbar } from '@/src/components/EditorToolbar';
import { EntryMeta, META_ROW_HEIGHT } from '@/src/components/EntryMeta';
import { EntryPlaceholder, placeholderBody } from '@/src/components/EntryPlaceholder';
import { ErrorText } from '@/src/components/ErrorText';
import { getCachedRow, useEntryActions, useEntryDetail } from '@/src/hooks/useEntries';
import { useEntryDoc } from '@/src/hooks/useEntryDoc';
import { noteActivity } from '@/src/hooks/useLockSession';
import { useTemplateLookup } from '@/src/hooks/useTemplates';
import { errorCode, useVault } from '@/src/hooks/useVault';
import { type PendingRestore, takePendingRestore } from '@/src/hooks/useVersions';
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

/** Says the restore failed, unless the lock screen (ERR_LOCKED) already shows. */
function restoreFailed(code: MemloreErrorCode | null = null) {
  if (code === 'ERR_LOCKED') return;
  Alert.alert(t('editor:versionHistory.restore_failed'));
}

/**
 * One entry: its title field and meta row above the DOM editor. The
 * placeholder paints in the push's frame from the list's row cache (then from
 * `getEntry`) and lies over all three until the editor says it is ready; it
 * leaves the meta row's fixed height under its title, so the swap does not
 * jump. Leaving (Back, swipe back) first saves the title and flushes the
 * editor; Move to Trash (header menu) leaves without it. A `template` param
 * (a new entry from the template picker) fills the body once the editor is
 * ready, only while it is still empty; the title is left alone. Version
 * history (header menu) opens at once while it saves; a restore confirmed
 * there runs here on return, after a snapshot of the current content (desktop
 * EditorPanel).
 */
export default function EntryScreen() {
  const params = useLocalSearchParams<{ id: string; template?: string | string[] }>();
  const id = params.id;
  // A repeated param arrives as an array: only a single id names a template.
  const templateId = typeof params.template === 'string' ? params.template : undefined;
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
  // The same, for callbacks that must not wait for a render.
  const editableRef = useRef<boolean | null>(null);
  // A restore taken on focus before the editor was ready: run by `onReady`.
  const parkedRestore = useRef<PendingRestore | null>(null);
  // From `onFormatState`: the toolbar shows while `focused`.
  const [format, setFormat] = useState<FormatState | null>(null);
  // A tap on the placeholder: focus the editor once it is ready.
  const pendingFocus = useRef(false);
  const lookupTemplate = useTemplateLookup();
  // The `template` param's row: undefined while it is read, null when gone.
  const [template, setTemplate] = useState<Template | null | undefined>(undefined);
  const templateApplied = useRef(false);

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

  // Read in parallel with the entry, so applying waits only for the editor.
  useEffect(() => {
    if (!templateId) return;
    let live = true;
    void lookupTemplate(templateId).then((row) => {
      if (live) setTemplate(row);
    });
    return () => {
      live = false;
    };
  }, [templateId, lookupTemplate]);

  // Once per screen, when both the editor and the row are ready. The DOM side
  // applies it only to an empty body, so a reopen never applies it twice.
  useEffect(() => {
    if (templateApplied.current || !templateId || editable === null) return;
    if (editable && template === undefined) return;
    templateApplied.current = true;
    const body = editable && template ? templateBody(template, t) : null;
    if (body) {
      const payload: SetContentPayload = { ...body, onlyIfEmpty: true };
      editorRef.current?.exec?.('setContent', JSON.stringify(payload));
    }
    router.setParams({ template: undefined });
  }, [templateId, editable, template, router]);

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
   * Asks the editor to save what its debounce holds and resolves true on its
   * `onFlushed` (the save ended, maybe failed: see `doc.lastSaveError`), false
   * after `FLUSH_CAP_MS`; true at once when the editor has not registered its
   * handle yet (nothing typed). Any `onFlushed` answers every flush waiting:
   * two flushes overlap only within one leave, lock, or history round trip.
   */
  // TODO(later): expo/dom can log an unhandled DomWebView rejection after Back
  // (docs/LATER.md "Mobile M3a notes").
  const flushEditor = useCallback((): Promise<boolean> => {
    const flush = editorRef.current?.flush;
    if (!flush) return Promise.resolve(true);
    return new Promise<boolean>((resolve) => {
      const finish = (flushed: boolean) => {
        clearTimeout(timer);
        flushWaiters.current.delete(answered);
        resolve(flushed);
      };
      const answered = () => finish(true);
      const timer = setTimeout(() => finish(false), FLUSH_CAP_MS);
      flushWaiters.current.add(answered);
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

  /**
   * Desktop's restore: the body is saved, then the stored content is kept as
   * a version (none when blank, as desktop), then the version replaces the
   * body as an edit. A save that failed or did not end within the cap, a
   * failed snapshot or a failed read aborts with nothing changed: the
   * snapshot reads the STORED doc, so restoring over unsaved edits would lose
   * them from every version. The editor answers the restore with `onRestored`.
   */
  const runRestore = useCallback(
    async (restore: PendingRestore) => {
      const flushed = await flushEditor();
      if (!mounted.current) return;
      const saveError = doc.lastSaveError();
      if (!flushed || saveError !== null) {
        // ERR_NOT_FOUND: "Entry not found" already says so.
        if (saveError !== 'ERR_NOT_FOUND') restoreFailed(saveError);
        return;
      }
      let kept: string | null;
      try {
        kept = await doc.snapshot();
      } catch (e) {
        // Locked or gone: the lock screen or "Entry not found" already says so.
        const code = errorCode(e);
        if (mounted.current && code !== 'ERR_LOCKED' && code !== 'ERR_NOT_FOUND') {
          restoreFailed();
        }
        return;
      }
      // Its first save must not keep the same content again.
      if (kept !== null) doc.markSnapshotTaken();
      const read = await doc.versionContent(restore.versionId);
      if (!mounted.current) return;
      if ('error' in read) {
        restoreFailed(read.error);
        return;
      }
      const exec = editorRef.current?.exec;
      if (!exec) {
        restoreFailed();
        return;
      }
      exec('restoreVersion', read.b64);
    },
    [flushEditor, doc.lastSaveError, doc.snapshot, doc.markSnapshotTaken, doc.versionContent],
  );

  // Back from version history: `take` empties the mailbox, so a restore runs
  // once (one left for another entry is dropped). Only an editable editor
  // restores; until the editor is ready the restore waits for `onReady`.
  useFocusEffect(
    useCallback(() => {
      const restore = takePendingRestore(id);
      if (!restore) return;
      if (editableRef.current === null) parkedRestore.current = restore;
      else if (editableRef.current) void runRestore(restore);
      else restoreFailed();
    }, [id, runRestore]),
  );

  const onReady = useCallback(
    async (canEdit: boolean) => {
      editableRef.current = canEdit;
      setEditable(canEdit);
      setOverlay(false);
      const restore = parkedRestore.current;
      parkedRestore.current = null;
      if (restore) {
        if (canEdit) void runRestore(restore);
        else restoreFailed();
      }
      if (!pendingFocus.current || !canEdit) return;
      pendingFocus.current = false;
      editorRef.current?.exec?.('focusEnd');
    },
    [runRestore],
  );

  const onRestored = useCallback(async (ok: boolean) => {
    if (!ok) restoreFailed();
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

  /**
   * Opens the list at once and saves the title and body meanwhile; the list
   * reads again once the save's session snapshot has likely landed.
   */
  const openHistory = useCallback(() => {
    void flushEntry();
    // Restore is offered only for an entry that opened editable.
    // Hidden only for an entry known to be read-only; before onReady the restore is
    // parked and refused there if the editor opens read-only.
    const canRestore = editableRef.current === false ? '0' : '1';
    router.push({ pathname: '/history/[id]', params: { id, editable: canRestore } });
  }, [flushEntry, id, router]);

  /** The header menu: Version history; Move to Trash, then its confirmation (decision 7). */
  const openMenu = useCallback(() => {
    ActionSheetIOS.showActionSheetWithOptions(
      {
        options: [
          t('editor:entry_context.version_history'),
          t('editor:entry_card.delete_title'),
          t('common:confirm_dialog.cancel'),
        ],
        destructiveButtonIndex: 1,
        cancelButtonIndex: 2,
      },
      (index) => {
        if (index === 0) {
          openHistory();
          return;
        }
        if (index !== 1) return;
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
  }, [moveToTrash, openHistory]);

  const onChangeTitle = (value: string) => {
    // Keys are not touches: typing the title keeps the second lock's reveal.
    noteActivity();
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
              onRestored={onRestored}
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

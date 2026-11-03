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
  Platform,
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
import { PasswordSheet } from '@/src/components/PasswordSheet';
import {
  type EntryFlags,
  getCachedRow,
  useEntryActions,
  useEntryDetail,
} from '@/src/hooks/useEntries';
import { useEntryDoc } from '@/src/hooks/useEntryDoc';
import { noteActivity, useLockSession } from '@/src/hooks/useLockSession';
import { useTemplateLookup } from '@/src/hooks/useTemplates';
import { errorCode, useVault } from '@/src/hooks/useVault';
import { type PendingRestore, takePendingRestore } from '@/src/hooks/useVersions';
import { t } from '@/src/i18n';
import { registerFlusher } from '@/src/lib/flushRegistry';
import { clearPendingRestore } from '@/src/lib/pendingRestore';

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
/** The engine's refusal to lock an entry in Trash (desktop `trashedEntryError.ts`). */
const TRASHED_ENTRY_MESSAGE = 'entry is in Trash; restore it to edit';

/** What a successful reveal retries: both reads, or the lock change that was refused. */
type RevealFor = 'read' | 'lock' | 'unlock';

function delay(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/** Says the restore failed, unless the lock screen (ERR_LOCKED) already shows. */
function restoreFailed(code: MemloreErrorCode | null = null) {
  if (code === 'ERR_LOCKED') return;
  Alert.alert(t('editor:versionHistory.restore_failed'));
}

/** "In Trash" for the engine's refusal of an entry in Trash, else the generic error. */
function lockFailedMessage(e: unknown): string {
  const message =
    typeof e === 'object' && e !== null && 'message' in e && typeof e.message === 'string'
      ? e.message
      : '';
  if (message.includes(TRASHED_ENTRY_MESSAGE)) return t('editor:panel.trashed_banner');
  return t('mobile.error');
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
 *
 * Second lock: Lock entry / Unlock entry (header menu, only while a second-lock
 * password is set). A read refused with ERR_SECOND_LOCK_REQUIRED (a covered
 * entry) shows the reveal sheet and reads once more after a reveal; a cancel,
 * or a second refusal, leaves. A locked entry while not revealed (a re-lock,
 * or a lock while not revealed) shows nothing and leaves without saving.
 *
 * Invisible vault: Add to invisible vault / Remove from invisible (header
 * menu). Adding with no vault open asks for a vault password first (open or
 * silently create) and continues after; on a locked entry a confirmation says
 * its second lock is removed (engine rule). The entry stays shown while its
 * vault is open; once the vault closes it shows nothing and leaves without
 * saving, like a hidden locked entry. Any refusal is the generic "Unable to
 * update".
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
  const session = useLockSession();
  const row = getCachedRow(id);
  // Bumped after a reveal: both reads run once more.
  const [attempt, setAttempt] = useState(0);
  // Both reads start in the same frame and run in parallel.
  const entry = useEntryDetail(id, attempt);
  const doc = useEntryDoc(id, attempt);
  const actions = useEntryActions(id, entry.detail);
  const notFound = entry.notFound || doc.status === 'notFound' || actions.notFound;
  const lockRequired = !notFound && (entry.lockRequired || doc.status === 'lockRequired');
  const failed = !notFound && !lockRequired && (entry.failed || doc.status === 'failed');
  // From a lock change here (the stored state, its journal's lock included).
  const [lockedNow, setLockedNow] = useState<boolean | null>(null);
  const locked = lockedNow ?? entry.detail?.isLocked ?? false;
  // The same for a vault move here (its journal's invisibility included).
  const [invisibleNow, setInvisibleNow] = useState<boolean | null>(null);
  const invisible = invisibleNow ?? entry.detail?.isInvisible ?? false;
  // Hidden again (a re-lock, or its vault closed): nothing of it may show, and the screen leaves.
  const hidden = (locked && !session.revealed) || (invisible && !session.vaultOpen);
  // The reveal sheet is open, and for what; `revealed` says the reveal succeeded.
  const [revealFor, setRevealFor] = useState<RevealFor | null>(null);
  const revealed = useRef(false);
  // The vault password sheet is open; `vaultOpened` says a vault opened.
  const [vaultSheet, setVaultSheet] = useState(false);
  const vaultOpened = useRef(false);
  // The change to run once a sheet has slid away after a reveal or a vault open.
  const afterReveal = useRef<(() => void) | null>(null);
  // The reads already ran once more after a reveal.
  const readRetried = useRef(false);

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
  // Hidden by the second lock: the leave must not save (the gate refuses it).
  const gone = useRef(false);
  const mounted = useRef(true);

  useEffect(() => {
    if (!notFound) return;
    const timer = setTimeout(() => {
      if (router.canGoBack()) router.back();
      else router.replace('/');
    }, NOT_FOUND_BACK_MS);
    return () => clearTimeout(timer);
  }, [notFound, router]);

  // Again for each read after a reveal.
  useEffect(() => {
    setOverlay(true);
    const timer = setTimeout(() => setOverlay(false), OVERLAY_FALLBACK_MS);
    return () => clearTimeout(timer);
  }, [attempt]);

  /** Leaves at once, without saving: the entry is hidden by the second lock. */
  const leaveHidden = useCallback(() => {
    // Once: a second back would pop the screen below.
    if (gone.current) return;
    gone.current = true;
    // A restore confirmed for it must not run on a later open.
    clearPendingRestore(id);
    setLeaving(true);
    if (router.canGoBack()) router.back();
    else router.replace('/');
  }, [id, router]);

  // Only while focused: with version history on top, a back would pop that
  // screen instead; this one then leaves when it gets the focus back.
  useFocusEffect(
    useCallback(() => {
      if (hidden) leaveHidden();
    }, [hidden, leaveHidden]),
  );

  // A covered entry: ask for the reveal, then read once more. A second
  // refusal after a reveal leaves (one retry). Not keyed on `attempt`: the
  // retry's first render still carries the old refusal.
  useEffect(() => {
    if (!lockRequired) return;
    if (readRetried.current) {
      leaveHidden();
      return;
    }
    revealed.current = false;
    setRevealFor('read');
  }, [lockRequired, leaveHidden]);

  // A body read before the detail read was refused (a re-lock in between)
  // must not stay held: the retry after a reveal reads it again.
  useEffect(() => {
    if (entry.lockRequired) doc.discard();
  }, [entry.lockRequired, doc.discard]);

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
    // A trashed or hidden entry is never saved again (the engine refuses it anyway).
    () =>
      trashed.current || gone.current
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
    // `leaving` from a trash or a hide may not have rendered yet: let the pop through.
    if (trashed.current || gone.current) {
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

  // Typing is activity for the second lock's idle re-lock (throttled by the editor).
  const onEditorActivity = useCallback(async () => noteActivity(), []);

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
    // Lets history leave at once on a re-lock or a vault close; left out while not read yet.
    const known = lockedNow ?? entry.detail?.isLocked;
    const lockedParam = known === undefined ? {} : { locked: known ? '1' : '0' };
    const knownInvisible = invisibleNow ?? entry.detail?.isInvisible;
    const invisibleParam =
      knownInvisible === undefined ? {} : { invisible: knownInvisible ? '1' : '0' };
    router.push({
      pathname: '/history/[id]',
      params: { id, editable: canRestore, ...lockedParam, ...invisibleParam },
    });
  }, [flushEntry, id, router, lockedNow, invisibleNow, entry.detail]);

  /**
   * Locks or unlocks the entry. A refusal for want of the reveal shows the
   * sheet and tries once more after a reveal; locking while not revealed
   * hides the entry, so the screen leaves.
   */
  const changeLock = useCallback(
    async (lock: boolean, retried = false) => {
      try {
        const nowLocked = await actions.setLocked(lock);
        if (mounted.current) setLockedNow(nowLocked);
      } catch (e) {
        if (!mounted.current) return;
        const code = errorCode(e);
        // The lock screen or "Entry not found" already says so.
        if (code === 'ERR_LOCKED' || code === 'ERR_NOT_FOUND') return;
        if (code === 'ERR_SECOND_LOCK_REQUIRED' && !retried) {
          revealed.current = false;
          setRevealFor(lock ? 'lock' : 'unlock');
          return;
        }
        Alert.alert(t('mobile.entry_lock.failed'), lockFailedMessage(e));
      }
    },
    [actions.setLocked],
  );

  /**
   * The flags read back after a change: what the screen hides on. A read that
   * failed counts as locked and invisible (fail closed), after the open edits
   * are saved: a hide never saves.
   */
  const applyFlags = useCallback(
    async (flags: EntryFlags | null) => {
      if (flags === null) await flushEntry();
      if (!mounted.current) return;
      setLockedNow(flags?.locked ?? true);
      setInvisibleNow(flags?.invisible ?? true);
    },
    [flushEntry],
  );

  /**
   * `actions.move`, then the stored flags read back: a move into a locked
   * journal locks the entry with it (the screen then leaves on the next
   * re-lock), a move out of one unlocks it; the same for a vault journal and
   * the vault's close.
   */
  const move = useCallback(
    async (journalId: string) => {
      const moved = await actions.move(journalId);
      if (!moved) return false;
      await applyFlags(await actions.readFlags());
      return true;
    },
    [actions.move, actions.readFlags, applyFlags],
  );

  /** Moves the entry into the open vault or out of it; a refusal shows the generic text. */
  const changeInvisible = useCallback(
    async (invisible: boolean) => {
      let flags: EntryFlags | null;
      try {
        flags = await actions.setInvisible(invisible);
      } catch (e) {
        if (!mounted.current) return;
        const code = errorCode(e);
        // The lock screen or "Entry not found" already says so.
        if (code === 'ERR_LOCKED' || code === 'ERR_NOT_FOUND') return;
        Alert.alert(t('editor:entry_context.invisible_mark_error'));
        return;
      }
      await applyFlags(flags);
    },
    [actions.setInvisible, applyFlags],
  );

  /** Moving in: on a locked entry, a confirmation that its second lock goes first. */
  const confirmInvisible = useCallback(
    (invisible: boolean, wasLocked: boolean) => {
      if (!invisible || !wasLocked) {
        void changeInvisible(invisible);
        return;
      }
      Alert.alert(
        t('editor:entry_context.add_to_invisible_vault'),
        t('mobile.invisible.entry_lock_removed'),
        [
          { text: t('common:confirm_dialog.cancel'), style: 'cancel' },
          {
            text: t('editor:entry_context.add_to_invisible_vault'),
            onPress: () => void changeInvisible(true),
          },
        ],
      );
    },
    [changeInvisible],
  );

  /**
   * The menu's vault item. Moving in needs an open vault: without one, the
   * vault password sheet shows at once and the move continues once it has
   * slid away after a vault opened (an Alert shown while it is still
   * dismissing may never appear).
   */
  const startInvisible = useCallback(
    (invisible: boolean, wasLocked: boolean) => {
      if (invisible && !session.vaultOpen) {
        afterReveal.current = () => confirmInvisible(true, wasLocked);
        vaultOpened.current = false;
        setVaultSheet(true);
        return;
      }
      confirmInvisible(invisible, wasLocked);
    },
    [session.vaultOpen, confirmInvisible],
  );

  function runAfterReveal() {
    const then = afterReveal.current;
    afterReveal.current = null;
    if (then) then();
  }

  /**
   * The reveal sheet closed: retry what it was for after a reveal, else leave
   * a covered entry. A lock change runs once the sheet has slid away (iOS): an
   * Alert shown while the sheet is still dismissing may never appear.
   */
  const closeReveal = useCallback(() => {
    const why = revealFor;
    const ok = revealed.current;
    revealed.current = false;
    afterReveal.current = null;
    setRevealFor(null);
    if (why === 'read') {
      if (!ok) {
        leaveHidden();
        return;
      }
      readRetried.current = true;
      setAttempt((n) => n + 1);
    } else if (why !== null && ok) {
      afterReveal.current = () => void changeLock(why === 'lock', true);
      if (Platform.OS !== 'ios') runAfterReveal();
    }
  }, [revealFor, leaveHidden, changeLock]);

  /** The vault sheet closed: the move continues only after a vault opened. */
  function closeVaultSheet() {
    if (!vaultOpened.current) afterReveal.current = null;
    vaultOpened.current = false;
    setVaultSheet(false);
    if (Platform.OS !== 'ios') runAfterReveal();
  }

  // Shown once the entry is read and only while a second-lock password is set.
  const canLock = session.enabled && entry.detail !== null;
  // Shown once the entry is read.
  const canMoveInvisible = entry.detail !== null;

  /**
   * The header menu: Version history; Move to Trash, then its confirmation
   * (decision 7); then Lock entry / Unlock entry when `canLock` (index 2);
   * then Add to invisible vault / Remove from invisible once the entry is
   * read (index 3 after the lock item, else 2).
   */
  const openMenu = useCallback(() => {
    const options = [
      t('editor:entry_context.version_history'),
      t('editor:entry_card.delete_title'),
    ];
    if (canLock) options.push(locked ? t('mobile.entry_lock.unlock') : t('mobile.entry_lock.lock'));
    const invisibleIndex = canMoveInvisible ? options.length : -1;
    // Decided now, as the menu shows it: a refresh before the tap never flips it.
    const wantInvisible = !invisible;
    if (canMoveInvisible) {
      options.push(
        invisible
          ? t('editor:entry_context.remove_invisible')
          : t('editor:entry_context.add_to_invisible_vault'),
      );
    }
    options.push(t('common:confirm_dialog.cancel'));
    ActionSheetIOS.showActionSheetWithOptions(
      {
        options,
        destructiveButtonIndex: 1,
        cancelButtonIndex: options.length - 1,
      },
      (index) => {
        if (index === 0) {
          openHistory();
          return;
        }
        if (index === 2 && canLock) {
          void changeLock(!locked);
          return;
        }
        if (index === invisibleIndex) {
          startInvisible(wantInvisible, locked);
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
  }, [
    moveToTrash,
    openHistory,
    canLock,
    locked,
    changeLock,
    canMoveInvisible,
    invisible,
    startInvisible,
  ]);

  const onChangeTitle = (value: string) => {
    // Keys are not touches: typing the title keeps the second lock's reveal.
    noteActivity();
    titleRef.current = value;
    setTitle(value);
    if (titleTimer.current) clearTimeout(titleTimer.current);
    titleTimer.current = setTimeout(() => void flushTitle(), TITLE_SAVE_DEBOUNCE_MS);
  };

  if (hidden) {
    // Nothing of a hidden entry shows while the screen leaves.
    return (
      <View style={styles.screen}>
        <Stack.Screen options={{ headerTitle: '', headerRight: () => null }} />
      </View>
    );
  }

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

  // The reveal sheet is all that shows of a refused read, even if the other
  // read succeeded first (a re-lock between them): no title, meta or body.
  const revealSheet = (
    <PasswordSheet
      visible={revealFor !== null}
      title={t('nav:sidebar.unlock_second_lock')}
      description={t('nav:sidebar.unlock_second_lock_description')}
      placeholder={t('editor:second_lock_prompt.password_placeholder')}
      submitLabel={t('editor:second_lock_prompt.unlock')}
      onSubmit={async (password) => {
        if (!(await session.reveal(password))) return false;
        revealed.current = true;
        return true;
      }}
      onClose={closeReveal}
      onDismissed={runAfterReveal}
    />
  );

  // Opens the vault the password opens, or silently an empty new one.
  const vaultPasswordSheet = (
    <PasswordSheet
      visible={vaultSheet}
      title={t('settings:security.invisible_lock.open_title')}
      description={t('settings:security.invisible_lock.open_description')}
      placeholder={t('settings:security.invisible_lock.open_placeholder')}
      submitLabel={t('settings:security.invisible_lock.open_button')}
      failedMessage={t('editor:entry_context.invisible_mark_error')}
      onSubmit={async (password) => {
        await session.openVault(password);
        vaultOpened.current = true;
      }}
      onClose={closeVaultSheet}
      onDismissed={runAfterReveal}
    />
  );

  const placeholderTitle = (entry.detail ?? row)?.title ?? null;
  // Once the entry is read, its own text (even empty) replaces the preview.
  const body = entry.detail
    ? placeholderBody(entry.detail.contentText ?? '')
    : (row?.previewText ?? null);

  // One sheet for both: the read after a reveal swaps the content above it,
  // never the sliding sheet itself.
  return (
    <View style={styles.screen}>
      {lockRequired ? (
        <Stack.Screen options={{ headerTitle: '', headerRight: () => null }} />
      ) : (
        <>
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
                  <SymbolView
                    name="ellipsis.circle"
                    tintColor={theme.colors.accentText}
                    size={24}
                  />
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
            <EntryMeta
              meta={actions.meta}
              actions={{ ...actions, move }}
              canAddTags={entry.detail !== null}
            />
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
                  onActivity={onEditorActivity}
                  readOnly={false}
                  dom={{ style: { flex: 1 }, hideKeyboardAccessoryView: true }}
                />
              ) : null}
            </View>
            {editable === true && format?.focused ? (
              <EditorToolbar state={format} exec={exec} />
            ) : null}
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
        </>
      )}
      {/* No ScrollView above them here, so their first tap is never taken. */}
      {revealSheet}
      {vaultPasswordSheet}
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

import { useCallback, useEffect, useRef, useState } from 'react';

import MemloreCore, { type MemloreErrorCode, type Template } from '@/modules/memlore-core';
import { useCodedFailure, useFocusList } from '@/src/hooks/useOrganize';
import { errorCode, useVault } from '@/src/hooks/useVault';

const loadTemplates = () => MemloreCore.listTemplates();

/**
 * Every template, predefined ones first, read on each focus (and by
 * `refresh`). Create and update trim the name (empty resolves ERR_FORMAT
 * without a call), store no content (desktop's form has none) and resolve the
 * stored template, else the refusal's code: ERR_NOT_FOUND for a template gone
 * or predefined (the list reloads). Components use this, never the native
 * module.
 */
export function useTemplates() {
  const { items: templates, setItems, loading, error, refresh } = useFocusList(loadTemplates);
  const failed = useCodedFailure(refresh);

  const createTemplate = useCallback(
    async (name: string, description: string): Promise<Template | MemloreErrorCode> => {
      const trimmed = name.trim();
      if (!trimmed) return 'ERR_FORMAT';
      try {
        const template = await MemloreCore.createTemplate(trimmed, description.trim() || null);
        setItems((prev) => [...prev.filter((tpl) => tpl.id !== template.id), template]);
        return template;
      } catch (e) {
        return failed(e);
      }
    },
    [refresh],
  );

  /** Also clears the template's content (desktop parity). */
  const updateTemplate = useCallback(
    async (
      id: string,
      name: string,
      description: string,
    ): Promise<Template | MemloreErrorCode> => {
      const trimmed = name.trim();
      if (!trimmed) return 'ERR_FORMAT';
      try {
        const template = await MemloreCore.updateTemplate(id, trimmed, description.trim() || null);
        setItems((prev) => prev.map((tpl) => (tpl.id === template.id ? template : tpl)));
        return template;
      } catch (e) {
        return failed(e);
      }
    },
    [refresh],
  );

  /** Resolves null once deleted (confirm first), else the refusal's code. */
  const deleteTemplate = useCallback(
    async (id: string): Promise<MemloreErrorCode | null> => {
      try {
        await MemloreCore.deleteTemplate(id);
        setItems((prev) => prev.filter((tpl) => tpl.id !== id));
        return null;
      } catch (e) {
        return failed(e);
      }
    },
    [refresh],
  );

  return { templates, loading, error, refresh, createTemplate, updateTemplate, deleteTemplate };
}

/**
 * One template by id, read once when asked (no list kept, no read on focus):
 * the entry screen applies the template picked for a new entry. Resolves null
 * when it is gone or the read failed; ERR_LOCKED shows the lock screen.
 */
export function useTemplateLookup() {
  const vault = useVault();
  return useCallback(async (id: string): Promise<Template | null> => {
    try {
      const templates = await MemloreCore.listTemplates();
      return templates.find((tpl) => tpl.id === id) ?? null;
    } catch (e) {
      if (errorCode(e) === 'ERR_LOCKED') vault.markLocked();
      return null;
    }
  }, []);
}

/**
 * Every template for the new-entry picker, read by `load` only (no read on
 * focus: the picker stays mounted on the Entries tab and reads when it opens).
 * An older answer is dropped; ERR_LOCKED shows the lock screen.
 */
export function useTemplateOptions() {
  const vault = useVault();
  const [templates, setTemplates] = useState<Template[]>([]);
  const [error, setError] = useState(false);
  // Bumped by each read and on unmount.
  const generation = useRef(0);

  useEffect(() => {
    return () => {
      generation.current += 1;
    };
  }, []);

  const load = useCallback(async () => {
    const gen = ++generation.current;
    try {
      const next = await MemloreCore.listTemplates();
      if (gen !== generation.current) return;
      setTemplates(next);
      setError(false);
    } catch (e) {
      if (gen !== generation.current) return;
      if (errorCode(e) === 'ERR_LOCKED') vault.markLocked();
      else setError(true);
    }
  }, []);

  return { templates, error, load };
}

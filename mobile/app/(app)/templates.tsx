import { Stack } from 'expo-router';
import { SymbolView } from 'expo-symbols';
import { useEffect, useRef, useState } from 'react';
import {
  ActionSheetIOS,
  ActivityIndicator,
  Alert,
  Animated,
  Modal,
  Pressable,
  ScrollView,
  Text,
  TextInput,
  View,
} from 'react-native';
import { StyleSheet, useUnistyles } from 'react-native-unistyles';

import type { MemloreErrorCode, Template } from '@/modules/memlore-core';
import { ErrorText } from '@/src/components/ErrorText';
import { templateLabel } from '@/src/components/TemplatePicker';
import { useTemplates } from '@/src/hooks/useTemplates';
import { t } from '@/src/i18n';

/** How long a load runs before its spinner shows (fast loads never flash it). */
const SPINNER_DELAY_MS = 150;

/** The form sheet: closed, a new template, or the custom template being edited. */
type FormTarget = { kind: 'closed' } | { kind: 'new' } | { kind: 'edit'; template: Template };

/**
 * Settings > Templates (desktop TemplatesSettings): the user's own, then the
 * built-ins, read-only. A tap on a custom template edits its name and
 * description, a long press deletes it after a confirmation. Content is never
 * edited here: saving stores none (desktop's form does the same).
 */
export default function TemplatesScreen() {
  const list = useTemplates();
  const { theme } = useUnistyles();
  const [form, setForm] = useState<FormTarget>({ kind: 'closed' });
  // A delete that failed for another reason than a lock.
  const [changeFailed, setChangeFailed] = useState(false);
  const showSpinner = useDelayed(list.loading && list.templates.length === 0, SPINNER_DELAY_MS);
  const builtIns = list.templates
    .filter((tpl) => tpl.isPredefined)
    .sort((a, b) => a.sortOrder - b.sortOrder);
  const custom = list.templates.filter((tpl) => !tpl.isPredefined);

  async function remove(template: Template) {
    setChangeFailed(false);
    const code = await list.deleteTemplate(template.id);
    // A lock shows the unlock screen; a template already gone leaves the reloaded list.
    if (code !== null && code !== 'ERR_LOCKED' && code !== 'ERR_NOT_FOUND') setChangeFailed(true);
  }

  function confirmDelete(template: Template) {
    // Desktop shows the name in bold, then "will be removed."
    const body = `${template.name} ${t('settings:templates_section.delete_removed')}`;
    Alert.alert(t('settings:templates_section.delete_title'), body, [
      { text: t('settings:templates_section.cancel'), style: 'cancel' },
      {
        text: t('settings:templates_section.delete'),
        style: 'destructive',
        onPress: () => void remove(template),
      },
    ]);
  }

  function openMenu(template: Template) {
    ActionSheetIOS.showActionSheetWithOptions(
      {
        title: template.name,
        options: [
          t('settings:templates_section.delete'),
          t('settings:templates_section.cancel'),
        ],
        destructiveButtonIndex: 0,
        cancelButtonIndex: 1,
      },
      (index) => {
        if (index === 0) confirmDelete(template);
      },
    );
  }

  return (
    <View style={styles.screen}>
      <Stack.Screen
        options={{
          title: t('settings:categories.templates.label'),
          headerRight: () => (
            <Pressable
              testID="new-template-button"
              accessibilityRole="button"
              accessibilityLabel={t('settings:templates_section.new')}
              hitSlop={8}
              onPress={() => setForm({ kind: 'new' })}
            >
              <SymbolView name="plus" tintColor={theme.colors.accentText} size={22} />
            </Pressable>
          ),
        }}
      />
      <ScrollView contentContainerStyle={styles.content}>
        <ErrorText message={list.error || changeFailed ? t('mobile.error') : null} />
        {list.loading && list.templates.length === 0 ? (
          <FadeIn visible={showSpinner} />
        ) : (
          <>
            <Text style={styles.sectionHeader}>
              {t('settings:templates_section.custom_heading')}
            </Text>
            {custom.length === 0 ? (
              <Text testID="templates-empty" style={styles.empty}>
                {t('settings:templates_section.empty')}
              </Text>
            ) : (
              custom.map((tpl, index) => (
                <Pressable
                  key={tpl.id}
                  testID={`template-row-${index}`}
                  accessibilityRole="button"
                  accessibilityActions={[
                    { name: 'delete', label: t('settings:templates_section.delete') },
                  ]}
                  onAccessibilityAction={(event) => {
                    if (event.nativeEvent.actionName === 'delete') confirmDelete(tpl);
                  }}
                  onPress={() => setForm({ kind: 'edit', template: tpl })}
                  onLongPress={() => openMenu(tpl)}
                  style={styles.row}
                >
                  <TemplateText label={tpl.name} description={tpl.description} />
                  <SymbolView name="chevron.right" tintColor={theme.colors.fgMuted} size={14} />
                </Pressable>
              ))
            )}
            <Text style={styles.sectionHeader}>
              {t('settings:templates_section.builtin_heading')}
            </Text>
            {builtIns.map((tpl) => {
              const { label, description } = templateLabel(tpl);
              return (
                <View key={tpl.id} style={styles.row}>
                  <TemplateText label={label} description={description} />
                </View>
              );
            })}
          </>
        )}
      </ScrollView>
      <TemplateForm
        target={form}
        create={list.createTemplate}
        update={list.updateTemplate}
        onClose={() => setForm({ kind: 'closed' })}
      />
    </View>
  );
}

function TemplateText(props: { label: string; description: string | null }) {
  return (
    <View style={styles.text}>
      <Text style={styles.name} numberOfLines={1}>
        {props.label}
      </Text>
      {props.description ? (
        <Text style={styles.description} numberOfLines={2}>
          {props.description}
        </Text>
      ) : null}
    </View>
  );
}

type SaveResult = Template | MemloreErrorCode;

/** desktop TemplateForm: a name (required) and a description, in a page sheet. */
function TemplateForm(props: {
  target: FormTarget;
  create: (name: string, description: string) => Promise<SaveResult>;
  update: (id: string, name: string, description: string) => Promise<SaveResult>;
  onClose: () => void;
}) {
  const { theme } = useUnistyles();
  const { target } = props;
  const editing = target.kind === 'edit' ? target.template : null;
  const [name, setName] = useState('');
  const [description, setDescription] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  // Each opening starts from the template being edited, or empty.
  useEffect(() => {
    if (target.kind === 'closed') return;
    setName(editing?.name ?? '');
    setDescription(editing?.description ?? '');
    setError(null);
    setSaving(false);
  }, [target]);

  async function save() {
    if (saving) return;
    if (!name.trim()) {
      setError(t('editor:template_form.name_required'));
      return;
    }
    setSaving(true);
    const result = editing
      ? await props.update(editing.id, name, description)
      : await props.create(name, description);
    setSaving(false);
    if (typeof result !== 'string' || result === 'ERR_LOCKED' || result === 'ERR_NOT_FOUND') {
      // Saved; or locked (the unlock screen covers it); or gone (the list reloaded).
      props.onClose();
    } else if (result === 'ERR_FORMAT') {
      setError(t('editor:template_form.name_required'));
    } else {
      setError(t('mobile.error'));
    }
  }

  return (
    <Modal
      visible={target.kind !== 'closed'}
      animationType="slide"
      presentationStyle="pageSheet"
      onRequestClose={props.onClose}
    >
      <View testID="template-form" style={styles.sheet}>
        <View style={styles.sheetHeader}>
          <Pressable accessibilityRole="button" hitSlop={8} onPress={props.onClose}>
            <Text style={styles.cancel}>{t('editor:template_form.cancel')}</Text>
          </Pressable>
          <Text style={styles.sheetTitle} numberOfLines={1}>
            {t(editing ? 'editor:template_form.edit_title' : 'editor:template_form.create_title')}
          </Text>
          <Pressable
            testID="template-save-button"
            accessibilityRole="button"
            hitSlop={8}
            disabled={saving}
            onPress={() => void save()}
            style={saving ? styles.busy : undefined}
          >
            <Text style={styles.save}>
              {editing ? t('editor:template_form.save') : t('editor:template_form.create')}
            </Text>
          </Pressable>
        </View>
        <ScrollView contentContainerStyle={styles.form} keyboardShouldPersistTaps="handled">
          <Text style={styles.label}>{t('editor:template_form.name_label')}</Text>
          <TextInput
            testID="template-name-input"
            style={styles.input}
            value={name}
            onChangeText={(value) => {
              setName(value);
              setError(null);
            }}
            placeholder={t('editor:template_form.name_placeholder')}
            placeholderTextColor={theme.colors.fgMuted}
            accessibilityLabel={t('editor:template_form.name_label')}
            autoFocus
            returnKeyType="next"
          />
          <ErrorText message={error} />
          <Text style={styles.label}>{t('editor:template_form.description_label')}</Text>
          <TextInput
            testID="template-description-input"
            style={[styles.input, styles.multiline]}
            value={description}
            onChangeText={setDescription}
            placeholder={t('editor:template_form.description_placeholder')}
            placeholderTextColor={theme.colors.fgMuted}
            accessibilityLabel={t('editor:template_form.description_label')}
            multiline
          />
        </ScrollView>
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
  content: {
    paddingBottom: theme.space(6),
  },
  sectionHeader: {
    paddingTop: theme.space(5),
    paddingBottom: theme.space(1.5),
    paddingHorizontal: theme.space(4),
    fontSize: 13,
    fontWeight: '600',
    color: theme.colors.fgMuted,
  },
  row: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: theme.space(3),
    paddingVertical: theme.space(3),
    paddingHorizontal: theme.space(4),
    borderBottomWidth: StyleSheet.hairlineWidth,
    borderBottomColor: theme.colors.borderSubtle,
  },
  text: {
    flex: 1,
    gap: theme.space(1),
  },
  name: {
    fontSize: 17,
    color: theme.colors.fg,
  },
  description: {
    fontSize: 15,
    color: theme.colors.fgSecondary,
  },
  empty: {
    paddingVertical: theme.space(3),
    paddingHorizontal: theme.space(4),
    fontSize: 15,
    color: theme.colors.fgMuted,
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
  cancel: {
    fontSize: 17,
    color: theme.colors.accentText,
  },
  save: {
    fontSize: 17,
    fontWeight: '600',
    color: theme.colors.accentText,
  },
  busy: {
    opacity: 0.4,
  },
  form: {
    gap: theme.space(2),
    paddingHorizontal: theme.space(4),
  },
  label: {
    marginTop: theme.space(2),
    fontSize: 15,
    fontWeight: '600',
    color: theme.colors.fgSecondary,
  },
  input: {
    paddingVertical: theme.space(2.5),
    paddingHorizontal: theme.space(3),
    borderRadius: theme.radii.sm,
    backgroundColor: theme.colors.surfaceControl,
    fontSize: 17,
    color: theme.colors.fg,
  },
  multiline: {
    minHeight: theme.space(24),
    textAlignVertical: 'top',
  },
}));

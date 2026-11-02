import { useEffect, useMemo } from 'react';
import { FlatList, Modal, Pressable, Text, View } from 'react-native';
import { StyleSheet } from 'react-native-unistyles';

import { isPredefinedSlug } from '@/editor/templateBody';
import type { Template } from '@/modules/memlore-core';
import { ErrorText } from '@/src/components/ErrorText';
import { useTemplateOptions } from '@/src/hooks/useTemplates';
import { t } from '@/src/i18n';

type Props = {
  visible: boolean;
  /** A row was tapped; the picker stays open until the caller hides it. */
  onSelect: (template: Template) => void;
  /** Done, or the sheet swiped down. */
  onClose: () => void;
};

/** A built-in's name and description come from the editor strings (its `name` is a slug). */
export function templateLabel(template: Template): { label: string; description: string | null } {
  if (template.isPredefined && isPredefinedSlug(template.name)) {
    return {
      label: t(`editor:templates.${template.name}.label`),
      description: t(`editor:templates.${template.name}.description`),
    };
  }
  return { label: template.name, description: template.description };
}

/**
 * A page sheet listing the templates for a new entry: the built-ins by their
 * order, then the user's own. The list reloads each time the sheet opens.
 */
export function TemplatePicker(props: Props) {
  const { templates, error, load } = useTemplateOptions();

  // Only when the sheet opens: it stays mounted while the tab is focused.
  useEffect(() => {
    if (props.visible) void load();
  }, [props.visible, load]);

  const options = useMemo(
    () => [
      ...templates.filter((tpl) => tpl.isPredefined).sort((a, b) => a.sortOrder - b.sortOrder),
      ...templates.filter((tpl) => !tpl.isPredefined),
    ],
    [templates],
  );

  return (
    <Modal
      visible={props.visible}
      animationType="slide"
      presentationStyle="pageSheet"
      onRequestClose={props.onClose}
    >
      <View testID="template-picker" style={styles.sheet}>
        <View style={styles.header}>
          <Text style={styles.title} numberOfLines={1}>
            {t('editor:templates.modal_title')}
          </Text>
          <Pressable accessibilityRole="button" hitSlop={8} onPress={props.onClose}>
            <Text style={styles.done}>{t('mobile.done')}</Text>
          </Pressable>
        </View>
        <ErrorText message={error ? t('mobile.error') : null} />
        <FlatList
          data={options}
          keyExtractor={(tpl) => tpl.id}
          renderItem={({ item, index }) => {
            const { label, description } = templateLabel(item);
            return (
              <Pressable
                testID={`template-option-${index}`}
                accessibilityRole="button"
                onPress={() => props.onSelect(item)}
                style={styles.row}
              >
                <Text style={styles.label} numberOfLines={1}>
                  {label}
                </Text>
                {description ? (
                  <Text style={styles.description} numberOfLines={2}>
                    {description}
                  </Text>
                ) : null}
              </Pressable>
            );
          }}
        />
      </View>
    </Modal>
  );
}

const styles = StyleSheet.create((theme) => ({
  sheet: {
    flex: 1,
    backgroundColor: theme.colors.panel1,
  },
  header: {
    flexDirection: 'row',
    alignItems: 'center',
    gap: theme.space(3),
    paddingVertical: theme.space(4),
    paddingHorizontal: theme.space(4),
  },
  title: {
    flex: 1,
    fontSize: 17,
    fontWeight: '600',
    color: theme.colors.fg,
  },
  done: {
    fontSize: 17,
    fontWeight: '600',
    color: theme.colors.accentText,
  },
  row: {
    gap: theme.space(1),
    paddingVertical: theme.space(3),
    paddingHorizontal: theme.space(4),
    borderBottomWidth: StyleSheet.hairlineWidth,
    borderBottomColor: theme.colors.borderSubtle,
  },
  label: {
    fontSize: 17,
    color: theme.colors.fg,
  },
  description: {
    fontSize: 15,
    color: theme.colors.fgSecondary,
  },
}));

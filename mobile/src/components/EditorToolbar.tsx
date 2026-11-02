import { type SFSymbol, SymbolView } from 'expo-symbols';
import { Alert, Pressable, ScrollView, Text, View } from 'react-native';
import { StyleSheet, useUnistyles } from 'react-native-unistyles';

import type { EntryEditorHandle, Format, FormatState } from '@/editor/EntryEditorDom';
import { t, type TKey } from '@/src/i18n';

/** The formats a button toggles; its command has the same name. */
type ToggleFormat = Exclude<Format, 'link'>;

const BUTTONS: { format: ToggleFormat; symbol: SFSymbol; label: TKey }[] = [
  { format: 'bold', symbol: 'bold', label: 'editor:bubble.bold' },
  { format: 'italic', symbol: 'italic', label: 'editor:bubble.italic' },
  { format: 'strike', symbol: 'strikethrough', label: 'editor:bubble.strike' },
  { format: 'highlight', symbol: 'highlighter', label: 'editor:bubble.highlight' },
  { format: 'heading', symbol: 'textformat.size', label: 'editor:bubble.heading' },
  { format: 'bulletList', symbol: 'list.bullet', label: 'editor:slash.bullet_list' },
  { format: 'orderedList', symbol: 'list.number', label: 'editor:more.ordered_list' },
  { format: 'taskList', symbol: 'checklist', label: 'editor:slash.checklist' },
  { format: 'blockquote', symbol: 'text.quote', label: 'editor:slash.quote' },
];

/**
 * The formatting bar above the keyboard while the editor is focused. Its
 * buttons are `Pressable`s, which never become first responder, so a press
 * leaves the WebView focused (and its keyboard up) and only sends `exec`.
 */
export function EditorToolbar(props: { state: FormatState; exec: EntryEditorHandle['exec'] }) {
  const { state, exec } = props;

  const editLink = () => {
    const href = state.linkHref;
    Alert.prompt(
      t('mobile.link.edit_title'),
      t('mobile.link.edit_message'),
      [
        { text: t('common:confirm_dialog.cancel'), style: 'cancel' },
        ...(href !== null
          ? [
              {
                text: t('mobile.link.remove'),
                style: 'destructive' as const,
                onPress: () => exec('setLink', ''),
              },
            ]
          : []),
        // An empty value removes the link.
        { text: t('mobile.link.save'), onPress: (value?: string) => exec('setLink', value ?? '') },
      ],
      'plain-text',
      href ?? '',
      'url',
    );
  };

  return (
    <View style={styles.bar}>
      {/* TODO(later): the buttons overflow narrow phones (docs/LATER.md "Mobile M3a notes"). */}
      <ScrollView
        horizontal
        keyboardShouldPersistTaps="always"
        showsHorizontalScrollIndicator={false}
        contentContainerStyle={styles.buttons}
      >
        {BUTTONS.map((button) => (
          <ToolbarButton
            key={button.format}
            testID={`toolbar-${button.format}`}
            symbol={button.symbol}
            label={t(button.label)}
            selected={state.active[button.format]}
            onPress={() => exec(button.format)}
          />
        ))}
        <ToolbarButton
          testID="toolbar-link"
          symbol="link"
          label={t('editor:bubble.link')}
          selected={state.active.link}
          onPress={editLink}
        />
      </ScrollView>
      <Pressable
        testID="toolbar-done"
        accessibilityRole="button"
        onPress={() => exec('blur')}
        style={styles.done}
      >
        <Text style={styles.doneLabel}>{t('mobile.done')}</Text>
      </Pressable>
    </View>
  );
}

function ToolbarButton(props: {
  testID: string;
  symbol: SFSymbol;
  label: string;
  selected: boolean;
  onPress: () => void;
}) {
  const { theme } = useUnistyles();
  return (
    <Pressable
      testID={props.testID}
      accessibilityRole="button"
      accessibilityLabel={props.label}
      accessibilityState={{ selected: props.selected }}
      onPress={props.onPress}
      style={({ pressed }) => [
        styles.button,
        props.selected && styles.buttonSelected,
        pressed && styles.buttonPressed,
      ]}
    >
      <SymbolView
        name={props.symbol}
        tintColor={props.selected ? theme.colors.accentText : theme.colors.fg}
        size={20}
      />
    </Pressable>
  );
}

const styles = StyleSheet.create((theme) => ({
  bar: {
    flexDirection: 'row',
    alignItems: 'center',
    borderTopWidth: StyleSheet.hairlineWidth,
    borderTopColor: theme.colors.borderSubtle,
    backgroundColor: theme.colors.panel2,
  },
  buttons: {
    alignItems: 'center',
    paddingHorizontal: theme.space(1),
    gap: theme.space(0.5),
  },
  button: {
    width: 44,
    height: 44,
    alignItems: 'center',
    justifyContent: 'center',
    borderRadius: theme.radii.sm,
  },
  buttonSelected: {
    backgroundColor: theme.colors.accentSoft,
  },
  buttonPressed: {
    backgroundColor: theme.colors.surfaceControlHover,
  },
  done: {
    height: 44,
    justifyContent: 'center',
    paddingHorizontal: theme.space(4),
  },
  doneLabel: {
    fontSize: 17,
    fontWeight: '600',
    color: theme.colors.accentText,
  },
}));

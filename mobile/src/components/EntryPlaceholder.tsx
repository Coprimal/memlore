import { Pressable, type StyleProp, Text, View, type ViewStyle } from 'react-native';
import { StyleSheet } from 'react-native-unistyles';

import { t } from '@/src/i18n';

/** About how much `content_text` the placeholder shows: a screenful and then some. */
const BODY_LIMIT = 4000;
/** How far back from the limit a word or line boundary is looked for. */
const BOUNDARY_WINDOW = 200;

/** `text` cut near `BODY_LIMIT`, at the last line break or space before it when one is close. */
export function placeholderBody(text: string): string {
  if (text.length <= BODY_LIMIT) return text;
  const head = text.slice(0, BODY_LIMIT);
  const lineEnd = head.lastIndexOf('\n');
  if (lineEnd >= BODY_LIMIT - BOUNDARY_WINDOW) return head.slice(0, lineEnd);
  const wordEnd = head.search(/\s\S*$/);
  if (wordEnd >= BODY_LIMIT - BOUNDARY_WINDOW) return head.slice(0, wordEnd);
  return head;
}

/**
 * The native stand-in for an entry: its title and plain text with the editor's
 * metrics (EntryEditorDom: padding 12, 17 px, line height 1.5, system font), so
 * the editor can replace it without a jump. Not scrollable: the text is clipped.
 * `style` lets a screen lay it over the editor (`StyleSheet.absoluteFill`);
 * `onPress` gets a tap on it (the screen focuses the editor once it is ready).
 * The title is one line, like the screen's single-line title field.
 */
export function EntryPlaceholder(props: {
  title: string | null;
  body: string | null;
  style?: StyleProp<ViewStyle>;
  onPress?: () => void;
}) {
  const title = props.title?.trim();
  return (
    // Not one accessibility element: its texts stay readable on their own.
    <Pressable
      testID="entry-placeholder"
      accessible={false}
      disabled={!props.onPress}
      onPress={props.onPress}
      style={[styles.root, props.style]}
    >
      <Text style={[styles.title, !title && styles.untitled]} numberOfLines={1}>
        {title || t('nav:media_gallery.untitled')}
      </Text>
      <View style={styles.body}>
        {props.body ? <Text style={styles.text}>{props.body}</Text> : null}
      </View>
    </Pressable>
  );
}

const styles = StyleSheet.create((theme) => ({
  root: {
    flex: 1,
    overflow: 'hidden',
    backgroundColor: theme.colors.panel1,
  },
  title: {
    paddingTop: 12,
    paddingHorizontal: 12,
    fontSize: 22,
    fontWeight: '600',
    color: theme.colors.fg,
  },
  untitled: {
    color: theme.colors.fgMuted,
  },
  // EntryEditorDom's wrapper: `padding: 12`, then the editor's 17 px / 1.5 text.
  body: {
    flex: 1,
    overflow: 'hidden',
    padding: 12,
  },
  text: {
    fontSize: 17,
    lineHeight: 25.5,
    color: theme.colors.fg,
  },
}));

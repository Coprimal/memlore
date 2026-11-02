// `@expo/ui`'s SwiftUI views exist on iOS only (`DateSheet.ios.tsx`); its
// modules throw when loaded anywhere else, including the Release build's
// server export, which evaluates every route for the web. Android gets its
// own picker in M10.
// TODO(later): a non-iOS date picker (docs/LATER.md "Mobile M3a notes").
export type DateSheetProps = {
  visible: boolean;
  value: number;
  onApply: (unixSeconds: number) => void;
  onClose: () => void;
};

export function DateSheet(_props: DateSheetProps) {
  return null;
}

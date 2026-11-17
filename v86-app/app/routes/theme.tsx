// Theme preview: every colour here is a role from global.css, so switching
// theme repaints the whole page. Open with exp://<your-ip>:8081/--/theme
import { Pressable, ScrollView, Text, View } from 'react-native';
import { THEMES, THEME_LABELS, setTheme } from '../../mixes/mm_theme/theme';

// Full class names: Tailwind only generates classes it can see written out.
const ROLES: [string, string][] = [
  ['surface', 'bg-surface'], ['card', 'bg-card'], ['text', 'bg-text'], ['text-muted', 'bg-text-muted'],
  ['accent', 'bg-accent'], ['line', 'bg-line'], ['success', 'bg-success'], ['warning', 'bg-warning'], ['danger', 'bg-danger'],
];

export default function ThemePreview() {
  return (
    <ScrollView className="flex-1 bg-surface" contentContainerClassName="p-5 pt-16 gap-4">
      <Text className="text-3xl font-bold text-text">Themes</Text>
      <Text className="text-text-muted">One file, every screen. Tap to switch.</Text>

      <View className="flex-row gap-2">
        {THEMES.map((t) => (
          <Pressable key={t} onPress={() => setTheme(t)} className="flex-1 rounded-full bg-accent py-3 items-center">
            <Text className="text-on-accent font-semibold text-xs">{THEME_LABELS[t]}</Text>
          </Pressable>
        ))}
      </View>

      <View className="bg-card rounded-card shadow-card border border-line p-4 gap-2">
        <Text className="text-lg font-bold text-text">Notes</Text>
        <Text className="text-text-muted">A plugin card, styled only with theme roles.</Text>
        <View className="self-start rounded-full bg-accent px-4 py-2">
          <Text className="text-on-accent font-semibold">Open</Text>
        </View>
      </View>

      <View className="flex-row flex-wrap gap-3">
        {ROLES.map(([r, cls]) => (
          <View key={r} className="items-center gap-1">
            <View className={`h-12 w-12 rounded-2xl border border-line ${cls}`} />
            <Text className="text-text-muted text-xs">{r}</Text>
          </View>
        ))}
      </View>

      <View className="bg-protected-surface rounded-card p-4 gap-1">
        <Text className="font-bold text-protected-text">Protected area</Text>
        <Text className="text-protected-text">Permission prompts look the same in every theme.</Text>
      </View>
    </ScrollView>
  );
}

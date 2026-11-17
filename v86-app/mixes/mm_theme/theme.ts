// v86 themes. Switch the whole app's look in one call.
import { Uniwind } from 'uniwind';

export const THEMES = ['v86', 'sunflower', 'ink', 'flat'] as const;
export type ThemeName = (typeof THEMES)[number];

export const THEME_LABELS: Record<ThemeName, string> = {
  v86: 'V86',
  sunflower: 'Sunflower · 3D',
  ink: 'Ink · 3D',
  flat: 'Flat · 2D',
};

export function setTheme(name: ThemeName) {
  Uniwind.setTheme(name);
}

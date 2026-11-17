// Created once by retro + bun so Figma prototype links can navigate.
// Safe to edit; it will not be overwritten.
import { useFonts } from 'expo-font';
import { Redirect, useLocalSearchParams } from 'expo-router';
import { screens } from '../../../mixes/mm_figma';
import { mixFonts } from '../../../mixes/mm_figma/fonts';

export default function MixScreen() {
  const [ready, error] = useFonts(mixFonts);
  const { screen } = useLocalSearchParams<{ screen: string }>();
  const name = String(screen ?? '');
  // Only synced screens: a link like /s/constructor must not reach Object.prototype.
  if (!Object.hasOwn(screens, name)) return <Redirect href="/" />;
  const C = screens[name as keyof typeof screens];
  // Draw once fonts load, or right away if they fail (system font beats a blank screen).
  return ready || error ? <C /> : null;
}

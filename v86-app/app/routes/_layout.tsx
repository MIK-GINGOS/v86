import '../../mixes/mm_theme/global.css';
import { Stack } from 'expo-router';
import { StatusBar } from 'expo-status-bar';
import { useEffect } from 'react';
import { setTheme } from '../../mixes/mm_theme/theme';

export default function RootLayout() {
  // v86 starts in the V86 brand theme; the Look screen can change it.
  useEffect(() => setTheme('v86'), []);
  return (
    <>
      <StatusBar style="auto" />
      <Stack screenOptions={{ headerShown: false }} />
    </>
  );
}

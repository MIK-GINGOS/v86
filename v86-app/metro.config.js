// Metro config for v86. Uniwind compiles Tailwind CSS v4 for React Native.
// It must stay the outermost wrapper.
const { getDefaultConfig } = require('expo/metro-config');
const { withUniwindConfig } = require('uniwind/metro');
const { checkThemes } = require('./scripts/check-themes');

// Named themes switchable at runtime with Uniwind.setTheme(name). Each one is a
// file in mixes/mm_theme/themes/.
const extraThemes = ['sunflower', 'ink', 'flat', 'v86'];

// A broken theme stops Metro here, before it can reach a phone.
const { problems } = checkThemes(__dirname, extraThemes);
if (problems.length) {
  throw new Error('Theme check failed:\n  - ' + problems.join('\n  - '));
}

const config = getDefaultConfig(__dirname);

module.exports = withUniwindConfig(config, {
  cssEntryFile: './mixes/mm_theme/global.css',
  dtsFile: './mixes/mm_theme/uniwind-types.d.ts',
  extraThemes,
});

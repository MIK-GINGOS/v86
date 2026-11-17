// Checks every theme against the official role list in
// contracts/theme-roles.json. Runs when Metro starts (metro.config.js) and
// on its own with:  bun run check:themes
// Fails if a theme is missing a role, has an unknown role, changes a protected
// role, or isn't registered in metro's extraThemes (and the other way round).
const { readFileSync, readdirSync } = require('node:fs');
const { join } = require('node:path');

function checkThemes(root, extraThemes) {
  const { roles, protected: guarded } = JSON.parse(readFileSync(join(root, 'contracts/theme-roles.json'), 'utf8'));
  const dir = join(root, 'mixes/mm_theme/themes');
  const entry = readFileSync(join(root, 'mixes/mm_theme/global.css'), 'utf8');
  const problems = [];
  const variants = new Map(); // name -> { file, vars: Map }

  for (const file of readdirSync(dir).filter((f) => f.endsWith('.css'))) {
    if (!entry.includes(`./themes/${file}`)) problems.push(`${file} is not imported in global.css`);
    const css = readFileSync(join(dir, file), 'utf8');
    for (const m of css.matchAll(/@variant\s+([\w-]+)\s*\{([^}]*)\}/g)) {
      const vars = new Map();
      for (const v of m[2].matchAll(/--([\w-]+)\s*:\s*([^;]+);/g)) vars.set(v[1], v[2].trim());
      if (variants.has(m[1])) problems.push(`theme "${m[1]}" is defined twice (${variants.get(m[1]).file} and ${file})`);
      variants.set(m[1], { file, vars });
    }
  }

  const first = {};
  for (const [name, { file, vars }] of variants) {
    for (const r of roles) if (!vars.has(r)) problems.push(`${file} · "${name}" is missing --${r}`);
    for (const k of vars.keys()) if (!roles.includes(k)) problems.push(`${file} · "${name}" has unknown --${k} (add it to theme-roles.json first)`);
    for (const r of guarded) {
      if (!vars.has(r)) continue;
      first[r] ??= vars.get(r);
      if (vars.get(r) !== first[r]) problems.push(`${file} · "${name}" changes protected --${r}; protected roles must match in every theme`);
    }
  }

  const builtIn = ['light', 'dark'];
  for (const t of [...builtIn, ...(extraThemes || [])]) if (!variants.has(t)) problems.push(`theme "${t}" has no @variant in themes/`);
  for (const t of variants.keys()) if (!builtIn.includes(t) && !(extraThemes || []).includes(t)) problems.push(`theme "${t}" is not in extraThemes in metro.config.js`);

  return { problems, themes: [...variants.keys()] };
}

module.exports = { checkThemes };

if (require.main === module) {
  const root = join(__dirname, '..');
  const metro = readFileSync(join(root, 'metro.config.js'), 'utf8');
  const extra = [...(metro.match(/extraThemes\s*[=:]\s*\[([^\]]*)\]/)?.[1] ?? '').matchAll(/'([\w-]+)'/g)].map((m) => m[1]);
  const { problems, themes } = checkThemes(root, extra);
  if (problems.length) { console.error('Theme check failed:\n  - ' + problems.join('\n  - ')); process.exit(1); }
  console.log('Theme check passed: ' + themes.join(', '));
}

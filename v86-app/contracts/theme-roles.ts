// The official list of theme roles. Every theme in mixes/theme/themes/ must
// define exactly these; screens may only use these. Add a role here first.
import data from './theme-roles.json';

export const THEME_ROLES = data.roles as readonly string[];
export const PROTECTED_ROLES = data.protected as readonly string[];

// ------------ Translator ------------
// Looks strings up in a set of language files: the chosen language, falling back to English, with plural forms and
// {placeholders}. lib/i18n.ts builds the launcher's from every string; the HUD builds its own from just its strings.

export type Dict = { [key: string]: string | Dict };

export type TVars = Record<string, string | number>;

interface LanguageInfo {
  code: string;
  name: string;
  englishName: string;
  locale: string;
  authors: string[];
}

export const SYSTEM_LANGUAGE = "system";

function flatten(dict: Dict, prefix = "", out = new Map<string, string>()): Map<string, string> {
  for (const [k, v] of Object.entries(dict)) {
    const key = prefix ? `${prefix}.${k}` : k;
    if (typeof v === "string") out.set(key, v);
    else if (v && typeof v === "object") flatten(v, key, out);
  }
  return out;
}

function interpolate(text: string, vars: TVars | undefined): string {
  if (!vars) return text;
  return text.replace(/\{(\w+)\}/g, (match, name: string) =>
    name in vars ? String(vars[name]) : match,
  );
}

export function createTranslator(files: Record<string, Dict>) {
  const dictionaries = new Map<string, Dict>();
  for (const [path, dict] of Object.entries(files)) {
    const code = path.slice(path.lastIndexOf("/") + 1, -".json".length);
    dictionaries.set(code, dict);
  }

  const flat = new Map<string, Map<string, string>>();

  function table(code: string): Map<string, string> {
    let t = flat.get(code);
    if (!t) {
      t = flatten(dictionaries.get(code) ?? {});
      flat.set(code, t);
    }
    return t;
  }

  function meta(code: string): LanguageInfo {
    const t = table(code);
    const authors = (dictionaries.get(code)?._meta as Dict | undefined)?.authors;
    return {
      code,
      name: t.get("_meta.name") || code,
      englishName: t.get("_meta.englishName") || t.get("_meta.name") || code,
      locale: t.get("_meta.locale") || code,
      authors: typeof authors === "string" ? authors.split(",").map((a) => a.trim()) : [],
    };
  }

  const languages: LanguageInfo[] = [...dictionaries.keys()]
    .map(meta)
    .sort((a, b) => (a.code === "en" ? -1 : b.code === "en" ? 1 : a.name.localeCompare(b.name)));

  function systemLanguage(): string {
    const wanted = typeof navigator === "undefined" ? [] : (navigator.languages ?? [navigator.language]);
    for (const tag of wanted) {
      const lower = tag.toLowerCase();
      const exact = languages.find((l) => l.code.toLowerCase() === lower);
      if (exact) return exact.code;
      const base = languages.find((l) => l.code.toLowerCase() === lower.split("-")[0]);
      if (base) return base.code;
    }
    return "en";
  }

  let preference = SYSTEM_LANGUAGE;
  let active = "en";
  let plurals = new Intl.PluralRules("en");
  const listeners = new Set<() => void>();

  function setLanguagePreference(value: string | undefined): void {
    const next = value && dictionaries.has(value) ? value : SYSTEM_LANGUAGE;
    const code = next === SYSTEM_LANGUAGE ? systemLanguage() : next;
    if (next === preference && code === active) return;
    preference = next;
    if (code !== active) {
      active = code;
      plurals = new Intl.PluralRules(meta(code).locale);
      if (typeof document !== "undefined") document.documentElement.lang = meta(code).locale;
    }
    listeners.forEach((l) => l());
  }

  function lookup(key: string): string | undefined {
    return table(active).get(key) ?? table("en").get(key);
  }

  function t(key: string, vars?: TVars): string {
    let text: string | undefined;
    if (vars && typeof vars.count === "number") {
      const form = plurals.select(vars.count);
      text = table(active).get(`${key}_${form}`) ?? table(active).get(`${key}_other`);
      text ??= table("en").get(`${key}_${new Intl.PluralRules("en").select(vars.count)}`);
      text ??= table("en").get(`${key}_other`);
    }
    text ??= lookup(key);
    if (text === undefined) {
      if (import.meta.env?.DEV) console.warn(`[i18n] missing key: ${key}`); // eslint-disable-line no-console
      return key;
    }
    return interpolate(text, vars);
  }

  return {
    languages,
    setLanguagePreference,
    activeLanguage: (): string => active,
    formatLocale: (): string | undefined => (preference === SYSTEM_LANGUAGE ? undefined : meta(active).locale),
    subscribeLanguage(cb: () => void): () => void {
      listeners.add(cb);
      return () => listeners.delete(cb);
    },
    t,
    hasKey: (key: string): boolean => lookup(key) !== undefined || lookup(`${key}_other`) !== undefined,
  };
}

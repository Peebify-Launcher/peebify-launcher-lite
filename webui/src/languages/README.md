# Translating Peebify Launcher

Every piece of text in the launcher lives in `en.json`. Each other `.json` file in this
folder is a translation of it.

## Starting a new language

1. Copy `en.json` and name the copy with the language code, e.g. `de.json`, `pt-BR.json`
   or `ja.json`.
2. Fill in `_meta` at the top:
   - `name`: the language's own name, shown in the picker (`"Deutsch"`, `"日本語"`).
   - `englishName`: its English name (`"German"`).
   - `locale`: the locale used for dates and numbers (`"de"`, `"pt-BR"`).
   - `authors`: your name(s), comma-separated.
3. Translate the values on the right. Never change the keys on the left.
4. Run `npm run i18n:check -- de` in `webui/` to check the file and see what's left.

You don't have to finish in one go. Anything you delete or haven't translated yet
shows in English.

## Rules for values

- **Keep `{placeholders}` as they are.** `"Uninstall {game}?"` becomes
  `"{game} deinstallieren?"`; `{game}` is swapped for the game's name. You can move a
  placeholder, but don't rename or translate it.
- **Keep `<tags>` as they are.** In `"Remove <b>{name}</b>?"` the text between `<b>`
  and `</b>` is shown in bold. Translate the text inside, keep the tags around it.
- **Plurals.** Keys ending in `_one` / `_other` are forms of one string chosen by a
  number. Add the forms your language needs: `_zero`, `_one`, `_two`, `_few`, `_many`,
  `_other` (see the [CLDR plural rules](https://www.unicode.org/cldr/charts/latest/supplemental/language_plural_rules.html)).
  `_other` is always required.
- Game names, "Peebify", "GameBanana" and other names stay as they are.

## Updating a translation

When the launcher adds or changes text, `en.json` changes. Run `npm run i18n:check -- <code>`
to list the keys your file is missing, with the English text for each. Keys the check
calls "not in en.json" were renamed or removed; delete them.

## For developers

- Add new text to `en.json`, never inline. Use `const t = useT()` in components and
  `import { t } from "../lib/i18n"` elsewhere; keys are type-checked against `en.json`.
- The in-game HUD only loads the `hud` group (and `_meta`), so text it shows goes under `hud`.
- `npm run i18n:check` says when code asks for a key that does not exist or a string is no
  longer used.
- Use `<Trans>` from `lib/useT` for sentences with markup inside them.
- `npm run i18n:check -- --hardcoded` lists English text still written inline. Mark an
  intentional one (a brand name, a log line) with `// i18n-ignore`.

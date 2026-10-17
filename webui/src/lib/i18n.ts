import type en from "../languages/en.json";
import { createTranslator, type Dict, type TVars } from "./translator";

export { SYSTEM_LANGUAGE, type TVars } from "./translator";

type PluralSuffix = "zero" | "one" | "two" | "few" | "many" | "other";

type Leaves<T, P extends string = ""> = {
  [K in keyof T & string]: T[K] extends string
    ? `${P}${K}`
    : T[K] extends object
      ? Leaves<T[K], `${P}${K}.`>
      : never;
}[keyof T & string];

type AllKeys = Exclude<Leaves<typeof en>, `_meta.${string}`>;
type PluralBase<K> = K extends `${infer B}_other` ? B : never;

export type TKey = Exclude<AllKeys, `${string}_${PluralSuffix}`> | PluralBase<AllKeys>;

const strings = createTranslator(
  import.meta.glob<Dict>("../languages/*.json", { eager: true, import: "default" }),
);

export const LANGUAGES = strings.languages;
export const { setLanguagePreference, activeLanguage, formatLocale, subscribeLanguage } = strings;

export const t: (key: TKey, vars?: TVars) => string = strings.t;

export const hasKey = strings.hasKey as (key: string) => key is TKey;

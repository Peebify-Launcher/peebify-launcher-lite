import { Fragment, useMemo, useSyncExternalStore, type ReactNode } from "react";
import { activeLanguage, subscribeLanguage, t, type TKey, type TVars } from "./i18n";

export function useT(): typeof t {
  const lang = useSyncExternalStore(subscribeLanguage, activeLanguage);
  // eslint-disable-next-line react-hooks/exhaustive-deps -- lang is the point: a new t per language
  return useMemo(() => ((key, vars) => t(key, vars)) as typeof t, [lang]);
}

const TAG = /<(\w+)>(.*?)<\/\1>/gs;

export function Trans({
  k,
  vars,
  tags,
}: {
  k: TKey;
  vars?: TVars;
  tags?: Record<string, (chunk: string) => ReactNode>;
}) {
  const tr = useT();
  const text = tr(k, vars);
  if (!tags) return <>{text}</>;
  const parts: ReactNode[] = [];
  let last = 0;
  for (const m of text.matchAll(TAG)) {
    if (m.index > last) parts.push(text.slice(last, m.index));
    const render = tags[m[1]];
    parts.push(<Fragment key={m.index}>{render ? render(m[2]) : m[2]}</Fragment>);
    last = m.index + m[0].length;
  }
  if (last < text.length) parts.push(text.slice(last));
  return <>{parts}</>;
}

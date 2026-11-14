// Interface languages. Korean is the source: its keys define what every
// other language must provide, so a missing translation fails the build.

import { en } from "./locales/en";
import { ko } from "./locales/ko";
import { zh } from "./locales/zh";

export type Lang = "ko" | "en" | "zh";
export type Key = keyof typeof ko;
export type Messages = Record<Key, string>;

const MESSAGES: Record<Lang, Messages> = { ko, en, zh };
const LOCALE: Record<Lang, string> = { ko: "ko-KR", en: "en-US", zh: "zh-CN" };

/** Languages offered in the settings, each under its own name. */
export const LANGS: { id: Lang; name: string }[] = [
  { id: "ko", name: "한국어" },
  { id: "en", name: "English" },
  { id: "zh", name: "简体中文" },
];

/** The system's language when it is one of ours, otherwise English. */
export function detectLang(): Lang {
  const system = (navigator.language || "en").toLowerCase();
  if (system.startsWith("ko")) return "ko";
  if (system.startsWith("zh")) return "zh";
  return "en";
}

/** The stored choice, or the system's language when none was made. */
export function resolveLang(setting: string | null | undefined): Lang {
  return setting === "ko" || setting === "en" || setting === "zh" ? setting : detectLang();
}

let lang: Lang = detectLang();

export function setLang(next: Lang) {
  lang = next;
  document.documentElement.lang = LOCALE[next];
}

export const getLang = () => lang;

/** BCP 47 tag for number, date and collation formatting. */
export const locale = () => LOCALE[lang];

export const hasKey = (key: string): key is Key => key in ko;

/** The message for `key`, with `{name}` placeholders filled from `params`. */
export function t(key: Key, params?: Record<string, string | number>): string {
  const text = MESSAGES[lang][key] ?? ko[key];
  if (!params) return text;
  return text.replace(/\{(\w+)\}/g, (whole, name: string) => (name in params ? String(params[name]) : whole));
}

/** A count the way the language writes it (thousands separators). */
export const num = (value: number) => value.toLocaleString(locale());

/** Fill the texts of the static page. Elements name their message in
 *  `data-i18n` (text), `data-i18n-placeholder`, `data-i18n-aria`
 *  (aria-label) or `data-i18n-title`. */
export function applyStatic(root: ParentNode = document) {
  const each = (attr: string, apply: (el: HTMLElement, text: string) => void) => {
    for (const el of root.querySelectorAll<HTMLElement>(`[${attr}]`)) {
      const key = el.getAttribute(attr) ?? "";
      if (hasKey(key)) apply(el, t(key));
    }
  };
  each("data-i18n", (el, text) => (el.textContent = text));
  each("data-i18n-placeholder", (el, text) => el.setAttribute("placeholder", text));
  each("data-i18n-aria", (el, text) => el.setAttribute("aria-label", text));
  each("data-i18n-title", (el, text) => el.setAttribute("title", text));
}

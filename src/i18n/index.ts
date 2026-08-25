import i18n from "i18next";
import { initReactI18next } from "react-i18next";
import en from "../locales/en.json";
import zh from "../locales/zh.json";

/// Read the persisted language from the zustand `nestra-ui` localStorage blob
/// (same key main.tsx uses for the theme pre-paint bootstrap). Defaults to en.
function persistedLanguage(): string {
  try {
    if (typeof localStorage === "undefined") return "en";
    const raw = localStorage.getItem("nestra-ui");
    const lang = raw ? (JSON.parse(raw)?.state?.language ?? "en") : "en";
    return typeof lang === "string" && lang ? lang : "en";
  } catch {
    return "en";
  }
}

void i18n.use(initReactI18next).init({
  resources: {
    en: { translation: en },
    zh: { translation: zh },
  },
  lng: persistedLanguage(),
  fallbackLng: "en",
  interpolation: { escapeValue: false },
  returnNull: false,
});

/// Keep the document language in sync with the active locale (a11y + correct
/// `toLocaleTimeString` behavior). Called after init and on changeLanguage.
/// Guarded for non-browser environments (vitest runs format.ts in Node).
function syncHtmlLang(lang: string) {
  if (typeof document !== "undefined") {
    document.documentElement.lang = lang;
  }
}

i18n.on("languageChanged", syncHtmlLang);
syncHtmlLang(i18n.language);

export default i18n;

import React from "react";
import ReactDOM from "react-dom/client";
import { App } from "./app";
import { initTheme } from "./components/layout/ThemeToggle";
import "./i18n";
import "./styles.css";

// Apply the persisted theme + language before first paint so a light choice
// (or OS light under `system`) never flashes the dark default and text renders
// in the saved locale immediately. `nestra-ui` is the zustand persist key;
// ThemeToggle / the Settings language row keep them in sync afterwards.
try {
  const saved = JSON.parse(localStorage.getItem("nestra-ui") ?? "{}")?.state;
  initTheme(saved?.theme === "dark" || saved?.theme === "light" ? saved.theme : "system");
  // Whitelist before touching <html lang>: anything else in localStorage
  // (corruption, manual edits) would pollute the accessibility attribute and
  // desync it from the locale i18n actually settled on.
  if (saved?.language === "en" || saved?.language === "zh") {
    document.documentElement.lang = saved.language;
  }
} catch {
  initTheme("system");
}

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
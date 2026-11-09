/** The page's theme: the person's choice (`system`, `light` or `dark`, kept
 * in localStorage under THEME_KEY), with `system` following
 * `prefers-color-scheme`. index.html applies the same rule before the first
 * paint, so the page never flashes the other theme; this module keeps it
 * current when the choice or the system's scheme changes. */
import { useEffect } from "react";

export const THEME_KEY = "spawn.theme";
export const THEME_CHOICES = ["system", "light", "dark"] as const;
export type ThemeChoice = (typeof THEME_CHOICES)[number];

/** Light only when the system asks for it: with no preference the page is
 * dark, as Spawn always was (the same rule as index.html's script). */
const LIGHT_QUERY = "(prefers-color-scheme: light)";

/** The theme a choice resolves to, given whether the system prefers dark. */
export function resolveTheme(choice: ThemeChoice, systemDark: boolean): "light" | "dark" {
  if (choice === "system") return systemDark ? "dark" : "light";
  return choice;
}

function apply(choice: ThemeChoice) {
  const light = window.matchMedia?.(LIGHT_QUERY).matches ?? false;
  const theme = resolveTheme(choice, !light);
  document.documentElement.dataset.theme = theme;
  document.querySelector('meta[name="color-scheme"]')?.setAttribute("content", theme);
}

/** Keep <html data-theme> on `choice`, following the system while it is
 * `system`. */
export function useTheme(choice: ThemeChoice) {
  useEffect(() => {
    apply(choice);
    if (choice !== "system" || !window.matchMedia) return;
    const query = window.matchMedia(LIGHT_QUERY);
    const follow = () => apply(choice);
    query.addEventListener("change", follow);
    return () => query.removeEventListener("change", follow);
  }, [choice]);
}

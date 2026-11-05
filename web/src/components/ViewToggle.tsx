/* The two-way switch between a time view and its list (Trace/List on a
 * session, Timeline/Cards on missions), and the remembered choice. Light on
 * purpose: no Motion, no d3, so the pages that hold it stay in the first
 * bundle while the time views load on demand. */

import { useState } from "react";
import type { ReactNode } from "react";

import { cx } from "./ui";

export interface ViewOption<T extends string> {
  id: T;
  label: string;
  icon: ReactNode;
}

/** A choice kept in localStorage under `key`; `fallback` when nothing (or
 * something no longer offered) is stored. */
export function useStoredChoice<T extends string>(
  key: string,
  options: readonly T[],
  fallback: T,
): [T, (next: T) => void] {
  const [value, setValue] = useState<T>(() => {
    try {
      const stored = window.localStorage.getItem(key);
      return options.includes(stored as T) ? (stored as T) : fallback;
    } catch {
      return fallback;
    }
  });
  const choose = (next: T) => {
    setValue(next);
    try {
      window.localStorage.setItem(key, next);
    } catch {
      // Private mode: the choice lasts for this page only.
    }
  };
  return [value, choose];
}

export function ViewToggle<T extends string>({
  label,
  options,
  value,
  onChange,
}: {
  label: string;
  options: ReadonlyArray<ViewOption<T>>;
  value: T;
  onChange: (next: T) => void;
}) {
  return (
    <div
      role="radiogroup"
      aria-label={label}
      className="inline-flex shrink-0 items-center rounded-md bg-muted p-0.5"
    >
      {options.map((option) => (
        <button
          key={option.id}
          type="button"
          role="radio"
          aria-checked={value === option.id}
          onClick={() => onChange(option.id)}
          className={cx(
            "inline-flex h-6 items-center gap-1.5 rounded-[5px] px-2 text-caption font-medium transition-colors",
            value === option.id
              ? "bg-surface-selected text-foreground shadow-[var(--surface-shadow)]"
              : "text-muted-foreground hover:text-foreground",
          )}
        >
          {option.icon}
          {option.label}
        </button>
      ))}
    </div>
  );
}

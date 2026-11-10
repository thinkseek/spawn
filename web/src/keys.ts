/* Named keys for an agent (backlog P1): Esc, the arrows, Enter, Y and N,
 * sent from the terminal panel as recorded commands of their own
 * (`POST /api/session/<id>/key`), never as text. A key goes only while the
 * terminal panel itself has focus: never from a text box, never with Ctrl,
 * Alt or Meta (the browser's and the page's shortcuts), and never an
 * auto-repeat (a held arrow would queue a command per repeat). Pure, tested
 * in web/test/keys.test.ts. */

import { KEY_LABEL } from "./commands.ts";

/** A key the server takes, by its name in the action. */
export type KeyName = "escape" | "up" | "down" | "left" | "right" | "enter" | "y" | "n";

/** The keys in the order the hint lists them. */
export const KEY_NAMES: readonly KeyName[] = ["escape", "up", "down", "left", "right", "enter", "y", "n"];

/** `KeyboardEvent.key` to the key's name. */
const BY_EVENT_KEY: Record<string, KeyName> = {
  Escape: "escape",
  ArrowUp: "up",
  ArrowDown: "down",
  ArrowLeft: "left",
  ArrowRight: "right",
  Enter: "enter",
  y: "y",
  Y: "y",
  n: "n",
  N: "n",
};

/** What `keyName` reads of a keydown. */
export interface KeyPress {
  key: string;
  ctrlKey: boolean;
  altKey: boolean;
  metaKey: boolean;
  repeat: boolean;
  /** The event's target is the terminal panel itself, not a control in it
   * (a text box, a button, a tab). */
  onPanel: boolean;
  /** A composition (an input method) is in progress. */
  isComposing?: boolean;
}

/** The named key a keydown sends, or null when it sends none. */
export function keyName(press: KeyPress): KeyName | null {
  if (!press.onPanel || press.repeat || press.isComposing) return null;
  if (press.ctrlKey || press.altKey || press.metaKey) return null;
  return BY_EVENT_KEY[press.key] ?? null;
}

/** The key as the page shows it: `Esc`, `↑`, `Enter`, `Y`. */
export function keyLabel(name: KeyName): string {
  return KEY_LABEL[name] ?? name;
}

/** The hint under a focused panel. */
export function keyHint(to: string): string {
  return `Keys go to ${to}: ${KEY_NAMES.map(keyLabel).join(" ")}`;
}

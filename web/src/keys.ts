/* Named keys for an agent (backlog P1): Esc, the arrows, Enter, Y and N,
 * sent from the terminal panel as recorded commands of their own
 * (`POST /api/session/<id>/key`), never as text. A key goes only while the
 * terminal panel itself has focus and is armed: never from a text box,
 * never with Ctrl, Alt or Meta (the browser's and the page's shortcuts), and
 * never an auto-repeat (a held arrow would queue a command per repeat).
 *
 * Armed (backlog P11): focus alone sends nothing, so a reader who tabs into
 * the panel, or presses Esc to close the drawer, sends no key to the agent.
 * A deliberate click into the panel arms it (clickArms: a plain primary
 * click inside the text that selected nothing, so neither a scrollbar drag
 * nor selecting text to copy arms it), and so does Enter on the focused
 * panel (the keyboard's click); the panel then shows that keys go to the
 * agent. Blur, a click outside, or the keys going to another agent disarms
 * it. While it is not armed,
 * Esc closes the drawer and the arrows scroll, as on any page. Pure, tested
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
  /** The panel was armed by a click (or Enter) since it took focus. */
  armed: boolean;
}

/** What a keydown on the panel does: send a named key, arm the panel, or
 * nothing (the page has it: Esc closes the drawer, an arrow scrolls). */
export type PanelKey = { send: KeyName } | { arm: true } | null;

/** The keydown's effect on the panel (see the module comment). */
export function panelKey(press: KeyPress): PanelKey {
  if (!press.onPanel || press.repeat || press.isComposing) return null;
  if (press.ctrlKey || press.altKey || press.metaKey) return null;
  if (!press.armed) return press.key === "Enter" ? { arm: true } : null;
  const name = BY_EVENT_KEY[press.key];
  return name ? { send: name } : null;
}

/** What `clickArms` reads of a click on the panel. */
export interface PanelClick {
  /** `MouseEvent.button`: 0 is the primary button. */
  button: number;
  /** Ctrl, Alt, Meta or Shift was held (Shift extends a selection). */
  modified: boolean;
  /** The page has a selection after the click: it was a drag that selected
   * text (or a double click that selected a word), not a plain click. */
  selected: boolean;
  /** The click fell inside the panel's client area, not on its scrollbar
   * (or border). */
  inside: boolean;
}

/** A click that arms the panel: a plain primary click inside the text that
 * selected nothing. */
export function clickArms(click: PanelClick): boolean {
  return click.button === 0 && !click.modified && !click.selected && click.inside;
}

/** The named key a keydown sends, or null when it sends none. */
export function keyName(press: KeyPress): KeyName | null {
  const effect = panelKey(press);
  return effect && "send" in effect ? effect.send : null;
}

/** The key as the page shows it: `Esc`, `↑`, `Enter`, `Y`. */
export function keyLabel(name: KeyName): string {
  return KEY_LABEL[name] ?? name;
}

/** The line under an armed panel. */
export function keyHint(to: string): string {
  return `Keys go to ${to}: ${KEY_NAMES.map(keyLabel).join(" ")}`;
}

/** The line under a focused panel that is not armed. */
export function armHint(to: string): string {
  return `Click the screen or press Enter to send keys to ${to}`;
}

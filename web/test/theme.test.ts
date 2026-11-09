import assert from "node:assert/strict";
import test from "node:test";

import { THEME_CHOICES, resolveTheme } from "../src/theme.ts";

test("a chosen theme is that theme; system follows the system's scheme", () => {
  assert.deepEqual(THEME_CHOICES, ["system", "light", "dark"]);
  assert.equal(resolveTheme("light", true), "light");
  assert.equal(resolveTheme("dark", false), "dark");
  assert.equal(resolveTheme("system", true), "dark");
  assert.equal(resolveTheme("system", false), "light");
});

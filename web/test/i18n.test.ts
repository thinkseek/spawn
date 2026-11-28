import assert from "node:assert/strict";
import { test } from "node:test";

import {
  DEFAULT_LOCALE,
  LOCALE_CHOICES,
  LOCALE_KEY,
  getLocale,
  navLabel,
  normalizeLocale,
  setLocale,
  subscribeLocale,
  tr,
} from "../src/i18n.ts";
import { DIAGNOSTIC_VIEWS, PRIMARY_VIEWS, SECONDARY_VIEWS } from "../src/navigation.ts";

test("locale defaults to zh, normalizes variants, and preserves exact LOCALE_CHOICES", () => {
  assert.deepEqual(LOCALE_CHOICES, ["zh", "en"]);
  assert.equal(DEFAULT_LOCALE, "zh");
  assert.equal(LOCALE_KEY, "spawn.locale");
  assert.equal(normalizeLocale("zh"), "zh");
  assert.equal(normalizeLocale("zh-CN"), "zh");
  assert.equal(normalizeLocale("zh_TW"), "zh");
  assert.equal(normalizeLocale("EN-us"), "en");
  assert.equal(normalizeLocale("fr"), "zh");
  assert.equal(normalizeLocale(null, "en"), "en");
  assert.equal(tr("zh", "系统总览", "Overview"), "系统总览");
  assert.equal(tr("en", "系统总览", "Overview"), "Overview");
});

test("setLocale persists, updates getLocale, and notifies subscribers", () => {
  const prevWindow = (globalThis as Record<string, unknown>).window;
  const store = new Map<string, string>();
  const listeners = new Map<string, Array<(ev: unknown) => void>>();

  (globalThis as Record<string, unknown>).window = {
    localStorage: {
      getItem: (k: string) => store.get(k) ?? null,
      setItem: (k: string, v: string) => void store.set(k, v),
    },
    addEventListener: (type: string, fn: (ev: unknown) => void) => {
      listeners.set(type, [...(listeners.get(type) ?? []), fn]);
    },
  };

  let changes = 0;
  const unsub = subscribeLocale(() => {
    changes += 1;
  });

  try {
    setLocale("zh");
    setLocale("en");
    assert.equal(getLocale(), "en");
    assert.equal(store.get(LOCALE_KEY), "en");
    assert.ok(changes >= 1);

    setLocale("zh");
    assert.equal(getLocale(), "zh");
    assert.equal(store.get(LOCALE_KEY), "zh");
  } finally {
    unsub();
    setLocale(DEFAULT_LOCALE);
    if (prevWindow === undefined) {
      delete (globalThis as Record<string, unknown>).window;
    } else {
      (globalThis as Record<string, unknown>).window = prevWindow;
    }
  }
});

test("every navigation view provides both Chinese and English labels", () => {
  const all = [...PRIMARY_VIEWS, ...SECONDARY_VIEWS, ...DIAGNOSTIC_VIEWS, "sessions"];
  for (const id of all) {
    assert.ok(navLabel(id, "zh").length > 0, `zh label missing for ${id}`);
    assert.ok(navLabel(id, "en").length > 0, `en label missing for ${id}`);
  }
});

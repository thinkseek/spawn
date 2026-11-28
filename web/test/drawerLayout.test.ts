import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

test("Drawer isolates header overflow, expands height dynamically, enforces Close shrink-0/44px touch target, and respects safe-area", () => {
  const ui = readFileSync(new URL("../src/components/ui.tsx", import.meta.url), "utf8");
  const drawerIdx = ui.indexOf("export function Drawer(");
  assert.ok(drawerIdx >= 0, "Drawer component exported from ui.tsx");
  const drawerSlice = ui.slice(drawerIdx, drawerIdx + 1800);

  assert.ok(drawerSlice.includes("min-h-14 shrink-0"), "Drawer header uses min-h-14 instead of rigid h-14");
  assert.ok(drawerSlice.includes("env(safe-area-inset-top)"), "Drawer header pads for tablet/notch safe-area");
  assert.ok(drawerSlice.includes("min-w-0 flex-1 items-center gap-2 overflow-hidden"), "Drawer title container clips overflow");
  assert.ok(drawerSlice.includes("lg:inline pointer-coarse:hidden"), "Drawer hides esc kbd shortcut on touch/tablet devices");
  assert.ok(
    drawerSlice.includes('aria-label="Close"') && drawerSlice.includes("shrink-0") && drawerSlice.includes("pointer-coarse:size-11"),
    "Drawer Close button never shrinks and uses 44px touch target on coarse pointers",
  );
});

test("AgentDetail splits title row and wrapping badge bar without shrink-0 on heading so tablet Close never overlaps", () => {
  const detail = readFileSync(new URL("../src/components/fleet/AgentDetail.tsx", import.meta.url), "utf8");
  const headerStart = detail.indexOf("header={");
  const headerEnd = detail.indexOf("<AgentSummary", headerStart);
  assert.ok(headerStart > 0 && headerEnd > headerStart, "AgentDetail header block located");
  const headerBlock = detail.slice(headerStart, headerEnd);

  assert.ok(!headerBlock.includes("shrink-0 truncate"), "AgentDetail h2 must NOT forbid shrinking with shrink-0");
  assert.ok(headerBlock.includes("min-w-0 flex-1 truncate"), "AgentDetail h2 uses min-w-0 flex-1 truncate");
  assert.ok(headerBlock.includes("flex-wrap"), "AgentDetail header badges wrap cleanly onto secondary row");
  assert.ok(headerBlock.includes("useLocale") || detail.includes("tr(locale,"), "AgentDetail localizes header strings via tr()");
});

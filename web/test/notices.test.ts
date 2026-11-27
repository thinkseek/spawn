import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

import type { Alert } from "../src/api.ts";
import {
  currentNotices,
  earlierWords,
  externalMainAdvanceCount,
  noticeTitle,
  safeNoticeEvidence,
  sourceMainNotice,
} from "../src/notices.ts";

function row(
  key: string,
  state: Alert["state"],
  superseded_by: string | null = null,
  more: Partial<Alert> = {},
): Alert {
  return {
    key,
    kind: key.split(":")[0],
    subject: key,
    state,
    fired_event_id: 0,
    fired_at_ms: 0,
    evidence: null,
    cleared_event_id: null,
    cleared_at_ms: null,
    cleared_evidence: null,
    text: key,
    superseded_by,
    ...more,
  };
}

// Objective 151: the Alerts view's notices are each subject's newest, with
// how many came before it.
test("each subject's newest notice is current, with how many came before", () => {
  const alerts = [
    row("task_state:7:10", "notice", "task_state:7:11"),
    row("memory_low:1", "open"),
    row("task_state:7:12", "notice"),
    row("task_state:8:15", "notice"),
    row("task_state:7:11", "notice", "task_state:7:12"),
    row("mission_state:4:20", "notice", "mission_state:4:99"),
    row("task_state:9:1", "cleared"),
  ];
  const got = currentNotices(alerts).map(({ alert, earlier }) => [alert.key, earlierWords(earlier)]);
  assert.deepEqual(got, [
    ["task_state:7:12", "2 earlier"],
    ["task_state:8:15", null],
    // Its next notice is not in the answer: it stands, nothing is hidden.
    ["mission_state:4:20", null],
  ]);
});

test("no notice, or only open and cleared rows, lists nothing", () => {
  assert.deepEqual(currentNotices([]), []);
  assert.deepEqual(currentNotices([row("memory_low:1", "open"), row("task_state:9:1", "cleared")]), []);
});

test("earlier notices are said by number, none by nothing", () => {
  assert.equal(earlierWords(0), null);
  assert.equal(earlierWords(1), "1 earlier");
  assert.equal(earlierWords(10), "10 earlier");
});

test("source_main_advanced fast-forward parses update_kind, range, ahead/behind, missions, and subjects", () => {
  const alert = row("source_main_advanced:1", "notice", null, {
    kind: "source_main_advanced",
    subject: "/data00/repos/spawn",
    text: "/data00/repos/spawn refs/heads/main moved externally 111111111111..222222222222 (fast_forward)",
    evidence: {
      source: "/data00/repos/spawn",
      ref: "refs/heads/main",
      from: "111111111111aaaaaaaaaaaaaaaaaaaaaaaaaaaa",
      to: "222222222222bbbbbbbbbbbbbbbbbbbbbbbbbbbb",
      update_kind: "fast_forward",
      old_reachable: true,
      ahead: 4,
      behind: 0,
      subjects: ["2222222 feat: add landing check", "1999999 fix: guard empty queue"],
      missions: [7, 4, 7],
    },
  });

  const parsed = sourceMainNotice(alert);
  assert.ok(parsed);
  assert.equal(parsed.kind, "source_main_advanced");
  assert.equal(parsed.title, "External main advance");
  assert.equal(parsed.source, "/data00/repos/spawn");
  assert.equal(parsed.ref, "refs/heads/main");
  assert.equal(parsed.from, "111111111111");
  assert.equal(parsed.to, "222222222222");
  assert.equal(parsed.range, "111111111111..222222222222");
  assert.equal(parsed.updateKind, "fast_forward");
  assert.equal(parsed.badge, "fast-forward");
  assert.equal(parsed.ahead, 4);
  assert.equal(parsed.behind, 0);
  assert.equal(parsed.delta, "+4 commits");
  assert.deepEqual(parsed.subjects, ["2222222 feat: add landing check", "1999999 fix: guard empty queue"]);
  assert.equal(parsed.moreSubjects, 2);
  assert.deepEqual(parsed.missions, [4, 7]);
  assert.equal(noticeTitle(alert), "External main advance · /data00/repos/spawn");
  assert.equal(
    parsed.summary,
    "/data00/repos/spawn refs/heads/main moved externally 111111111111..222222222222 (fast-forward, +4 commits) (missions #4, #7)",
  );
});

test("source_main_advanced distinguishes unreachable non_fast_forward, rebased non_fast_forward, and rewound", () => {
  const unreachable = sourceMainNotice(
    row("source_main_advanced:u", "notice", null, {
      kind: "source_main_advanced",
      subject: "/repos/app",
      evidence: {
        source: "/repos/app",
        from: "aaaa1111bbbb2222",
        to: "cccc3333dddd4444",
        update_kind: "non_fast_forward",
        old_reachable: false,
        ahead: 0,
        behind: 0,
        subjects: ["cccc3333 force-pushed branch"],
      },
    }),
  );
  assert.equal(unreachable?.badge, "non-fast-forward");
  assert.equal(unreachable?.delta, "previous commit unreachable");
  assert.equal(unreachable?.moreSubjects, 0);

  const rebased = sourceMainNotice(
    row("source_main_advanced:r", "notice", null, {
      kind: "source_main_advanced",
      subject: "/repos/app",
      evidence: {
        source: "/repos/app",
        from: "aaaa1111bbbb",
        to: "cccc3333dddd",
        update_kind: "non_fast_forward",
        old_reachable: true,
        ahead: 2,
        behind: 1,
      },
    }),
  );
  assert.equal(rebased?.delta, "+2 / -1");

  const rewound = sourceMainNotice(
    row("source_main_advanced:w", "notice", null, {
      kind: "source_main_advanced",
      subject: "/repos/app",
      evidence: {
        source: "/repos/app",
        from: "aaaa1111bbbb",
        to: "cccc3333dddd",
        update_kind: "rewound",
        old_reachable: true,
        ahead: 0,
        behind: 1,
      },
    }),
  );
  assert.equal(rewound?.badge, "rewound");
  assert.equal(rewound?.delta, "-1 commit");
});

test("source_main_synced renders baseline and integrated owner landings", () => {
  const baseline = sourceMainNotice(
    row("source_main_synced:init", "notice", null, {
      kind: "source_main_synced",
      subject: "/repos/spawn",
      evidence: {
        source: "/repos/spawn",
        ref: "refs/heads/main",
        head: "8b6bc347eca63d2a6b6989b75d9b4053a50c1dff",
        reason: "baseline",
        missions: [9],
      },
    }),
  );
  assert.equal(baseline?.title, "Main tracking synced");
  assert.equal(baseline?.badge, "baseline");
  assert.equal(baseline?.range, "8b6bc347eca6");
  assert.equal(baseline?.summary, "Tracking /repos/spawn refs/heads/main at 8b6bc347eca6 (mission #9)");

  const landed = sourceMainNotice(
    row("source_main_synced:landed", "notice", null, {
      kind: "source_main_synced",
      subject: "/repos/spawn",
      evidence: {
        source: "/repos/spawn",
        ref: "refs/heads/main",
        from: "1111222233334444",
        head: "5555666677778888",
        reason: "source.moved",
        landing_event_id: 412,
        missions: [3],
      },
    }),
  );
  assert.equal(landed?.badge, "source.moved");
  assert.equal(landed?.landingEventId, 412);
  assert.equal(landed?.range, "111122223333..555566667777");
  assert.equal(
    landed?.summary,
    "Synced /repos/spawn refs/heads/main (111122223333..555566667777, source.moved) (mission #3)",
  );
});

test("source-main notices sanitize embedded URL credentials, strip unsafe fields, and tolerate malformed evidence", () => {
  const dirty = row("source_main_advanced:dirty", "notice", null, {
    kind: "source_main_advanced",
    subject: "https://bot:super-secret@git.example.com/org/repo.git?token=leak#frag",
    text: "https://bot:super-secret@git.example.com/org/repo.git?token=leak pushed",
    evidence: {
      source: "https://bot:super-secret@git.example.com/org/repo.git?token=leak",
      ref: "refs/heads/main\n",
      from: "aaaa11112222",
      to: "bbbb33334444",
      update_kind: "fast_forward",
      ahead: 1,
      behind: 0,
      subjects: ["deadbeef sync https://u:pw@mirror.internal/repo.git?auth=secret\tline2", "", 123],
      missions: [4, -1, "bad"],
      token: "must-not-appear",
      authorization: "Bearer secret",
      env: { SPAWN_DATABASE_URL: "postgres://secret" },
    },
  });

  const parsed = sourceMainNotice(dirty);
  assert.equal(parsed?.source, "https://git.example.com/org/repo.git");
  assert.equal(parsed?.subjects[0], "deadbeef sync https://mirror.internal/repo.git line2");
  assert.deepEqual(parsed?.missions, [4]);
  assert.ok(!parsed?.summary.includes("super-secret"));
  assert.ok(!parsed?.summary.includes("token=leak"));

  const safeEv = safeNoticeEvidence(dirty) as Record<string, unknown>;
  assert.deepEqual(Object.keys(safeEv).sort(), [
    "ahead",
    "behind",
    "from",
    "missions",
    "ref",
    "source",
    "subjects",
    "to",
    "update_kind",
  ]);
  assert.equal(safeEv.source, "https://git.example.com/org/repo.git");

  // Unrelated alert kinds remain null for sourceMainNotice and keep original evidence untouched.
  const other = row("memory_low:1", "open", null, { evidence: { free_mb: 120 } });
  assert.equal(sourceMainNotice(other), null);
  assert.equal(noticeTitle(other), null);
  assert.deepEqual(safeNoticeEvidence(other), { free_mb: 120 });

  // Malformed/empty evidence never throws.
  const sparse = row("source_main_advanced:sparse", "notice", null, {
    kind: "source_main_advanced",
    subject: "/repos/x",
    evidence: "broken" as unknown as Alert["evidence"],
  });
  assert.equal(sourceMainNotice(sparse)?.updateKind, null);
  assert.equal(safeNoticeEvidence(sparse), null);
});

test("externalMainAdvanceCount and AlertsView wire source-main notice formatting", () => {
  const notices = currentNotices([
    row("source_main_synced:s1", "notice", "source_main_advanced:s2", { kind: "source_main_synced" }),
    row("source_main_advanced:s2", "notice", null, { kind: "source_main_advanced" }),
    row("task_state:7:1", "notice"),
  ]);
  assert.equal(externalMainAdvanceCount(notices), 1);

  const view = readFileSync(new URL("../src/components/alerts/AlertsView.tsx", import.meta.url), "utf8");
  for (const symbol of [
    "sourceMainNotice",
    "noticeTitle",
    "safeNoticeEvidence",
    "externalMainAdvanceCount",
    "SourceMainDetails",
  ]) {
    assert.ok(view.includes(symbol), `AlertsView.tsx uses ${symbol}`);
  }
});

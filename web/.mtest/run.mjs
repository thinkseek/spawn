import { readFileSync } from "node:fs";
import { createElement as h } from "react";
const { normalizeMissions, Markdown, V, renderToString } = await import("./out/entry.js");
// Render internal components by mocking fetch-free path: test MissionsView pieces via normalized data.
const live = JSON.parse(readFileSync("/tmp/mtest/live.json","utf8"));
const cases = {
  live, null: null, empty: {}, junk: "x", nulls: {missions:null,residents:null},
  partial: {missions:[null, {}, {id:9, tasks:null, sources:null}, {id:3, created_at_ms: 5, sources:[1,"a",null], tasks:[null,{},{state:"revise",revisions:2,review_note:"fix **this**"},{state:"cancelled"},{state:"weird"},{state:"accepted",report:"# Done\n\n- a `x`\n- b\n\n```\ncode\n```\n1. one\n2. two\n> quote\n\n<script>alert(1)</script>"}]}],
    residents:[null,{},{name:"docs",state:"proposed",charter:"Keep docs"},{name:"old",state:"retired"}]},
};
for (const [k,v] of Object.entries(cases)) {
  const d = normalizeMissions(v);
  console.log(k, d.missions.map(m=>m.id), d.residents.map(r=>r.name+":"+r.state));
}
const p = normalizeMissions(cases.partial);
const t = p.missions.find(m=>m.id===3).tasks.find(t=>t.state==="accepted");
console.log(renderToString(h(Markdown,{source:t.report})));
for (const [k,v] of Object.entries(cases)) {
  const html = renderToString(h(V.MissionsBody,{data: normalizeMissions(v)}));
  console.log("render", k, html.length, /waiting for human approval/.test(html), /spawn resident approve --name docs/.test(html), /#\/fleet\/session_/.test(html), html.includes("<script>"));
}

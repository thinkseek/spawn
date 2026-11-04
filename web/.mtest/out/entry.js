import { renderToString } from "react-dom/server";
import { ArrowUpRight, Ban, Check, ChevronRight, Hourglass, RotateCcw, Target, Terminal, Users, WifiOff } from "lucide-react";
import { useEffect, useRef, useState } from "react";
import { Fragment, jsx, jsxs } from "react/jsx-runtime";
// \0rolldown/runtime.js
var __defProp = Object.defineProperty;
var __exportAll = (all, no_symbols) => {
	let target = {};
	for (var name in all) __defProp(target, name, {
		get: all[name],
		enumerable: true
	});
	if (!no_symbols) __defProp(target, Symbol.toStringTag, { value: "Module" });
	return target;
};
// src/missions.ts
var TASK_STATES = [
	"assigned",
	"delivered",
	"reported",
	"revise",
	"accepted",
	"cancelled"
];
/** The happy path a task walks; `revise` and `cancelled` branch off it. */
var PIPELINE = [
	"assigned",
	"delivered",
	"reported",
	"accepted"
];
function isObject(value) {
	return value !== null && typeof value === "object" && !Array.isArray(value);
}
function list(value) {
	return Array.isArray(value) ? value : [];
}
function optionalText(value) {
	return typeof value === "string" && value !== "" ? value : null;
}
function optionalNumber(value) {
	return typeof value === "number" && Number.isFinite(value) ? value : null;
}
function taskState(value) {
	if (typeof value !== "string") return "unknown";
	const key = value.toLowerCase();
	return TASK_STATES.includes(key) ? key : "unknown";
}
function residentState(value) {
	if (typeof value !== "string") return "unknown";
	const key = value.toLowerCase();
	return key === "proposed" || key === "approved" || key === "retired" ? key : "unknown";
}
function normalizeTask(value) {
	if (!isObject(value)) return null;
	return {
		id: optionalNumber(value.id),
		kind: optionalText(value.kind),
		agent_id: optionalText(value.agent_id),
		session_id: optionalText(value.session_id),
		brief: optionalText(value.brief),
		state: taskState(value.state),
		raw_state: optionalText(value.state),
		revisions: Math.max(0, optionalNumber(value.revisions) ?? 0),
		review_note: optionalText(value.review_note),
		report: optionalText(value.report),
		created_by: optionalText(value.created_by),
		created_at_ms: optionalNumber(value.created_at_ms),
		updated_at_ms: optionalNumber(value.updated_at_ms)
	};
}
function normalizeMission(value) {
	if (!isObject(value)) return null;
	const sources = list(value.sources).map((source) => optionalText(source)).filter((source) => source !== null);
	const tasks = list(value.tasks).map(normalizeTask).filter((task) => task !== null).sort((left, right) => (left.created_at_ms ?? 0) - (right.created_at_ms ?? 0) || (left.id ?? 0) - (right.id ?? 0));
	return {
		id: optionalNumber(value.id),
		text: optionalText(value.text),
		origin: optionalText(value.origin),
		sources,
		state: optionalText(value.state),
		summary: optionalText(value.summary),
		created_at_ms: optionalNumber(value.created_at_ms),
		updated_at_ms: optionalNumber(value.updated_at_ms),
		tasks
	};
}
function normalizeResident(value) {
	if (!isObject(value)) return null;
	return {
		name: optionalText(value.name),
		agent_id: optionalText(value.agent_id),
		session_id: optionalText(value.session_id),
		charter: optionalText(value.charter),
		state: residentState(value.state),
		raw_state: optionalText(value.state),
		proposed_by: optionalText(value.proposed_by),
		approved_by: optionalText(value.approved_by),
		created_at_ms: optionalNumber(value.created_at_ms),
		updated_at_ms: optionalNumber(value.updated_at_ms)
	};
}
var RESIDENT_RANK = {
	proposed: 0,
	approved: 1,
	unknown: 2,
	retired: 3
};
function normalizeMissions(value) {
	const raw = isObject(value) ? value : {};
	return {
		missions: list(raw.missions).map(normalizeMission).filter((mission) => mission !== null).sort((left, right) => (right.created_at_ms ?? 0) - (left.created_at_ms ?? 0) || (right.id ?? 0) - (left.id ?? 0)),
		residents: list(raw.residents).map(normalizeResident).filter((resident) => resident !== null).sort((left, right) => RESIDENT_RANK[left.state] - RESIDENT_RANK[right.state] || (left.name ?? "").localeCompare(right.name ?? ""))
	};
}
async function fetchMissions(signal) {
	const response = await fetch("/api/missions", {
		signal,
		cache: "no-store",
		headers: { Accept: "application/json" }
	});
	if (!response.ok) throw new Error(`HTTP ${response.status}`);
	return normalizeMissions(await response.json());
}
/** The exact CLI command a human runs to approve a proposed resident. */
function approveCommand(name) {
	return `spawn resident approve --name ${name ?? "<name>"}`;
}
// src/components/ui.tsx
/** Join class names, skipping falsy ones. */
function cx(...parts) {
	return parts.filter(Boolean).join(" ");
}
function Card({ className, children }) {
	return /* @__PURE__ */ jsx("section", {
		className: cx("rounded-lg border border-surface-border bg-surface", className),
		children
	});
}
function CardHeader({ icon, title, meta, className }) {
	return /* @__PURE__ */ jsxs("header", {
		className: cx("flex h-10 shrink-0 items-center gap-2 border-b border-surface-border px-4", className),
		children: [
			icon && /* @__PURE__ */ jsx("span", {
				className: "flex shrink-0 text-faint-foreground",
				children: icon
			}),
			/* @__PURE__ */ jsx("h2", {
				className: "min-w-0 truncate text-label font-medium text-foreground",
				children: title
			}),
			meta && /* @__PURE__ */ jsx("div", {
				className: "ml-auto flex min-w-0 shrink-0 items-center gap-2 text-caption text-muted-foreground",
				children: meta
			})
		]
	});
}
function PageHeader({ title, children }) {
	return /* @__PURE__ */ jsxs("header", {
		className: "flex h-12 shrink-0 items-center gap-3 border-b border-surface-border bg-page-canvas px-4 sm:px-5",
		children: [/* @__PURE__ */ jsx("div", {
			className: "flex min-w-0 items-center gap-2.5",
			children: title
		}), /* @__PURE__ */ jsx("div", {
			className: "ml-auto flex shrink-0 items-center gap-2",
			children
		})]
	});
}
/** Connection status of a poll: a calm dot when live, red when not. */
function LiveStatus({ error, label }) {
	if (error) return /* @__PURE__ */ jsxs("span", {
		title: error,
		className: "inline-flex h-6 items-center gap-1.5 rounded-md bg-destructive/12 px-2 text-caption font-medium text-destructive ring-1 ring-destructive/25 ring-inset",
		children: [/* @__PURE__ */ jsx(WifiOff, { size: 12 }), "Disconnected"]
	});
	return /* @__PURE__ */ jsxs("span", {
		className: "inline-flex h-6 items-center gap-1.5 text-caption text-muted-foreground",
		children: [/* @__PURE__ */ jsxs("span", {
			className: "relative flex size-2",
			children: [/* @__PURE__ */ jsx("span", { className: "absolute inline-flex size-full animate-spawn-pulse rounded-full bg-success/50" }), /* @__PURE__ */ jsx("span", { className: "relative inline-flex size-2 rounded-full bg-success" })]
		}), /* @__PURE__ */ jsx("span", {
			className: "tnum",
			children: label
		})]
	});
}
function EmptyState({ icon, title, hint, className }) {
	return /* @__PURE__ */ jsxs("div", {
		className: cx("flex flex-col items-center justify-center gap-1.5 px-6 py-8 text-center", className),
		children: [
			icon && /* @__PURE__ */ jsx("span", {
				className: "mb-1 text-faint-foreground",
				children: icon
			}),
			/* @__PURE__ */ jsx("p", {
				className: "text-label font-medium text-muted-foreground",
				children: title
			}),
			hint && /* @__PURE__ */ jsx("p", {
				className: "max-w-sm text-caption text-faint-foreground",
				children: hint
			})
		]
	});
}
// src/components/missions/Markdown.tsx
var HEADING = /^(#{1,6})\s+(.*)$/;
var FENCE = /^\s*(```|~~~)\s*([\w+-]*)\s*$/;
var BULLET = /^\s*[-*+]\s+(.*)$/;
var NUMBERED = /^\s*\d+[.)]\s+(.*)$/;
var QUOTE = /^\s*>\s?(.*)$/;
var RULE = /^\s*([-*_])(\s*\1){2,}\s*$/;
function parse(source) {
	const lines = source.replace(/\r\n?/g, "\n").split("\n");
	const blocks = [];
	let index = 0;
	while (index < lines.length) {
		const line = lines[index];
		if (line.trim() === "") {
			index += 1;
			continue;
		}
		const fence = FENCE.exec(line);
		if (fence) {
			const marker = fence[1];
			const body = [];
			index += 1;
			while (index < lines.length && !lines[index].trim().startsWith(marker)) {
				body.push(lines[index]);
				index += 1;
			}
			index += 1;
			blocks.push({
				kind: "code",
				lang: fence[2],
				text: body.join("\n")
			});
			continue;
		}
		const heading = HEADING.exec(line);
		if (heading) {
			blocks.push({
				kind: "heading",
				level: heading[1].length,
				text: heading[2].replace(/\s+#+\s*$/, "")
			});
			index += 1;
			continue;
		}
		if (RULE.test(line)) {
			blocks.push({ kind: "rule" });
			index += 1;
			continue;
		}
		if (BULLET.test(line) || NUMBERED.test(line)) {
			const ordered = !BULLET.test(line);
			const pattern = ordered ? NUMBERED : BULLET;
			const items = [];
			while (index < lines.length) {
				const current = lines[index];
				const item = pattern.exec(current);
				if (item) items.push(item[1]);
				else if (current.trim() !== "" && /^\s{2,}/.test(current) && items.length > 0) items[items.length - 1] += ` ${current.trim()}`;
				else break;
				index += 1;
			}
			blocks.push({
				kind: "list",
				ordered,
				items
			});
			continue;
		}
		if (QUOTE.test(line)) {
			const body = [];
			while (index < lines.length) {
				const quoted = QUOTE.exec(lines[index]);
				if (!quoted) break;
				body.push(quoted[1]);
				index += 1;
			}
			blocks.push({
				kind: "quote",
				text: body.join(" ")
			});
			continue;
		}
		const body = [];
		while (index < lines.length) {
			const current = lines[index];
			if (current.trim() === "" || FENCE.test(current) || HEADING.test(current) || BULLET.test(current) || NUMBERED.test(current) || QUOTE.test(current)) break;
			body.push(current);
			index += 1;
		}
		blocks.push({
			kind: "paragraph",
			text: body.join("\n")
		});
	}
	return blocks;
}
var INLINE = /(`[^`\n]+`)|(\*\*[^*\n]+\*\*)|(__[^_\n]+__)|(\*[^*\n]+\*)/g;
function inline(text) {
	const out = [];
	let last = 0;
	let key = 0;
	for (const match of text.matchAll(INLINE)) {
		const start = match.index ?? 0;
		if (start > last) out.push(text.slice(last, start));
		const token = match[0];
		if (match[1]) out.push(/* @__PURE__ */ jsx("code", {
			className: "rounded bg-muted px-1 py-px font-mono text-[0.92em] text-foreground",
			children: token.slice(1, -1)
		}, key++));
		else if (match[2] || match[3]) out.push(/* @__PURE__ */ jsx("strong", {
			className: "font-semibold text-foreground",
			children: token.slice(2, -2)
		}, key++));
		else out.push(/* @__PURE__ */ jsx("em", { children: token.slice(1, -1) }, key++));
		last = start + token.length;
	}
	if (last < text.length) out.push(text.slice(last));
	return out;
}
var HEADING_CLASS = {
	1: "text-body font-semibold",
	2: "text-label font-semibold",
	3: "text-label font-medium"
};
function Markdown({ source, className }) {
	if (!source) return null;
	const blocks = parse(source);
	return /* @__PURE__ */ jsx("div", {
		className: cx("min-w-0 space-y-2 break-words text-caption text-foreground", className),
		children: blocks.map((block, index) => {
			switch (block.kind) {
				case "heading": return /* @__PURE__ */ jsx("div", {
					role: "heading",
					"aria-level": block.level,
					className: cx("pt-1 text-foreground", HEADING_CLASS[block.level] ?? "text-caption font-medium"),
					children: inline(block.text)
				}, index);
				case "code": return /* @__PURE__ */ jsx("pre", {
					className: "overflow-x-auto rounded-md border border-surface-border bg-muted/60 px-3 py-2 font-mono text-micro leading-relaxed text-foreground",
					children: /* @__PURE__ */ jsx("code", { children: block.text })
				}, index);
				case "list": {
					const ListTag = block.ordered ? "ol" : "ul";
					return /* @__PURE__ */ jsx(ListTag, {
						className: cx("space-y-0.5 pl-5", block.ordered ? "list-decimal" : "list-disc", "marker:text-faint-foreground"),
						children: block.items.map((item, itemIndex) => /* @__PURE__ */ jsx("li", { children: inline(item) }, itemIndex))
					}, index);
				}
				case "quote": return /* @__PURE__ */ jsx("blockquote", {
					className: "border-l-2 border-border-strong pl-3 text-muted-foreground",
					children: inline(block.text)
				}, index);
				case "rule": return /* @__PURE__ */ jsx("hr", { className: "border-surface-border" }, index);
				case "paragraph": return /* @__PURE__ */ jsx("p", {
					className: "whitespace-pre-wrap",
					children: inline(block.text)
				}, index);
			}
		})
	});
}
// src/api.ts
/** Poll a read endpoint every `intervalMs`, starting immediately. */
function usePoll(key, load, intervalMs = 2e3) {
	const [data, setData] = useState(null);
	const [error, setError] = useState(null);
	const loadRef = useRef(load);
	loadRef.current = load;
	const keyRef = useRef(null);
	useEffect(() => {
		let cancelled = false;
		let inFlight = false;
		let controller = null;
		if (keyRef.current !== key) {
			keyRef.current = key;
			setData(null);
			setError(null);
		}
		const tick = async () => {
			if (inFlight) return;
			inFlight = true;
			controller = new AbortController();
			try {
				const next = await loadRef.current(controller.signal);
				if (!cancelled) {
					setData(next);
					setError(null);
				}
			} catch (cause) {
				const aborted = cause instanceof DOMException && cause.name === "AbortError";
				if (!cancelled && !aborted) setError(cause instanceof Error ? cause.message : "request failed");
			} finally {
				inFlight = false;
			}
		};
		tick();
		const timer = window.setInterval(() => void tick(), intervalMs);
		return () => {
			cancelled = true;
			controller?.abort();
			window.clearInterval(timer);
		};
	}, [key, intervalMs]);
	return {
		data,
		error
	};
}
// src/format.ts
/** Format an epoch-millisecond time as a short age. */
function relativeTime(ms) {
	if (ms == null || !Number.isFinite(ms) || ms <= 0) return "—";
	const seconds = Math.floor(Math.max(0, Date.now() - ms) / 1e3);
	if (seconds < 1) return "just now";
	if (seconds < 60) return `${seconds}s ago`;
	const minutes = Math.floor(seconds / 60);
	if (minutes < 60) return `${minutes}m ago`;
	const hours = Math.floor(minutes / 60);
	if (hours < 24) return `${hours}h ago`;
	return `${Math.floor(hours / 24)}d ago`;
}
// src/components/fleet/bits.tsx
/** Small label/value pair for dense fact rows. */
function Fact({ label, children, mono = false, className }) {
	return /* @__PURE__ */ jsxs("div", {
		className: cx("min-w-0", className),
		children: [/* @__PURE__ */ jsx("div", {
			className: "text-micro text-faint-foreground",
			children: label
		}), /* @__PURE__ */ jsx("div", {
			className: cx("truncate text-caption text-foreground", mono && "font-mono"),
			children
		})]
	});
}
/** Uppercase section label used inside cards. */
function SectionLabel({ children, trailing }) {
	return /* @__PURE__ */ jsxs("div", {
		className: "flex items-center gap-2 text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground",
		children: [children, trailing && /* @__PURE__ */ jsx("span", {
			className: "ml-auto normal-case tracking-normal",
			children: trailing
		})]
	});
}
// src/components/rsi/Phase.tsx
var INFO = {
	badge: "bg-info/12 text-info ring-info/25",
	dot: "bg-info",
	bar: "bg-info",
	text: "text-info"
};
({ ...INFO }), { ...INFO };
/** Wall clock that re-renders its caller every `intervalMs`. */
function useNow(intervalMs = 1e3) {
	const [now, setNow] = useState(() => Date.now());
	useEffect(() => {
		const timer = window.setInterval(() => setNow(Date.now()), intervalMs);
		return () => window.clearInterval(timer);
	}, [intervalMs]);
	return now;
}
// src/components/missions/MissionsView.tsx
var MissionsView_exports = /* @__PURE__ */ __exportAll({
	MissionsBody: () => MissionsBody,
	MissionsView: () => MissionsView
});
/**
* Missions and the residents that work them. Strictly read-only: the only
* request is GET /api/missions, and the only "actions" are links into the
* fleet view and commands a human copies into a terminal.
*/
function MissionsView() {
	useNow(1e3);
	const missions = usePoll("missions", fetchMissions, 5e3);
	const data = missions.data;
	return /* @__PURE__ */ jsxs("main", {
		className: "flex min-h-0 min-w-0 flex-1 flex-col bg-page-canvas",
		children: [/* @__PURE__ */ jsx(PageHeader, {
			title: /* @__PURE__ */ jsx("h1", {
				className: "truncate text-body font-semibold text-foreground",
				children: "Missions"
			}),
			children: /* @__PURE__ */ jsx(LiveStatus, {
				error: missions.error,
				label: data ? `${data.missions.length} mission${data.missions.length === 1 ? "" : "s"}` : "Connecting"
			})
		}), !data ? /* @__PURE__ */ jsx("div", {
			className: "grid flex-1 place-items-center",
			children: /* @__PURE__ */ jsx("p", {
				className: "text-caption text-faint-foreground",
				children: missions.error ? `Could not reach the missions API: ${missions.error}` : "Loading missions…"
			})
		}) : /* @__PURE__ */ jsx(MissionsBody, { data })]
	});
}
/** The page body for one snapshot of /api/missions. */
function MissionsBody({ data }) {
	return /* @__PURE__ */ jsx("div", {
		className: "min-h-0 flex-1 overflow-y-auto",
		children: /* @__PURE__ */ jsxs("div", {
			className: "mx-auto flex max-w-[1600px] flex-col gap-3 p-3 sm:p-4",
			children: [/* @__PURE__ */ jsx(ApprovalQueue, { residents: data.residents }), /* @__PURE__ */ jsxs("div", {
				className: "grid min-w-0 gap-3 xl:grid-cols-[minmax(0,1fr)_340px]",
				children: [/* @__PURE__ */ jsx("div", {
					className: "flex min-w-0 flex-col gap-3",
					children: data.missions.length === 0 ? /* @__PURE__ */ jsx(Card, { children: /* @__PURE__ */ jsx(EmptyState, {
						icon: /* @__PURE__ */ jsx(Target, { size: 20 }),
						title: "No missions yet",
						hint: "Missions appear here once the supervisor records one."
					}) }) : data.missions.map((mission, index) => /* @__PURE__ */ jsx(MissionCard, { mission }, mission.id ?? `index-${index}`))
				}), /* @__PURE__ */ jsx(ResidentRoster, { residents: data.residents })]
			})]
		})
	});
}
/** Proposed residents block on a human; they lead the page. */
function ApprovalQueue({ residents }) {
	const proposed = residents.filter((resident) => resident.state === "proposed");
	if (proposed.length === 0) return null;
	return /* @__PURE__ */ jsxs("section", {
		"aria-label": "Residents waiting for human approval",
		className: "rounded-lg border border-warning/30 bg-warning/[0.07]",
		children: [/* @__PURE__ */ jsxs("header", {
			className: "flex items-center gap-2 border-b border-warning/20 px-4 py-2.5",
			children: [/* @__PURE__ */ jsx(Hourglass, {
				size: 15,
				className: "shrink-0 text-warning"
			}), /* @__PURE__ */ jsxs("h2", {
				className: "text-label font-medium text-foreground",
				children: [
					proposed.length,
					" resident",
					proposed.length === 1 ? "" : "s",
					" waiting for human approval"
				]
			})]
		}), /* @__PURE__ */ jsx("ul", {
			className: "divide-y divide-warning/15",
			children: proposed.map((resident, index) => /* @__PURE__ */ jsxs("li", {
				className: "space-y-2 px-4 py-3",
				children: [
					/* @__PURE__ */ jsxs("div", {
						className: "flex flex-wrap items-center gap-2",
						children: [
							/* @__PURE__ */ jsx("span", {
								className: "text-label font-semibold text-foreground",
								children: resident.name ?? "unnamed resident"
							}),
							/* @__PURE__ */ jsx(ResidentStateBadge, { resident }),
							resident.proposed_by && /* @__PURE__ */ jsxs("span", {
								className: "text-caption text-muted-foreground",
								children: ["proposed by ", resident.proposed_by]
							}),
							/* @__PURE__ */ jsx("span", {
								className: "tnum ml-auto text-micro text-faint-foreground",
								children: relativeTime(resident.created_at_ms)
							})
						]
					}),
					resident.charter && /* @__PURE__ */ jsx(Markdown, {
						source: resident.charter,
						className: "text-muted-foreground"
					}),
					/* @__PURE__ */ jsx(CommandLine, { command: approveCommand(resident.name) })
				]
			}, resident.name ?? `index-${index}`))
		})]
	});
}
function CommandLine({ command }) {
	return /* @__PURE__ */ jsxs("div", {
		className: "flex min-w-0 items-center gap-2 rounded-md border border-surface-border bg-surface px-2.5 py-1.5",
		children: [/* @__PURE__ */ jsx(Terminal, {
			size: 12,
			className: "shrink-0 text-faint-foreground"
		}), /* @__PURE__ */ jsx("code", {
			title: "Run this in a terminal to approve",
			className: "min-w-0 flex-1 overflow-x-auto whitespace-nowrap font-mono text-caption text-foreground select-all",
			children: command
		})]
	});
}
var RESIDENT_STYLE = {
	proposed: "bg-warning/15 text-warning ring-warning/30",
	approved: "bg-success/12 text-success ring-success/25",
	retired: "bg-muted text-faint-foreground ring-surface-border",
	unknown: "bg-transparent text-faint-foreground ring-surface-border"
};
function ResidentStateBadge({ resident }) {
	const label = resident.state === "proposed" ? "waiting for human approval" : resident.state === "unknown" ? resident.raw_state ?? "unknown" : resident.state;
	return /* @__PURE__ */ jsx("span", {
		className: cx("inline-flex h-5 shrink-0 items-center rounded-md px-1.5 text-micro font-medium ring-1 ring-inset", RESIDENT_STYLE[resident.state]),
		children: label
	});
}
function ResidentRoster({ residents }) {
	return /* @__PURE__ */ jsxs(Card, {
		className: "h-fit min-w-0",
		children: [/* @__PURE__ */ jsx(CardHeader, {
			icon: /* @__PURE__ */ jsx(Users, { size: 14 }),
			title: "Residents",
			meta: /* @__PURE__ */ jsx("span", {
				className: "tnum",
				children: residents.length || ""
			})
		}), residents.length === 0 ? /* @__PURE__ */ jsx(EmptyState, {
			title: "No residents",
			hint: "Long-lived agents with a charter appear here once proposed."
		}) : /* @__PURE__ */ jsx("ul", {
			className: "divide-y divide-surface-border",
			children: residents.map((resident, index) => /* @__PURE__ */ jsxs("li", {
				className: cx("space-y-1.5 px-4 py-3", resident.state === "proposed" && "bg-warning/[0.05]", resident.state === "retired" && "opacity-60"),
				children: [
					/* @__PURE__ */ jsxs("div", {
						className: "flex min-w-0 items-center gap-2",
						children: [/* @__PURE__ */ jsx("span", {
							className: "min-w-0 truncate text-label font-medium text-foreground",
							children: resident.name ?? "unnamed resident"
						}), /* @__PURE__ */ jsx("span", {
							className: "ml-auto",
							children: /* @__PURE__ */ jsx(ResidentStateBadge, { resident })
						})]
					}),
					resident.charter ? /* @__PURE__ */ jsx(Markdown, {
						source: resident.charter,
						className: "text-muted-foreground"
					}) : /* @__PURE__ */ jsx("p", {
						className: "text-caption text-faint-foreground",
						children: "No charter."
					}),
					/* @__PURE__ */ jsxs("div", {
						className: "flex flex-wrap items-center gap-x-3 gap-y-1 text-micro text-faint-foreground",
						children: [
							resident.proposed_by && /* @__PURE__ */ jsxs("span", { children: ["proposed by ", resident.proposed_by] }),
							resident.approved_by && /* @__PURE__ */ jsxs("span", { children: ["approved by ", resident.approved_by] }),
							/* @__PURE__ */ jsx(AgentLink, {
								sessionId: resident.session_id,
								agentId: resident.agent_id
							})
						]
					}),
					resident.state === "proposed" && /* @__PURE__ */ jsx(CommandLine, { command: approveCommand(resident.name) })
				]
			}, resident.name ?? `index-${index}`))
		})]
	});
}
var MISSION_STATE_STYLE = {
	active: "bg-info/12 text-info ring-info/25",
	done: "bg-success/12 text-success ring-success/25",
	completed: "bg-success/12 text-success ring-success/25",
	accepted: "bg-success/12 text-success ring-success/25",
	failed: "bg-destructive/12 text-destructive ring-destructive/25",
	cancelled: "bg-muted text-faint-foreground ring-surface-border"
};
function Pill({ className, children, title }) {
	return /* @__PURE__ */ jsx("span", {
		title,
		className: cx("inline-flex h-5 shrink-0 items-center gap-1 rounded-md px-1.5 text-micro font-medium ring-1 ring-inset", className ?? "bg-muted text-muted-foreground ring-surface-border"),
		children
	});
}
function MissionCard({ mission }) {
	const state = mission.state ?? "unknown";
	const title = firstLine(mission.text) ?? "Untitled mission";
	const counts = countStates(mission.tasks);
	return /* @__PURE__ */ jsxs(Card, {
		className: "min-w-0",
		children: [
			/* @__PURE__ */ jsxs("header", {
				className: "flex flex-wrap items-center gap-2 border-b border-surface-border px-4 py-2.5",
				children: [
					/* @__PURE__ */ jsxs("span", {
						className: "tnum font-mono text-caption text-faint-foreground",
						children: ["#", mission.id ?? "?"]
					}),
					/* @__PURE__ */ jsx("h2", {
						className: "min-w-0 flex-1 truncate text-label font-medium text-foreground",
						children: title
					}),
					/* @__PURE__ */ jsx(Pill, {
						className: MISSION_STATE_STYLE[state.toLowerCase()],
						children: state
					})
				]
			}),
			/* @__PURE__ */ jsxs("div", {
				className: "space-y-3 px-4 py-3",
				children: [
					/* @__PURE__ */ jsxs("div", {
						className: "grid grid-cols-2 gap-x-4 gap-y-2 sm:grid-cols-4",
						children: [
							/* @__PURE__ */ jsx(Fact, {
								label: "Origin",
								children: mission.origin ?? "—"
							}),
							/* @__PURE__ */ jsx(Fact, {
								label: "Created",
								children: /* @__PURE__ */ jsx("span", {
									title: absolute(mission.created_at_ms),
									children: relativeTime(mission.created_at_ms)
								})
							}),
							/* @__PURE__ */ jsx(Fact, {
								label: "Updated",
								children: /* @__PURE__ */ jsx("span", {
									title: absolute(mission.updated_at_ms),
									children: relativeTime(mission.updated_at_ms)
								})
							}),
							/* @__PURE__ */ jsx(Fact, {
								label: "Tasks",
								children: /* @__PURE__ */ jsxs("span", {
									className: "tnum",
									children: [
										mission.tasks.length,
										counts.accepted > 0 && ` · ${counts.accepted} accepted`,
										counts.cancelled > 0 && ` · ${counts.cancelled} cancelled`
									]
								})
							})
						]
					}),
					mission.summary && /* @__PURE__ */ jsxs("div", {
						className: "rounded-md border border-success/20 bg-success/[0.05] px-3 py-2",
						children: [/* @__PURE__ */ jsx(SectionLabel, { children: "Summary" }), /* @__PURE__ */ jsx(Markdown, {
							source: mission.summary,
							className: "mt-1"
						})]
					}),
					mission.text && /* @__PURE__ */ jsx(Collapsible, {
						label: "Mission text",
						children: /* @__PURE__ */ jsx(Markdown, { source: mission.text })
					}),
					mission.sources.length > 0 && /* @__PURE__ */ jsxs("div", { children: [/* @__PURE__ */ jsx(SectionLabel, { children: "Sources" }), /* @__PURE__ */ jsx("ul", {
						className: "mt-1 space-y-0.5",
						children: mission.sources.map((source, index) => /* @__PURE__ */ jsx("li", {
							className: "truncate font-mono text-micro text-muted-foreground",
							title: source,
							children: source
						}, `${source}-${index}`))
					})] })
				]
			}),
			/* @__PURE__ */ jsxs("div", {
				className: "border-t border-surface-border",
				children: [/* @__PURE__ */ jsx("div", {
					className: "px-4 pt-3",
					children: /* @__PURE__ */ jsx(SectionLabel, {
						trailing: /* @__PURE__ */ jsx("span", {
							className: "tnum",
							children: mission.tasks.length || ""
						}),
						children: "Tasks"
					})
				}), mission.tasks.length === 0 ? /* @__PURE__ */ jsx("p", {
					className: "px-4 py-3 text-caption text-faint-foreground",
					children: "No tasks dispatched yet."
				}) : /* @__PURE__ */ jsx("ul", {
					className: "divide-y divide-surface-border",
					children: mission.tasks.map((task, index) => /* @__PURE__ */ jsx(TaskRow, { task }, task.id ?? `index-${index}`))
				})]
			})
		]
	});
}
function TaskRow({ task }) {
	return /* @__PURE__ */ jsxs("li", {
		className: cx("space-y-2.5 px-4 py-3", task.state === "cancelled" && "bg-muted/30"),
		children: [
			/* @__PURE__ */ jsxs("div", {
				className: "flex flex-wrap items-center gap-2",
				children: [
					/* @__PURE__ */ jsxs("span", {
						className: "tnum font-mono text-caption text-faint-foreground",
						children: ["task #", task.id ?? "?"]
					}),
					task.kind && /* @__PURE__ */ jsx(Pill, {
						className: task.kind === "resident" ? "bg-jev/12 text-jev ring-jev/30" : void 0,
						children: task.kind
					}),
					/* @__PURE__ */ jsx(AgentLink, {
						sessionId: task.session_id,
						agentId: task.agent_id
					}),
					/* @__PURE__ */ jsxs("span", {
						className: "tnum ml-auto text-micro text-faint-foreground",
						children: [task.created_by && /* @__PURE__ */ jsxs(Fragment, { children: [
							"by ",
							task.created_by,
							" · "
						] }), /* @__PURE__ */ jsxs("span", {
							title: absolute(task.updated_at_ms),
							children: ["updated ", relativeTime(task.updated_at_ms)]
						})]
					})
				]
			}),
			/* @__PURE__ */ jsx(Pipeline, { task }),
			(task.state === "revise" || task.revisions > 0) && /* @__PURE__ */ jsxs("div", {
				className: "rounded-md border border-warning/25 bg-warning/[0.06] px-3 py-2",
				children: [/* @__PURE__ */ jsxs("div", {
					className: "flex items-center gap-1.5 text-caption font-medium text-warning",
					children: [
						/* @__PURE__ */ jsx(RotateCcw, { size: 12 }),
						task.state === "revise" ? "Sent back for revision" : "Revised",
						/* @__PURE__ */ jsxs("span", {
							className: "tnum",
							children: [
								"· ",
								task.revisions,
								" revision",
								task.revisions === 1 ? "" : "s"
							]
						})
					]
				}), task.review_note ? /* @__PURE__ */ jsx(Markdown, {
					source: task.review_note,
					className: "mt-1"
				}) : /* @__PURE__ */ jsx("p", {
					className: "mt-1 text-caption text-faint-foreground",
					children: "No review note."
				})]
			}),
			task.review_note && task.state !== "revise" && task.revisions === 0 && /* @__PURE__ */ jsxs("div", {
				className: "rounded-md border border-surface-border px-3 py-2",
				children: [/* @__PURE__ */ jsx(SectionLabel, { children: "Review note" }), /* @__PURE__ */ jsx(Markdown, {
					source: task.review_note,
					className: "mt-1"
				})]
			}),
			task.brief && /* @__PURE__ */ jsx(Collapsible, {
				label: "Brief",
				children: /* @__PURE__ */ jsx(Markdown, { source: task.brief })
			}),
			task.report ? /* @__PURE__ */ jsx(Collapsible, {
				label: "Report",
				defaultOpen: true,
				children: /* @__PURE__ */ jsx(Markdown, { source: task.report })
			}) : /* @__PURE__ */ jsx("p", {
				className: "text-caption text-faint-foreground",
				children: "No report yet."
			})
		]
	});
}
/** How far along the happy path a task has got (index into PIPELINE). */
function reached(state) {
	switch (state) {
		case "assigned": return 0;
		case "delivered": return 1;
		case "reported":
		case "revise": return 2;
		case "accepted": return 3;
		default: return -1;
	}
}
function Pipeline({ task }) {
	const cancelled = task.state === "cancelled";
	const revise = task.state === "revise";
	const at = reached(task.state);
	return /* @__PURE__ */ jsxs("ol", {
		"aria-label": `Task state: ${task.raw_state ?? "unknown"}`,
		className: "flex flex-wrap items-center gap-1",
		children: [
			PIPELINE.map((step, index) => {
				const done = !cancelled && index <= at;
				const current = !cancelled && !revise && index === at;
				return /* @__PURE__ */ jsxs("li", {
					className: "flex items-center gap-1",
					children: [index > 0 && /* @__PURE__ */ jsx(ChevronRight, {
						size: 12,
						className: done ? "text-muted-foreground" : "text-faint-foreground/50"
					}), /* @__PURE__ */ jsxs("span", {
						className: cx("inline-flex h-6 items-center gap-1 rounded-md px-2 text-micro font-medium ring-1 ring-inset", cancelled ? "text-faint-foreground/60 ring-surface-border line-through" : current ? step === "accepted" ? "bg-success/15 text-success ring-success/30" : "bg-info/15 text-info ring-info/30" : done ? "bg-muted text-foreground ring-surface-border" : "text-faint-foreground ring-surface-border"),
						children: [done && !current && /* @__PURE__ */ jsx(Check, { size: 11 }), step]
					})]
				}, step);
			}),
			revise && /* @__PURE__ */ jsxs("li", {
				className: "flex items-center gap-1",
				children: [/* @__PURE__ */ jsx(RotateCcw, {
					size: 12,
					className: "text-warning"
				}), /* @__PURE__ */ jsxs("span", {
					className: "inline-flex h-6 items-center gap-1 rounded-md bg-warning/15 px-2 text-micro font-medium text-warning ring-1 ring-warning/30 ring-inset",
					children: ["revise", /* @__PURE__ */ jsxs("span", {
						className: "tnum",
						children: ["×", task.revisions]
					})]
				})]
			}),
			cancelled && /* @__PURE__ */ jsx("li", {
				className: "flex items-center gap-1 pl-1",
				children: /* @__PURE__ */ jsxs("span", {
					className: "inline-flex h-6 items-center gap-1 rounded-md bg-destructive/12 px-2 text-micro font-medium text-destructive ring-1 ring-destructive/25 ring-inset",
					children: [/* @__PURE__ */ jsx(Ban, { size: 11 }), "cancelled"]
				})
			}),
			task.state === "unknown" && /* @__PURE__ */ jsx("li", {
				className: "pl-1",
				children: /* @__PURE__ */ jsx(Pill, { children: task.raw_state ?? "no state" })
			})
		]
	});
}
function AgentLink({ sessionId, agentId }) {
	const label = agentId ?? sessionId;
	if (!label) return null;
	if (!sessionId) return /* @__PURE__ */ jsx("span", {
		className: "truncate font-mono text-caption text-muted-foreground",
		children: label
	});
	return /* @__PURE__ */ jsxs("a", {
		href: `#/fleet/${encodeURIComponent(sessionId)}`,
		title: `Open ${sessionId} in the fleet view`,
		className: "inline-flex min-w-0 items-center gap-0.5 truncate font-mono text-caption text-info hover:underline",
		children: [label, /* @__PURE__ */ jsx(ArrowUpRight, {
			size: 12,
			className: "shrink-0"
		})]
	});
}
function Collapsible({ label, children, defaultOpen = false }) {
	return /* @__PURE__ */ jsxs("details", {
		open: defaultOpen,
		className: "group min-w-0",
		children: [/* @__PURE__ */ jsxs("summary", {
			className: "flex cursor-pointer list-none items-center gap-1 text-micro font-medium uppercase tracking-[0.08em] text-faint-foreground select-none hover:text-muted-foreground [&::-webkit-details-marker]:hidden",
			children: [/* @__PURE__ */ jsx(ChevronRight, {
				size: 12,
				className: "transition-transform group-open:rotate-90"
			}), label]
		}), /* @__PURE__ */ jsx("div", {
			className: "mt-1.5 rounded-md border border-surface-border bg-page-canvas/60 px-3 py-2",
			children
		})]
	});
}
function firstLine(text) {
	if (!text) return null;
	const line = text.split("\n").map((part) => part.trim()).find((part) => part !== "");
	return line ? line.replace(/^#+\s*/, "") : null;
}
function countStates(tasks) {
	const counts = {
		assigned: 0,
		delivered: 0,
		reported: 0,
		revise: 0,
		accepted: 0,
		cancelled: 0,
		unknown: 0
	};
	for (const task of tasks) counts[task.state] += 1;
	return counts;
}
function absolute(ms) {
	if (ms == null || ms <= 0) return void 0;
	return new Date(ms).toLocaleString();
}
export { Markdown, MissionsView_exports as V, normalizeMissions, renderToString };

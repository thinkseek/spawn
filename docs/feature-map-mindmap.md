# Feature Map Mind Map

The Feature Map (`#/features`) defaults to an interactive **Mind Map** view over a mission's capabilities, while keeping the **State Columns** audit view one click away and preserving Spawn's core invariant: **the UI never derives capability state**. Every state (`planned`, `implemented`, `verified`, `live`) and its four independent booleans come straight from Spawn's server-side records in `crates/spawn-cli/src/serve/feature_map.rs`.

## 1. Architecture & Library Seams

| Layer | File | Responsibility |
| --- | --- | --- |
| Backend schema, validation & API | [`crates/spawn-cli/src/serve/feature_map.rs`](../crates/spawn-cli/src/serve/feature_map.rs) | Parses optional `parent`, `kind`, `tags`, `files`, `relations` on each feature; enforces strict structural & cycle validation; emits per-feature fields and mission-level `graph` summary in `GET /api/features` |
| Normalized read model | [`web/src/features.ts`](../web/src/features.ts) | Normalizes all server fields (with backward-compatible defaults for older payloads) without deriving states |
| Pure graph & layout engine | [`web/src/featuresGraph.ts`](../web/src/featuresGraph.ts) | Zero-React, zero-DOM module: mind-map tree construction, dense-mission auto-clustering & folding, bidirectional file index, typed-relation index, graph validation, search with ancestor expansion, and deterministic left-to-right Bezier layout |
| Interactive canvas | [`web/src/components/features/FeatureMindMap.tsx`](../web/src/components/features/FeatureMindMap.tsx) | Pan/zoom SVG + HTML canvas, expand/collapse controls with `+N` descendant badges, live search, relation-kind legend & toggles, and connected-subgraph hover/selection highlighting |
| Detail inspector | [`web/src/components/features/FeatureInspector.tsx`](../web/src/components/features/FeatureInspector.tsx) | Displays all 4 independent state booleans, hierarchy links, outgoing & incoming typed feature links (click to jump & focus), linked & shared multi-feature files (click any co-owner to jump), owner session timeline link, evidence, sources, gaps, and blockers |
| View shell | [`web/src/components/features/FeatureMapView.tsx`](../web/src/components/features/FeatureMapView.tsx) | Coordinates Mind Map (default) and State Columns views, mission switcher, and MissionBlocker / BacklogError / MappingError banners |

## 2. Backward-Compatible Schema Extensions

Existing mappings (`id`, `title`, optional `summary`, optional `sources`) continue to work unchanged. Each feature entry in a mapping file now accepts five optional fields:

```json
{
  "features": [
    {
      "id": "feature-mindmap",
      "title": "Feature Mind Map",
      "summary": "Interactive navigable mind map over capabilities and links",
      "parent": "dashboard-ui",
      "kind": "initiative",
      "tags": ["web", "graph"],
      "files": [
        "web/src/features.ts",
        "web/src/featuresGraph.ts",
        "web/src/components/features/FeatureMindMap.tsx"
      ],
      "relations": [
        {
          "kind": "depends_on",
          "target": "feature-map-api",
          "label": "reads /api/features"
        },
        {
          "kind": "documented_by",
          "target": "feature-mindmap-docs"
        }
      ],
      "sources": [
        { "kind": "objective", "objective": 488 }
      ]
    }
  ]
}
```

### Field Reference

- **`parent`** (`string | null`, optional): Feature ID of this node's parent in the mission hierarchy.
- **`kind`** (`string`, optional, default `"capability"`): One of `domain`, `capability`, `initiative`, `slice`, `research-initiative`, `subsystem`.
- **`tags`** (`string[]`, optional): Lowercase ASCII tags (`[a-z0-9-_]`, max 40 bytes each, max 50 tags, deduplicated).
- **`files`** (`string[]`, optional): Repo-relative normalized POSIX paths (max 500 per feature, deduplicated within a feature). Multiple features may reference the same file (**many-to-many**).
- **`relations`** (`Relation[]`, optional): Typed directed links `{ kind, target, label? }` to another feature ID in the same mission. Duplicate `(kind, target)` pairs within a feature are merged (preserving the first non-empty `label`).

### Supported Relation Kinds

| Relation `kind` | Semantics | Acyclic Enforced? |
| --- | --- | --- |
| `depends_on` | Source requires target | Yes |
| `blocks` | Source prevents target from progressing | No |
| `implements` | Source implements specification/domain target | No |
| `extends` | Source builds on target | No |
| `relates_to` | Associative cross-reference | No |
| `supersedes` | Source replaces target | No |
| `verifies` | Source validates target | No |
| `documented_by` | Source is documented by target | No |
| `contains` | Source structurally contains target | Yes |

### Strict Validation Rules

`parse_mapping` (and client-side `validateFeatureGraph` as a defensive layer) refuses:
- Unknown fields on the root object, feature objects, sources, or relation objects.
- Self-links (`parent == feature.id` or `relation.target == feature.id`).
- Dangling `parent` or `relation.target` IDs not present in the mapping.
- Cycles in `parent` (must form a valid forest), `depends_on`, or `contains`.
- Unsafe or non-POSIX file paths: `..` traversal segments, leading `/`, backslashes `\`, empty paths, or control/NUL characters.

### Mission-Level `graph` Summary

Each mission in `GET /api/features` includes a computed `graph` object:

```json
"graph": {
  "relations_total": 4,
  "files_indexed": 5,
  "shared_files": {
    "web/src/features.ts": ["feature-api", "feature-mindmap"]
  },
  "hierarchy_roots": ["dashboard-ui", "feature-api"]
}
```

## 3. Dense-Graph Layout Strategy

Live Spawn missions frequently have no mapping file yet and contain >150 flat backlog objectives (for example, Mission #4 with 189 objectives). Rendering 189 nodes directly off one root creates an unreadable vertical wall.

`buildFeatureMindMapTree` handles both regimes automatically:

1. **Explicit hierarchy**: When any feature specifies a valid `parent`, the tree preserves the exact declared parent–child structure under the Mission root.
2. **Dense flat missions (`> 12` flat roots)**:
   - In **`auto`** mode, extracts deterministic domain/topic clusters using (in priority order) explicit `tags[0]`, title prefixes before `:` / `—` / ` - `, top-level `files[0]` directories, or domain keyword buckets. If coherent clusters cover at least 40% of features across ≥2 multi-item groups, features are organized under those domain branches; otherwise they group cleanly by the 4 server states (`Planned`, `Implemented`, `Verified`, `Live`).
   - Users can also switch between **Auto branches** and **By state** directly in the toolbar.
   - Whenever any flat cluster holds more than **12** direct feature leaves, it is deterministically folded into objective-range sub-branches (e.g. `Objectives 4/001–4/012`) so every branch stays calm and scannable.
   - Dense missions initialize with only the Mission root expanded (`defaultExpandedIds`), showing top-level branches with state mini-counters and `+N` collapsed descendant badges.
3. **Deterministic Left-to-Right Geometry**:
   - Post-order subtree height measurement + pre-order vertical centering guarantees zero node overlap.
   - Hierarchy links render as smooth horizontal cubic Bezier curves.
   - Typed cross-cutting relations render as distinct curved directional SVG arcs with arrowheads (routing right-bowed arcs for same-column siblings and offset arcs for parallel edges) so they never collide with the hierarchy trunk.
   - Searching auto-expands every ancestor of every matching node while dimming non-matches, and clicking any linked feature or shared-file co-owner in the Inspector auto-expands its ancestors and smoothly pans the canvas to center it.

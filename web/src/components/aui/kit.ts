// Spawn's door to affectUI (objective 120): the kit's files in
// components/ui/ are third-party and byte-identical (web/THIRD_PARTY_NOTICES.md),
// and Spawn's pages import them from here. Each by its full path: "../ui"
// alone is Spawn's own components/ui.tsx. They wear Spawn's theme through
// styles/aui.css (objective 119).

export { Badge } from "../ui/badge";
export type { BadgeTone } from "../ui/badge";
export { Button } from "../ui/button";
export { Card, CardContent, CardHeader } from "../ui/card";
export { EmptyState } from "../ui/empty-state";
export { KpiChart } from "../ui/kpi-chart";
export { Loading, Skeleton } from "../ui/skeleton";
export { Tooltip } from "../ui/tooltip";
export { SegmentedControl } from "../ui/segmented-control";

import type { CoreFigure, CoreMark } from "../../coreLines.ts";
import { unitWords } from "../../coreLines.ts";
import { KpiChart } from "./kit";

/**
 * A core line's north star on the kit's KPI chart (objective 120): its name
 * as the title, coreLinesView's value text exactly as the value, its 7-day
 * trend (the points `/api/core-lines` gives) as the series, and its range
 * as the footer, its target after it (objective 181). No delta: the kit colours a rise green, which is wrong
 * where lower is better. Each number on the axis and in the tooltip is
 * worded in the figure's unit, as the value is (rev-g's N1).
 */
export function NorthStarChart({ mark, figure }: { mark: CoreMark; figure: CoreFigure | undefined }) {
  const trend = figure?.trend ?? [];
  const unit = figure?.unit ?? "count";
  return (
    <KpiChart
      title={mark.name}
      value={mark.value}
      labels={trend.map((point) => point.day.slice(5))}
      tooltipLabels={trend.map((point) => point.day)}
      series={[{ label: mark.name, color: "var(--aui-primary)", values: trend.map((point) => point.value) }]}
      format={(value) => unitWords(unit, value)}
      footer={mark.target === undefined ? mark.range : `${mark.range} · ${mark.target}`}
      emptyText="no trend yet: fewer than two days sampled"
    />
  );
}

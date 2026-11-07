import { useCallback, useState } from "react";

import { fetchWholePrompt, promptTitle } from "../../rsi";
import type { RsiObjective } from "../../rsi";
import { isCut, OnDemand } from "../../validated";

/** Whole prompts read so far, by mission, objective and length: shared by
 * every place that names an objective, and read again when the prompt
 * changes length. */
const wholePrompts = new OnDemand<string>();

/** An objective's title, its prompt as the pointer's title. `/api/rsi`
 * sends the prompt's first lines; pointed at, a cut prompt is read whole
 * (one GET, once per prompt), in time for the browser's tooltip. */
export function ObjectiveName({
  mission,
  objective,
  heading = false,
  className,
}: {
  mission: number;
  objective: RsiObjective;
  heading?: boolean;
  className?: string;
}) {
  const cut = isCut(objective.prompt, objective.prompt_bytes ?? null);
  const key = `${mission}/${objective.id}/${objective.prompt_bytes ?? ""}`;
  const [whole, setWhole] = useState<{ key: string; prompt: string } | null>(null);
  const want = useCallback(() => {
    if (!cut) return;
    wholePrompts
      .get(key, () =>
        fetchWholePrompt(mission, objective.id, new AbortController().signal).then(
          (answer) => answer.prompt,
        ),
      )
      .then((prompt) => setWhole({ key, prompt }))
      // Unread, the first lines stay, marked as cut; the next pointing tries
      // again.
      .catch(() => undefined);
  }, [cut, key, mission, objective.id]);
  const title = whole?.key === key ? whole.prompt : promptTitle(objective);
  const Tag = heading ? "h2" : "span";
  return (
    <Tag className={className} title={title} onMouseEnter={want}>
      {objective.title}
    </Tag>
  );
}

import type { ReactNode } from "react";

import { Card, CardHeader, Loading, Skeleton } from "./kit";

/** A Home block on the kit's card: its icon, title and what sits at the end
 * of the title line (a count, a toggle), then its content. */
export function HomeCard({
  icon,
  title,
  meta,
  children,
}: {
  icon?: ReactNode;
  title: ReactNode;
  meta?: ReactNode;
  children: ReactNode;
}) {
  return (
    <Card as="section">
      <CardHeader titleAs="h2" icon={icon} title={title} action={meta || undefined} />
      {children}
    </Card>
  );
}

/** A block still loading: the kit's skeletons, in the shape of a few lines. */
export function LoadingLines({ label, lines = 3 }: { label: string; lines?: number }) {
  return (
    <Loading label={label} className="space-y-1.5">
      {Array.from({ length: lines }, (_, index) => (
        <Skeleton key={index} line width={index === 0 ? "40%" : index % 2 ? "85%" : "65%"} />
      ))}
    </Loading>
  );
}

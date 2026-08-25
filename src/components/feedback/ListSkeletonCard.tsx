import { Card } from "../controls/Card"
import { Skeleton } from "../ui/skeleton"

/** Loading placeholder for list surfaces — a card-shaped block with a
 * title-line skeleton and two row skeletons. Standardizes the identical
 * inline blocks the Skills page, the MCP page, and the MCP import section
 * each hand-rolled. */
export function ListSkeletonCard() {
  return (
    <Card padding="md">
      <div className="space-y-3">
        <Skeleton className="h-4 w-1/4" />
        <Skeleton className="h-8 w-full" />
        <Skeleton className="h-8 w-full" />
      </div>
    </Card>
  )
}

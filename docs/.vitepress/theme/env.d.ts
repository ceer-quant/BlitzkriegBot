declare const __BB_RELEASE__: {
  version: string
  tag: string
  url: string
  /** Commits since the previous stable release. Shared with the home counters. */
  changes: number | null
  /** Merged PRs in that same release window. */
  pullRequests: number | null
} | null

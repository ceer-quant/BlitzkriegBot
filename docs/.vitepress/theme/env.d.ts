declare const __BB_RELEASE__: {
  version: string
  tag: string
  url: string
  /** Commits since the previous stable release. Banner only. */
  changes: number | null
} | null

declare const __BB_REPO_COUNTS__: {
  commits: number | null
  pullRequests: number | null
}

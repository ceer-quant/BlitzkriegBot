/**
 * Build-time facts for the docs banner and the home-page counters.
 * Source: the public GitHub API for this repo. A failed or empty response
 * is null — callers hide the widget. Nothing here is a checked-in version.
 */
const REPO = 'ceer-quant/BlitzkriegBot'
const API = 'https://api.github.com'

export type ReleaseBanner = {
  /** Tag without a single leading "v", for display. */
  version: string
  tag: string
  url: string
  /** Commits on the latest release since the previous stable release. */
  changes: number | null
}

export type RepoCounts = {
  /** Commits reachable from the default branch (main). */
  commits: number | null
  /** Merged pull requests in the repository. */
  pullRequests: number | null
}

type GhRelease = {
  tag_name?: string
  html_url?: string
  published_at?: string
  draft?: boolean
  prerelease?: boolean
}

async function getJson(path: string): Promise<{ ok: boolean; status: number; json: unknown; headers: Headers }> {
  const res = await fetch(`${API}${path}`, {
    headers: {
      Accept: 'application/vnd.github+json',
      'User-Agent': 'BlitzkriegBot-docs',
      'X-GitHub-Api-Version': '2022-11-28',
    },
    signal: AbortSignal.timeout(8000),
  })
  const json = res.ok ? await res.json() : null
  return { ok: res.ok, status: res.status, json, headers: res.headers }
}

function displayVersion(tag: string): string {
  return tag.startsWith('v') ? tag.slice(1) : tag
}

async function commitCount(): Promise<number | null> {
  const res = await fetch(`${API}/repos/${REPO}/commits?sha=main&per_page=1`, {
    headers: {
      Accept: 'application/vnd.github+json',
      'User-Agent': 'BlitzkriegBot-docs',
      'X-GitHub-Api-Version': '2022-11-28',
    },
    signal: AbortSignal.timeout(8000),
  })
  if (!res.ok) return null
  const link = res.headers.get('link') ?? ''
  const last = link.match(/[?&]page=(\d+)>;\s*rel="last"/)
  if (!last) return null
  const n = Number(last[1])
  return Number.isFinite(n) && n > 0 ? n : null
}

async function mergedPullRequests(): Promise<number | null> {
  const q = encodeURIComponent(`repo:${REPO} is:pr is:merged`)
  const { ok, json } = await getJson(`/search/issues?q=${q}&per_page=1`)
  if (!ok || !json || typeof json !== 'object') return null
  const total = (json as { total_count?: unknown }).total_count
  return typeof total === 'number' && total >= 0 ? total : null
}

export async function loadRepoCounts(): Promise<RepoCounts> {
  try {
    const [commits, pullRequests] = await Promise.all([commitCount(), mergedPullRequests()])
    return { commits, pullRequests }
  } catch {
    return { commits: null, pullRequests: null }
  }
}

export async function loadReleaseBanner(): Promise<ReleaseBanner | null> {
  try {
    const { ok, json } = await getJson(`/repos/${REPO}/releases?per_page=10`)
    if (!ok || !Array.isArray(json)) return null
    const stable = (json as GhRelease[]).filter(
      (r) => r && !r.draft && !r.prerelease && r.tag_name && r.html_url,
    )
    const latest = stable[0]
    if (!latest?.tag_name || !latest.html_url) return null
    const previous = stable[1]
    let changes: number | null = null
    if (previous?.tag_name) {
      const cmp = await getJson(
        `/repos/${REPO}/compare/${encodeURIComponent(previous.tag_name)}...${encodeURIComponent(latest.tag_name)}`,
      )
      const ahead = cmp.ok && cmp.json && typeof cmp.json === 'object'
        ? (cmp.json as { ahead_by?: unknown }).ahead_by
        : null
      if (typeof ahead === 'number' && ahead >= 0) changes = ahead
    }
    return {
      version: displayVersion(latest.tag_name),
      tag: latest.tag_name,
      url: latest.html_url,
      changes,
    }
  } catch {
    return null
  }
}


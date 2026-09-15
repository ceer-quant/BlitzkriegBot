/**
 * Session share URIs.
 *
 * `blitzkrieg://session/<id>?key=<hash>` is the only scheme.
 */

export const SESSION_URI_SCHEME = 'blitzkrieg://';

export interface SessionUri {
  sessionId: string;
  key?: string;
}

export function buildSessionUri(sessionId: string, key: string): string {
  return `${SESSION_URI_SCHEME}session/${sessionId}?key=${key}`;
}

/** Resolve a session URI; returns null unless it carries the canonical scheme. */
export function parseSessionUri(uri: string): SessionUri | null {
  if (!uri.startsWith(SESSION_URI_SCHEME)) return null;

  const rest = uri.slice(SESSION_URI_SCHEME.length);
  const match = /^session\/([^?]+)(?:\?key=([\w-]+))?$/.exec(rest);
  if (!match) return null;
  return { sessionId: match[1], key: match[2] };
}

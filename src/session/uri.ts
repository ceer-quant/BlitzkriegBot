/**
 * Session share URIs (E1-d).
 *
 * `blitzkrieg://session/<id>?key=<hash>` is canonical. The legacy
 * `clodds://` scheme is accepted when resolving an incoming link so shared
 * links issued before the rename keep working for one release.
 */

export const SESSION_URI_SCHEME = 'blitzkrieg://';
export const LEGACY_SESSION_URI_SCHEME = 'clodds://';

export interface SessionUri {
  sessionId: string;
  key?: string;
  legacy: boolean;
}

export function buildSessionUri(sessionId: string, key: string): string {
  return `${SESSION_URI_SCHEME}session/${sessionId}?key=${key}`;
}

/** Resolve a canonical or legacy session URI; returns null if it is neither. */
export function parseSessionUri(uri: string): SessionUri | null {
  let rest: string;
  let legacy: boolean;
  if (uri.startsWith(SESSION_URI_SCHEME)) {
    rest = uri.slice(SESSION_URI_SCHEME.length);
    legacy = false;
  } else if (uri.startsWith(LEGACY_SESSION_URI_SCHEME)) {
    rest = uri.slice(LEGACY_SESSION_URI_SCHEME.length);
    legacy = true;
  } else {
    return null;
  }

  const match = /^session\/([^?]+)(?:\?key=([\w-]+))?$/.exec(rest);
  if (!match) return null;
  return { sessionId: match[1], key: match[2], legacy };
}

/**
 * Outbound product identity (E1-d).
 *
 * Single source for the name we present on the wire: HTTP User-Agent headers,
 * Copilot editor headers, MCP client/server info, health info. Keeping these
 * together prevents the old name coming back one file at a time.
 */

import { readBrandEnv } from './env';

export const PRODUCT_NAME = 'Blitzkrieg';
export const PRODUCT_VERSION = '1.0';
export const PRODUCT_CONTACT_URL = 'https://github.com/ceer-quant/BlitzkriegBot';

/** `Blitzkrieg/1.0`, or `Blitzkrieg/1.0 (<detail>)` when a component is named. */
export function userAgent(detail?: string): string {
  const base = `${PRODUCT_NAME}/${PRODUCT_VERSION}`;
  return detail ? `${base} (${detail})` : base;
}

// GitHub Copilot token endpoints expect editor-style version headers.
export const COPILOT_EDITOR_VERSION = `${PRODUCT_NAME}/1.0.0`;
export const COPILOT_PLUGIN_VERSION = 'blitzkrieg/1.0.0';
export const COPILOT_INTEGRATION_ID = 'blitzkrieg';

/**
 * Base URL of the hosted identity/profile service (handle pages, referral
 * links). The service is not deployed yet; operators override it via
 * BLITZKRIEG_IDENTITY_BASE_URL. `.example` is the IANA-reserved placeholder.
 */
export const IDENTITY_PUBLIC_BASE_URL =
  readBrandEnv('IDENTITY_BASE_URL')?.replace(/\/+$/, '') ||
  'https://blitzkrieg.example';

export function identityUrl(path: string): string {
  return `${IDENTITY_PUBLIC_BASE_URL}${path.startsWith('/') ? '' : '/'}${path}`;
}

const DEFAULT_ANTHROPIC_BASE_URL = 'https://api.anthropic.com';

export function getAnthropicBaseUrl(): string {
  return (process.env.ANTHROPIC_BASE_URL || DEFAULT_ANTHROPIC_BASE_URL).replace(/\/+$/, '');
}

export function getAnthropicMessagesUrl(baseUrl?: string): string {
  return `${(baseUrl || getAnthropicBaseUrl()).replace(/\/+$/, '')}/v1/messages`;
}

export function getAnthropicHeaders(apiKey: string): HeadersInit {
  return {
    'Content-Type': 'application/json',
    'x-api-key': apiKey,
    'anthropic-version': '2023-06-01',
  };
}

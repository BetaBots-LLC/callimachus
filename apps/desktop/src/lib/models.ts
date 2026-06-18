// Persistent per-provider model-list cache. The dropdown shows cached models
// instantly, and TanStack Query refetches in the background when the cache is
// older than the TTL (routine refresh) or when CACHE_VERSION changes (on release),
// so banned/retired models drop off and new ones appear without user action.

const TTL = 24 * 60 * 60 * 1000; // 24h
const CACHE_VERSION = 1; // bump on release to force a refresh for all users

type Cached = { models: string[]; at: number; ver: number };

const key = (provider: string) => `cm:models:${provider}`;

export function loadModelCache(provider: string): Cached | undefined {
  try {
    const raw = localStorage.getItem(key(provider));
    if (!raw) return undefined;
    const o = JSON.parse(raw) as Cached;
    if (o.ver !== CACHE_VERSION || !Array.isArray(o.models)) return undefined;
    return o;
  } catch {
    return undefined;
  }
}

export function saveModelCache(provider: string, models: string[]): void {
  try {
    localStorage.setItem(
      key(provider),
      JSON.stringify({ models, at: Date.now(), ver: CACHE_VERSION }),
    );
  } catch {
    /* ignore quota / unavailable storage */
  }
}

export const MODELS_TTL = TTL;

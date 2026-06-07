/**
 * Pure secret-redaction for terminal output.
 *
 * Scrubs likely secrets out of a string before it's handed to a local LLM or
 * persisted in a summary, replacing each with the literal token `[REDACTED]`
 * and leaving everything else intact.
 *
 * Invariants:
 *  - pure (no I/O, no globals) and total — never throws, any string is valid.
 *  - idempotent — `redact(redact(x)) === redact(x)`; redacting `[REDACTED]` is a no-op.
 *  - conservative — ordinary prose, paths, code, and non-secret `KEY=VALUE`
 *    (e.g. `NODE_ENV=production`, `PORT=3000`) pass through unchanged.
 *  - line-count preserving for in-place redactions (only the multiline PEM
 *    block, by spec, collapses to a single token).
 */

const TOKEN = '[REDACTED]';

/** Key names (case-insensitive) that mark a `KEY=VALUE` value as a secret. */
const SECRET_KEY =
  '(?:SECRET|TOKEN|PASSWORD|PASSWD|API[_-]?KEY|PRIVATE[_-]?KEY|ACCESS[_-]?KEY|CREDENTIAL)';

// Multiline PEM private-key block → one token. Run first so its inner base64
// can't be mistaken for other patterns.
const PEM_RE =
  /-----BEGIN (?:[A-Z0-9 ]+ )?PRIVATE KEY-----[\s\S]*?-----END (?:[A-Z0-9 ]+ )?PRIVATE KEY-----/g;

// JSON Web Token: three base64url segments separated by dots.
const JWT_RE = /eyJ[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+/g;

// AWS access key id.
const AWS_KEY_RE = /AKIA[0-9A-Z]{16}/g;

// Authorization header — keep the scheme, redact only the credential.
const AUTH_RE = /(Authorization:\s*(?:Bearer|Basic)\s+)(\S+)/gi;

// Secret-ish `KEY=VALUE`, honouring optional spaces and single/double quotes.
const KV_RE = new RegExp(
  `([\\w.-]*${SECRET_KEY}[\\w.-]*)(\\s*=\\s*)(?:"([^"\\r\\n]*)"|'([^'\\r\\n]*)'|([^\\s\\r\\n]*))`,
  'gi',
);

export function redact(text: string): string {
  if (typeof text !== 'string' || text.length === 0) return text;

  return text
    .replace(PEM_RE, TOKEN)
    .replace(JWT_RE, TOKEN)
    .replace(AWS_KEY_RE, TOKEN)
    .replace(AUTH_RE, (_m, prefix) => `${prefix}${TOKEN}`)
    .replace(KV_RE, (_m, key, eq, dq, sq) => {
      if (dq !== undefined) return `${key}${eq}"${TOKEN}"`;
      if (sq !== undefined) return `${key}${eq}'${TOKEN}'`;
      return `${key}${eq}${TOKEN}`;
    });
}

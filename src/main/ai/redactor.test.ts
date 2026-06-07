import { describe, expect, it } from 'vitest';
import { redact } from './redactor';

describe('redact', () => {
  it('redacts an AWS access key id', () => {
    expect(redact('key AKIAIOSFODNN7EXAMPLE here')).toBe('key [REDACTED] here');
  });

  it('redacts a JWT', () => {
    const jwt =
      'eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U';
    expect(redact(`token: ${jwt}`)).toBe('token: [REDACTED]');
  });

  it('redacts only the value of an Authorization header, keeping the scheme', () => {
    expect(redact('Authorization: Bearer abc123def456')).toBe('Authorization: Bearer [REDACTED]');
    expect(redact('Authorization: Basic dXNlcjpwYXNz')).toBe('Authorization: Basic [REDACTED]');
  });

  it('redacts a multiline PEM private key block as a single token', () => {
    const pem = [
      '-----BEGIN RSA PRIVATE KEY-----',
      'MIIEowIBAAKCAQEA0Z3VS5JJcds3xfn/ygWyF3SXnUgtMcKfDM3oqXBwM3uJ4uJ',
      'aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789+/abcdefghijklmnopqrstuvwxyz',
      '-----END RSA PRIVATE KEY-----',
    ].join('\n');
    expect(redact(`my key:\n${pem}\ndone`)).toBe('my key:\n[REDACTED]\ndone');
  });

  it('redacts secret-ish KEY=VALUE while keeping the key name', () => {
    expect(redact('SECRET=supersecret')).toBe('SECRET=[REDACTED]');
    expect(redact('API_KEY = "abc123"')).toBe('API_KEY = "[REDACTED]"');
    expect(redact("DB_PASSWORD='hunter2'")).toBe("DB_PASSWORD='[REDACTED]'");
    expect(redact('AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE')).toBe('AWS_ACCESS_KEY_ID=[REDACTED]');
  });

  it('leaves ordinary text and non-secret KEY=VALUE untouched', () => {
    const clean = 'NODE_ENV=production\nPORT=3000\njust some prose, paths/like/this, and code.';
    expect(redact(clean)).toBe(clean);
  });

  it('is idempotent', () => {
    const blob = [
      'AKIAIOSFODNN7EXAMPLE',
      'Authorization: Bearer abc.def.ghi',
      'SECRET="topsecret"',
      'PORT=3000',
    ].join('\n');
    const once = redact(blob);
    expect(redact(once)).toBe(once);
  });

  it('preserves line count for in-place redactions', () => {
    const input = 'SECRET=a\nPORT=3000\nTOKEN=b';
    expect(redact(input).split('\n')).toHaveLength(3);
  });

  it('scrubs a mixed multi-secret blob, leaving non-secrets intact', () => {
    const jwt =
      'eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ4In0.Dk5f2nL9q-7gWmVw3Yx1pQrStUvWxYz0123456789AB';
    const input = [
      'starting up on PORT=3000',
      'aws id AKIAIOSFODNN7EXAMPLE',
      `jwt ${jwt}`,
      'Authorization: Bearer s3cr3t-token',
      'DATABASE_PASSWORD=hunter2',
      'NODE_ENV=production',
    ].join('\n');
    const out = redact(input);
    expect(out).toBe(
      [
        'starting up on PORT=3000',
        'aws id [REDACTED]',
        'jwt [REDACTED]',
        'Authorization: Bearer [REDACTED]',
        'DATABASE_PASSWORD=[REDACTED]',
        'NODE_ENV=production',
      ].join('\n'),
    );
    expect(redact(out)).toBe(out);
  });

  it('never throws on odd input and returns a string', () => {
    expect(redact('')).toBe('');
    expect(typeof redact('-----BEGIN PRIVATE KEY-----')).toBe('string');
  });
});

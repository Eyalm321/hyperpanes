// Normalize control-API `send_input` line endings to what the local pty actually
// submits a line on. Windows conpty runs a line on CR (\r), not LF (\n): an agent
// that ends send_input with a bare "\n" types the line but never executes it
// (live finding, 2026-06-05). On Windows, collapse every newline — CRLF or a lone
// LF — to a single CR so "\n" submits exactly as it does on a POSIX pty, where LF
// is itself a canonical line delimiter. No-op off Windows. The platform is a
// parameter (defaulting to the real one) so this stays pure and unit-testable.
export function submitNewlines(data: string, platform: NodeJS.Platform = process.platform): string {
  if (platform !== 'win32') return data;
  return data.replace(/\r\n/g, '\r').replace(/\n/g, '\r');
}

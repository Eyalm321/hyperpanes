# Security policy

## Reporting a vulnerability

**Please do not open a public issue, discussion or pull request for a security problem.**

Report it privately through GitHub instead:

1. Go to the repository's **Security** tab → **Report a vulnerability**
   ([direct link](https://github.com/Eyalm321/hyperpanes/security/advisories/new)).
2. Describe the problem, the affected version (`hyperpanes --version`) and OS,
   and how to reproduce it. A proof of concept helps; please keep it minimal.

Only the maintainers can see the report. We will work with you on a fix in a private advisory,
credit you there (unless you prefer otherwise), and publish the advisory once a fixed release
is out.

What to expect:

- an acknowledgement within **7 days**;
- an initial assessment (confirmed or not, and how severe) within **14 days**;
- a fix or a mitigation plan for confirmed issues, with a release as soon as one is ready.

This is a small, volunteer-run project, so these are goals rather than guarantees.

## Supported versions

Only the **latest release** on the [Releases page](https://github.com/Eyalm321/hyperpanes/releases)
gets security fixes. Please check that a problem still reproduces there before reporting.

## Scope

Areas where a vulnerability matters most:

- the **Control API** (off by default): token checks, token scoping and minting, the loopback
  bind, and the input gates that let an agent type into a shell;
- **mobile pairing**: per-device tokens, the pairing URL / QR code, and the non-loopback bind
  it enables;
- **session spawning**: anything that runs a process with the user's privileges in a way the
  user did not ask for;
- **secret redaction** on the optional AI subtitle path;
- the **self-updater** (release download and install);
- the companion MCP server ([hyperpanes-mcp](https://github.com/Eyalm321/hyperpanes-mcp)),
  whose input gates sit in front of the same API.

Not in scope:

- anything that needs an attacker to already run code as your user, or to already hold the
  control token from your `control.json`;
- commands you, or an agent you authorised with input access, deliberately type into a pane;
- missing code signing on the release binaries (a known limitation; see the README).

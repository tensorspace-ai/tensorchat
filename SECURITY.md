# Security Policy

## Supported versions

TensorChat is pre-1.0. Security fixes land on the latest release only; there are
no backported patch branches yet. No version carries a support commitment, and
the software is provided as is, without warranty, as the [LICENSE](LICENSE) sets
out — nothing on this page changes that.

| Version | Supported |
| --- | --- |
| 0.1.x | ✅ |
| < 0.1 | ❌ |

## Reporting a vulnerability

**Do not open a public issue for a security vulnerability.**

Report it privately through
[GitHub Security Advisories](https://github.com/tensorspace-ai/tensorchat/security/advisories/new),
or by email to <support@tensorspace.ai>.

Please include:

- What the issue is and what an attacker gains from it.
- Steps to reproduce, ideally against a fresh `cargo run -p tensorchat-server`.
- The version or commit you tested.
- Any proof-of-concept you have.

This is a small project with no staffed security team, so no response time is
promised. Reports are read and triaged as maintainers are available. Where one
is agreed to be a vulnerability, the aim is to fix it before it is described
publicly and to coordinate timing with the reporter, who is credited in the
advisory and changelog unless they ask not to be. None of this is a commitment
or a term of service; it describes intent.

## What is in scope

The server, the store, the wire protocol, and the web client — anything in this
repository. Particularly:

- Authentication and session handling.
- Authorization: any path where a user reaches a channel, message, or file they
  are not a member of or entitled to.
- Cross-site scripting, cross-site request forgery, and Content-Security-Policy
  bypasses.
- Anything reachable through the upload path.
- SQL injection, and denial of service that a single unprivileged account can
  cause.

## What is out of scope

- **Deployment misconfiguration.** Binding `TC_BIND=0.0.0.0:8080` directly to
  the internet without TLS is documented as the operator's responsibility.
- **Missing rate limits outside authentication.** Only `/api/login` and
  `/api/register` are rate limited today; this is known, and general request
  throttling belongs at the reverse proxy.
- **The known limitations** listed in the [README](README.md#limitations) —
  single-node operation, the user directory shipping whole at connect, and so
  on. These are design boundaries, not vulnerabilities.
- Reports generated solely by an automated scanner with no demonstrated impact.
- Social engineering, physical attacks, and anything requiring a compromised
  host.

## Deployment notes

Two things matter most when running TensorChat exposed:

**Terminate TLS in front of it.** The server speaks plain HTTP. Session tokens
are bearer credentials; over an unencrypted connection they are readable in
transit.

**Raise the auth rate limits behind a proxy.** The limiter keys on the client
peer address. A reverse proxy that presents every client as the same address
will make one user's failed logins throttle everyone — set `TC_AUTH_BURST` and
`TC_AUTH_PER_SECOND` accordingly, and rate limit at the proxy instead.

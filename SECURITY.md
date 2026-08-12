# Security Policy

## Supported Versions

Pylon is currently pre-1.0. Until a 1.0 release, security fixes are made only
against the latest published release / `canary` branch.

| Version | Supported          |
| ------- | ------------------ |
| latest  | :white_check_mark: |
| older   | :x:                |

## Reporting a Vulnerability

**Please do not open a public GitHub issue for security vulnerabilities.**

If you believe you've found a security issue in Pylon — for example, a way to
inject SQL through a PyQL query or bound parameter, read or write data a
query shouldn't reach, or crash or hang a server via a malicious query,
schema, or `NOTIFY` payload — please report it privately:

* Email: **oss@jaldis.com**
* Alternatively, if the repository is hosted on GitHub, you can use
  [GitHub's private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing/privately-reporting-a-security-vulnerability)
  feature under the Security tab.

Please include:

* A description of the issue and its potential impact
* Steps to reproduce, or a minimal example schema/query that triggers it
* The Pylon version / commit affected, and the PostgreSQL version
* Whether you believe the issue is exploitable by an untrusted client (e.g.
  through a query or parameter value reaching a Pylon-backed endpoint) or
  only by a trusted schema author

### What to expect

* We'll acknowledge your report within **3 business days**.
* We'll aim to provide an initial assessment (confirmed, not a bug, needs
  more info) within **7 business days**.
* We'll credit reporters in the release notes for confirmed issues, unless
  you'd prefer to remain anonymous.
* We ask that you give us a reasonable window to ship a fix before any public
  disclosure. For most issues this will be on the order of 30-90 days
  depending on severity and complexity.

### Scope

In scope:

* **SQL injection through any Pylon-controlled path** — a PyQL query, a bound
  `$param` value, a schema identifier, or a channel name that reaches the
  generated SQL unescaped. This is the highest-severity class for this
  project: Pylon's entire job is generating SQL, and a caller passing a
  hostile parameter value must never be able to change the statement's shape.
* `pylon-core` (PyQL parsing, IR compilation, SQL emission, migration
  diffing) and `pylon-pgcon` (the PostgreSQL driver, including wire-format
  decoding of untrusted server responses)
* The `pylon` Python package and `pylon._core` bindings
* `pylon-server`: authentication/authorization handling, and any route that
  exposes schema or data beyond what the request should reach
* Migration execution — in particular anything that could cause a migration to
  be recorded as applied when it wasn't, or to skip a step, since that
  silently diverges the database from its declared schema
* Denial-of-service vectors reachable by an untrusted client, such as
  unbounded recursion in query compilation or a malformed `NOTIFY` payload or
  wire-protocol response that hangs a listener or worker
* Secret handling: database passwords, embedding-provider API keys, and
  search-backend credentials read from `pylon.toml` or the environment
  leaking into logs, error messages, or API responses

Out of scope:

* Vulnerabilities in application code that *uses* Pylon (e.g. an endpoint that
  passes user input as a PyQL query *string* rather than as a bound parameter
  — that is the caller's own injection, and the parameter API exists to
  prevent it)
* Vulnerabilities in third-party dependencies, including PostgreSQL itself —
  please report those upstream; we'll track and update as fixes become
  available
* Issues that require an already-compromised or fully-trusted schema author.
  Pylon assumes the schema author is trusted: a schema can declare arbitrary
  PyQL expressions in computed pointers, triggers, rewrites, and constraints,
  all of which compile into SQL by design. The threat model is untrusted
  *clients* and untrusted *data*, not untrusted schema code.
* Anything requiring direct database access the attacker already has —
  Pylon can't defend a database whose credentials are already compromised

## Disclosure Policy

Once a fix is released, we'll publish a security advisory describing the
issue, its severity, affected versions, and the fixed version. Reporters who
want credit will be named unless they ask otherwise.

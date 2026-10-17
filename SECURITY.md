# Security policy

DBeast handles database credentials and, through its local MCP server, gives AI
agents controlled access to databases. Security reports are taken seriously.

## Supported versions

DBeast does not publish versioned releases yet. Security fixes land on the
latest `master` branch only; please make sure you can reproduce the issue
there before reporting it.

## Reporting a vulnerability

Please do **not** open a public issue or pull request for a security problem.
Report it privately through
[GitHub private vulnerability reporting](https://github.com/macolszewski/dbeast_manager/security/advisories/new)
(the **Report a vulnerability** button on the repository's Security tab).

Include as much of the following as you can:

- the affected area (MCP server and SQL policy, credential storage, SSH
  tunnel, TLS, file import, session restore) and the commit you tested;
- a description of the issue and its impact, for example what an agent could
  read or change beyond what was shared or approved;
- step-by-step reproduction, ideally with a minimal schema or SQL;
- the operating system and database server version.

You can expect an acknowledgement within a few days. Once a fix is ready it
is published on `master` and the advisory is disclosed with credit, unless
you prefer to stay anonymous.

## Scope

In scope:

- escaping the MCP read-only policy, executing SQL other than the approved
  statement, or reaching resources that were not shared with an agent
  connection;
- leaking passwords, private keys or agent tokens to disk, logs or other
  processes;
- bypassing SSH host key or TLS certificate verification;
- code execution through crafted files (CSV, Data Package, SQLite) or
  database responses.

Out of scope:

- anything that requires an attacker who already controls the local user
  account or the database server with full privileges;
- denial of service by a database server you connect to on purpose;
- issues in third-party crates without a demonstrated impact on DBeast
  (please report those upstream).

The MCP security model is described in the
[MCP contract](docs/design/mcp-contract.md).

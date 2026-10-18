# Design notes

Design documents, plans and verification records for selected DBeast features. They describe decisions, trust boundaries and the evidence collected when each increment was completed. For day-to-day usage, see the user guide.

| Document | What it covers |
|---|---|
| [Local MCP contract](mcp-contract.md) | The permission model, tools, approval flow, idempotency, error handling and limits of the local MCP server. |
| [MCP server in DBeast](mcp-plan.md) | The goals, trust boundary, technology choices, acceptance criteria and verification results of the MCP server increment. |
| [Skills package for DBeast MCP](mcp-skills.md) | The structure and maintenance of the agent skills in `.agents/skills`, and how they were validated with simulated scenarios. |
| [Passwords in the system keychain](keychain-plan.md) | The first increment that moved data source secrets into the macOS Keychain, with its migration and verification results. |
| [A single protected Keychain item](keychain-vault.md) | The shared `vault-v1` Keychain item that replaced per-source entries, including its write ordering, migration and the limits of macOS prompts. |
| [SSL/TLS for database connections](tls-plan.md) | TLS support for PostgreSQL and MySQL (also over SSH), certificate handling decisions and measured connection overhead. |
| [Explain Analyse](explain-analyse-plan.md) | The scope, types, transaction and cancellation rules, and verification criteria of the PostgreSQL Explain Analyse increment. |
| [Performance and memory audit of 0.3.0](performance-audit-0.3.md) | The findings of the 0.3.0 source audit, how each was verified, the fixes and the measurements before and after. |
| [Spatial Database Graph and Depth View](spatial-data-exploration.md) | The research proposal behind Depth View and Database Graph: mental model, SQL semantics, stages, acceptance criteria and open questions. |

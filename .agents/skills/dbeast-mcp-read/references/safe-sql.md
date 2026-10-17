# Supported read-only SQL

`query_sql` accepts one verified SELECT-style statement. The current server validates its entire syntax tree, referenced physical tables and types, then executes in a read-only transaction. The live schema and returned policy error take precedence over examples here; this is not unrestricted SQL.

## Constructs to use

SELECT, DISTINCT, WHERE, ordinary JOIN, GROUP BY, HAVING, ORDER BY, UNION/EXCEPT/INTERSECT, nonrecursive CTEs, subqueries, CASE, and supported simple operators are available. Fully qualify physical tables, including those inside CTEs and subqueries. Quote actual identifiers according to the driver; do not treat an alias as a physical schema name.

The function allowlist is `count`, `sum`, `avg`, `min`, `max`, `abs`, `lower`, `upper`, `length`, `char_length`, `octet_length`, `round`, `coalesce`, and `nullif`, where the engine supports their signatures. An allowed function name does not make every engine-specific form valid. Use a bounded aggregate or explicit projection; `*` is not a reason to retrieve all data.

For example, after confirming these columns on a shared PostgreSQL table:

```sql
SELECT customer_id, COUNT(*) AS order_count, SUM(amount) AS total_amount
FROM public.orders
WHERE status = 'paid'
GROUP BY customer_id
ORDER BY total_amount DESC, customer_id
LIMIT 10
```

This example is a pattern, not knowledge of the user's schema or permission to read it. A request needs the real catalog `source_id`, private current `session_token`, exact `sql`, and a fresh UUID `request_key`. No grant field is part of its arguments.

## Constructs to avoid

Writes, transaction or setting commands, SELECT INTO, row locks, hints, user-defined or unverified functions, casts, custom operators or collations, variables, window functions, recursive CTEs, and system objects are denied. Backslashes are rejected in every dialect; MySQL executable comments are also denied. Whole-row values are not supported. Use short identifiers; PostgreSQL names and aliases must fit 63 UTF-8 bytes.

The current budget includes at most 16 physical tables, 32 aliases, and 64 table/result columns, with additional syntax size and recursion limits. Reducing the projection does not bypass validation of an unsupported column elsewhere in the same table.

## Physical table restrictions

| Driver | Current strict-read boundary |
|---|---|
| PostgreSQL | Persistent ordinary heap tables on a primary, without RLS, inheritance, partitioning, generated columns, or CHECK constraints. Only verified built-in scalar column types and plain B-tree indexes with built-in operator classes; no expression or partial indexes. PostGIS and other extension types, domains, enums, arrays, and composites are rejected even if not selected. The configured account needs permission for a metadata-stabilizing `SHARE UPDATE EXCLUSIVE` lock. |
| MySQL | Nonpartitioned physical InnoDB tables in the Data Source's configured database, without generated columns. The existing account needs `LOCK TABLES` and `PROCESS` access for stable table-identity verification. |
| SQLite | Ordinary physical tables in `main`, without views, virtual/shadow tables, or generated columns. The read-only handle and authorizer enforce table and function access. |

These restrictions also matter for `describe_table` and `read_table`. `list_tables` returns bounded candidates; metadata or data retrieval may still reject a candidate. Do not claim a source is fully supported because listing worked, and do not issue GRANT, ALTER, or other changes to make a read pass.

`unsupported_safe_read` is a policy refusal, not a request to escalate. Check the static reason, correct an ordinary naming or supported-syntax mistake if that preserves intent, then stop if the required object or operation is outside this boundary. `resource_unavailable` can mean the table changed and its former grant no longer applies. Re-discovery and a fresh approval may be needed; the old grant cannot be transferred to a replacement object.

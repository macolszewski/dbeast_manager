# Execution outcomes and recovery

Do not infer execution from transport success alone. A submission returns a request record; `get_request` reports its state. MCP `isError` reports a tool-call error, while a request can return `status: "failed"` and an error code inside `result` through a successful tool call.

| Observation | What to do |
|---|---|
| `pending` | Await the user's DBeast decision and poll the existing request at the returned interval. Do not resubmit or ask the user to approve an invented chat-side grant. |
| `running` | Poll the same request. Expiration of an HTTP call is not expiration of the database operation. |
| `completed` | Inspect the bounded result and any affected count. Use an appropriately authorized read if more verification is needed. |
| `denied` | Stop this operation. Do not seek approval again through new keys, another client, or a new session. |
| `outcome_unknown` | Effects may have committed. Stop replay attempts and report the request ID and uncertainty without secrets. Reconcile through an authorized read or human inspection before considering a separate corrective task. |
| `completed_result_revoked` | Execution had completed; access to its result was revoked. This is not rollback, nor permission to perform the write again. |
| `result_expired` | The result is gone but the key remains reserved for this session. Its absence says nothing about whether effects committed. Do not create a replacement write. |
| `failed` | Inspect the returned static code/message. Report the failure; do not treat it as a general guarantee of rollback or automatically replay a write. A definitively rejected input can be corrected as a new intended operation within the user's scope. |
| `cancelled`, `expired`, or `revoked` | The request is terminal. Do not relabel it as a completed write or as proof of rollback. Respect the reason and any uncertainty in the result. |
| `session_expired`, revoked connection, server restart, or unavailable request | Prior effects cannot be recovered merely by starting another session. If a write was submitted, treat its missing outcome as uncertain and do not replay it. |

After the write's session expires, you may open a fresh session solely for still-authorized read-only reconciliation of current state: rediscover the source and follow normal native approval. This neither recovers the old result nor permits replaying the write.

When a submission reply is lost but the original session is still valid:

- If `request_id` is known, use `get_request`.
- Otherwise, the exact original `request_sql` arguments with the same UUID `request_key` safely recover the request record through deduplication. Keep every argument identical; do not make this recovery call after the server has already reported a terminal uncertain outcome.
- Reusing a key with different arguments gives `request_conflict`. Investigate the mismatch; never substitute a new key to bypass it.
- Deduplication is scoped to the original application session and does not survive a server restart. A new UUID, new session, or new client can execute the statement again and is not recovery.

`limit_exceeded` can mean the request was not admitted, or it can appear inside an already executed request's terminal result. Inspect where it occurred and the status before acting. A Full SQL result-size failure can be `outcome_unknown`; shrinking output and replaying the statement could duplicate effects. Capacity may remain occupied during native cancellation cleanup, even after the visible request is terminal. Back off rather than generating concurrent replacements.

Do not call `end_session` merely to clear a timeout while expecting to recover the previous result later. Finish observation first when possible. If the user requests cancellation or revocation, carry it out and report that this does not guarantee rollback. Never automatically repeat a write based on a timeout, lost reply, zero rows observed elsewhere, or the absence of a durable audit record.

# Trefoil/Plexus Parity Audit

Last audited by a dedicated comparison agent: current overall weighted estimate **54/100**.

| Area | Score | Status | Priority |
|---|---:|---|---:|
| Management CRUD/config/auth | 85% | Broadly implemented; differential coverage is strong | Medium |
| Inference APIs | 45% | Routes exist, but dispatch, transforms, retry/failover, and stream accounting are partial | Highest |
| Quotas/usage | 50% | Persistence and basic enforcement exist; exact summaries/checkers/streaming remain | High |
| MCP | 35% | CRUD/log/OAuth surfaces exist; proxy, protocol, and subprocess lifecycle remain | High |
| OAuth | 45% | Registration and partial PKCE exist; authorization and refresh lifecycle remain | High |
| Operations/backup/metrics | 25% | Several surfaces are partial or absent | Medium |
| Frontend | 0% | Minimal dashboard exists; parity tests and management pages are not demonstrated | Medium |
| Verification | 35% | Rust tests pass; differential cases are management-heavy | Highest |

## Verified checklist

- [x] 159 Rust tests pass.
- [x] Management/config differential behavior is covered broadly.
- [x] PostgreSQL migrations and persistence foundation are present.
- [ ] Differential coverage includes inference failure, retry, streaming, MCP, OAuth, and frontend behavior.
- [ ] Shared provider dispatch handles target iteration and alternate-provider failover.
- [ ] Responses API fully persists and reconstructs canonical response records/items/conversations.
- [ ] OAuth authorization-code, PKCE validation, refresh, revocation, and token validation are complete.
- [ ] MCP upstream proxying, JSON-RPC tools, streamable HTTP, and subprocess lifecycle are complete.
- [ ] Full backup archive/restore is complete.
- [ ] Frontend management and browser test parity is complete.

## Next gate

Stabilize the differential runner and expand it with deterministic mock-provider tests. Then implement the shared provider dispatch service so retries, failover, cooldowns, attempt history, usage accounting, and streaming behavior are consistent across inference routes.

Scores are estimates until each checklist item has an executable parity test.

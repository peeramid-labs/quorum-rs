# Plans

Types shared by the noolog node, which records plans, and the services that
read them (`quorum_rs::plans`).

| Type | Holds |
|---|---|
| `OperatorPlan` | `free` (default), `basic`, `pro`; ordered by what each grants |
| `PlanLimits` | `agents`, `storage_bytes`, `listed`, `lists` (default `0`) |
| `PlanConfig` | `free`, `basic`, `pro` limits; each defaults when absent |
| `PlanRecord` | `{ plan, limits }`, what the node answers for an operator |

| Plan | agents | storage | listed | lists |
|---|---|---|---|---|
| free | 1 | 100 MiB | no | 0 |
| basic | 1 | 100 MiB | yes | 1 |
| pro | 10 | 20 GiB | yes | 5 |

Defaults; a deployment overrides them in its `PlanConfig`.

Reading rules:

- `OperatorPlan::from_str_or_free` reads a stored value. Unknown is `free`:
  failing to the smallest plan is the recoverable direction.
- `OperatorPlan::named` reads a requested value. Unknown is `None`, which the
  caller refuses.

# Read a deliberation's consensus

Per round in `GET /deliberation/{job}/details` (`rounds[]`) and its events:

| To show | Use |
|---|---|
| How far the room agrees | `convergence_score` ∈ [-1, +1], shown as `(score + 1) / 2` % |
| What is disputed | `claim_assessments`: each claim, verdict and reason per evaluator |
| Whether it is over | the job's status (`/result`); the engine decides, never compute it |
| How one agent stands | its proposal `score` in the round, scaled to 0–1 |

Not agreement: `claim_convergence` and `leader_claim_convergence` are
diagnostics; deprecated fields are for old displays only.

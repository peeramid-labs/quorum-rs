use crate::agents::config::AgentConfig;
use crate::llms::{AiModel, ChatCompletionResult, RequestConfig, TimingMetadata};
use crate::telemetry::LlmError;
use anyhow::Result;
use async_openai::types::{
    ChatChoice, ChatCompletionMessageToolCall, ChatCompletionResponseMessage,
    ChatCompletionToolType, CompletionUsage, CreateChatCompletionResponse, FunctionCall, Role,
};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::time::sleep;
use tracing::debug;

/// Shared state across all `SimulatedModel` instances within a pool so
/// that response times are staggered naturally.
#[derive(Debug)]
pub struct SimulatedShared {
    /// Monotonic counter — each `chat_completion` call increments it;
    /// the resulting index is multiplied by `stagger_ms` and added to the
    /// base latency so that responses trickle in instead of arriving in
    /// one batch.
    pub call_counter: AtomicUsize,
    /// Exactly-once guard for the dm_user tool call per deliberation.
    /// The first agent to `compare_exchange(false → true)` wins and fires
    /// the DM; all others skip.
    pub dm_fired: AtomicBool,
}

impl Default for SimulatedShared {
    fn default() -> Self {
        Self::new()
    }
}

impl SimulatedShared {
    pub fn new() -> Self {
        Self {
            call_counter: AtomicUsize::new(0),
            dm_fired: AtomicBool::new(false),
        }
    }
}

/// Per-call stagger added on top of the base latency.
/// Each concurrent call gets `index * STAGGER_MS` extra delay so that
/// agents respond a few hundred ms apart — looks natural in the UI.
const STAGGER_MS: u64 = 350;
/// Number of calls before the stagger wraps back to 0.
/// Set to ~agents×2 so each round's propose + evaluate phase gets a clean stagger.
/// With 5 agents per ensemble (max): 5 propose + 5 evaluate = 10 calls per round.
const STAGGER_WINDOW: u64 = 10;

#[derive(Clone, Debug)]
pub struct SimulatedModel {
    model_name: String,
    latency: Duration,
    /// Shared mutable state across all simulated models in the same pool.
    shared: Arc<SimulatedShared>,
}

/// Voting scenario for simulation: determines how evaluations evolve across rounds.
///
/// Each scenario is selected deterministically per job (from the task hash)
/// and produces a distinct convergence pattern visible in the dashboard.
#[derive(Debug, Clone, Copy)]
enum VotingScenario {
    /// Weak agreement → total consensus: R1 weak positive, converges to all +80.
    DisagreementToConsensus,
    /// Joint reject: R1 weak positive, then one candidate drifts to rejection.
    JointReject,
    /// Joint support: one candidate strongly endorsed, others neutral.
    JointSupport,
    /// Converging → disruption at R3 → recovery.
    DisruptedConsensus,
    /// Immediate consensus: all agents agree strongly from R1.
    ImmediateConsensus,
}

impl SimulatedModel {
    pub fn new(model_name: String, latency_ms: u64) -> Self {
        Self {
            model_name,
            latency: Duration::from_millis(latency_ms),
            shared: Arc::new(SimulatedShared::new()),
        }
    }

    /// Create a new SimulatedModel that shares state with an existing pool.
    /// All models in the same deliberation should share this so that
    /// response times are staggered across agents.
    pub fn new_shared(model_name: String, latency_ms: u64, shared: Arc<SimulatedShared>) -> Self {
        Self {
            model_name,
            latency: Duration::from_millis(latency_ms),
            shared,
        }
    }

    /// Returns a reference to the shared state so callers can pass it
    /// to subsequent `new_shared` calls.
    pub fn shared(&self) -> Arc<SimulatedShared> {
        self.shared.clone()
    }

    /// Extract the user's task description from the `<task>"..."</task>` XML tag
    /// that appears in proposer/evaluator prompts.
    fn extract_task_description(
        messages: &[async_openai::types::ChatCompletionRequestMessage],
    ) -> Option<String> {
        for msg in messages {
            let text = match msg {
                async_openai::types::ChatCompletionRequestMessage::User(u) => {
                    if let async_openai::types::ChatCompletionRequestUserMessageContent::Text(t) =
                        &u.content
                    {
                        t.clone()
                    } else {
                        continue;
                    }
                }
                async_openai::types::ChatCompletionRequestMessage::System(s) => {
                    if let async_openai::types::ChatCompletionRequestSystemMessageContent::Text(t) =
                        &s.content
                    {
                        t.clone()
                    } else {
                        continue;
                    }
                }
                _ => continue,
            };

            // Look for <task>"..."</task>
            if let Some(start) = text.find("<task>") {
                let after_tag = start + "<task>".len();
                if let Some(end) = text[after_tag..].find("</task>") {
                    let inner = text[after_tag..after_tag + end].trim();
                    // Strip surrounding quotes if present
                    let inner = inner.strip_prefix('"').unwrap_or(inner);
                    let inner = inner.strip_suffix('"').unwrap_or(inner);
                    if !inner.is_empty() {
                        return Some(inner.to_string());
                    }
                }
            }
        }
        None
    }

    /// Detect the scenario domain from the task description and return a key.
    fn detect_scenario(task: &str) -> &'static str {
        let lower = task.to_lowercase();
        // Long-form intent takes priority over domain routing so prompts
        // like "security audit in detail" route to the lengthy generator
        // (exercising UI scroll / clamp) rather than the domain-specific
        // canned answer. UX stress is the caller's intent when they
        // spell out "in detail" / "comprehensive" / etc.
        if lower.contains("essay")
            || lower.contains("long-form")
            || lower.contains("in detail")
            || lower.contains("comprehensive")
            || lower.contains("deep dive")
            || lower.contains("explain thoroughly")
        {
            return "lengthy";
        }
        if lower.contains("supply chain")
            || lower.contains("supplier")
            || lower.contains("inventory")
            || lower.contains("semiconductor")
            || lower.contains("sourcing")
            || lower.contains("procurement")
            || lower.contains("logistics")
        {
            "supply"
        } else if lower.contains("re-entrancy")
            || lower.contains("reentrancy")
            || lower.contains("smart contract")
            || lower.contains("vault contract")
            || lower.contains("vulnerability")
            || lower.contains("solidity")
            || lower.contains("audit")
            || lower.contains("flash loan")
            || lower.contains("fuzzing")
            || lower.contains("static analysis")
        {
            "security"
        } else if lower.contains("portfolio")
            || lower.contains("allocation")
            || lower.contains("quantitative")
            || lower.contains("hedge fund")
            || lower.contains("hedge portfolio")
            || lower.contains("asset class")
            || lower.contains("drawdown")
            || lower.contains("rebalancing")
            || lower.contains("momentum")
            || lower.contains("trading")
        {
            "quant"
        } else if lower.contains("contract")
            || lower.contains("legal")
            || lower.contains("clause")
            || lower.contains("liability")
            || lower.contains("license agreement")
            || lower.contains("ip ")
            || lower.contains("intellectual property")
        {
            "legal"
        } else {
            "generic"
        }
    }

    /// Return a scenario-appropriate DM question for the user mid-deliberation.
    fn dm_user_message(scenario: &str) -> String {
        match scenario {
            "supply" => "Before I finalize my next proposal, could you clarify whether expedited air-freight for the ShenZhen Semi TC-4090 alternatives is within budget? The spot-market premium is roughly 22% above contract pricing.".to_string(),
            "security" => "I need to verify a detail before my next analysis: does the vault's withdraw() function use a reentrancy guard (e.g., OpenZeppelin's ReentrancyGuard), or is it relying solely on the checks-effects-interactions pattern? This significantly changes the severity rating of the cross-function re-entrancy vector I identified.".to_string(),
            "quant" => "Quick question before my next proposal: do you have a preference on maximum single-name concentration? The tail-risk hedge I am modeling uses a 5% collar in SPY puts, but a 3% cap would change the structure meaningfully.".to_string(),
            "legal" => "I need clarification before continuing: does your data science team currently train models on data processed through CloudMatrix, or is that a planned future use? The IP co-ownership clause risk depends heavily on this.".to_string(),
            "lengthy" => "Before I expand further: what depth of detail do you want — a high-level synthesis with bullets, or a chapter-by-chapter treatment with worked examples for each section?".to_string(),
            _ => "I have a clarifying question before continuing — could you provide any additional context or constraints I should factor in?".to_string(),
        }
    }

    fn generate_fake_proposal(&self, agent_name: &str, round: usize, task: Option<&str>) -> String {
        let scenario = task.map_or("generic", Self::detect_scenario);
        let voting = Self::select_voting_scenario(task);
        let agent_seed = agent_name
            .bytes()
            .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
        // Pick a perspective index based on agent seed — different agents take different angles
        let perspective = (agent_seed as usize) % 4;

        let (thought, content) = match scenario {
            "supply" => Self::supply_chain_proposal(agent_name, round, perspective),
            "security" => Self::security_proposal(agent_name, round, perspective),
            "quant" => Self::quant_proposal(agent_name, round, perspective),
            "legal" => Self::legal_proposal(agent_name, round, perspective),
            "lengthy" => Self::lengthy_proposal(agent_name, round, perspective),
            _ => Self::generic_proposal(agent_name, round, task),
        };

        // Prefix thought_process with the voting scenario for dashboard visibility
        let scenario_label = Self::voting_scenario_label(voting);
        if round == 1 {
            tracing::info!(
                agent = %agent_name,
                voting_scenario = %scenario_label,
                "🎲 Simulation voting scenario selected"
            );
        }
        let thought = format!("_simulation_:{scenario_label}\n\n{thought}");
        let content = format!("[sim:{scenario_label}] {content}");

        json!({
            "thought_process": thought,
            "solution_content": content,
        })
        .to_string()
    }

    fn generic_proposal(agent_name: &str, round: usize, task: Option<&str>) -> (String, String) {
        let task_snippet = task
            .map(|t| {
                if t.len() > 120 {
                    format!("{}...", &t[..120])
                } else {
                    t.to_string()
                }
            })
            .unwrap_or_else(|| "the given task".to_string());

        let thought = format!(
            "Round {round} analysis by {agent_name}: I'm examining the core requirements of the task: \"{task_snippet}\". \
             Considering multiple approaches and evaluating trade-offs between completeness, feasibility, and impact. \
             My focus this round is on {}.",
            match round {
                1 => "establishing a solid baseline approach with clear reasoning",
                2 => "refining based on peer feedback and addressing identified gaps",
                3 => "synthesizing the strongest elements across all proposals",
                _ => "final optimization and verification of the solution",
            }
        );
        let content = "Based on my analysis of the task, I recommend the following approach:\n\n\
             1. **Core Strategy**: Address the primary requirements systematically, prioritizing high-impact items first.\n\
             2. **Key Considerations**: Balance thoroughness with practical constraints; ensure the solution is actionable.\n\
             3. **Risk Mitigation**: Identify potential failure modes and build in safeguards.\n\n\
             This approach optimizes for both correctness and implementability."
            .to_string();
        (thought, content)
    }

    fn supply_chain_proposal(
        agent_name: &str,
        round: usize,
        perspective: usize,
    ) -> (String, String) {
        let perspectives = [
            // 0: Inventory triage specialist
            (
                format!(
                    "Round {round}: Analyzing inventory triage priorities. Current stock of ~6,200 units covers roughly 3 weeks. \
                     I need to identify which Tier-2 customers have the earliest SLA penalty triggers and allocate remaining inventory \
                     to minimize contractual exposure. {}",
                    if round > 1 {
                        "Incorporating peer feedback on cost modeling and supplier lead times."
                    } else {
                        "Starting with a customer-priority matrix based on penalty severity and relationship value."
                    }
                ),
                "**Inventory Triage & Customer Prioritization**\n\n\
                     **Immediate Actions (Week 1-2):**\n\
                     - Freeze all non-committed inventory; halt any non-Tier-2 shipments\n\
                     - Rank customers by: (1) penalty clause activation timeline, (2) penalty $ amount, (3) strategic relationship value\n\
                     - Allocate 4,000 units to highest-priority customer, 1,500 to second, hold 700 as safety buffer\n\n\
                     **Short-Term Sourcing (Week 2-8):**\n\
                     - Place immediate PO with ShenZhen Semi for 5,000 units despite 15% premium — total excess cost ~$340K\n\
                     - Simultaneously probe spot market for TC-4090 compatible chips; budget up to 25% premium for 2,000 units\n\n\
                     **Financial Impact:**\n\
                     - Without action: ~$4.2M revenue at risk (40% of Q3) plus $800K-1.2M in SLA penalties\n\
                     - With this plan: ~$340K sourcing premium + ~$180K spot market premium = $520K total mitigation cost\n\
                     - ROI on mitigation: 7-8x penalty avoidance".to_string(),
            ),
            // 1: Supply diversification strategist
            (
                format!(
                    "Round {round}: Evaluating medium-term supply chain resilience. The single-supplier dependency on TaiwanChip is the root cause. \
                     {}",
                    if round > 1 {
                        "Refining the dual-sourcing model based on peer analysis of ShenZhen Semi's capacity constraints and Nordic nRF redesign timeline."
                    } else {
                        "Building a phased diversification roadmap that addresses both the immediate crisis and long-term structural vulnerability."
                    }
                ),
                "**Supply Chain Diversification Strategy**\n\n\
                     **Phase 1 — Crisis Response (Weeks 1-8):**\n\
                     - Dual-source: ShenZhen Semi (primary alt) + spot market procurement\n\
                     - Accept 15% unit cost increase as insurance premium; negotiate volume discount for >10K unit commitment\n\n\
                     **Phase 2 — Structural Redesign (Months 2-6):**\n\
                     - Initiate parallel product redesign for Nordic nRF9160 compatibility\n\
                     - Target: pin-compatible adapter board (8 weeks) + firmware port (12 weeks)\n\
                     - Estimated NRE cost: $280K; breaks even at ~15K units vs. ShenZhen premium\n\n\
                     **Phase 3 — Resilient Architecture (Months 6-12):**\n\
                     - Qualify 3 suppliers for next-gen product: TaiwanChip (primary), ShenZhen Semi, Nordic\n\
                     - Implement vendor-managed inventory (VMI) with 6-week safety stock minimum\n\
                     - Build contractual dual-source requirements into all new designs".to_string(),
            ),
            // 2: Customer communications
            (
                format!(
                    "Round {round}: Focusing on customer communication strategy and relationship management. \
                     {} The key is transparency without triggering panic — we need to control the narrative.",
                    if round > 1 {
                        "Adjusting communication timeline based on peer feedback about sourcing confirmation windows."
                    } else {
                        "Designing a tiered disclosure strategy matched to each customer's penalty clause structure."
                    }
                ),
                "**Stakeholder Communication & Relationship Strategy**\n\n\
                     **Tier 1 — Immediate (Within 48 Hours):**\n\
                     - Direct call to VP-level contacts at all 3 Tier-2 customers\n\
                     - Message: \"We are proactively managing a component supply disruption. Your orders are prioritized. \
                     We have activated alternative sourcing and expect to confirm revised timelines within 10 business days.\"\n\
                     - Do NOT disclose specific inventory numbers or supplier names\n\n\
                     **Tier 2 — Confirmation (Week 2-3):**\n\
                     - Once ShenZhen Semi PO is confirmed, send formal written update with revised delivery schedules\n\
                     - Offer: expedited shipment at our cost for any units delayed beyond original schedule\n\
                     - For customer with most aggressive SLA: propose temporary partial shipments to stay within penalty window\n\n\
                     **Internal Communication:**\n\
                     - Brief sales team immediately; provide approved talking points\n\
                     - Escalate to board: present this as managed risk, not crisis — show mitigation cost vs. penalty avoidance ROI".to_string(),
            ),
            // 3: Financial modeler
            (
                format!(
                    "Round {round}: Building financial impact model across all scenarios. \
                     {} Modeling three paths: do-nothing, conservative mitigation, aggressive dual-source.",
                    if round > 1 {
                        "Updating cost assumptions based on peer sourcing analysis and customer priority data."
                    } else {
                        "Quantifying revenue impact, penalty exposure, and mitigation costs to find the optimal spend threshold."
                    }
                ),
                "**Financial Impact Analysis & Scenario Modeling**\n\n\
                     | Scenario | Revenue Impact | Penalty Exposure | Mitigation Cost | Net Impact |\n\
                     |----------|---------------|-----------------|-----------------|------------|\n\
                     | Do Nothing | -$4.2M | $800K-1.2M | $0 | **-$5.0M to -$5.4M** |\n\
                     | Conservative (ShenZhen only) | -$1.1M (gap weeks) | $200K | $340K | **-$1.6M** |\n\
                     | Aggressive (ShenZhen + spot + redesign) | -$400K | $0 | $800K | **-$1.2M** |\n\n\
                     **Recommendation: Aggressive scenario.**\n\
                     - Worst-case cost: $1.2M (23% of at-risk revenue)\n\
                     - Best-case cost: $650K (if spot market yields are good)\n\
                     - Strategic value: Establishes dual-source capability worth ~$2M/year in risk reduction\n\
                     - Payback period on Nordic redesign NRE: 8 months at current volumes".to_string(),
            ),
        ];

        let idx = perspective.min(perspectives.len() - 1);
        let (thought, content) = &perspectives[idx];
        (format!("[{agent_name}] {thought}"), content.clone())
    }

    fn security_proposal(agent_name: &str, round: usize, perspective: usize) -> (String, String) {
        let perspectives = [
            // 0: Re-entrancy analysis
            (
                format!(
                    "Round {round}: Deep-diving on re-entrancy vectors in the vault contract. \
                     {} The withdraw() refactor moved state updates after the external call — classic CEI violation.",
                    if round > 1 {
                        "Cross-referencing with static analysis findings — the same pattern exists in the batch withdrawal path."
                    } else {
                        "Mapping all external call sites to identify checks-effects-interactions violations."
                    }
                ),
                "**Re-Entrancy Vulnerability Analysis**\n\n\
                     **Severity: CRITICAL**\n\n\
                     **Finding 1 — Cross-Function Re-Entrancy in withdraw()**\n\
                     The refactored `withdraw()` performs an external ETH transfer via `.call{value: amount}(\"\")` \
                     BEFORE updating the internal `balances[msg.sender]` mapping. An attacker contract can re-enter \
                     through `flashLoan()` → `onFlashLoan()` callback to drain the vault.\n\n\
                     **Attack Scenario:**\n\
                     ```\n\
                     1. Attacker calls withdraw(1 ETH)\n\
                     2. Vault sends 1 ETH → triggers attacker's receive()\n\
                     3. Attacker re-enters via flashLoan() → borrows remaining TVL\n\
                     4. Balance check passes (state not yet updated)\n\
                     5. Vault drained in single transaction\n\
                     ```\n\n\
                     **Recommended Fix:**\n\
                     - Add OpenZeppelin `ReentrancyGuard` with `nonReentrant` modifier on ALL external-facing functions\n\
                     - Refactor to checks-effects-interactions: update `balances` BEFORE the `.call`\n\
                     - Add `reentrancyGuard` to flashLoan(), withdraw(), and batchWithdraw()\n\
                     - Estimated effort: 2-3 days; **Priority: P0 — blocks deployment**".to_string(),
            ),
            // 1: Static analysis findings
            (
                format!(
                    "Round {round}: Running Slither and Mythril against the vault bytecode. \
                     {} Static analysis flagged 14 findings across 4 severity levels.",
                    if round > 1 {
                        "Correlating with re-entrancy and fuzz findings to eliminate false positives."
                    } else {
                        "Triaging findings: focusing on access-control flaws and unchecked return values."
                    }
                ),
                "**Static Analysis Report (Slither + Mythril)**\n\n\
                     **Summary: 14 findings — 3 Critical, 4 High, 5 Medium, 2 Low**\n\n\
                     **Critical Findings:**\n\
                     1. **Unchecked Return Value (L147)**: `token.transfer()` return value not checked in `distributeFees()`. \
                     ERC-20 tokens that return `false` on failure (e.g., USDT) will silently fail.\n\
                     2. **Access Control Gap**: `setOracle()` uses a custom role system instead of OpenZeppelin AccessControl. \
                     The role check has a logic error: `hasRole(ADMIN) || hasRole(OPERATOR)` should be `&&` for sensitive operations.\n\
                     3. **Integer Overflow in Fee Calc**: `fee = amount * feeRate / 10000` can overflow for large amounts \
                     when `feeRate > 0`. Use SafeMath or Solidity 0.8+ checked arithmetic.\n\n\
                     **High Findings:**\n\
                     1. **Unprotected `selfdestruct`**: Emergency shutdown function lacks timelock\n\
                     2. **Oracle manipulation**: Single-block TWAP window vulnerable to flash-loan manipulation\n\
                     3. **Missing event emissions**: State changes in `updateRewards()` emit no events\n\
                     4. **Tx.origin authentication**: `isAdmin()` uses `tx.origin` instead of `msg.sender`\n\n\
                     **Remediation Priority:** Fix critical findings before mainnet; highs within 1 sprint".to_string(),
            ),
            // 2: Fuzz testing results
            (
                format!(
                    "Round {round}: Running Echidna and Foundry invariant tests against the vault state machine. \
                     {} Property-based testing found 3 invariant violations across 10M iterations.",
                    if round > 1 {
                        "Extending corpus based on re-entrancy vectors identified by peers."
                    } else {
                        "Testing core invariants: total_deposits >= total_withdrawals, share_price monotonically non-decreasing, TVL accounting."
                    }
                ),
                "**Fuzz Testing Report (Echidna + Foundry)**\n\n\
                     **Invariant Violations Found: 3 / 8 tested**\n\n\
                     **Violation 1 — TVL Accounting Drift (Critical)**\n\
                     After 847,293 iterations, Echidna found a sequence where `vault.totalAssets()` diverges from \
                     `sum(balances[])` by up to 0.3 ETH. Root cause: the flash loan callback can manipulate the \
                     vault's balance during the same transaction, causing `totalAssets()` (which reads `address(this).balance`) \
                     to include flash-loaned funds.\n\n\
                     **Violation 2 — Share Price Manipulation (High)**\n\
                     Foundry found a 4-step sequence that inflates share price by 12%:\n\
                     1. Deposit 0.001 ETH (get 1 share)\n\
                     2. Direct-transfer 100 ETH to vault (no shares minted)\n\
                     3. Share price = 100.001 ETH/share\n\
                     4. First depositor extracts donated ETH via withdrawal\n\n\
                     **Violation 3 — Rounding Error Accumulation (Medium)**\n\
                     After 5M+ deposit/withdraw cycles, rounding errors in `shares = amount * totalShares / totalAssets` \
                     accumulate to ~0.01% of TVL. Recommend using virtual shares (offset) pattern.\n\n\
                     **All fuzzing seeds and reproduction scripts committed to `/test/fuzz/corpus/`**".to_string(),
            ),
            // 3: Regulatory / architecture synthesis
            (
                format!(
                    "Round {round}: Assessing regulatory compliance and synthesizing all findings into remediation roadmap. \
                     {} The vault's design has implications under MiCA and SEC's DeFi enforcement framework.",
                    if round > 1 {
                        "Integrating re-entrancy, static, and fuzz findings to build prioritized remediation timeline."
                    } else {
                        "Evaluating whether the vault's flash loan feature triggers securities classification under Howey test."
                    }
                ),
                "**Security Posture & Remediation Roadmap**\n\n\
                     **Overall Rating: HIGH RISK — Do Not Deploy**\n\n\
                     **Consolidated Findings (de-duplicated):**\n\
                     | # | Finding | Severity | Source | Fix Effort |\n\
                     |---|---------|----------|--------|------------|\n\
                     | 1 | Cross-function re-entrancy | Critical | Manual + Fuzz | 2 days |\n\
                     | 2 | Unchecked ERC-20 returns | Critical | Slither | 1 day |\n\
                     | 3 | Access control logic error | Critical | Slither | 1 day |\n\
                     | 4 | TVL accounting drift | Critical | Echidna | 3 days |\n\
                     | 5 | Share price manipulation | High | Foundry | 2 days |\n\
                     | 6 | Oracle TWAP manipulation | High | Mythril | 3 days |\n\
                     | 7 | tx.origin authentication | High | Slither | 0.5 days |\n\n\
                     **Remediation Phases:**\n\
                     - **Phase 1 (Week 1)**: Fix all Critical findings. Add ReentrancyGuard, SafeERC20, fix access control.\n\
                     - **Phase 2 (Week 2)**: Fix High findings. Implement EIP-4626 vault standard, extend TWAP window.\n\
                     - **Phase 3 (Week 3)**: Re-run full fuzz suite + formal verification on critical paths.\n\
                     - **Phase 4**: External audit by Trail of Bits or OpenZeppelin (4-6 week lead time, ~$80K-120K)\n\n\
                     **Regulatory Note:** Flash loan feature may trigger MiCA lending provisions. Recommend legal review before EU launch.".to_string(),
            ),
        ];

        let idx = perspective.min(perspectives.len() - 1);
        let (thought, content) = &perspectives[idx];
        (format!("[{agent_name}] {thought}"), content.clone())
    }

    fn quant_proposal(agent_name: &str, round: usize, perspective: usize) -> (String, String) {
        let perspectives = [
            // 0: Macro strategist
            (
                format!(
                    "Round {round}: Analyzing macro regime and asset allocation. \
                     {} The inverted yield curve at 16 months and elevated MOVE index signal rate volatility — \
                     historically this regime favors quality + short duration.",
                    if round > 1 {
                        "Revising allocation based on peer tail-risk analysis and factor exposure recommendations."
                    } else {
                        "Building the core allocation framework: overweight quality, underweight duration, tactical commodity exposure."
                    }
                ),
                "**Macro-Driven Asset Allocation**\n\n\
                     **Recommended Allocation:**\n\
                     | Asset Class | Allocation | Rationale |\n\
                     |------------|-----------|----------|\n\
                     | US Large Cap (Quality Factor) | 25% | SPY P/E elevated but quality factor provides downside protection |\n\
                     | International Developed (Hedged) | 12% | ECB rate cuts = tailwind; FX hedge neutralizes USD strength |\n\
                     | EM Equities (Asia ex-China) | 8% | Beneficiary of supply chain diversification trend |\n\
                     | Short-Duration Investment Grade | 20% | 5.2% yield with minimal rate sensitivity |\n\
                     | TIPS (1-5yr) | 10% | Real yield at 2.1% is historically attractive |\n\
                     | Gold & Commodity Basket | 10% | Portfolio hedge; gold momentum + copper supply constraints |\n\
                     | Cash / T-Bills | 10% | 5.3% risk-free; liquidity buffer for rebalancing |\n\
                     | Tail Risk Hedges | 5% | See hedge portfolio design |\n\n\
                     **Expected Return**: 13.2% annualized | **Max Drawdown (modeled)**: -6.8% | **Sharpe**: 1.1".to_string(),
            ),
            // 1: Factor/quant specialist
            (
                format!(
                    "Round {round}: Focusing on factor exposure optimization for the current regime. \
                     {} VIX at 14.2 means vol-selling strategies are cheap to enter but carry extreme left-tail risk.",
                    if round > 1 {
                        "Incorporating macro allocation framework from peers; refining factor tilts within equity sleeve."
                    } else {
                        "Running factor decomposition: quality + momentum are the regime-appropriate factors; low-vol is a trap at VIX 14."
                    }
                ),
                "**Factor Exposure Strategy**\n\n\
                     **Recommended Factor Tilts (within equity allocation):**\n\
                     - **Quality (Overweight +3σ)**: Companies with high ROE, low debt/equity, stable earnings. \
                     In late-cycle with elevated P/E, quality has outperformed by 340bps annually in similar regimes (2006-07, 2018-19).\n\
                     - **Momentum (Overweight +2σ)**: Trend-following has positive carry when VIX is sub-15. \
                     Implementation: cross-sectional momentum within S&P 500 + international developed.\n\
                     - **Value (Neutral)**: Value spread is compressing but not yet at levels that signal sustained outperformance.\n\
                     - **Low Volatility (Underweight -2σ)**: Low-vol stocks are overcrowded and will underperform if VIX regime-shifts higher. \
                     The VIX-MOVE divergence (14 vs 115) historically resolves with equity vol catching up.\n\n\
                     **Implementation:**\n\
                     - Use QUAL ETF (25% of equity sleeve) + MTUM ETF (20%)\n\
                     - Direct factor exposure via L/S portfolio for remaining 55%: long high-quality momentum names, short low-quality mean-reversion candidates\n\
                     - Rebalance factor weights monthly using rolling 6-month factor momentum scores".to_string(),
            ),
            // 2: Tail risk/derivatives specialist
            (
                format!(
                    "Round {round}: Designing the tail risk hedge portfolio. \
                     {} With VIX at 14.2, put options are historically cheap — this is the optimal entry point for convex hedges.",
                    if round > 1 {
                        "Refining hedge construction based on peers' allocation sizes and rebalancing triggers."
                    } else {
                        "Targeting 3x+ payoff in a -20% scenario while keeping annual cost under 50bps of portfolio NAV."
                    }
                ),
                "**Tail Risk Hedge Portfolio**\n\n\
                     **Budget**: 50bps of $50M = $250K annually\n\n\
                     **Structure:**\n\
                     1. **SPX Put Spread Collar** (60% of hedge budget = $150K/yr)\n\
                        - Buy 90-95% moneyness SPX puts, 3-month rolling\n\
                        - Sell 80% moneyness puts to reduce cost\n\
                        - Current cost: ~35bps/quarter at VIX 14; payoff 2.8x at -15%, 4.2x at -20%\n\n\
                     2. **VIX Call Spreads** (25% of budget = $62K/yr)\n\
                        - Buy VIX 20/35 call spreads, 2-month rolling\n\
                        - VIX at 14 → these are deeply OTM and cheap (~$0.80 per spread)\n\
                        - Payoff: 6x+ when VIX spikes to 30+ (COVID: VIX hit 82; 2022: VIX hit 36)\n\n\
                     3. **CDX IG Payer Swaptions** (15% of budget = $38K/yr)\n\
                        - 3-month payer swaptions on CDX.NA.IG at +20bps OTM\n\
                        - Diversified: pays off on credit stress independent of equity vol\n\n\
                     **Backtest Results:**\n\
                     - 2022 drawdown (-25%): hedge portfolio returned +$1.4M (5.6x cost)\n\
                     - COVID crash (-34%): hedge returned +$2.1M (8.4x cost)\n\
                     - 2023 (no crash): hedge cost -$245K (within budget)".to_string(),
            ),
            // 3: Rebalancing/risk management
            (
                format!(
                    "Round {round}: Defining systematic rebalancing rules and risk triggers. \
                     {} The -8% max drawdown constraint requires proactive de-risking triggers, not just calendar rebalancing.",
                    if round > 1 {
                        "Calibrating triggers based on peers' allocation weights and tail hedge payoff profiles."
                    } else {
                        "Building a rule-based framework: calendar rebalancing + volatility triggers + drawdown circuit breakers."
                    }
                ),
                "**Rebalancing & Risk Management Framework**\n\n\
                     **Systematic Rebalancing Rules:**\n\n\
                     1. **Calendar Rebalance** (Monthly)\n\
                        - Rebalance any asset class >3% from target weight\n\
                        - Tax-aware: use new cash flows for rebalancing before selling positions\n\n\
                     2. **Volatility Regime Triggers**\n\
                        - VIX > 20: Reduce equity to 35% (from 45%), increase cash to 20%\n\
                        - VIX > 30: Reduce equity to 25%, increase short-duration bonds to 30%\n\
                        - MOVE > 140: Reduce all duration exposure by 50%; shift to T-Bills\n\n\
                     3. **Drawdown Circuit Breakers** (max DD = -8%)\n\
                        - At -4% portfolio drawdown: Reduce all risk by 25% (move to cash)\n\
                        - At -6% portfolio drawdown: Reduce all risk by 50%; activate full hedge overlay\n\
                        - At -7.5% portfolio drawdown: Move to 80% cash/T-Bills (capital preservation mode)\n\
                        - Re-entry: gradual (20% per week) once drawdown recovers to -4%\n\n\
                     **Liquidity Compliance:**\n\
                     - 50% liquidation within 5 days: Achievable at all times — no position requires >2 days to liquidate\n\
                     - Monthly liquidity stress test against 3x average daily volume".to_string(),
            ),
        ];

        let idx = perspective.min(perspectives.len() - 1);
        let (thought, content) = &perspectives[idx];
        (format!("[{agent_name}] {thought}"), content.clone())
    }

    fn legal_proposal(agent_name: &str, round: usize, perspective: usize) -> (String, String) {
        let perspectives = [
            // 0: IP specialist
            (
                format!(
                    "Round {round}: Analyzing IP and data rights under Section 4 — the joint ownership clause. \
                     {} This is the highest-risk clause in the entire agreement.",
                    if round > 1 {
                        "Cross-referencing with liability and termination analyses from peers."
                    } else {
                        "Assessing whether 'derivative insights generated using the Platform' could capture our proprietary ML models."
                    }
                ),
                "**IP & Data Rights Analysis (Section 4)**\n\n\
                     **Risk Level: CRITICAL** | **Negotiation Priority: #1**\n\n\
                     **Analysis:**\n\
                     The clause \"any models, algorithms, or derivative insights generated using the Platform shall be jointly owned\" \
                     is extraordinarily broad. Under this language:\n\
                     - Any ML model trained on data that passes through their platform → jointly owned by CloudMatrix\n\
                     - They could license YOUR proprietary algorithms to your competitors\n\
                     - Joint ownership under US copyright law means either party can exploit without accounting to the other (Oddo v. Ries)\n\n\
                     **Recommended Redline:**\n\
                     > \"Licensee retains sole and exclusive ownership of all models, algorithms, data, and derivative works created by Licensee, \
                     > including those developed using data processed through the Platform. Licensor shall have no rights in Licensee's intellectual property. \
                     > Platform usage data and aggregated, anonymized performance metrics may be used by Licensor solely for Platform improvement.\"\n\n\
                     **Fallback Position:**\n\
                     If they resist full carve-out, narrow to: \"Joint ownership applies only to improvements to the Platform itself, \
                     not to Licensee's independent models or business logic.\"".to_string(),
            ),
            // 1: Liability/risk specialist
            (
                format!(
                    "Round {round}: Reviewing limitation of liability provisions in Section 9. \
                     {} The absence of data breach and IP infringement carve-outs is concerning given PII processing.",
                    if round > 1 {
                        "Integrating IP risk assessment — if CloudMatrix has joint ownership, their liability exposure for IP misuse is undefined."
                    } else {
                        "Benchmarking the liability cap against market standards for SaaS platforms processing financial PII."
                    }
                ),
                "**Limitation of Liability Analysis (Section 9)**\n\n\
                     **Risk Level: HIGH** | **Negotiation Priority: #2**\n\n\
                     **Analysis:**\n\
                     - Liability cap of 12-month fees (~$400K) is below market for PII-processing platforms\n\
                     - Market standard for similar deals: 2x annual fees or $2M floor, whichever is greater\n\
                     - Missing carve-outs for: (a) data breaches, (b) IP infringement, (c) confidentiality violations\n\
                     - If CloudMatrix suffers a breach exposing our customer PII, our exposure could be $10M+ but their liability is capped at $400K\n\n\
                     **Recommended Redline:**\n\
                     > Add Section 9.2: \"The limitation in Section 9.1 shall not apply to: (a) breaches of Section [Data Security]; \
                     > (b) infringement of the other party's intellectual property rights; (c) breaches of confidentiality obligations; \
                     > (d) Licensor's indemnification obligations. For such excluded claims, liability shall be capped at 3x the total contract value.\"\n\n\
                     **Minimum Acceptable:**\n\
                     Data breach carve-out with 2x TCV super-cap ($2.4M). This is non-negotiable for Series C due diligence.".to_string(),
            ),
            // 2: Termination/portability specialist
            (
                format!(
                    "Round {round}: Evaluating termination and data portability provisions in Section 12. \
                     {} The 'standard formats' language is dangerously vague for a $1.2M platform commitment.",
                    if round > 1 {
                        "Connecting to non-compete analysis — if we can't get our data out cleanly, the non-compete clause becomes a lock-in weapon."
                    } else {
                        "Assessing bankruptcy/acquisition scenarios and data portability completeness guarantees."
                    }
                ),
                "**Termination & Data Portability Analysis (Section 12)**\n\n\
                     **Risk Level: MEDIUM-HIGH** | **Negotiation Priority: #3**\n\n\
                     **Analysis:**\n\
                     - 90-day notice period is standard; acceptable\n\
                     - \"Standard formats\" is undefined — could mean CSV dumps without schema or proprietary binary exports\n\
                     - 30-day export window has no SLA for completeness or validation\n\
                     - **Bankruptcy gap**: No source code escrow or data escrow provisions. If CloudMatrix enters receivership, \
                     our data could be treated as an asset of the estate\n\n\
                     **Recommended Redline:**\n\
                     > Section 12.4: \"Upon termination, Licensor shall export all Licensee Data in [JSON/Parquet/specified format] \
                     > within 30 calendar days. Licensor shall provide a data completeness certificate and Licensee shall have \
                     > 15 business days to validate the export. Any gaps shall be remediated within 10 business days at Licensor's expense.\"\n\n\
                     > Section 12.5 (new): \"Licensor shall maintain a data escrow arrangement with [escrow agent] ensuring Licensee \
                     > data access in the event of Licensor insolvency, acquisition, or material change of control.\"\n\n\
                     **Non-Negotiable:** Defined export format + validation period. Escrow is preferred but can be traded for other concessions.".to_string(),
            ),
            // 3: Non-compete/SLA specialist
            (
                format!(
                    "Round {round}: Analyzing the non-compete clause (Section 14.3) and SLA remedies (Exhibit B). \
                     {} The non-compete is the sleeper risk — it could trap us on a poor-performing platform with no recourse.",
                    if round > 1 {
                        "Integrating liability and portability analyses — the combination of weak SLA + non-compete + vague data export creates a lock-in trilemma."
                    } else {
                        "Assessing enforceability of the non-compete and whether the SLA provides adequate protection during the lock-in period."
                    }
                ),
                "**Non-Compete & SLA Analysis (Section 14.3 + Exhibit B)**\n\n\
                     **Risk Level: HIGH** | **Negotiation Priority: #2 (tied with Liability)**\n\n\
                     **Non-Compete Analysis:**\n\
                     - \"Substantially similar competing platforms\" during term + 12 months post-termination\n\
                     - This prevents migration to Databricks, Snowflake, or even building internal tooling\n\
                     - Enforceability: Likely enforceable in most jurisdictions as a reasonable restraint in a commercial license \
                     (unlike employment non-competes, B2B non-competes have fewer statutory protections)\n\
                     - Combined with the 90-day notice period: effective lock-in of 15 months after deciding to leave\n\n\
                     **SLA Analysis:**\n\
                     - 99.9% uptime = 8.7 hours downtime/year — standard for this tier\n\
                     - Service credits capped at 10% monthly fees as SOLE remedy — no termination right for chronic underperformance\n\
                     - If platform degrades to 99% uptime (87.6 hrs/year), we're stuck paying full price with only 10% credit\n\n\
                     **Recommended Redlines:**\n\
                     > Strike Section 14.3 entirely. If rejected:\n\
                     > Narrow to: \"Licensee shall not use the Platform's proprietary API specifications to build a directly competing product\" \
                     > (protects CloudMatrix's legitimate interest without creating lock-in)\n\
                     > Remove post-termination restriction entirely.\n\n\
                     > Exhibit B: Add \"If SLA falls below 99.5% for 3 consecutive months, Licensee may terminate for cause with 30-day notice \
                     > and receive pro-rata refund of prepaid fees.\"".to_string(),
            ),
        ];

        let idx = perspective.min(perspectives.len() - 1);
        let (thought, content) = &perspectives[idx];
        (format!("[{agent_name}] {thought}"), content.clone())
    }

    /// Voting scenario for simulation: determines how evaluations evolve across rounds.
    ///
    /// Each scenario is selected deterministically per job (from the task hash)
    /// and produces a distinct convergence pattern visible in the dashboard.
    /// Select a voting scenario from the task description hash.
    fn select_voting_scenario(task: Option<&str>) -> VotingScenario {
        let h = task.unwrap_or("").bytes().fold(2654435761u32, |acc, b| {
            acc.wrapping_mul(31).wrapping_add(b as u32)
        });
        match h % 5 {
            0 => VotingScenario::DisagreementToConsensus,
            1 => VotingScenario::JointReject,
            2 => VotingScenario::JointSupport,
            3 => VotingScenario::DisruptedConsensus,
            _ => VotingScenario::ImmediateConsensus,
        }
    }

    /// Long-form proposal generator — produces ~2-3k char multi-
    /// section answers so UI work can exercise modal scrolling,
    /// carousel card line-clamping, and long claim summaries
    /// without needing live model responses of the same length.
    /// Four perspectives rotate among agents so round transitions
    /// deliver visibly different long-form content.
    fn lengthy_proposal(agent_name: &str, round: usize, perspective: usize) -> (String, String) {
        let (angle, body) = match perspective {
            0 => (
                "structural-synthesis",
                "**Section 1 — Framing the question.**\n\nThe prompt invites a deliberately expansive answer. Rather than collapse the inquiry into a headline recommendation, I will unpack the concept across five interlocking dimensions: historical origin, current state-of-practice, failure modes observed in production, comparative strengths against adjacent alternatives, and a forward-looking synthesis. Each section is self-contained so the reader can dip in at any depth.\n\n**Section 2 — Historical origin.**\n\nThe idea emerged from a convergence of pressure points: academic publications in the late 2000s hinted at the direction, early industry adopters prototyped in 2012-2014, and the practice crystallised into its present form around 2018 when a handful of high-profile postmortems forced the community to revisit foundational assumptions. The lineage matters because today's conventions — naming, interfaces, tooling defaults — carry forward decisions that were locally sensible but globally accidental.\n\n**Section 3 — Current state-of-practice.**\n\nMainstream implementations favour a three-tier layout: an ingress surface that canonicalises input, a deliberation core that encodes the domain logic, and a projection layer that serves read-optimised views. Teams that skip the projection layer and serve directly from the core typically hit scaling cliffs between 10× and 30× of their initial load. Operationally, the observed pattern is that the projection layer accounts for roughly 55% of ongoing maintenance cost — a surprise to teams that treated it as an afterthought during design.\n\n**Section 4 — Failure modes.**\n\nThe three failure modes worth naming explicitly are (a) coupling drift, where the core accumulates fields that exist solely to serve projection quirks; (b) snapshot desynchronisation, where the projection's reconciliation window lags the core's commit cadence; and (c) ownership erosion, where on-call rotation ambiguity leaves incidents to fall between teams. Each has well-documented early warning signs but they compound quickly if untreated.\n\n**Section 5 — Synthesis.**\n\nMy recommendation is to invest early in a disciplined contract between core and projection, codified as a versioned schema with CI-enforced compatibility checks, and to assign a single named owner responsible for reconciliation SLOs. That pair of decisions eliminates the dominant incident class in the failure-mode catalog above and buys roughly 18 months of runway before the next structural rework is needed.",
            ),
            1 => (
                "evidence-dense",
                "# Long-form analysis\n\n## Premise\n\nThe question asks for depth, so I have organised my response as a literature-style review with concrete citations to the practices I am comparing. Every assertion below is backed by either a production postmortem, a referenced benchmark, or a reproducible experiment I can describe end-to-end.\n\n## Key findings\n\n1. **Throughput scaling is non-linear past the hot-path threshold.** Benchmarks across three published datasets (arXiv 2019.04, SIGCOMM '21, Netflix Tech Blog 2022) converge on a ~4.7× amplification factor once the system crosses its p95 queue-depth ceiling.\n\n2. **Latency variance dominates tail behaviour more than mean latency.** A system with a 40ms mean and 60ms p99 outperforms a system with 30ms mean and 140ms p99 on nearly every user-perceived metric. Tail compression — not mean reduction — is the lever with real leverage.\n\n3. **Operational cost tracks coordination overhead, not infrastructure spend.** Teams that invest heavily in observability, runbooks, and blameless postmortems spend 2-3× on tooling but experience roughly 40% fewer incidents and resolve them in a third of the time. The infrastructure bill is a rounding error compared with engineer-hours lost to on-call churn.\n\n## Recommended investments\n\nGiven the findings, the three highest-ROI investments are (a) a rigorous tail-latency measurement harness with synthetic load generators reproducing real traffic shapes, (b) a coordination playbook that explicitly names decision owners for each incident class, and (c) a quarterly postmortem review meeting where recurring patterns get promoted to architectural actions. The combined capex for these three is modest — typically one quarter of one staff engineer's time — and the observed payback is under six months.\n\n## Caveats\n\nThe findings generalise poorly to systems with fundamentally different failure semantics (e.g., strongly-consistent storage vs eventually-consistent). Readers working in those regimes should treat the numerical benchmarks as directional only and re-derive thresholds from their own traffic.",
            ),
            2 => (
                "narrative-exposition",
                "Let me walk through this at length — the question deserves a thorough treatment and I think a narrative structure will surface nuances that bullet lists tend to flatten.\n\nThe story begins, as these stories usually do, with a team encountering a problem they did not expect. Their system had been running happily for two years. Traffic grew linearly. Infrastructure scaled sideways without drama. Then, almost overnight, latencies began drifting upward in a way that no single metric explained. The dashboards showed nothing anomalous; the traces showed a distribution that had quietly stretched on the right tail while the mean held steady. Customer complaints trickled in — not many, but enough to suggest that the tail was now visible to real users.\n\nThe investigation took three weeks. It began with the obvious suspects: GC pauses, noisy neighbours, disk contention, network microbursts. Each was plausible, each was ruled out. The team then reconstructed the full request-to-response lifecycle for a sample of slow requests and discovered that the slowness was not located in any single hop. It was distributed across the call graph — a few milliseconds here, a dozen there — and only when added up did it cross the threshold of user-visible lag.\n\nThe diagnosis was a slow accumulation of coordination overhead. As the system had grown, the number of cross-service RPCs for a single user-facing request had crept up from ~8 to ~22. Each new RPC was individually cheap, but the sum had grown past the point where a single tail event — one slow service, one GC pause, one network blip — could no longer be absorbed by the remaining budget. The fix was not a single change but a sequence of them: consolidating a handful of downstream calls, adding a request-hedging layer for the most variance-prone hop, and introducing a deadline-propagation primitive so that budgets failed fast rather than compounded.\n\nThe lesson that emerged is that systems fail quietly for a long time before they fail loudly. The right thing to invest in, when you suspect this shape of problem, is not more dashboards but a disciplined practice of measuring the request-lifecycle distribution and tracking how it evolves over months. Teams that do this routinely catch the drift early; teams that don't are forced into three-week investigations with unhappy customers in the loop.",
            ),
            _ => (
                "contrarian-take",
                "I will offer a deliberately contrarian long-form response. The received wisdom on this topic — which most of my peer proposals probably endorse — is, in my view, over-indexed on best practices that make sense in aggregate but mislead in the specific case the prompt describes.\n\nThe first contrarian point is about complexity budgets. The standard advice is to invest early in abstraction layers, separation of concerns, and generalised infrastructure. That advice is correct for teams above a certain scale. For teams below it, premature abstraction is the single most common cause of projects that stall at 60% completion. The abstraction layers absorb engineering bandwidth disproportionately to the value they deliver, and worse, they calcify poorly-understood assumptions into hard-to-reverse architectural commitments. A pragmatic counter-move is to defer abstractions until at least two concrete use cases exist, and to refuse to generalise on the basis of one.\n\nThe second contrarian point is about observability. The industry narrative is that you cannot have too much telemetry. In practice, teams regularly drown in signal — dashboards proliferate, alert fatigue sets in, noisy false positives push real incidents out of the active-attention window. A more effective posture is to treat observability as a curated product: fewer, sharper signals, each with a clear owner and a documented response. Retiring noisy signals is at least as important as adding new ones.\n\nThe third contrarian point is about postmortems. The standard recommendation is blameless culture plus detailed timelines plus systemic root-cause analysis. All three are good. The failure mode no one talks about is postmortems that produce action items no one ever completes. A postmortem with three unimplemented actions from six months ago is worse than no postmortem at all — it trains the team to see the ritual as theatre. The fix is to enforce a small, fixed WIP limit on open action items: completing existing ones becomes the gate for filing new ones.\n\nThese three points will not be popular with readers expecting orthodoxy. They are, however, supported by repeated observation across teams I have worked with, and I would rather be directionally correct and provocative than blandly agreeable.",
            ),
        };

        let thought = format!(
            "Round {round}: taking a long-form {angle} angle as {agent_name}. Drafting a multi-section response that exercises the option-modal scroll path, carousel card line-clamp, and claim-summary rendering against a realistic body length.",
        );
        (thought, body.to_string())
    }

    fn voting_scenario_label(s: VotingScenario) -> &'static str {
        match s {
            VotingScenario::DisagreementToConsensus => "disagreement_to_consensus",
            VotingScenario::JointReject => "joint_reject",
            VotingScenario::JointSupport => "joint_support",
            VotingScenario::DisruptedConsensus => "disrupted_consensus",
            VotingScenario::ImmediateConsensus => "immediate_consensus",
        }
    }

    fn generate_fake_evaluation(
        &self,
        agent_name: &str,
        round: usize,
        candidate_ids: &[String],
        task: Option<&str>,
    ) -> String {
        let evaluator_seed = agent_name
            .bytes()
            .chain(self.model_name.bytes())
            .fold(2654435761u32, |acc, b| {
                acc.wrapping_mul(31).wrapping_add(b as u32)
            });

        let scenario = task.map_or("generic", Self::detect_scenario);
        let voting = Self::select_voting_scenario(task);

        let evaluations: Vec<serde_json::Value> = if candidate_ids.is_empty() {
            let w = (60.0_f32 + (round as f32) * 5.0).clamp(-100.0, 100.0);
            vec![json!({
                "candidate_id":      "Other_Agent",
                "endorsement_weight": w,
                "justification":     "Solid approach with reasonable trade-offs.",
                "is_final_solution": false,
                "stance":            Self::stance_for_weight(w),
                "claim_assessments": Self::generate_claim_assessments(scenario, true, false, evaluator_seed, 0, round),
                "disagreements":     [],
                "category_scores":   Self::generate_category_scores(w, evaluator_seed, 0),
            })]
        } else {
            let n = candidate_ids.len();

            // Stable "true best" and "reject target" — both are derived
            // from the *sorted* candidate-id hash so the choice is
            // independent of the order `candidate_ids` happens to be
            // in. The downstream comparisons must then match by id
            // string, not by enumeration index, because `i` runs over
            // the unsorted `candidate_ids` slice. Earlier code stored
            // `(h % n, (h+1) % n)` as raw indices into `sorted_ids`
            // and compared them against `i` from the unsorted iter —
            // a real determinism bug whenever the orchestrator handed
            // us candidates in a different order between rounds.
            let (true_best_id, reject_target_id): (String, String) = {
                let mut sorted_ids: Vec<&str> = candidate_ids.iter().map(|s| s.as_str()).collect();
                sorted_ids.sort();
                let h = sorted_ids
                    .iter()
                    .flat_map(|id| id.bytes())
                    .fold(2654435761u32, |acc, b| {
                        acc.wrapping_mul(31).wrapping_add(b as u32)
                    });
                let best_idx = (h as usize) % n;
                let reject_idx = ((h as usize).wrapping_add(1)) % n;
                (
                    sorted_ids[best_idx].to_string(),
                    sorted_ids[reject_idx].to_string(),
                )
            };

            // Per-evaluator noise from seed
            let noise_for = |ph: u32| -> f32 {
                ((evaluator_seed
                    .wrapping_mul(ph.wrapping_add(1))
                    .wrapping_add(round as u32 * 17))
                    % 10) as f32
                    - 5.0
            };

            candidate_ids
                .iter()
                .map(|cid| {
                    let ph = cid.bytes()
                        .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
                    let noise = noise_for(ph);

                    let weight = match voting {
                        // Scenario 1: Mixed → total consensus
                        // R1: scattered (-20 to +30), progressively shifts positive
                        VotingScenario::DisagreementToConsensus => {
                            let personal_offset = ((evaluator_seed.wrapping_add(ph)) % 50) as f32 - 20.0;
                            let convergence_shift = (round as f32 - 1.0) * 25.0;
                            (personal_offset + convergence_shift + noise).clamp(-100.0, 100.0)
                        }

                        // Scenario 2: Joint reject one candidate
                        // R1: all weak positive (10 to 30), then reject target drifts negative from R2.
                        // Match by candidate id string, not enumeration index — the unsorted
                        // candidate order changes between runs but the sorted-id hash is stable.
                        VotingScenario::JointReject => {
                            if cid.as_str() == reject_target_id.as_str() {
                                let base = 15.0 - (round as f32 - 1.0) * 30.0;
                                (base + noise).clamp(-100.0, 100.0)
                            } else {
                                let base = 20.0 + (round as f32) * 8.0;
                                (base + noise).clamp(0.0, 100.0)
                            }
                        }

                        // Scenario 3: Joint support for one candidate, neutral others.
                        // Same id-based comparison as JointReject — see comment above.
                        VotingScenario::JointSupport => {
                            if cid.as_str() == true_best_id.as_str() {
                                let base = 60.0 + (round as f32) * 8.0;
                                (base + noise).clamp(30.0, 100.0)
                            } else {
                                let base = 5.0 + noise;
                                base.clamp(-15.0, 25.0)
                            }
                        }

                        // Scenario 4: Mixed → converging → disruption at R3 → recovery
                        // R1: scattered (-30 to +20), shifts positive R2, disruption R3, recovery R4+
                        VotingScenario::DisruptedConsensus => {
                            let is_disruptor = evaluator_seed % 3 == 0;
                            if round == 3 && is_disruptor {
                                (-70.0 + noise).clamp(-100.0, -40.0)
                            } else {
                                let personal_offset = ((evaluator_seed.wrapping_add(ph)) % 50) as f32 - 30.0;
                                let shift = (round as f32 - 1.0) * 25.0;
                                (personal_offset + shift + noise).clamp(-100.0, 100.0)
                            }
                        }

                        // Scenario 5: Immediate consensus — all agents agree strongly from R1
                        VotingScenario::ImmediateConsensus => {
                            let base = 75.0 + (round as f32) * 5.0;
                            (base + noise).clamp(50.0, 100.0)
                        }
                    };

                    debug!(
                        model = %self.model_name,
                        round = round,
                        candidate = %cid,
                        voting_scenario = %Self::voting_scenario_label(voting),
                        weight = weight,
                        "SimulatedModel evaluation weight"
                    );

                    let is_high = weight > 40.0;
                    let is_low = weight < -20.0;
                    let is_medium = !is_high && !is_low;
                    let proposal_hash = ph as usize;

                    let justification = Self::generate_justification(
                        scenario,
                        round,
                        is_high,
                        is_low,
                        evaluator_seed,
                        proposal_hash,
                    );

                    json!({
                        "candidate_id":      cid,
                        "endorsement_weight": weight,
                        "justification":     justification,
                        "is_final_solution": false,
                        "stance":            Self::stance_for_weight(weight),
                        "claim_assessments": Self::generate_claim_assessments(scenario, is_high, is_low, evaluator_seed, proposal_hash, round),
                        "disagreements":     Self::generate_disagreements(scenario, is_low, is_medium, round, proposal_hash),
                        "category_scores":   Self::generate_category_scores(weight, evaluator_seed, proposal_hash),
                    })
                })
                .collect()
        };

        json!({ "evaluations": evaluations }).to_string()
    }

    /// Generate a scenario-appropriate justification for an evaluation.
    fn generate_justification(
        scenario: &str,
        round: usize,
        is_high: bool,
        is_low: bool,
        evaluator_seed: u32,
        proposal_hash: usize,
    ) -> String {
        // Use seed + proposal hash to select a template variant for diversity
        let variant = ((evaluator_seed as usize)
            .wrapping_add(proposal_hash.wrapping_mul(7))
            .wrapping_add(round * 3))
            % 4;

        match scenario {
            "supply" => {
                if is_high {
                    match variant {
                        0 => "The inventory triage priorities are well-structured. The allocation logic correctly prioritizes SLA penalty avoidance, and the financial impact quantification is rigorous.".to_string(),
                        1 => "This proposal presents a strong dual-sourcing strategy. The phased approach from crisis response to structural redesign is practical. Lead-time assumptions for ShenZhen Semi are realistic.".to_string(),
                        2 => "Excellent proposal. The customer communication timeline is well-calibrated — early transparency without premature commitment. The internal escalation framework is board-ready.".to_string(),
                        _ => "The cost-benefit analysis here is the most thorough. The three-scenario comparison clearly demonstrates the aggressive strategy's ROI. Payback period calculations are sound.".to_string(),
                    }
                } else if is_low {
                    match variant {
                        0 => "The spot market premium risk is underestimated. No contingency if ShenZhen Semi's 8-week lead time slips. Penalty exposure calculation appears to ignore compounding clauses.".to_string(),
                        1 => "This proposal lacks urgency on the customer communication front. Waiting for sourcing confirmation before reaching out risks SLA penalty activation. Timeline is too conservative.".to_string(),
                        2 => "Weak financial modeling. The revenue-at-risk figure doesn't account for downstream pipeline impact. Missing sensitivity analysis on chip pricing volatility.".to_string(),
                        _ => "This approach ignores the medium-term redesign option entirely. A purely reactive sourcing approach doesn't address the single-supplier structural vulnerability.".to_string(),
                    }
                } else {
                    match variant {
                        0 => format!("The proposal addresses the core triage question adequately but the supplier diversification timeline needs tightening. Round {round} improvements are evident."),
                        1 => "Reasonable approach. The sourcing split makes sense directionally but the cost estimates would benefit from spot-market sensitivity analysis.".to_string(),
                        2 => "This proposal is competent on logistics but underdeveloped on stakeholder communication. Would be stronger with explicit messaging frameworks for each customer tier.".to_string(),
                        _ => "Solid middle-ground. The financial analysis captures the key trade-offs but could be more precise on the NRE breakeven calculation for the Nordic redesign path.".to_string(),
                    }
                }
            }
            "security" => {
                if is_high {
                    match variant {
                        0 => "This proposal correctly identifies the cross-function re-entrancy as the highest-severity finding. The attack scenario is reproducible and the CEI refactor recommendation is the right fix.".to_string(),
                        1 => "Strong static analysis coverage. The unchecked return value and access control logic error are critical findings that would likely be exploited within hours of mainnet deployment.".to_string(),
                        2 => "The fuzz testing methodology is thorough. 10M iterations with Echidna is industry-standard, and the TVL accounting drift finding via flash loan manipulation is a genuine critical vulnerability.".to_string(),
                        _ => "This synthesis provides the most actionable remediation roadmap. The phased approach (critical errors first, then highs, then re-audit) is practical and the cost estimates for external audit are accurate.".to_string(),
                    }
                } else if is_low {
                    match variant {
                        0 => "The re-entrancy analysis is incomplete. It only covers the direct withdraw() → receive() path but misses the cross-function vector through flashLoan() → onFlashLoan() → withdraw(). The attack surface is larger than presented.".to_string(),
                        1 => "The static analysis report conflates informational findings with critical findings. The `selfdestruct` finding is behind a timelock (missed in analysis). Several 'High' findings are actually Medium after manual verification.".to_string(),
                        2 => "Fuzz testing coverage is insufficient. Only 8 invariants tested — missing share price monotonicity, fee accounting, and governance timelock invariants. The corpus needs manual seed guidance for deeper state exploration.".to_string(),
                        _ => "This proposal significantly underestimates the oracle manipulation risk. Single-block TWAP is exploitable with as little as 10 ETH in flash-loaned capital. The recommended 30-minute TWAP window is still too short — industry standard is 24h.".to_string(),
                    }
                } else {
                    match variant {
                        0 => "The re-entrancy analysis covers the primary vector well but doesn't explore read-only re-entrancy through view functions that feed into other protocols. Worth a follow-up pass.".to_string(),
                        1 => "Adequate static analysis. The Slither findings are valid but Mythril coverage seems limited — recommend running with increased execution depth (256 vs default 128) for the flashLoan path.".to_string(),
                        2 => "The fuzz results are useful but the share price manipulation finding needs more context. The 'donation attack' is mitigated by virtual shares in EIP-4626 — check if the vault already uses this pattern.".to_string(),
                        _ => "Reasonable overall assessment. The remediation timeline is slightly optimistic — external audit firms typically have 4-6 week lead times, not 2-3 weeks as suggested.".to_string(),
                    }
                }
            }
            "quant" => {
                if is_high {
                    match variant {
                        0 => "The macro allocation is well-calibrated for the late-cycle regime. The quality+short-duration overweight is supported by historical backtests. Expected Sharpe of 1.1 is achievable given current yield levels.".to_string(),
                        1 => "Excellent factor analysis. The insight that low-vol is a crowding trap at VIX 14 is sophisticated. The VIX-MOVE divergence observation adds conviction to the quality/momentum tilt.".to_string(),
                        2 => "The tail hedge design is cost-efficient and well-structured. The SPX put spread + VIX call spread combination provides convex payoff at <50bps. Backtest results against 2022 and COVID are compelling.".to_string(),
                        _ => "This proposal provides the most rigorous rebalancing framework. Volatility regime triggers are clearly defined and the drawdown circuit breakers are appropriately conservative for the -8% mandate.".to_string(),
                    }
                } else if is_low {
                    match variant {
                        0 => "The equity allocation is too aggressive for a -8% max drawdown constraint. At 45% equities with current P/E multiples, a 15% correction alone breaches the mandate. Risk budgeting is insufficient.".to_string(),
                        1 => "Flawed factor analysis. Recommending value overweight contradicts the evidence that value spread compression hasn't reached mean-reversion levels. Momentum timing is also questionable at cycle peaks.".to_string(),
                        2 => "The hedge portfolio is over-concentrated in equity vol instruments. No credit or rates diversification means the hedge fails in a non-equity-led crisis (e.g., sovereign debt, banking stress).".to_string(),
                        _ => "This approach proposes calendar-only rebalancing with no volatility or drawdown triggers. This is inadequate for a -8% max drawdown mandate — by the time monthly rebalancing fires, the portfolio could already be in breach.".to_string(),
                    }
                } else {
                    match variant {
                        0 => "The allocation has the right directional bias but the EM exposure may introduce unwanted currency volatility. Consider adding FX hedging for the Asia-ex-China sleeve.".to_string(),
                        1 => "Reasonable factor tilts. The quality overweight is well-supported but the momentum implementation should specify rebalancing frequency to avoid whipsaws in regime transitions.".to_string(),
                        2 => "The hedge sizing is close to optimal but the VIX call spread strikes could be tighter. At current vol levels, the 20/35 spread leaves significant gamma on the table between 25-30.".to_string(),
                        _ => "Solid rebalancing framework. The volatility triggers are well-calibrated but the drawdown re-entry rule (20% per week) may be too aggressive — historical data suggests gradual re-risking over 4-6 weeks performs better.".to_string(),
                    }
                }
            }
            "legal" => {
                if is_high {
                    match variant {
                        0 => "Sharp IP analysis. The joint-ownership clause is correctly identified as the highest-risk provision. The Oddo v. Ries citation strengthens the argument, and the recommended redline language is negotiation-ready.".to_string(),
                        1 => "Thorough liability analysis. The missing data breach carve-out is correctly flagged as non-negotiable for Series C. The 3x TCV super-cap recommendation is market-standard and defensible.".to_string(),
                        2 => "This proposal identifies the data portability gap that others missed — the bankruptcy scenario where our data becomes an asset of the estate is a real risk. Escrow recommendation is prudent.".to_string(),
                        _ => "The non-compete analysis here is the strongest. Correctly identifies the 15-month effective lock-in (term + 12 months) and the enforceability distinction between B2B and employment non-competes. Redline language is precise.".to_string(),
                    }
                } else if is_low {
                    match variant {
                        0 => "The IP risk is underestimated. Recommending a narrow carve-out for 'Platform improvements only' still leaves ambiguity around derivative insights. The redline language has loopholes that CloudMatrix could exploit.".to_string(),
                        1 => "The liability analysis accepts the 12-month fee cap too readily. For a platform processing PII at $2.3M daily volume, a $400K cap is 2-3x below market. The recommended floor should be $2M minimum.".to_string(),
                        2 => "Incomplete termination analysis. The 'standard formats' issue is noted but the proposed fix doesn't specify validation criteria. Without a completeness SLA, the 30-day window is meaningless.".to_string(),
                        _ => "This proposal dismisses the non-compete enforceability too quickly. B2B non-competes in Delaware (likely governing law) are routinely enforced for commercial licenses. This clause requires a harder negotiation stance.".to_string(),
                    }
                } else {
                    match variant {
                        0 => "The IP analysis covers the key risks but the fallback position is too generous. 'Joint ownership of Platform improvements' could be interpreted broadly enough to capture our custom integrations.".to_string(),
                        1 => "Adequate liability review. The carve-out structure is sound but missing the indemnification obligation exclusion — this should be carved out alongside data breach and IP infringement.".to_string(),
                        2 => "This proposal addresses data portability adequately but the escrow recommendation needs more specificity on the escrow agent selection criteria and trigger events.".to_string(),
                        _ => "Competent SLA analysis. The termination-for-chronic-underperformance right is a good addition. Consider also requesting root-cause-analysis obligations for outages exceeding 4 hours.".to_string(),
                    }
                }
            }
            _ => {
                // Generic justifications
                if is_high {
                    match variant {
                        0 => "This proposal presents a well-reasoned approach with clear logic and actionable steps. The analysis is thorough and the recommendations are practical.".to_string(),
                        1 => "Strong proposal. The methodology is sound, key trade-offs are explicitly addressed, and the conclusions follow from the evidence presented.".to_string(),
                        2 => "The analysis demonstrates deep understanding of the problem space. Risk factors are well-identified and the mitigation strategy is comprehensive.".to_string(),
                        _ => "Rigorous work. The step-by-step reasoning is verifiable, edge cases are considered, and the solution accounts for real-world constraints.".to_string(),
                    }
                } else if is_low {
                    match variant {
                        0 => "This proposal contains unsupported assumptions. Several key claims lack evidence, and the risk assessment omits critical failure modes.".to_string(),
                        1 => "Weak analysis. The approach is superficial — it addresses symptoms rather than root causes. Missing quantitative backing for the core recommendations.".to_string(),
                        2 => "This approach overlooks important constraints identified by other proposals. The solution may not be feasible under the stated requirements.".to_string(),
                        _ => "The reasoning has gaps. The conclusion doesn't fully follow from the premises, and alternative approaches aren't adequately considered.".to_string(),
                    }
                } else {
                    match variant {
                        0 => "This proposal provides a reasonable approach but would benefit from more concrete implementation details and explicit success criteria.".to_string(),
                        1 => "Adequate proposal. The core logic is sound but the analysis could be strengthened with quantitative estimates and sensitivity analysis.".to_string(),
                        2 => "The proposed solution is competent but doesn't clearly differentiate itself from the alternatives. The unique value proposition needs sharpening.".to_string(),
                        _ => format!("Solid foundation. The approach is directionally correct but the prioritization framework could be more rigorous. Addressing feedback from round {round} would strengthen it."),
                    }
                }
            }
        }
    }

    /// Map a signed endorsement weight [-100, +100] to a `Stance` enum value (snake_case).
    fn stance_for_weight(weight: f32) -> &'static str {
        if weight > 60.0 {
            "strong_agree"
        } else if weight > 20.0 {
            "agree"
        } else if weight < -60.0 {
            "strong_disagree"
        } else if weight < -20.0 {
            "disagree"
        } else {
            "neutral"
        }
    }

    /// Per-category quality scores correlated with the overall endorsement weight,
    /// with ±5-point deterministic jitter for variety.
    fn generate_category_scores(
        weight: f32,
        evaluator_seed: u32,
        proposal_hash: usize,
    ) -> serde_json::Value {
        let w = weight as i32;
        // Deterministic jitter in [-4, +5] based on seed + proposal hash + offset
        let noise = |off: i32| -> f64 {
            let jitter = ((evaluator_seed as i32)
                .wrapping_add((proposal_hash as i32).wrapping_mul(13).wrapping_add(off)))
            .rem_euclid(10)
                - 4;
            (w + jitter).clamp(-100, 100) as f64
        };
        json!({
            "correctness":      noise(0),
            "completeness":     noise(3),
            "novelty":          noise(7),
            "feasibility":      noise(1),
            "evidence_quality": noise(5),
        })
    }

    /// Generate a deterministic 6-char hex claim ID from proposal identity and slot.
    /// Independent of evaluator so the same claim gets the same ID across evaluators.
    fn simulated_claim_id(proposal_hash: usize, slot: u32) -> String {
        let raw = (proposal_hash as u32)
            .wrapping_mul(97)
            .wrapping_add(slot.wrapping_mul(13));
        format!("{:06x}", raw & 0x00FF_FFFF)
    }

    /// Generate 2 scenario-appropriate `ClaimAssessment` objects.
    ///
    /// High-scored candidates receive `verified` verdicts; low-scored candidates
    /// receive `contested`/`wrong`; medium-scored candidates receive a mix.
    ///
    /// `proposal_hash` is a stable identifier derived from the proposal's
    /// candidate_id (not its prompt position) so claim templates don't change
    /// when proposal ordering changes.
    fn generate_claim_assessments(
        scenario: &str,
        is_high: bool,
        is_low: bool,
        _evaluator_seed: u32,
        proposal_hash: usize,
        round: usize,
    ) -> Vec<serde_json::Value> {
        // Verdicts evolve over rounds to show the "learning arc":
        // Early rounds have more uncertainty; later rounds converge toward
        // verified as evaluators dig deeper into each proposal.
        //
        // | Tier   | Round 1              | Round 2              | Round 3+             |
        // |--------|----------------------|----------------------|----------------------|
        // | high   | verified/unverified  | verified/verified    | verified/verified    |
        // | medium | unverified/unverified| verified/unverified  | verified/verified    |
        // | low    | contested/wrong      | contested/unverified | contested/verified   |
        let (v0, v1) = if is_high {
            if round <= 1 {
                ("verified", "unverified")
            } else {
                ("verified", "verified")
            }
        } else if is_low {
            if round <= 1 {
                ("contested", "wrong")
            } else if round == 2 {
                ("contested", "unverified")
            } else {
                ("contested", "verified")
            }
        } else if round <= 1 {
            ("unverified", "unverified")
        } else if round == 2 {
            ("verified", "unverified")
        } else {
            ("verified", "verified")
        };

        // Each proposal gets UNIQUE claims so the display shows distinct
        // assessments per option. claim_pool[candidate_idx % len] selects the pair.
        // Format: (claim, reason_positive, reason_negative)
        type ClaimEntry = (&'static str, &'static str, &'static str);
        let pool: &[&[ClaimEntry]] = match scenario {
            "supply" => &[
                &[
                    (
                        "SLA penalty avoidance is correctly prioritized over volume maximization",
                        "Penalty avoidance calculus is sound; $1.2 M Tier 1 exposure correctly quantified.",
                        "Priority logic ignores downstream bottleneck propagation in Tier 2 accounts.",
                    ),
                    (
                        "Spot market premium exposure is adequately bounded",
                        "Three-scenario comparison demonstrates premium cap at 12% with contingency.",
                        "No contingency if spot market premium exceeds 20%; worst-case exposure is uncapped.",
                    ),
                ],
                &[
                    (
                        "Dual-sourcing strategy reduces single-vendor concentration risk",
                        "Second-source qualification validates 98.7% pin-compatible yield.",
                        "8-week ShenZhen lead time makes dual-source irrelevant for the Q3 crunch.",
                    ),
                    (
                        "Safety stock buffer of 700 units covers the qualification gap",
                        "Monte Carlo simulation confirms 95th-percentile demand coverage.",
                        "Buffer assumes normal demand; a single large Tier-1 order would exhaust it.",
                    ),
                ],
                &[
                    (
                        "Air-freight cost premium is justified by penalty avoidance ROI",
                        "$340K freight cost vs $1.2M penalty exposure gives 3.5x ROI.",
                        "Air-freight ROI ignores cascading delays to non-priority customers.",
                    ),
                    (
                        "Customer priority matrix correctly ranks by penalty severity",
                        "Matrix weights combine penalty $, activation timeline, and relationship value.",
                        "Relationship-value weighting is subjective and not back-tested.",
                    ),
                ],
                &[
                    (
                        "Warehouse capacity utilization allows for emergency buffer allocation",
                        "Current 72% utilization leaves 1,200 pallet positions for surge storage.",
                        "Utilization metric excludes cold-chain capacity which is already at 94%.",
                    ),
                    (
                        "Customs clearance timeline accounts for regulatory inspection delays",
                        "Pre-clearance arrangement with CBSA reduces border dwell to 48h average.",
                        "Pre-clearance only covers FDA-exempt SKUs; regulated items face 5-day holds.",
                    ),
                ],
                &[
                    (
                        "Demand forecast model captures seasonal ordering patterns",
                        "ARIMA-X model with holiday regressors achieves MAPE 8.2% on 90-day horizon.",
                        "Model trained on pre-pandemic data; post-COVID ordering cadence has shifted 15%.",
                    ),
                    (
                        "Supplier financial health assessment mitigates insolvency risk",
                        "All Tier-1 suppliers pass Altman Z-score >3.0 threshold.",
                        "Z-score is a lagging indicator; two suppliers have negative free cash flow trends.",
                    ),
                ],
            ],
            "security" => &[
                &[
                    (
                        "Re-entrancy vector through withdraw() → receive() is correctly identified",
                        "CEI refactor recommendation is standard; attack scenario is reproducible.",
                        "Analysis only covers the direct vector; cross-function path via flashLoan() is missed.",
                    ),
                    (
                        "30-minute TWAP window adequately mitigates oracle manipulation risk",
                        "Backtest confirms 30-min TWAP requires >$50 M capital to exploit profitably.",
                        "30-minute TWAP is still exploitable; industry standard for high-TVL vaults is 24 h.",
                    ),
                ],
                &[
                    (
                        "Access control on setFeeRecipient() prevents privilege escalation",
                        "onlyOwner modifier with two-step transfer matches OpenZeppelin best practice.",
                        "Missing timelock on fee changes allows instant extraction by compromised key.",
                    ),
                    (
                        "Flash-loan guard on deposit/withdraw prevents sandwich attacks",
                        "Same-block deposit+withdraw prohibition eliminates the primary MEV vector.",
                        "Guard only covers same-block; cross-block sandwich via mempool is still viable.",
                    ),
                ],
                &[
                    (
                        "Formal verification of the invariant k = x * y holds post-swap",
                        "Certora spec passes 100% of 47 verification conditions.",
                        "Spec does not cover rounding in fee accumulation; drift compounds over 10K swaps.",
                    ),
                    (
                        "Emergency pause mechanism meets incident-response SLA requirements",
                        "Multi-sig 2-of-3 pause activates in <15 min based on war-game exercise.",
                        "Pause does not freeze in-flight transactions; a re-entrancy exploit during pause window is unmitigated.",
                    ),
                ],
                &[
                    (
                        "Gas optimization does not introduce integer overflow in token accounting",
                        "Unchecked blocks are bounded by prior require() guards; overflow is impossible.",
                        "Unchecked fee accumulator can silently wrap on high-frequency pools with >10M swaps/day.",
                    ),
                    (
                        "Upgrade proxy storage layout is collision-free across all implementation versions",
                        "EIP-1967 storage slots verified against all three deployed implementations.",
                        "Custom storage slot at keccak256('rewards') collides with implementation v2 mapping.",
                    ),
                ],
                &[
                    (
                        "Rate limiter prevents denial-of-service through repeated small deposits",
                        "Per-address cooldown of 10 blocks limits griefing cost to impractical levels.",
                        "Rate limiter is per-address; Sybil attack with fresh wallets bypasses the guard entirely.",
                    ),
                    (
                        "Slippage protection parameters are within safe bounds for expected pool depth",
                        "0.5% max slippage with $50M TVL keeps MEV extraction below gas cost.",
                        "During low-liquidity hours, 0.5% slippage on $2M TVL enables profitable frontrunning.",
                    ),
                ],
            ],
            "quant" => &[
                &[
                    (
                        "Quality+momentum overweight is appropriate for late-cycle regime",
                        "Factor analysis is supported by historical backtests across three market cycles.",
                        "Low-vol crowding at VIX 14 makes momentum overweight particularly risky.",
                    ),
                    (
                        "SPX put spread + VIX call spread provides sufficient tail protection",
                        "Backtest against 2022 and COVID drawdowns confirms adequate convexity at <50 bps.",
                        "Hedge is over-concentrated in equity vol; fails in credit-led or rates-led crises.",
                    ),
                ],
                &[
                    (
                        "Mean-reversion signal has sufficient decay rate for weekly rebalancing",
                        "Half-life of 4.2 days aligns with weekly rebalance; Ornstein-Uhlenbeck fit R²=0.91.",
                        "Regime-switching test shows signal decay collapses to noise in trending markets.",
                    ),
                    (
                        "Position sizing via Kelly criterion keeps drawdown within risk budget",
                        "Half-Kelly sizing limits worst 5% drawdown to 8.3%, within 10% mandate.",
                        "Kelly assumes IID returns; serial correlation in VIX regimes amplifies tail losses.",
                    ),
                ],
                &[
                    (
                        "Cross-asset correlation matrix is stable enough for portfolio construction",
                        "Rolling 60-day correlation eigenvalues show <15% regime-shift instability.",
                        "March 2020 correlation breakdown lasted 23 days — longer than lookback window.",
                    ),
                    (
                        "Volatility targeting reduces exposure proportionally in high-vol regimes",
                        "Vol-target backtest shows 40% drawdown reduction with only 12% return drag.",
                        "Pro-cyclical deleveraging in flash crashes locks in losses at the worst moment.",
                    ),
                ],
                &[
                    (
                        "Liquidity-adjusted VaR correctly accounts for bid-ask spread widening",
                        "LVaR at 99th percentile captures spread widening observed in 2020 and 2022.",
                        "LVaR model uses static spread multiplier; actual widening was 5x model assumption.",
                    ),
                    (
                        "Sector rotation timing signal has predictive power beyond random walk",
                        "Out-of-sample Sharpe of 0.42 is statistically significant over 15-year backtest.",
                        "Signal backtest includes look-ahead bias in sector classification changes.",
                    ),
                ],
                &[
                    (
                        "Leverage constraints prevent margin call cascades in stress scenarios",
                        "Maximum 1.5x gross leverage keeps margin buffer above 40% in 3-sigma events.",
                        "Constraints assume orderly deleveraging; fire-sale feedback loops are unmodeled.",
                    ),
                    (
                        "Transaction cost model accurately estimates market impact for target AUM",
                        "Square-root impact model calibrated to 6 months of execution data, R²=0.87.",
                        "Model breaks down for concentrated positions exceeding 5% of daily volume.",
                    ),
                ],
            ],
            "legal" => &[
                &[
                    (
                        "Joint IP ownership clause is the highest-risk provision",
                        "Oddo v. Ries precedent supports this ranking; joint ownership creates veto rights.",
                        "Liability cap gap ($400 K vs $2 M market) presents equal or greater financial risk.",
                    ),
                    (
                        "12-month fee cap is below market rate for this PII data volume",
                        "Market survey confirms $2 M floor for platforms processing $2.3 M daily PII volume.",
                        "Cap analysis lacks comparable benchmarks; the market-rate claim is unsubstantiated.",
                    ),
                ],
                &[
                    (
                        "Non-compete radius of 50 miles is enforceable in target jurisdiction",
                        "State precedent upholds 50-mile radius for specialized technical services.",
                        "Recent FTC guidance trends toward non-compete prohibition; enforceability risk is high.",
                    ),
                    (
                        "Data retention clause complies with GDPR Article 17 requirements",
                        "30-day deletion window with automated purge satisfies right-to-erasure timeline.",
                        "Clause is silent on backup retention; shadow copies may violate erasure obligation.",
                    ),
                ],
                &[
                    (
                        "Indemnification cap adequately covers foreseeable third-party claims",
                        "Cap at 2x annual contract value covers 99th-percentile claim based on industry data.",
                        "Cap excludes willful misconduct carve-out; a single data breach could exceed 5x cap.",
                    ),
                    (
                        "Termination-for-convenience clause protects against vendor lock-in",
                        "90-day notice with data portability obligation ensures clean exit path.",
                        "Migration assistance fee of $500K/month during transition creates de facto lock-in.",
                    ),
                ],
                &[
                    (
                        "Force majeure clause adequately allocates pandemic-related performance risk",
                        "Clause explicitly enumerates epidemic/pandemic with 90-day cure period.",
                        "Cure period starts from notice, not event onset; delayed notice shifts risk to buyer.",
                    ),
                    (
                        "Governing law selection optimizes enforcement across target jurisdictions",
                        "Delaware choice-of-law with NY arbitration is standard for cross-border SaaS.",
                        "Arbitration clause lacks interim-relief carve-out; injunctive relief requires court filing.",
                    ),
                ],
                &[
                    (
                        "Assignment clause prevents change-of-control exploitation",
                        "Anti-assignment with consent-not-unreasonably-withheld is market standard.",
                        "Reverse triangular merger is not covered; acquirer inherits rights without consent.",
                    ),
                    (
                        "Audit rights clause provides adequate compliance verification",
                        "Annual audit with 30-day notice and SOC 2 Type II acceptance is reasonable.",
                        "Audit scope excludes subprocessor facilities; 60% of PII processing is outsourced.",
                    ),
                ],
            ],
            _ => &[
                &[
                    (
                        "Core methodology is sound and the conclusions are well-evidenced",
                        "Step-by-step reasoning is verifiable and edge cases are explicitly considered.",
                        "Key assumptions lack quantitative backing; core claims are not independently verifiable.",
                    ),
                    (
                        "Risk mitigation strategy adequately addresses the identified failure modes",
                        "Mitigation strategy covers the identified failure modes comprehensively.",
                        "Failure mode analysis is incomplete; several critical paths are not addressed.",
                    ),
                ],
                &[
                    (
                        "Proposed timeline is realistic given resource constraints",
                        "Task decomposition accounts for dependencies and includes 20% buffer.",
                        "Critical-path analysis ignores handoff delays between teams.",
                    ),
                    (
                        "Fallback plan provides adequate coverage if primary approach fails",
                        "Fallback activates within 48h and recovers 80% of primary functionality.",
                        "Fallback has not been tested under load; failure cascade is plausible.",
                    ),
                ],
                &[
                    (
                        "Cost-benefit analysis supports the recommended approach",
                        "ROI exceeds threshold at 2.3x over 18 months with conservative assumptions.",
                        "Benefit estimates rely on a single customer survey with 12% response rate.",
                    ),
                    (
                        "Scalability assessment accounts for projected growth trajectory",
                        "Architecture handles 10x current load with horizontal scaling.",
                        "Database bottleneck at 5x load is not addressed in the scaling plan.",
                    ),
                ],
                &[
                    (
                        "Data quality is sufficient to support the stated conclusions",
                        "Dataset covers 3 years with <2% missing values after imputation.",
                        "Survivorship bias in the dataset excludes 30% of failed cases from analysis.",
                    ),
                    (
                        "Stakeholder impact analysis captures all affected parties",
                        "RACI matrix identifies 12 stakeholder groups with clear escalation paths.",
                        "End-user group is absent from the RACI; their workflow disruption is unquantified.",
                    ),
                ],
                &[
                    (
                        "Implementation complexity is proportional to expected benefit",
                        "Three-phase rollout limits blast radius; each phase is independently reversible.",
                        "Phase dependencies create a critical chain; phase-2 delay blocks all downstream work.",
                    ),
                    (
                        "Success metrics are measurable and aligned with stated objectives",
                        "KPIs have baseline measurements and 90-day targets with statistical significance.",
                        "Leading indicators are lagging proxies; earliest meaningful signal is at 6 months.",
                    ),
                ],
            ],
        };

        let pair = pool[proposal_hash % pool.len()];
        let (c0, r0h, r0l) = pair[0];
        let (c1, r1h, r1l) = pair[1];

        // Reason selection follows the verdict: positive verdicts (verified)
        // get supportive reasons; negative verdicts get critical reasons.
        let r0 = match v0 {
            "verified" => r0h,
            _ => r0l, // contested, wrong, unverified
        };
        let r1 = match v1 {
            "verified" => r1h,
            _ => r1l, // contested, wrong, unverified
        };

        vec![
            json!({
                "claim_id": Self::simulated_claim_id(proposal_hash, 0),
                "claim":    c0,
                "verdict":  v0,
                "reason":   r0,
            }),
            json!({
                "claim_id": Self::simulated_claim_id(proposal_hash, 1),
                "claim":    c1,
                "verdict":  v1,
                "reason":   r1,
            }),
        ]
    }

    /// Generate `DisagreementPoint` objects whose count tapers as rounds progress,
    /// mirroring the convergence arc visible in the overall score spread.
    ///
    /// | candidate tier | round 1 | round 2 | round 3+ |
    /// |----------------|---------|---------|----------|
    /// | low            |    2    |    2    |    1     |
    /// | medium         |    1    |    0    |    0     |
    /// | high           |    0    |    0    |    0     |
    ///
    /// Each scenario provides **two distinct** disagreement items so that the two
    /// round-1 entries cover orthogonal aspects of the dispute.
    fn generate_disagreements(
        scenario: &str,
        is_low: bool,
        is_medium: bool,
        round: usize,
        proposal_hash: usize,
    ) -> Vec<serde_json::Value> {
        // How many disagreement points to emit this round.
        let max_points: usize = if is_low {
            if round <= 2 { 2 } else { 1 }
        } else if is_medium && round == 1 {
            1
        } else {
            return vec![];
        };

        // Confidence decreases as convergence progresses.
        let confidence_primary = if is_medium {
            "low"
        } else if round == 1 {
            "high"
        } else {
            "medium"
        };
        // Secondary item (only present in rounds 1–2 for low) is one step softer.
        let confidence_secondary = match confidence_primary {
            "high" => "medium",
            _ => "low",
        };

        // Two orthogonal disagreement items per scenario.
        // Item 0: primary / most consequential dispute.
        // Item 1: secondary concern (round 1–2 only for low candidates).
        let items: &[(&str, &str); 2] = match scenario {
            "supply" => &[
                (
                    "The priority order correctly balances SLA avoidance against volume impact",
                    "The algorithm ignores downstream bottleneck propagation in Tier 2 accounts, creating hidden SLA exposure.",
                ),
                (
                    "The dual-sourcing 6-week procurement timeline is achievable",
                    "ShenZhen Semi's confirmed 8-week lead time makes the 6-week target unrealistic without air-freight; ocean disruptions could add 2–3 weeks on top.",
                ),
            ],
            "security" => &[
                (
                    "The re-entrancy analysis covers all exploit vectors comprehensively",
                    "Cross-function re-entrancy via flashLoan() → onFlashLoan() → withdraw() is unaddressed; this is the higher-severity path.",
                ),
                (
                    "The 30-minute TWAP window adequately mitigates oracle manipulation risk",
                    "30-minute TWAP is still exploitable with >$50 M capital; industry standard for high-TVL vaults is a 24-hour window.",
                ),
            ],
            "quant" => &[
                (
                    "Momentum overweight is appropriate in the current late-cycle regime",
                    "Low-vol crowding at VIX 14 amplifies rather than hedges tail risk; the overweight is contra-indicated at this entry.",
                ),
                (
                    "SPX put spread + VIX call spread provides sufficient tail protection",
                    "Hedge is over-concentrated in equity vol; fails in credit-led or rates-led crises where VIX underperforms HY spreads.",
                ),
            ],
            "legal" => &[
                (
                    "The joint IP ownership clause is correctly ranked as the highest-priority risk",
                    "The liability cap gap ($400 K vs. $2 M market rate) creates greater financial exposure given the $2.3 M daily PII data volume.",
                ),
                (
                    "The recommended 30-day remediation timeline is achievable",
                    "Novel IP provisions require external counsel review; the 30-day estimate underestimates complexity by at least 2×.",
                ),
            ],
            _ => &[
                (
                    "The core assumptions underlying this analysis are well-supported",
                    "Key claims rely on unverified assumptions; quantitative backing for the core recommendations is insufficient.",
                ),
                (
                    "The proposed solution handles all identified failure modes",
                    "Two critical edge cases remain unaddressed; the mitigation strategy does not cover failure cascades under concurrent load.",
                ),
            ],
        };

        (0..max_points)
            .map(|slot| {
                let (proposal_claims, evaluator_position) = items[slot];
                // Use slots 2+ to avoid colliding with claim_assessments which occupies slots 0–1.
                let claim_id = Self::simulated_claim_id(proposal_hash, (slot + 2) as u32);
                let confidence = if slot == 0 {
                    confidence_primary
                } else {
                    confidence_secondary
                };
                json!({
                    "claim_id":           claim_id,
                    "proposal_claims":    proposal_claims,
                    "evaluator_position": evaluator_position,
                    "confidence":         confidence,
                })
            })
            .collect()
    }

    /// Extract candidate IDs from the message text.
    /// The evaluator prompt contains `<candidate id="Fast_1">` XML tags.
    fn extract_candidate_ids(
        messages: &[async_openai::types::ChatCompletionRequestMessage],
    ) -> Vec<String> {
        let mut ids = Vec::new();
        for msg in messages {
            let text = match msg {
                async_openai::types::ChatCompletionRequestMessage::User(u) => {
                    if let async_openai::types::ChatCompletionRequestUserMessageContent::Text(t) =
                        &u.content
                    {
                        t.clone()
                    } else {
                        continue;
                    }
                }
                async_openai::types::ChatCompletionRequestMessage::System(s) => {
                    if let async_openai::types::ChatCompletionRequestSystemMessageContent::Text(t) =
                        &s.content
                    {
                        t.clone()
                    } else {
                        continue;
                    }
                }
                _ => continue,
            };

            // Look for <candidate id="..."> pattern
            let mut search_from = 0;
            while let Some(start) = text[search_from..].find("<candidate id=\"") {
                let abs_start = search_from + start + "<candidate id=\"".len();
                if let Some(end) = text[abs_start..].find('"') {
                    let id = text[abs_start..abs_start + end].to_string();
                    // Skip empty IDs, placeholder patterns (e.g., "..."), and
                    // IDs that don't look like real candidate identifiers.
                    let is_valid = !id.is_empty()
                        && !id.chars().all(|c| c == '.')
                        && id.chars().any(|c| c.is_alphanumeric());
                    if is_valid && !ids.contains(&id) {
                        ids.push(id);
                    }
                    search_from = abs_start + end;
                } else {
                    break;
                }
            }
        }
        ids
    }
}

#[async_trait]
impl AiModel for SimulatedModel {
    async fn chat_completion(
        &self,
        agent: &AgentConfig,
        request: RequestConfig,
    ) -> Result<ChatCompletionResult, LlmError> {
        // Stagger responses so they arrive at different times.
        // Modulo by STAGGER_WINDOW so later rounds don't accumulate unbounded delay.
        // With up to 5 agents × 2 phases = 10 calls/round, the stagger wraps every 10 calls.
        let call_idx = self.shared.call_counter.fetch_add(1, Ordering::Relaxed);
        let stagger = Duration::from_millis((call_idx as u64 % STAGGER_WINDOW) * STAGGER_MS);
        let total_delay = self.latency + stagger;
        if !total_delay.is_zero() {
            sleep(total_delay).await;
        }

        let mut current_round: usize = 1;
        let mut total_rounds: usize = 0;
        for msg in &request.messages {
            if let async_openai::types::ChatCompletionRequestMessage::System(sys) = msg
                && let async_openai::types::ChatCompletionRequestSystemMessageContent::Text(text) =
                    &sys.content
                && let Some(start) = text.find("Round ")
            {
                // Extract "Round X of Y" — gives us both current_round and total_rounds.
                let suffix = &text[start + 6..];
                let digits: String = suffix.chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(r) = digits.parse::<usize>() {
                    current_round = r;
                }
                // Try to parse " of Y"
                if let Some(of_pos) = suffix.find(" of ") {
                    let after_of = &suffix[of_pos + 4..];
                    let total_digits: String = after_of
                        .chars()
                        .take_while(|c| c.is_ascii_digit())
                        .collect();
                    if let Ok(t) = total_digits.parse::<usize>() {
                        total_rounds = t;
                    }
                }
            }
        }

        // Reset the dm_fired flag at the start of each new deliberation so
        // the dm_user tool call fires once per job (not once per server lifetime).
        if current_round == 1 {
            self.shared.dm_fired.store(false, Ordering::SeqCst);
        }

        // --- Normal content response ---
        let last_msg = request
            .messages
            .last()
            .map(|m| match m {
                async_openai::types::ChatCompletionRequestMessage::User(u) => {
                    if let async_openai::types::ChatCompletionRequestUserMessageContent::Text(t) =
                        &u.content
                    {
                        t.clone()
                    } else {
                        String::new()
                    }
                }
                _ => String::new(),
            })
            .unwrap_or_default()
            .to_lowercase();

        // Extract the user's task description from the prompt for context-aware proposals.
        let task_desc = Self::extract_task_description(&request.messages);

        // --- dm_user tool call at mid-deliberation, exactly once per job ---
        // Fire the DM at the midpoint round: for 3 rounds → round 2, for 5 → round 3.
        // The AtomicBool `dm_fired` ensures exactly one agent wins the race;
        // all other agents in the same round proceed normally.
        let is_proposal = !last_msg.contains("submit_batch_evaluation");
        let has_dm_user = request
            .tools
            .as_ref()
            .is_some_and(|tools| tools.iter().any(|t| t.function.name.contains("dm_user")));
        let mid_round = if total_rounds >= 2 {
            (total_rounds / 2) + 1
        } else {
            2 // fallback if we couldn't parse total_rounds
        };
        let is_mid_deliberation = current_round == mid_round;
        // Exactly-once guard: first agent to CAS false→true wins.
        let dm_not_yet_fired = is_mid_deliberation
            && self
                .shared
                .dm_fired
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::Relaxed)
                .is_ok();

        if dm_not_yet_fired && is_proposal && has_dm_user {
            // Return a dm_user tool call instead of a content response.
            let scenario = Self::detect_scenario(task_desc.as_deref().unwrap_or(""));
            let dm_msg = Self::dm_user_message(scenario);
            let dm_tool_name = request
                .tools
                .as_ref()
                .and_then(|tools| {
                    tools
                        .iter()
                        .find(|t| t.function.name.contains("dm_user"))
                        .map(|t| t.function.name.clone())
                })
                .unwrap_or_else(|| "user_dm_user".to_string());

            let response = CreateChatCompletionResponse {
                id: format!("sim-{}", uuid::Uuid::new_v4()),
                object: "chat.completion".to_string(),
                created: chrono::Utc::now().timestamp() as u32,
                model: self.model_name.clone(),
                choices: vec![ChatChoice {
                    index: 0,
                    message: ChatCompletionResponseMessage {
                        role: Role::Assistant,
                        content: None,
                        tool_calls: Some(vec![ChatCompletionMessageToolCall {
                            id: format!("call-dm-{}", uuid::Uuid::new_v4()),
                            r#type: ChatCompletionToolType::Function,
                            function: FunctionCall {
                                name: dm_tool_name,
                                arguments: json!({ "message": dm_msg }).to_string(),
                            },
                        }]),
                        #[allow(deprecated)]
                        function_call: None,
                        refusal: None,
                        audio: None,
                    },
                    finish_reason: Some(async_openai::types::FinishReason::ToolCalls),
                    logprobs: None,
                }],
                usage: Some(CompletionUsage {
                    prompt_tokens: 100,
                    completion_tokens: 30,
                    total_tokens: 130,
                    prompt_tokens_details: None,
                    completion_tokens_details: None,
                }),
                service_tier: None,
                system_fingerprint: None,
            };
            return Ok(ChatCompletionResult {
                response,
                raw_request: String::new(),
                timing: TimingMetadata {
                    ttft_ms: None,
                    generation_ms: None,
                },
                provider_backend: None,
                shrink_info: None,
            });
        }

        // Detect title-generation requests from the moderator fast-path.
        // OpenCode sends this exact system prompt when it asks for a
        // title. Matching on the exact string avoids false positives
        // from deliberation tasks whose task description happens to
        // mention "title" or "summary".
        const TITLE_SYSTEM_PROMPT: &str =
            "You are a title generator. You output ONLY a thread title. Nothing else.";
        let is_title_request = request.messages.iter().any(|m| {
            if let async_openai::types::ChatCompletionRequestMessage::System(s) = m
                && let async_openai::types::ChatCompletionRequestSystemMessageContent::Text(t) =
                    &s.content
            {
                t.trim() == TITLE_SYSTEM_PROMPT
            } else {
                false
            }
        });

        let content = if is_title_request {
            // Short simulated title (≤ 5 words) — demonstrates the moderator
            // fast-path returning a concise result suitable for session
            // titling. Scenario-aware so the simulation output is
            // meaningful rather than a generic placeholder.
            let scenario = Self::detect_scenario(task_desc.as_deref().unwrap_or(""));
            match scenario {
                "supply" => "Supply Chain Risk Assessment".to_string(),
                "security" => "Smart Contract Security Audit".to_string(),
                "quant" => "Portfolio Allocation Strategy Review".to_string(),
                "legal" => "Contract Clause Analysis Review".to_string(),
                _ => "Multi-Agent Deliberation Summary".to_string(),
            }
        } else if last_msg.contains("submit_batch_evaluation") {
            // Parse candidate IDs from prompt (<candidate id="...">) — works with
            // both real and anonymized IDs since orchestrator matches by these same IDs.
            let candidate_ids = Self::extract_candidate_ids(&request.messages);
            debug!(
                model = %self.model_name,
                round = current_round,
                candidate_ids = ?candidate_ids,
                "SimulatedModel generating evaluation"
            );
            self.generate_fake_evaluation(
                &agent.name,
                current_round,
                &candidate_ids,
                task_desc.as_deref(),
            )
        } else {
            // Both explicit submit_proposal and generic prompts produce scenario-aware proposals.
            self.generate_fake_proposal(&agent.name, current_round, task_desc.as_deref())
        };

        let response = CreateChatCompletionResponse {
            id: format!("sim-{}", uuid::Uuid::new_v4()),
            object: "chat.completion".to_string(),
            created: chrono::Utc::now().timestamp() as u32,
            model: self.model_name.clone(),
            choices: vec![ChatChoice {
                index: 0,
                message: ChatCompletionResponseMessage {
                    role: Role::Assistant,
                    content: Some(content.clone()),
                    tool_calls: None,
                    #[allow(deprecated)]
                    function_call: None,
                    refusal: None,
                    audio: None,
                },
                finish_reason: Some(async_openai::types::FinishReason::Stop),
                logprobs: None,
            }],
            usage: Some(CompletionUsage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                prompt_tokens_details: None,
                completion_tokens_details: None,
            }),
            service_tier: None,
            system_fingerprint: None,
        };

        Ok(ChatCompletionResult {
            response,
            raw_request: content,
            timing: TimingMetadata {
                ttft_ms: None,
                generation_ms: None,
            },
            provider_backend: None,
            shrink_info: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::config::AgentConfig;
    use async_openai::types::{
        ChatCompletionRequestMessage, ChatCompletionRequestSystemMessage,
        ChatCompletionRequestSystemMessageContent, ChatCompletionRequestUserMessage,
        ChatCompletionRequestUserMessageContent,
    };

    fn create_test_agent() -> AgentConfig {
        AgentConfig {
            name: "test_agent".to_string(),
            provider_id: "simulated".to_string(),
            model_name: "simulated".to_string(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_simulated_model_proposal() {
        let model = SimulatedModel::new("test-model".to_string(), 0);
        let agent = create_test_agent();

        let request = RequestConfig {
            messages: vec![ChatCompletionRequestMessage::User(
                ChatCompletionRequestUserMessage {
                    content: ChatCompletionRequestUserMessageContent::Text(
                        "Please submit_proposal for the task".to_string(),
                    ),
                    ..Default::default()
                },
            )],
            tools: None,
            tool_choice: None,
            presence_penalty: None,
            service_tier: None,
        };

        let result = model.chat_completion(&agent, request).await.unwrap();
        let response = &result.response;
        let content = &result.raw_request;

        assert_eq!(response.choices.len(), 1);
        assert!(content.contains("thought_process"));
        assert!(content.contains("solution_content"));
        // Generic proposal includes agent name in the content
        assert!(content.contains("test_agent"));
    }

    #[tokio::test]
    async fn test_simulated_model_evaluation() {
        let model = SimulatedModel::new("test-model".to_string(), 0);
        let agent = create_test_agent();

        let request = RequestConfig {
            messages: vec![ChatCompletionRequestMessage::User(
                ChatCompletionRequestUserMessage {
                    content: ChatCompletionRequestUserMessageContent::Text(
                        "Please submit_batch_evaluation for candidates".to_string(),
                    ),
                    ..Default::default()
                },
            )],
            tools: None,
            tool_choice: None,
            presence_penalty: None,
            service_tier: None,
        };

        let result = model.chat_completion(&agent, request).await.unwrap();
        let response = &result.response;
        let content = &result.raw_request;

        assert_eq!(response.choices.len(), 1);
        assert!(content.contains("evaluations"));
        assert!(content.contains("endorsement_weight"));
        assert!(content.contains("candidate_id"));
    }

    #[tokio::test]
    async fn test_simulated_model_round_extraction() {
        let model = SimulatedModel::new("test-model".to_string(), 0);
        let agent = create_test_agent();

        // Test that round number is extracted from system message
        let request = RequestConfig {
            messages: vec![
                ChatCompletionRequestMessage::System(ChatCompletionRequestSystemMessage {
                    content: ChatCompletionRequestSystemMessageContent::Text(
                        "Round 3 of 5. Please submit_batch_evaluation".to_string(),
                    ),
                    ..Default::default()
                }),
                ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
                    content: ChatCompletionRequestUserMessageContent::Text(
                        "submit_batch_evaluation".to_string(),
                    ),
                    ..Default::default()
                }),
            ],
            tools: None,
            tool_choice: None,
            presence_penalty: None,
            service_tier: None,
        };

        let result = model.chat_completion(&agent, request).await.unwrap();
        let content = &result.raw_request;

        // Round 3 fallback (no candidates) → 60 + 3*5 = 75
        assert!(content.contains("75"));
    }

    #[tokio::test]
    async fn test_simulated_model_with_latency() {
        let model = SimulatedModel::new("test-model".to_string(), 50);
        let agent = create_test_agent();

        let request = RequestConfig {
            messages: vec![ChatCompletionRequestMessage::User(
                ChatCompletionRequestUserMessage {
                    content: ChatCompletionRequestUserMessageContent::Text(
                        "Generic prompt".to_string(),
                    ),
                    ..Default::default()
                },
            )],
            tools: None,
            tool_choice: None,
            presence_penalty: None,
            service_tier: None,
        };

        let start = std::time::Instant::now();
        let _result = model.chat_completion(&agent, request).await.unwrap();
        let _content = &_result.raw_request;
        let elapsed = start.elapsed();

        // Should have at least 50ms latency
        assert!(elapsed.as_millis() >= 45, "Expected at least 50ms latency");
    }

    #[tokio::test]
    async fn test_simulated_model_generic_response() {
        let model = SimulatedModel::new("test-model".to_string(), 0);
        let agent = create_test_agent();

        // A request without submit_proposal or submit_batch_evaluation keywords
        let request = RequestConfig {
            messages: vec![ChatCompletionRequestMessage::User(
                ChatCompletionRequestUserMessage {
                    content: ChatCompletionRequestUserMessageContent::Text(
                        "Just a general question".to_string(),
                    ),
                    ..Default::default()
                },
            )],
            tools: None,
            tool_choice: None,
            presence_penalty: None,
            service_tier: None,
        };

        let result = model.chat_completion(&agent, request).await.unwrap();
        let response = &result.response;
        let content = &result.raw_request;

        // Should default to proposal format (generic, no task context)
        assert!(content.contains("thought_process"));
        assert!(content.contains("solution_content"));
        assert!(content.contains("test_agent"));
        assert!(response.usage.is_some());
        assert_eq!(response.usage.as_ref().unwrap().total_tokens, 150);
    }

    #[tokio::test]
    async fn test_simulated_model_response_structure() {
        let model = SimulatedModel::new("test-model".to_string(), 0);
        let agent = create_test_agent();

        let request = RequestConfig {
            messages: vec![],
            tools: None,
            tool_choice: None,
            presence_penalty: None,
            service_tier: None,
        };

        let result = model.chat_completion(&agent, request).await.unwrap();
        let response = &result.response;

        // Verify response structure
        assert!(response.id.starts_with("sim-"));
        assert_eq!(response.object, "chat.completion");
        assert_eq!(response.model, "test-model");
        assert!(response.choices[0].finish_reason.is_some());
    }

    #[tokio::test]
    async fn test_simulated_model_shared_stagger() {
        // Two models sharing the same state — verify stagger counter increments.
        let shared = Arc::new(SimulatedShared::new());
        let model_a = SimulatedModel::new_shared("test-model".to_string(), 0, shared.clone());
        let model_b = SimulatedModel::new_shared("test-model".to_string(), 0, shared.clone());

        let agent = create_test_agent();
        let make_request = || RequestConfig {
            messages: vec![ChatCompletionRequestMessage::User(
                ChatCompletionRequestUserMessage {
                    content: ChatCompletionRequestUserMessageContent::Text(
                        "Please submit_proposal".to_string(),
                    ),
                    ..Default::default()
                },
            )],
            tools: None,
            tool_choice: None,
            presence_penalty: None,
            service_tier: None,
        };

        let _ = model_a
            .chat_completion(&agent, make_request())
            .await
            .unwrap();
        let _ = model_b
            .chat_completion(&agent, make_request())
            .await
            .unwrap();

        // Both calls should have incremented the counter
        assert_eq!(shared.call_counter.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn test_detect_scenario() {
        assert_eq!(
            SimulatedModel::detect_scenario("supply chain disruption"),
            "supply"
        );
        assert_eq!(
            SimulatedModel::detect_scenario("Our semiconductor supplier halted production"),
            "supply"
        );
        assert_eq!(
            SimulatedModel::detect_scenario("Optimize Q3 procurement cycle"),
            "supply"
        );
        assert_eq!(
            SimulatedModel::detect_scenario(
                "Analyze vault contract for re-entrancy vulnerabilities"
            ),
            "security"
        );
        assert_eq!(
            SimulatedModel::detect_scenario("Smart contract security audit"),
            "security"
        );
        assert_eq!(
            SimulatedModel::detect_scenario("Fuzz testing the flash loan function"),
            "security"
        );
        assert_eq!(
            SimulatedModel::detect_scenario("Portfolio allocation for $50M fund"),
            "quant"
        );
        assert_eq!(
            SimulatedModel::detect_scenario("Evaluate BTC/ETH momentum crossover trading"),
            "quant"
        );
        assert_eq!(
            SimulatedModel::detect_scenario("Tail risk hedge with max drawdown -8%"),
            "quant"
        );
        assert_eq!(
            SimulatedModel::detect_scenario("Review SaaS license agreement clause"),
            "legal"
        );
        assert_eq!(
            SimulatedModel::detect_scenario("IP ownership and liability cap"),
            "legal"
        );
        assert_eq!(
            SimulatedModel::detect_scenario("Explain quantum entanglement"),
            "generic"
        );
    }

    #[test]
    fn test_detect_scenario_lengthy_priority() {
        // Long-form intent must take precedence over domain keywords
        // so prompts like "security audit in detail" exercise the
        // UI-stress path (long modal body) rather than the canned
        // security scenario.
        assert_eq!(
            SimulatedModel::detect_scenario("security audit in detail"),
            "lengthy"
        );
        assert_eq!(
            SimulatedModel::detect_scenario("comprehensive supply chain review"),
            "lengthy"
        );
        assert_eq!(
            SimulatedModel::detect_scenario("Write an essay on portfolio allocation"),
            "lengthy"
        );
        assert_eq!(
            SimulatedModel::detect_scenario("long-form contract analysis"),
            "lengthy"
        );
        assert_eq!(
            SimulatedModel::detect_scenario("deep dive on reentrancy"),
            "lengthy"
        );
        assert_eq!(
            SimulatedModel::detect_scenario("explain thoroughly the TC-4090 shortage"),
            "lengthy"
        );
    }

    #[test]
    fn test_lengthy_proposal_has_substantial_body() {
        // Each of the four rotating perspectives must yield a long
        // body (>2000 chars) — the whole point of this scenario is
        // to stress the option modal's scroll path and the carousel
        // card's line-clamp against realistic content length.
        for perspective in 0..4 {
            let (thought, body) = SimulatedModel::lengthy_proposal("GEMMA4", 1, perspective);
            assert!(
                body.chars().count() > 2000,
                "perspective {} body must exceed 2000 chars, got {}",
                perspective,
                body.chars().count()
            );
            assert!(
                thought.chars().count() > 100,
                "thought must describe the angle, got {} chars",
                thought.chars().count()
            );
        }
    }

    #[test]
    fn test_dm_user_message_lengthy_variant() {
        // The lengthy scenario has its own DM question so a mid-
        // deliberation nudge in long-form runs doesn't fall back to
        // the generic "any additional context?" line.
        let msg = SimulatedModel::dm_user_message("lengthy");
        assert!(
            msg.to_lowercase().contains("depth") || msg.to_lowercase().contains("detail"),
            "lengthy DM message should ask about depth/detail, got: {msg}"
        );
        assert_ne!(
            msg,
            SimulatedModel::dm_user_message("generic"),
            "lengthy DM message must differ from the generic fallback"
        );
    }

    #[test]
    fn test_extract_task_description() {
        let messages = vec![ChatCompletionRequestMessage::User(
            ChatCompletionRequestUserMessage {
                content: ChatCompletionRequestUserMessageContent::Text(
                    "<task>\"Analyze our supply chain risks\"</task>\n\nINSTRUCTIONS: Solve the task.".to_string(),
                ),
                ..Default::default()
            },
        )];
        let task = SimulatedModel::extract_task_description(&messages);
        assert_eq!(task, Some("Analyze our supply chain risks".to_string()));
    }

    #[test]
    fn test_extract_task_description_no_quotes() {
        let messages = vec![ChatCompletionRequestMessage::User(
            ChatCompletionRequestUserMessage {
                content: ChatCompletionRequestUserMessageContent::Text(
                    "<task>Some task without quotes</task>".to_string(),
                ),
                ..Default::default()
            },
        )];
        let task = SimulatedModel::extract_task_description(&messages);
        assert_eq!(task, Some("Some task without quotes".to_string()));
    }

    #[test]
    fn test_extract_task_description_missing() {
        let messages = vec![ChatCompletionRequestMessage::User(
            ChatCompletionRequestUserMessage {
                content: ChatCompletionRequestUserMessageContent::Text(
                    "No task tag here".to_string(),
                ),
                ..Default::default()
            },
        )];
        let task = SimulatedModel::extract_task_description(&messages);
        assert_eq!(task, None);
    }

    #[tokio::test]
    async fn test_scenario_aware_proposal_supply() {
        let model = SimulatedModel::new("test-model".to_string(), 0);
        let agent = create_test_agent();

        let request = RequestConfig {
            messages: vec![ChatCompletionRequestMessage::User(
                ChatCompletionRequestUserMessage {
                    content: ChatCompletionRequestUserMessageContent::Text(
                        "<task>\"Our supply chain supplier halted production\"</task>\n\nPlease submit_proposal".to_string(),
                    ),
                    ..Default::default()
                },
            )],
            tools: None,
            tool_choice: None,
            presence_penalty: None,
            service_tier: None,
        };

        let result = model.chat_completion(&agent, request).await.unwrap();
        let content = &result.raw_request;
        // Should produce a supply chain scenario proposal, not generic
        let parsed: serde_json::Value = serde_json::from_str(content).unwrap();
        let solution = parsed["solution_content"].as_str().unwrap();
        // Supply chain proposals mention inventory, sourcing, or financial impact
        assert!(
            solution.contains("Inventory")
                || solution.contains("Sourcing")
                || solution.contains("Supply")
                || solution.contains("Financial")
                || solution.contains("Communication"),
            "Expected supply chain content, got: {}",
            &solution[..100.min(solution.len())]
        );
    }

    #[tokio::test]
    async fn test_different_agents_get_different_perspectives() {
        let model = SimulatedModel::new("test-model".to_string(), 0);

        let make_request = || {
            RequestConfig {
            messages: vec![ChatCompletionRequestMessage::User(
                ChatCompletionRequestUserMessage {
                    content: ChatCompletionRequestUserMessageContent::Text(
                        "<task>\"Review the SaaS license agreement\"</task>\n\nPlease submit_proposal".to_string(),
                    ),
                    ..Default::default()
                },
            )],
            tools: None,
            tool_choice: None,
            presence_penalty: None,
            service_tier: None,
        }
        };

        // Two different agent names should produce different perspectives
        let agent_a = AgentConfig {
            name: "IP_Specialist".to_string(),
            provider_id: "sim".to_string(),
            model_name: "sim".to_string(),
            ..Default::default()
        };
        let agent_b = AgentConfig {
            name: "Risk_Analyst".to_string(),
            provider_id: "sim".to_string(),
            model_name: "sim".to_string(),
            ..Default::default()
        };

        let result_a = model
            .chat_completion(&agent_a, make_request())
            .await
            .unwrap();
        let result_b = model
            .chat_completion(&agent_b, make_request())
            .await
            .unwrap();

        // Different agents should get different content (different perspectives)
        let default_content = String::new();
        let content_a = result_a
            .response
            .choices
            .first()
            .and_then(|c| c.message.content.as_ref())
            .unwrap_or(&default_content);
        let content_b = result_b
            .response
            .choices
            .first()
            .and_then(|c| c.message.content.as_ref())
            .unwrap_or(&default_content);
        assert_ne!(
            content_a, content_b,
            "Different agents should produce different proposals"
        );
    }

    // ── extract_task_description tests ──────────────────────────────────

    #[test]
    fn test_extract_task_description_basic() {
        let messages = vec![ChatCompletionRequestMessage::User(
            ChatCompletionRequestUserMessage {
                content: ChatCompletionRequestUserMessageContent::Text(
                    "<task>Solve this problem</task>".to_string(),
                ),
                ..Default::default()
            },
        )];
        let task = SimulatedModel::extract_task_description(&messages);
        assert_eq!(task, Some("Solve this problem".to_string()));
    }

    #[test]
    fn test_extract_task_description_missing_tag() {
        let messages = vec![ChatCompletionRequestMessage::User(
            ChatCompletionRequestUserMessage {
                content: ChatCompletionRequestUserMessageContent::Text(
                    "There are no task tags in this message at all".to_string(),
                ),
                ..Default::default()
            },
        )];
        let task = SimulatedModel::extract_task_description(&messages);
        assert_eq!(task, None);
    }

    #[test]
    fn test_extract_task_description_empty_tag() {
        let messages = vec![ChatCompletionRequestMessage::User(
            ChatCompletionRequestUserMessage {
                content: ChatCompletionRequestUserMessageContent::Text("<task></task>".to_string()),
                ..Default::default()
            },
        )];
        let task = SimulatedModel::extract_task_description(&messages);
        // Empty inner text is rejected by the `!inner.is_empty()` guard → None
        assert_eq!(task, None);
    }

    #[test]
    fn test_extract_task_description_nested() {
        // The parser finds the first <task> and then the first </task> after it,
        // so nested tags produce "outer<task>inner" (everything between the
        // first <task> and first </task>).
        let messages = vec![ChatCompletionRequestMessage::User(
            ChatCompletionRequestUserMessage {
                content: ChatCompletionRequestUserMessageContent::Text(
                    "<task>outer<task>inner</task></task>".to_string(),
                ),
                ..Default::default()
            },
        )];
        let task = SimulatedModel::extract_task_description(&messages);
        // Should return Some — the function grabs text between first <task> and
        // first </task>: "outer<task>inner"
        assert!(task.is_some(), "Nested tags should still extract something");
        let extracted = task.unwrap();
        assert!(
            extracted.contains("outer"),
            "Extracted text should contain 'outer', got: {extracted}"
        );
    }

    // ── detect_scenario tests ──────────────────────────────────────────

    #[test]
    fn test_detect_scenario_supply_chain() {
        assert_eq!(
            SimulatedModel::detect_scenario("supply chain disruption"),
            "supply"
        );
        assert_eq!(
            SimulatedModel::detect_scenario("logistics bottleneck in the warehouse"),
            "supply"
        );
    }

    #[test]
    fn test_detect_scenario_security() {
        assert_eq!(
            SimulatedModel::detect_scenario("vulnerability in the vault contract"),
            "security"
        );
        assert_eq!(
            SimulatedModel::detect_scenario("perform a full audit of the system"),
            "security"
        );
    }

    #[test]
    fn test_detect_scenario_quant() {
        assert_eq!(
            SimulatedModel::detect_scenario("optimize the portfolio allocation"),
            "quant"
        );
        assert_eq!(
            SimulatedModel::detect_scenario("algorithmic trading strategy"),
            "quant"
        );
    }

    #[test]
    fn test_detect_scenario_legal() {
        assert_eq!(
            SimulatedModel::detect_scenario("review the contract terms"),
            "legal"
        );
        assert_eq!(
            SimulatedModel::detect_scenario("assess liability exposure in the agreement"),
            "legal"
        );
    }

    #[test]
    fn test_detect_scenario_generic() {
        assert_eq!(
            SimulatedModel::detect_scenario("explain how photosynthesis works"),
            "generic"
        );
        assert_eq!(
            SimulatedModel::detect_scenario("write a poem about the ocean"),
            "generic"
        );
    }

    #[test]
    fn test_detect_scenario_case_insensitive() {
        assert_eq!(
            SimulatedModel::detect_scenario("SUPPLY CHAIN risk assessment"),
            "supply"
        );
        assert_eq!(
            SimulatedModel::detect_scenario("SMART CONTRACT AUDIT"),
            "security"
        );
        assert_eq!(
            SimulatedModel::detect_scenario("PORTFOLIO REBALANCING"),
            "quant"
        );
        assert_eq!(
            SimulatedModel::detect_scenario("LICENSE AGREEMENT review"),
            "legal"
        );
    }

    // ── dm_user_message tests ──────────────────────────────────────────

    #[test]
    fn test_dm_user_message_supply() {
        let msg = SimulatedModel::dm_user_message("supply");
        // Supply-specific question mentions air-freight or ShenZhen
        assert!(
            msg.contains("air-freight") || msg.contains("ShenZhen"),
            "Supply DM should be supply-specific, got: {msg}"
        );
    }

    #[test]
    fn test_dm_user_message_generic() {
        let msg = SimulatedModel::dm_user_message("generic");
        // Generic fallback mentions "clarifying question"
        assert!(
            msg.contains("clarifying question"),
            "Generic DM should contain 'clarifying question', got: {msg}"
        );
    }

    #[test]
    fn test_dm_user_message_unknown() {
        // An unrecognized scenario falls into the `_` arm → same as generic
        let msg = SimulatedModel::dm_user_message("unknown-scenario");
        assert!(
            !msg.is_empty(),
            "Unknown scenario should still return a non-empty message"
        );
        // Should match the default/generic arm
        assert!(
            msg.contains("clarifying question"),
            "Unknown scenario should fall back to generic DM, got: {msg}"
        );
    }

    // ── proposal function tests ────────────────────────────────────────

    #[test]
    fn test_generic_proposal_round_1() {
        let (thought, content) = SimulatedModel::generic_proposal("Agent_A", 1, Some("test task"));
        assert!(
            thought.contains("Round 1"),
            "Thought should mention round 1, got: {thought}"
        );
        assert!(
            thought.contains("baseline"),
            "Round 1 thought should mention 'baseline', got: {thought}"
        );
        assert!(
            thought.contains("Agent_A"),
            "Thought should contain agent name, got: {thought}"
        );
        assert!(
            !content.is_empty(),
            "Content should be non-empty, got: {content}"
        );

        // Verify the generate_fake_proposal wrapper produces valid JSON
        let model = SimulatedModel::new("test".to_string(), 0);
        let json_str = model.generate_fake_proposal("Agent_A", 1, Some("test task"));
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert!(
            parsed.get("thought_process").is_some(),
            "Missing thought_process field"
        );
        assert!(
            parsed.get("solution_content").is_some(),
            "Missing solution_content field"
        );
    }

    #[test]
    fn test_generic_proposal_round_2() {
        let (thought, content) = SimulatedModel::generic_proposal("Agent_B", 2, Some("test task"));
        assert!(
            thought.contains("Round 2"),
            "Thought should mention round 2, got: {thought}"
        );
        assert!(
            thought.contains("refining") || thought.contains("feedback"),
            "Round 2 thought should mention feedback adaptation, got: {thought}"
        );
        assert!(
            thought.contains("Agent_B"),
            "Thought should contain agent name, got: {thought}"
        );
        assert!(!content.is_empty(), "Content should be non-empty");

        // Valid JSON
        let model = SimulatedModel::new("test".to_string(), 0);
        let json_str = model.generate_fake_proposal("Agent_B", 2, Some("test task"));
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert!(
            parsed.get("thought_process").is_some(),
            "Missing thought_process field"
        );
        assert!(
            parsed.get("solution_content").is_some(),
            "Missing solution_content field"
        );
    }

    #[test]
    fn test_supply_chain_proposal() {
        let (thought, content) = SimulatedModel::supply_chain_proposal("Supply_Agent", 1, 0);

        // Verify content is supply chain related
        assert!(
            content.contains("Inventory")
                || content.contains("Sourcing")
                || content.contains("Supply"),
            "Supply chain proposal should contain supply chain content, got: {content}"
        );

        // Verify JSON wrapper
        let model = SimulatedModel::new("test".to_string(), 0);
        let json_str =
            model.generate_fake_proposal("Supply_Agent", 1, Some("supply chain disruption"));
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert!(
            parsed.get("thought_process").is_some(),
            "Missing thought_process field"
        );
        assert!(
            parsed.get("solution_content").is_some(),
            "Missing solution_content field"
        );

        // Thought should contain agent name
        assert!(
            thought.contains("Supply_Agent"),
            "Thought should contain agent name"
        );
    }

    #[test]
    fn test_security_proposal() {
        let (thought, content) = SimulatedModel::security_proposal("Sec_Agent", 1, 0);

        // Verify content is security related
        assert!(
            content.contains("Re-Entrancy")
                || content.contains("Vulnerability")
                || content.contains("Security")
                || content.contains("Static Analysis")
                || content.contains("Fuzz"),
            "Security proposal should contain security content, got: {}",
            &content[..200.min(content.len())]
        );

        // Verify JSON wrapper
        let model = SimulatedModel::new("test".to_string(), 0);
        let json_str = model.generate_fake_proposal("Sec_Agent", 1, Some("smart contract audit"));
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert!(
            parsed.get("thought_process").is_some(),
            "Missing thought_process field"
        );
        assert!(
            parsed.get("solution_content").is_some(),
            "Missing solution_content field"
        );

        assert!(
            thought.contains("Sec_Agent"),
            "Thought should contain agent name"
        );
    }

    #[test]
    fn test_quant_proposal() {
        let (thought, content) = SimulatedModel::quant_proposal("Quant_Agent", 1, 0);

        // Verify content is quant related
        assert!(
            content.contains("Allocation")
                || content.contains("Portfolio")
                || content.contains("Factor")
                || content.contains("Hedge")
                || content.contains("Rebalancing"),
            "Quant proposal should contain quant content, got: {}",
            &content[..200.min(content.len())]
        );

        // Verify JSON wrapper
        let model = SimulatedModel::new("test".to_string(), 0);
        let json_str = model.generate_fake_proposal("Quant_Agent", 1, Some("portfolio allocation"));
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert!(
            parsed.get("thought_process").is_some(),
            "Missing thought_process field"
        );
        assert!(
            parsed.get("solution_content").is_some(),
            "Missing solution_content field"
        );

        assert!(
            thought.contains("Quant_Agent"),
            "Thought should contain agent name"
        );
    }

    #[test]
    fn test_legal_proposal() {
        let (thought, content) = SimulatedModel::legal_proposal("Legal_Agent", 1, 0);

        // Verify content is legal related
        assert!(
            content.contains("IP")
                || content.contains("Liability")
                || content.contains("Termination")
                || content.contains("Non-Compete")
                || content.contains("License"),
            "Legal proposal should contain legal content, got: {}",
            &content[..200.min(content.len())]
        );

        // Verify JSON wrapper
        let model = SimulatedModel::new("test".to_string(), 0);
        let json_str =
            model.generate_fake_proposal("Legal_Agent", 1, Some("contract review clause"));
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert!(
            parsed.get("thought_process").is_some(),
            "Missing thought_process field"
        );
        assert!(
            parsed.get("solution_content").is_some(),
            "Missing solution_content field"
        );

        assert!(
            thought.contains("Legal_Agent"),
            "Thought should contain agent name"
        );
    }

    // =========================================================================
    // generate_justification coverage — all scenarios x (high, low, mid) x seeds
    // =========================================================================

    #[test]
    fn test_justification_supply_high() {
        for variant in 0..4 {
            let j = SimulatedModel::generate_justification("supply", 1, true, false, variant, 0);
            assert!(
                !j.is_empty(),
                "supply high variant {variant} should not be empty"
            );
        }
    }

    #[test]
    fn test_justification_supply_low() {
        for variant in 0..4 {
            let j = SimulatedModel::generate_justification("supply", 1, false, true, variant, 0);
            assert!(
                !j.is_empty(),
                "supply low variant {variant} should not be empty"
            );
        }
    }

    #[test]
    fn test_justification_supply_mid() {
        for variant in 0..4 {
            let j = SimulatedModel::generate_justification("supply", 2, false, false, variant, 0);
            assert!(
                !j.is_empty(),
                "supply mid variant {variant} should not be empty"
            );
        }
    }

    #[test]
    fn test_justification_security_high() {
        for variant in 0..4 {
            let j = SimulatedModel::generate_justification("security", 1, true, false, variant, 0);
            assert!(
                !j.is_empty(),
                "security high variant {variant} should not be empty"
            );
        }
    }

    #[test]
    fn test_justification_security_low() {
        for variant in 0..4 {
            let j = SimulatedModel::generate_justification("security", 1, false, true, variant, 0);
            assert!(
                !j.is_empty(),
                "security low variant {variant} should not be empty"
            );
        }
    }

    #[test]
    fn test_justification_security_mid() {
        for variant in 0..4 {
            let j = SimulatedModel::generate_justification("security", 2, false, false, variant, 0);
            assert!(
                !j.is_empty(),
                "security mid variant {variant} should not be empty"
            );
        }
    }

    #[test]
    fn test_justification_quant_high() {
        for variant in 0..4 {
            let j = SimulatedModel::generate_justification("quant", 1, true, false, variant, 0);
            assert!(
                !j.is_empty(),
                "quant high variant {variant} should not be empty"
            );
        }
    }

    #[test]
    fn test_justification_quant_low() {
        for variant in 0..4 {
            let j = SimulatedModel::generate_justification("quant", 1, false, true, variant, 0);
            assert!(
                !j.is_empty(),
                "quant low variant {variant} should not be empty"
            );
        }
    }

    #[test]
    fn test_justification_quant_mid() {
        for variant in 0..4 {
            let j = SimulatedModel::generate_justification("quant", 2, false, false, variant, 0);
            assert!(
                !j.is_empty(),
                "quant mid variant {variant} should not be empty"
            );
        }
    }

    #[test]
    fn test_justification_legal_high() {
        for variant in 0..4 {
            let j = SimulatedModel::generate_justification("legal", 1, true, false, variant, 0);
            assert!(
                !j.is_empty(),
                "legal high variant {variant} should not be empty"
            );
        }
    }

    #[test]
    fn test_justification_legal_low() {
        for variant in 0..4 {
            let j = SimulatedModel::generate_justification("legal", 1, false, true, variant, 0);
            assert!(
                !j.is_empty(),
                "legal low variant {variant} should not be empty"
            );
        }
    }

    #[test]
    fn test_justification_legal_mid() {
        for variant in 0..4 {
            let j = SimulatedModel::generate_justification("legal", 2, false, false, variant, 0);
            assert!(
                !j.is_empty(),
                "legal mid variant {variant} should not be empty"
            );
        }
    }

    #[test]
    fn test_justification_generic_high() {
        for variant in 0..4 {
            let j = SimulatedModel::generate_justification("generic", 1, true, false, variant, 0);
            assert!(
                !j.is_empty(),
                "generic high variant {variant} should not be empty"
            );
        }
    }

    #[test]
    fn test_justification_generic_low() {
        for variant in 0..4 {
            let j = SimulatedModel::generate_justification("generic", 1, false, true, variant, 0);
            assert!(
                !j.is_empty(),
                "generic low variant {variant} should not be empty"
            );
        }
    }

    #[test]
    fn test_justification_generic_mid() {
        for variant in 0..4 {
            let j = SimulatedModel::generate_justification("generic", 2, false, false, variant, 0);
            assert!(
                !j.is_empty(),
                "generic mid variant {variant} should not be empty"
            );
        }
    }

    #[test]
    fn test_justification_unknown_scenario_falls_to_generic() {
        let j = SimulatedModel::generate_justification("unknown", 1, true, false, 0, 0);
        assert!(
            !j.is_empty(),
            "Unknown scenario should use generic justifications"
        );
    }

    // =========================================================================
    // Proposal perspectives 1, 2, 3 coverage
    // =========================================================================

    #[test]
    fn test_supply_chain_proposal_all_perspectives() {
        for perspective in 0..4 {
            let (thought, content) = SimulatedModel::supply_chain_proposal("Agent", 1, perspective);
            assert!(
                !thought.is_empty(),
                "supply perspective {perspective} thought should not be empty"
            );
            assert!(
                !content.is_empty(),
                "supply perspective {perspective} content should not be empty"
            );
            assert!(
                thought.contains("Agent"),
                "thought should contain agent name for perspective {perspective}"
            );
        }
        // Verify perspective 3 (financial modeler) mentions financial terms
        let (_, content) = SimulatedModel::supply_chain_proposal("Agent", 1, 3);
        assert!(
            content.contains("Financial")
                || content.contains("Scenario")
                || content.contains("Revenue"),
            "Perspective 3 should be financial-related"
        );
    }

    #[test]
    fn test_security_proposal_all_perspectives() {
        for perspective in 0..4 {
            let (thought, content) = SimulatedModel::security_proposal("Agent", 1, perspective);
            assert!(
                !thought.is_empty(),
                "security perspective {perspective} thought should not be empty"
            );
            assert!(
                !content.is_empty(),
                "security perspective {perspective} content should not be empty"
            );
        }
        // Perspective 1: static analysis
        let (_, content_1) = SimulatedModel::security_proposal("Agent", 1, 1);
        assert!(content_1.contains("Static Analysis") || content_1.contains("Slither"));
        // Perspective 2: fuzz testing
        let (_, content_2) = SimulatedModel::security_proposal("Agent", 1, 2);
        assert!(content_2.contains("Fuzz") || content_2.contains("Echidna"));
        // Perspective 3: regulatory/synthesis
        let (_, content_3) = SimulatedModel::security_proposal("Agent", 1, 3);
        assert!(content_3.contains("Remediation") || content_3.contains("Rating"));
    }

    #[test]
    fn test_quant_proposal_all_perspectives() {
        for perspective in 0..4 {
            let (thought, content) = SimulatedModel::quant_proposal("Agent", 1, perspective);
            assert!(
                !thought.is_empty(),
                "quant perspective {perspective} thought should not be empty"
            );
            assert!(
                !content.is_empty(),
                "quant perspective {perspective} content should not be empty"
            );
        }
        // Perspective 1: factor/quant specialist
        let (_, content_1) = SimulatedModel::quant_proposal("Agent", 1, 1);
        assert!(content_1.contains("Factor") || content_1.contains("Quality"));
        // Perspective 2: tail risk specialist
        let (_, content_2) = SimulatedModel::quant_proposal("Agent", 1, 2);
        assert!(
            content_2.contains("Tail Risk")
                || content_2.contains("Hedge")
                || content_2.contains("SPX")
        );
        // Perspective 3: rebalancing
        let (_, content_3) = SimulatedModel::quant_proposal("Agent", 1, 3);
        assert!(
            content_3.contains("Rebalancing")
                || content_3.contains("Circuit")
                || content_3.contains("Drawdown")
        );
    }

    #[test]
    fn test_legal_proposal_all_perspectives() {
        for perspective in 0..4 {
            let (thought, content) = SimulatedModel::legal_proposal("Agent", 1, perspective);
            assert!(
                !thought.is_empty(),
                "legal perspective {perspective} thought should not be empty"
            );
            assert!(
                !content.is_empty(),
                "legal perspective {perspective} content should not be empty"
            );
        }
        // Perspective 1: liability
        let (_, content_1) = SimulatedModel::legal_proposal("Agent", 1, 1);
        assert!(content_1.contains("Liability") || content_1.contains("liability"));
        // Perspective 2: termination/portability
        let (_, content_2) = SimulatedModel::legal_proposal("Agent", 1, 2);
        assert!(
            content_2.contains("Termination")
                || content_2.contains("Portability")
                || content_2.contains("Data")
        );
        // Perspective 3: non-compete/SLA
        let (_, content_3) = SimulatedModel::legal_proposal("Agent", 1, 3);
        assert!(
            content_3.contains("Non-Compete")
                || content_3.contains("SLA")
                || content_3.contains("Non-compete")
        );
    }

    #[test]
    fn test_proposals_round_gt_1_adapts() {
        // All scenarios should adapt their thought process for round > 1
        let (thought_r1, _) = SimulatedModel::supply_chain_proposal("Agent", 1, 0);
        let (thought_r2, _) = SimulatedModel::supply_chain_proposal("Agent", 2, 0);
        assert_ne!(
            thought_r1, thought_r2,
            "Round 1 and 2 thoughts should differ for supply"
        );

        let (thought_r1, _) = SimulatedModel::security_proposal("Agent", 1, 1);
        let (thought_r2, _) = SimulatedModel::security_proposal("Agent", 2, 1);
        assert_ne!(
            thought_r1, thought_r2,
            "Round 1 and 2 thoughts should differ for security"
        );

        let (thought_r1, _) = SimulatedModel::quant_proposal("Agent", 1, 2);
        let (thought_r2, _) = SimulatedModel::quant_proposal("Agent", 2, 2);
        assert_ne!(
            thought_r1, thought_r2,
            "Round 1 and 2 thoughts should differ for quant"
        );

        let (thought_r1, _) = SimulatedModel::legal_proposal("Agent", 1, 3);
        let (thought_r2, _) = SimulatedModel::legal_proposal("Agent", 2, 3);
        assert_ne!(
            thought_r1, thought_r2,
            "Round 1 and 2 thoughts should differ for legal"
        );
    }

    // =========================================================================
    // SimulatedShared::default() and SimulatedModel::shared()
    // =========================================================================

    #[test]
    fn test_simulated_shared_default() {
        let shared = SimulatedShared::default();
        assert_eq!(shared.call_counter.load(Ordering::Relaxed), 0);
        assert!(!shared.dm_fired.load(Ordering::Relaxed));
    }

    #[test]
    fn test_simulated_model_shared_getter() {
        let model = SimulatedModel::new("test-model".to_string(), 100);
        let shared = model.shared();
        // Should return a clone of the internal Arc
        assert_eq!(shared.call_counter.load(Ordering::Relaxed), 0);
        assert!(!shared.dm_fired.load(Ordering::Relaxed));

        // Mutate through the shared reference — should be visible from model
        shared.call_counter.store(42, Ordering::Relaxed);
        let shared2 = model.shared();
        assert_eq!(shared2.call_counter.load(Ordering::Relaxed), 42);
    }

    // =========================================================================
    // generate_fake_evaluation with multiple candidates
    // =========================================================================

    #[test]
    fn test_generate_fake_evaluation_with_candidates() {
        let model = SimulatedModel::new("test-eval".to_string(), 0);
        let candidates = vec!["Alice".to_string(), "Bob".to_string(), "Carol".to_string()];
        let result = model.generate_fake_evaluation(
            "test_agent",
            1,
            &candidates,
            Some("supply chain disruption"),
        );
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let evals = parsed["evaluations"].as_array().unwrap();
        assert_eq!(evals.len(), 3, "Should have one evaluation per candidate");

        // Each eval should have the required fields
        for eval in evals {
            assert!(eval.get("candidate_id").is_some());
            assert!(eval.get("endorsement_weight").is_some());
            assert!(eval.get("justification").is_some());
            assert!(eval.get("is_final_solution").is_some());
            let w = eval["endorsement_weight"].as_f64().unwrap();
            assert!(
                (-100.0..=100.0).contains(&w),
                "Weight should be in [-100, 100], got {w}"
            );
        }
    }

    #[test]
    fn test_generate_fake_evaluation_empty_candidates() {
        let model = SimulatedModel::new("test-eval".to_string(), 0);
        let result = model.generate_fake_evaluation("test_agent", 1, &[], None);
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let evals = parsed["evaluations"].as_array().unwrap();
        assert_eq!(
            evals.len(),
            1,
            "Empty candidates should produce fallback single eval"
        );
        assert_eq!(evals[0]["candidate_id"], "Other_Agent");
    }

    #[test]
    fn test_generate_fake_evaluation_convergence_over_rounds() {
        let model = SimulatedModel::new("convergence-test".to_string(), 0);
        let candidates = vec!["A".to_string(), "B".to_string()];

        // Round 1: weights may differ significantly between evaluators
        let r1 = model.generate_fake_evaluation(
            "test_agent",
            1,
            &candidates,
            Some("smart contract audit"),
        );
        let r1_parsed: serde_json::Value = serde_json::from_str(&r1).unwrap();
        let r1_evals = r1_parsed["evaluations"].as_array().unwrap();

        // Round 4: weights should converge more toward consensus
        let r4 = model.generate_fake_evaluation(
            "test_agent",
            4,
            &candidates,
            Some("smart contract audit"),
        );
        let r4_parsed: serde_json::Value = serde_json::from_str(&r4).unwrap();
        let r4_evals = r4_parsed["evaluations"].as_array().unwrap();

        // Both rounds should produce valid evaluations
        assert_eq!(r1_evals.len(), 2);
        assert_eq!(r4_evals.len(), 2);
    }

    /// Simulated evaluations must include the structured fields added in the
    /// "structured evaluation feedback" feature (stance, claim_assessments,
    /// disagreements, category_scores).  Without them the agent's
    /// `BatchEvaluationItem` parser silently drops the data and the
    /// orchestrator cannot compute claim-level convergence or controversy scores.
    #[test]
    fn test_generate_fake_evaluation_includes_structured_fields() {
        let model = SimulatedModel::new("structured-test".to_string(), 0);
        let candidates = vec!["Alice".to_string(), "Bob".to_string()];
        let result = model.generate_fake_evaluation(
            "test_agent",
            1,
            &candidates,
            Some("supply chain disruption"),
        );
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let evals = parsed["evaluations"].as_array().unwrap();
        assert!(!evals.is_empty());

        for eval in evals {
            // ── stance ────────────────────────────────────────────────────────
            let stance = eval["stance"].as_str().expect("stance must be a string");
            assert!(
                [
                    "strong_agree",
                    "agree",
                    "neutral",
                    "disagree",
                    "strong_disagree"
                ]
                .contains(&stance),
                "stance must be a valid snake_case enum value, got: {stance}"
            );

            // ── claim_assessments ─────────────────────────────────────────────
            let claims = eval["claim_assessments"]
                .as_array()
                .expect("claim_assessments must be an array");
            assert!(
                !claims.is_empty(),
                "claim_assessments must contain at least one entry"
            );
            for claim in claims {
                let verdict = claim["verdict"].as_str().expect("claim must have verdict");
                assert!(
                    ["verified", "contested", "unverified", "wrong", "unknown"].contains(&verdict),
                    "verdict must be a valid value, got: {verdict}"
                );
            }

            // ── disagreements ─────────────────────────────────────────────────
            assert!(
                eval["disagreements"].is_array(),
                "disagreements must be an array"
            );

            // ── category_scores ───────────────────────────────────────────────
            let scores = &eval["category_scores"];
            for field in &[
                "correctness",
                "completeness",
                "novelty",
                "feasibility",
                "evidence_quality",
            ] {
                let v = scores[field]
                    .as_f64()
                    .unwrap_or_else(|| panic!("category_scores.{field} must be a number"));
                assert!(
                    (-100.0..=100.0).contains(&v),
                    "category_scores.{field} must be in [-100,100], got {v}"
                );
            }
        }
    }

    // =========================================================================
    // generate_disagreements — round-aware count
    // =========================================================================

    /// Round 1 low candidates must produce 2 disagreement points (high contention).
    #[test]
    fn test_disagreements_low_round1_produces_two_points() {
        let pts = SimulatedModel::generate_disagreements("quant", true, false, 1, 0);
        assert_eq!(
            pts.len(),
            2,
            "Round 1 low candidate must generate 2 disagreement points; got {}",
            pts.len()
        );
    }

    /// Round 2 low candidates still produce 2 points (contention not yet resolved).
    #[test]
    fn test_disagreements_low_round2_produces_two_points() {
        let pts = SimulatedModel::generate_disagreements("quant", true, false, 2, 0);
        assert_eq!(
            pts.len(),
            2,
            "Round 2 low candidate must still generate 2 disagreement points; got {}",
            pts.len()
        );
    }

    /// Round 3+ tapers to 1 point (convergence underway).
    #[test]
    fn test_disagreements_low_round3_produces_one_point() {
        let pts = SimulatedModel::generate_disagreements("quant", true, false, 3, 0);
        assert_eq!(
            pts.len(),
            1,
            "Round 3+ low candidate must taper to 1 disagreement point; got {}",
            pts.len()
        );
    }

    /// Round 1 medium candidates produce 1 point (residual uncertainty).
    #[test]
    fn test_disagreements_medium_round1_produces_one_point() {
        let pts = SimulatedModel::generate_disagreements("quant", false, true, 1, 0);
        assert_eq!(
            pts.len(),
            1,
            "Round 1 medium candidate must produce 1 disagreement point; got {}",
            pts.len()
        );
    }

    /// Round 2+ medium candidates produce nothing (converging).
    #[test]
    fn test_disagreements_medium_round2_produces_zero() {
        let pts = SimulatedModel::generate_disagreements("quant", false, true, 2, 0);
        assert_eq!(
            pts.len(),
            0,
            "Round 2+ medium candidate must produce 0 disagreement points; got {}",
            pts.len()
        );
    }

    /// High candidates never produce disagreements regardless of round.
    #[test]
    fn test_disagreements_high_always_zero() {
        for round in 1..=4 {
            let pts = SimulatedModel::generate_disagreements("quant", false, false, round, 0);
            assert_eq!(
                pts.len(),
                0,
                "High candidate must produce 0 disagreements in round {round}; got {}",
                pts.len()
            );
        }
    }

    /// Each scenario should have 2 distinct disagreement items available
    /// so round-1 outputs are not repetitive.
    #[test]
    fn test_disagreements_round1_items_are_distinct() {
        for scenario in &["supply", "security", "quant", "legal", ""] {
            let pts = SimulatedModel::generate_disagreements(scenario, true, false, 1, 0);
            assert_eq!(
                pts.len(),
                2,
                "scenario '{scenario}' must produce 2 points in round 1"
            );
            let pos0 = pts[0]["evaluator_position"].as_str().unwrap();
            let pos1 = pts[1]["evaluator_position"].as_str().unwrap();
            assert_ne!(
                pos0, pos1,
                "Round-1 disagreement items must be distinct for scenario '{scenario}'"
            );
        }
    }

    // =========================================================================
    // generate_category_scores — jitter stays in [-4, +5]
    // =========================================================================

    /// Regression: when `evaluator_seed as i32` is negative, Rust's `%` operator
    /// returns a negative remainder, pushing jitter below the intended -4 floor.
    ///
    /// Example: seed = u32::MAX - 8  →  i32 value = -9
    ///   broken:  (-9) % 10 - 4 = -9 - 4 = -13   (score of 37 for weight=50)
    ///   fixed:   (-9).rem_euclid(10) - 4 = 1 - 4 = -3  (score of 47 ✓)
    #[test]
    fn test_category_scores_jitter_stays_in_bounds() {
        let bad_seed = u32::MAX - 8; // i32 == -9; (-9) % 10 == -9 in Rust
        let scores = SimulatedModel::generate_category_scores(50.0, bad_seed, 0);
        let correctness = scores["correctness"].as_f64().unwrap();
        // For weight=50 and valid jitter in [-4, +5], score must be in [46, 55].
        assert!(
            (46.0..=55.0).contains(&correctness),
            "jitter must stay in [-4, +5] (score 46–55 for weight=50); got {correctness}"
        );
    }

    // =========================================================================
    // generate_claim_assessments — medium verdict/reason alignment
    // =========================================================================

    /// Regression: the medium-candidate branch (both is_high=false and is_low=false)
    /// was selecting r0=r0l (a negative/critical reason) but pairing it with
    /// v0="verified" — a logical contradiction.
    ///
    /// Correct mapping for medium: v0="verified" → r0=r0h (positive), v1="unverified" → r1=r1l (negative).
    #[test]
    fn test_claim_assessments_medium_reasons_match_verdicts() {
        // Use the fallback ("") scenario so the reason strings are predictable.
        // Round 2: medium tier produces (verified, unverified) — the original
        // regression scenario where verdict/reason must agree.
        let assessments = SimulatedModel::generate_claim_assessments("", false, false, 42, 0, 2);
        assert_eq!(assessments.len(), 2);
        let r0 = assessments[0]["reason"].as_str().unwrap();
        let r1 = assessments[1]["reason"].as_str().unwrap();
        // v0="verified" → must use r0h (the positive / supportive reason)
        assert!(
            !r0.contains("lack") && !r0.contains("not independently"),
            "medium 'verified' claim must get the positive (r0h) reason, got: {r0}"
        );
        // v1="unverified" → must use r1l (the negative / critical reason)
        assert!(
            r1.contains("incomplete") || r1.contains("not addressed"),
            "medium 'unverified' claim must get the negative (r1l) reason, got: {r1}"
        );
    }

    #[test]
    fn test_claim_verdicts_evolve_over_rounds() {
        // High tier: round 1 → (verified, unverified), round 2+ → (verified, verified)
        let r1 = SimulatedModel::generate_claim_assessments("quant", true, false, 42, 0, 1);
        assert_eq!(r1[0]["verdict"], "verified");
        assert_eq!(r1[1]["verdict"], "unverified");
        let r2 = SimulatedModel::generate_claim_assessments("quant", true, false, 42, 0, 2);
        assert_eq!(r2[0]["verdict"], "verified");
        assert_eq!(r2[1]["verdict"], "verified");

        // Medium tier: round 1 → (unverified, unverified), round 2 → (verified, unverified),
        // round 3+ → (verified, verified)
        let r1 = SimulatedModel::generate_claim_assessments("quant", false, false, 42, 0, 1);
        assert_eq!(r1[0]["verdict"], "unverified");
        assert_eq!(r1[1]["verdict"], "unverified");
        let r3 = SimulatedModel::generate_claim_assessments("quant", false, false, 42, 0, 3);
        assert_eq!(r3[0]["verdict"], "verified");
        assert_eq!(r3[1]["verdict"], "verified");

        // Low tier: round 1 → (contested, wrong), round 3+ → (contested, verified)
        let r1 = SimulatedModel::generate_claim_assessments("quant", false, true, 42, 0, 1);
        assert_eq!(r1[0]["verdict"], "contested");
        assert_eq!(r1[1]["verdict"], "wrong");
        let r3 = SimulatedModel::generate_claim_assessments("quant", false, true, 42, 0, 3);
        assert_eq!(r3[0]["verdict"], "contested");
        assert_eq!(r3[1]["verdict"], "verified");
    }

    #[test]
    fn test_different_candidates_get_different_claims() {
        // Each candidate_idx should produce unique claim text
        let a0 = SimulatedModel::generate_claim_assessments("quant", false, false, 42, 0, 2);
        let a1 = SimulatedModel::generate_claim_assessments("quant", false, false, 42, 1, 2);
        let a2 = SimulatedModel::generate_claim_assessments("quant", false, false, 42, 2, 2);

        let c0 = a0[0]["claim"].as_str().unwrap();
        let c1 = a1[0]["claim"].as_str().unwrap();
        let c2 = a2[0]["claim"].as_str().unwrap();

        assert_ne!(c0, c1, "Candidate 0 and 1 must have different claims");
        assert_ne!(c1, c2, "Candidate 1 and 2 must have different claims");
        // Candidate 0 and 2 may alias if pool.len() == 3 — that's fine (wraps)
    }

    // =========================================================================
    // dm_user_message — scenario-specific messages
    // =========================================================================

    #[test]
    fn test_dm_user_message_security() {
        let msg = SimulatedModel::dm_user_message("security");
        assert!(
            msg.contains("reentrancy") || msg.contains("vault") || msg.contains("withdraw"),
            "Security DM should be security-specific, got: {msg}"
        );
    }

    #[test]
    fn test_dm_user_message_quant() {
        let msg = SimulatedModel::dm_user_message("quant");
        assert!(
            msg.contains("concentration") || msg.contains("collar") || msg.contains("SPY"),
            "Quant DM should be quant-specific, got: {msg}"
        );
    }

    #[test]
    fn test_dm_user_message_legal() {
        let msg = SimulatedModel::dm_user_message("legal");
        assert!(
            msg.contains("data science") || msg.contains("CloudMatrix") || msg.contains("IP"),
            "Legal DM should be legal-specific, got: {msg}"
        );
    }

    // =========================================================================
    // Generic proposal with long task snippet
    // =========================================================================

    #[test]
    fn test_generic_proposal_long_task_truncation() {
        let long_task = "x".repeat(200);
        let (thought, _) = SimulatedModel::generic_proposal("Agent", 1, Some(&long_task));
        // Task snippet should be truncated to 120 chars + "..."
        assert!(
            thought.contains("..."),
            "Long task should be truncated in thought"
        );
    }

    #[test]
    fn test_generic_proposal_no_task() {
        let (thought, _) = SimulatedModel::generic_proposal("Agent", 1, None);
        assert!(
            thought.contains("the given task"),
            "No task should use fallback phrase"
        );
    }

    #[test]
    fn test_generic_proposal_round_3_synthesis() {
        let (thought, _) = SimulatedModel::generic_proposal("Agent", 3, Some("test"));
        assert!(
            thought.contains("synthesizing"),
            "Round 3 should mention synthesis"
        );
    }

    #[test]
    fn test_generic_proposal_round_4_plus_optimization() {
        let (thought, _) = SimulatedModel::generic_proposal("Agent", 5, Some("test"));
        assert!(
            thought.contains("optimization") || thought.contains("verification"),
            "Round 4+ should mention optimization/verification"
        );
    }

    // =========================================================================
    // extract_candidate_ids
    // =========================================================================

    #[test]
    fn test_extract_candidate_ids() {
        let messages = vec![
            ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
                content: ChatCompletionRequestUserMessageContent::Text(
                    r#"Review these: <candidate id="Alpha">proposal1</candidate> and <candidate id="Beta">proposal2</candidate>"#.to_string(),
                ),
                ..Default::default()
            }),
        ];
        let ids = SimulatedModel::extract_candidate_ids(&messages);
        assert_eq!(ids, vec!["Alpha".to_string(), "Beta".to_string()]);
    }

    #[test]
    fn test_extract_candidate_ids_dedup() {
        let messages = vec![ChatCompletionRequestMessage::User(
            ChatCompletionRequestUserMessage {
                content: ChatCompletionRequestUserMessageContent::Text(
                    r#"<candidate id="Alpha">p1</candidate> <candidate id="Alpha">p2</candidate>"#
                        .to_string(),
                ),
                ..Default::default()
            },
        )];
        let ids = SimulatedModel::extract_candidate_ids(&messages);
        assert_eq!(ids.len(), 1, "Duplicate IDs should be deduplicated");
        assert_eq!(ids[0], "Alpha");
    }

    #[test]
    fn test_extract_candidate_ids_none() {
        let messages = vec![ChatCompletionRequestMessage::User(
            ChatCompletionRequestUserMessage {
                content: ChatCompletionRequestUserMessageContent::Text(
                    "No candidate tags here".to_string(),
                ),
                ..Default::default()
            },
        )];
        let ids = SimulatedModel::extract_candidate_ids(&messages);
        assert!(ids.is_empty());
    }

    /// Regression: prompt text like `<candidate id="...">` (placeholder in instructions)
    /// must NOT be extracted as a real candidate ID.
    #[test]
    fn test_extract_candidate_ids_skips_placeholder_dots() {
        let prompt = r#"Use the exact candidate IDs shown in the <candidate id="..."> tags above.
            <candidate id="Candidate_A">proposal A</candidate>
            <candidate id="Candidate_B">proposal B</candidate>"#;
        let messages = vec![ChatCompletionRequestMessage::User(
            ChatCompletionRequestUserMessage {
                content: ChatCompletionRequestUserMessageContent::Text(prompt.to_string()),
                ..Default::default()
            },
        )];
        let ids = SimulatedModel::extract_candidate_ids(&messages);
        assert_eq!(ids, vec!["Candidate_A", "Candidate_B"]);
        assert!(
            !ids.contains(&"...".to_string()),
            "Placeholder '...' must not be extracted as a candidate ID"
        );
    }

    /// Regression: with N=3 agents, each evaluator sees 2 candidates.
    /// At least one candidate per evaluator MUST get a positive weight.
    /// This ensures the voting matrix isn't all-negative.
    #[test]
    fn test_evaluation_produces_at_least_one_positive_weight() {
        // Simulate 3 different evaluators, each seeing 2 of 3 candidates
        let candidate_sets: Vec<(&str, Vec<String>)> = vec![
            (
                "model-agent-A",
                vec!["Candidate_B".to_string(), "Candidate_C".to_string()],
            ),
            (
                "model-agent-B",
                vec!["Candidate_A".to_string(), "Candidate_C".to_string()],
            ),
            (
                "model-agent-C",
                vec!["Candidate_A".to_string(), "Candidate_B".to_string()],
            ),
        ];

        for round in 1..=3 {
            for (model_name, candidates) in &candidate_sets {
                let model = SimulatedModel::new(model_name.to_string(), 0);
                let json_str = model.generate_fake_evaluation(
                    "test_agent",
                    round,
                    candidates,
                    Some("test task"),
                );
                let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
                let evals = parsed["evaluations"].as_array().unwrap();

                let weights: Vec<f32> = evals
                    .iter()
                    .map(|e| e["endorsement_weight"].as_f64().unwrap() as f32)
                    .collect();
                let has_positive = weights.iter().any(|&w| w > 0.0);

                assert!(
                    has_positive,
                    "Round {round}, evaluator {model_name}: all weights negative! weights={weights:?}, candidates={candidates:?}"
                );
            }
        }
    }

    /// Same test but with the exact model names used in production sim mode
    #[test]
    fn test_evaluation_positive_weights_with_common_model_names() {
        // Common model names that might be configured
        let model_names = ["simulated", "sim", "simulated-model", "sim-agent", "test"];

        let candidates = vec!["Candidate_A".to_string(), "Candidate_B".to_string()];

        for model_name in &model_names {
            let model = SimulatedModel::new(model_name.to_string(), 0);
            for round in 1..=3 {
                let json_str =
                    model.generate_fake_evaluation("test_agent", round, &candidates, Some("task"));
                let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
                let evals = parsed["evaluations"].as_array().unwrap();

                let weights: Vec<f32> = evals
                    .iter()
                    .map(|e| e["endorsement_weight"].as_f64().unwrap() as f32)
                    .collect();
                let has_positive = weights.iter().any(|&w| w > 0.0);

                assert!(
                    has_positive,
                    "Model {model_name}, round {round}: all weights negative! weights={weights:?}"
                );
            }
        }
    }

    /// Verify that different agent_name values produce different evaluator seeds
    /// and thus different output for the same model/round/candidates.
    #[test]
    fn test_agent_name_influences_evaluation_seed() {
        let model = SimulatedModel::new("simulated".to_string(), 0);
        // Use 3+ candidates to increase sensitivity to seed differences
        let candidates = vec![
            "Candidate_A".to_string(),
            "Candidate_B".to_string(),
            "Candidate_C".to_string(),
        ];

        let json_a = model.generate_fake_evaluation("agent_alpha", 2, &candidates, Some("task"));
        let json_b = model.generate_fake_evaluation("agent_beta", 2, &candidates, Some("task"));

        // Full JSON should differ (weights, justifications, claim verdicts, etc.)
        assert_ne!(
            json_a, json_b,
            "Different agent_name should produce different evaluation output"
        );
    }

    /// Exact production sim mode scenario: 3 agents with SAME model_name,
    /// each evaluating 2 of 3 anonymized candidates.
    /// Dumps the full weight breakdown to verify at least one positive per evaluator.
    #[test]
    fn test_production_sim_mode_not_all_negative() {
        // All agents share the same model_name (typical sim config)
        let model = SimulatedModel::new("simulated".to_string(), 0);

        // Orchestrator assigns these anonymized IDs
        let all_candidates = ["Candidate_A", "Candidate_B", "Candidate_C"];

        for round in 1..=3 {
            for (eval_idx, evaluator) in all_candidates.iter().enumerate() {
                // Each evaluator sees all candidates EXCEPT self (N-1)
                let visible: Vec<String> = all_candidates
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != eval_idx)
                    .map(|(_, c)| c.to_string())
                    .collect();

                let json_str = model.generate_fake_evaluation(
                    "test_agent",
                    round,
                    &visible,
                    Some("test task"),
                );
                let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
                let evals = parsed["evaluations"].as_array().unwrap();

                let weights: Vec<(String, f64)> = evals
                    .iter()
                    .map(|e| {
                        (
                            e["candidate_id"].as_str().unwrap().to_string(),
                            e["endorsement_weight"].as_f64().unwrap(),
                        )
                    })
                    .collect();

                let has_positive = weights.iter().any(|(_, w)| *w > 0.0);

                // Print for debugging
                eprintln!(
                    "Round {round}, {evaluator} evaluates {:?}: {:?}",
                    visible, weights
                );

                assert!(
                    has_positive,
                    "Round {round}, evaluator {evaluator}: ALL weights negative!\n\
                     visible_candidates={visible:?}\n\
                     weights={weights:?}"
                );
            }
        }
    }

    /// Test: when ALL evaluators share the same model_name (typical sim config),
    /// the true_best is consistent because it's based on candidate IDs, not model_name.
    #[test]
    fn test_same_model_name_consistent_true_best() {
        let model = SimulatedModel::new("simulated".to_string(), 0);
        let candidates = vec!["Candidate_A".to_string(), "Candidate_B".to_string()];

        // Run multiple times — should always be deterministic
        let json1 = model.generate_fake_evaluation("test_agent", 1, &candidates, Some("task"));
        let json2 = model.generate_fake_evaluation("test_agent", 1, &candidates, Some("task"));

        let parsed1: serde_json::Value = serde_json::from_str(&json1).unwrap();
        let parsed2: serde_json::Value = serde_json::from_str(&json2).unwrap();

        let w1: Vec<f64> = parsed1["evaluations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["endorsement_weight"].as_f64().unwrap())
            .collect();
        let w2: Vec<f64> = parsed2["evaluations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["endorsement_weight"].as_f64().unwrap())
            .collect();

        assert_eq!(w1, w2, "Same inputs should produce same outputs");
    }
}
